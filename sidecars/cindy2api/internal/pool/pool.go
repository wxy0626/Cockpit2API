// pool.go —— 账号池：发现、健康检查、选号与失败降级。
//
// 池只回答两个问题：现在哪些账号可用、下一个请求该发给谁。协议内容它不关心，
// 因此上游是 OpenAI 兼容还是别的形态都不影响这一层。
package pool

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"sort"
	"strings"
	"sync"
	"time"

	"cindy2api/internal/cindyaccount"
	"cindy2api/internal/hiddenstore"
	"cindy2api/internal/manualaccounts"
)

// 账号级失败：这些状态码换一个账号重试是有意义的；业务错误（400 等）不换号。
var accountLevelStatus = map[int]bool{
	401: true, 403: true, 404: true, 429: true,
	500: true, 502: true, 503: true, 504: true,
}

// failureThreshold 连续失败达到该次数后账号被标记不可用并暂时跳过
const failureThreshold = 3

// checkTimeout 健康检查超时
const checkTimeout = 15 * time.Second

// IsAccountLevelFailure 判断某状态码是否值得换号重试。
func IsAccountLevelFailure(status int) bool { return accountLevelStatus[status] }

// describeConnectError 把底层连接错误翻译成**可操作**的提示。
//
// 最常见的两类失败都不是凭据问题，但原始错误（EOF / handshake failed）会让人
// 误以为账号坏了：
//   - TLS 握手被中断：本机代理软件把该域名判成直连（实测 api.laxa.com 就是这种）
//   - 超时 / 解析失败：网络或 DNS 问题
func DescribeConnectError(err error) string {
	text := err.Error()
	switch {
	case strings.Contains(text, "EOF"),
		strings.Contains(text, "handshake"),
		strings.Contains(text, "tls:"),
		strings.Contains(text, "connection reset"):
		return fmt.Sprintf(
			"TLS 握手被中断（%v）：TCP 已连上但握手中途被丢弃，说明中间有东西在拦截这个域名。"+
				"常见原因是本机代理软件把该域名判成了 REJECT / 直连（或节点本身到该域名不通）。"+
				"排查：在代理软件里查看该域名命中的规则与走的是哪个节点，必要时换节点重试；"+
				"若确认是分流判错，可在本服务 runtime/config.json 的 proxy 字段填代理地址强制走代理。", err)
	case strings.Contains(text, "timeout"), strings.Contains(text, "deadline exceeded"):
		return fmt.Sprintf("连接超时（%v）。本机到该域名网络不通或被拦截。", err)
	case strings.Contains(text, "no such host"):
		return fmt.Sprintf("域名解析失败（%v）。请检查 DNS 或代理软件的解析设置。", err)
	default:
		return fmt.Sprintf("连接失败: %v", err)
	}
}

// AccountState 账号在池中的运行态。
type AccountState struct {
	Account       cindyaccount.Account
	Status        string // unknown | ok | error
	Detail        string // 中文状态说明或具体错误
	Models        []string
	LatencyMs     int64
	Failures      int
	LastCheckedAt time.Time
	// AllowedModels 上游实测允许调用的模型集合。
	//
	// 背景：/v1/models 会列出全部模型，但真正发起 chat/completions 时，
	// 服务端按账号订阅二次校验（403 "user not allowed to access model"，
	// 并在错误里附上该账号实际可用的模型清单）。把这份清单记下来，
	// 选号时直接跳过没权限的账号，省掉无意义的上游请求。
	AllowedModels map[string]bool
	// LastDeniedModel 最近一次被 403 拒绝的模型（诊断用）
	LastDeniedModel string
}

// allowsModel 该账号是否（可能）支持指定模型。
//
// 没有任何 403 记录时返回 true —— 不能因为"还没验证过"就把账号排除，
// 否则新账号永远轮不到。只有在服务端明确拒绝过之后才开始过滤。
func (state *AccountState) allowsModel(model string) bool {
	if model == "" || len(state.AllowedModels) == 0 {
		return true
	}
	if state.AllowedModels[model] {
		return true
	}
	// 服务端白名单支持通配（如 cindy/*），逐段比对一次
	for pattern := range state.AllowedModels {
		if strings.HasSuffix(pattern, "/*") && strings.HasPrefix(model, strings.TrimSuffix(pattern, "*")) {
			return true
		}
	}
	return false
}

// AccountView 对外视图（脱敏，可安全返回给前端）。
type AccountView struct {
	OwnerID       string   `json:"ownerId"`
	KeyMasked     string   `json:"keyMasked"`
	Endpoint      string   `json:"endpoint"`
	Profiles      []string `json:"profiles"`
	Subscriptions []string `json:"subscriptions"`
	// Source 账号来源：local（本机登录态）/ oauth（授权添加），前端据此区分卡片
	Source        string `json:"source"`
	Status        string `json:"status"`
	StatusDetail  string `json:"statusDetail"`
	ModelCount    int    `json:"modelCount"`
	LatencyMs     int64  `json:"latencyMs"`
	LastCheckedAt int64  `json:"lastCheckedAt"`
}

// Snapshot 账号池快照。
type Snapshot struct {
	Total     int           `json:"total"`
	Available int           `json:"available"`
	Accounts  []AccountView `json:"accounts"`
}

// Pool 账号池。
type Pool struct {
	mu       sync.Mutex
	accounts []*AccountState
	cursor   int
	client   *http.Client
	// manual 手动添加（OAuth 授权）的账号表；为 nil 时只使用自动发现的账号
	manual *manualaccounts.Store
	// hidden 用户主动屏蔽的账号 id（本机账号无法真删，只能这样"移除"）
	hidden *hiddenstore.Store
}

// New 创建账号池。
//
// manual / hidden 均可为 nil —— 那样账号池只由本机 Cindy 登录态供给、且不屏蔽任何账号。
func New(client *http.Client, manual *manualaccounts.Store, hidden *hiddenstore.Store) *Pool {
	if client == nil {
		client = &http.Client{Timeout: checkTimeout}
	}
	return &Pool{client: client, manual: manual, hidden: hidden}
}

// Refresh 重新从本机 Cindy 数据目录读取账号。
//
// 只在 ownerID 与 endpoint、key 三者都一致时继承既有健康状态；否则视为新账号，
// 必须重新探测（key 轮换后旧结论不成立）。
func (p *Pool) Refresh() (added, removed, total int) {
	// 来源一：本机 Cindy 桌面端登录态（自动发现，零交互）
	found, _ := cindyaccount.LoadAll()
	for i := range found {
		found[i].Source = "local"
	}
	// 来源二：本工具通过 OAuth 授权添加的账号（独立存储，生命周期由本工具维护）
	if p.manual != nil {
		for _, item := range p.manual.List() {
			if item.Endpoint == "" || item.APIKey == "" {
				continue // 缺一半凭据的账号无法使用
			}
			label := item.Label
			if label == "" {
				label = item.ID
			}
			found = append(found, cindyaccount.Account{
				OwnerID:  "oauth-" + item.ID,
				Endpoint: item.Endpoint,
				APIKey:   item.APIKey,
				Profiles: []string{label},
				Source:   "oauth",
			})
		}
	}

	// 屏蔽名单：用户主动要求不再使用的账号。
	// 本机账号的凭据属于 Cindy 客户端，不能真删，只能在这里过滤掉（可恢复）。
	if p.hidden != nil {
		kept := found[:0]
		for _, account := range found {
			if !p.hidden.Contains(account.OwnerID) {
				kept = append(kept, account)
			}
		}
		found = kept
	}

	p.mu.Lock()
	defer p.mu.Unlock()

	previous := make(map[string]*AccountState, len(p.accounts))
	for _, item := range p.accounts {
		previous[item.Account.OwnerID] = item
	}

	next := make([]*AccountState, 0, len(found))
	for _, account := range found {
		if old, ok := previous[account.OwnerID]; ok &&
			old.Account.Endpoint == account.Endpoint && old.Account.APIKey == account.APIKey {
			old.Account.Profiles = account.Profiles
			old.Account.Subscriptions = account.Subscriptions
			next = append(next, old)
			continue
		}
		added++
		next = append(next, &AccountState{
			Account: account,
			Status:  "unknown",
			Detail:  "尚未探测",
		})
	}

	removed = len(p.accounts) - (len(next) - added)
	p.accounts = next
	if p.cursor >= len(p.accounts) {
		p.cursor = 0
	}
	return added, removed, len(p.accounts)
}

// CheckResult 单个账号的健康检查结果。
type CheckResult struct {
	OwnerID   string   `json:"ownerId"`
	Endpoint  string   `json:"endpoint"`
	OK        bool     `json:"ok"`
	Status    int      `json:"status"`
	Models    []string `json:"-"`
	LatencyMs int64    `json:"latencyMs"`
	Error     string   `json:"error,omitempty"`
}

// CheckAll 并发检查所有账号，并把结果写回池。
func (p *Pool) CheckAll(ctx context.Context) []CheckResult {
	p.mu.Lock()
	targets := make([]*AccountState, len(p.accounts))
	copy(targets, p.accounts)
	p.mu.Unlock()

	results := make([]CheckResult, len(targets))
	var wg sync.WaitGroup
	for i, state := range targets {
		wg.Add(1)
		go func(index int, state *AccountState) {
			defer wg.Done()
			results[index] = p.checkOne(ctx, state)
		}(i, state)
	}
	wg.Wait()
	return results
}

// checkOne 对单个账号读 /v1/models，成功即认为可用并顺带取到模型清单。
func (p *Pool) checkOne(ctx context.Context, state *AccountState) CheckResult {
	url := state.Account.Endpoint + "/v1/models"
	// 端点与 key 必须成对使用，因此这里永远用账号自己的 key
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return p.recordFailure(state, fmt.Sprintf("构造请求失败: %v", err), 0)
	}
	req.Header.Set("Authorization", "Bearer "+state.Account.APIKey)
	req.Header.Set("Accept", "application/json")

	started := time.Now()
	resp, err := p.client.Do(req)
	latency := time.Since(started).Milliseconds()
	if err != nil {
		return p.recordFailure(state, DescribeConnectError(err), latency)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		body, _ := io.ReadAll(io.LimitReader(resp.Body, 300))
		return p.recordFailure(state,
			fmt.Sprintf("HTTP %d %s", resp.StatusCode, strings.TrimSpace(string(body))), latency)
	}

	var payload struct {
		Data []struct {
			ID string `json:"id"`
		} `json:"data"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&payload); err != nil {
		return p.recordFailure(state, fmt.Sprintf("响应解析失败: %v", err), latency)
	}
	models := make([]string, 0, len(payload.Data))
	for _, item := range payload.Data {
		if item.ID != "" {
			models = append(models, item.ID)
		}
	}
	sort.Strings(models)

	p.mu.Lock()
	state.Status = "ok"
	state.Detail = "可用"
	state.Models = models
	state.LatencyMs = latency
	state.Failures = 0
	state.LastCheckedAt = time.Now()
	p.mu.Unlock()

	return CheckResult{
		OwnerID: state.Account.OwnerID, Endpoint: state.Account.Endpoint,
		OK: true, Status: 200, Models: models, LatencyMs: latency,
	}
}

// recordFailure 记录一次失败并在超阈值时把账号降级。
func (p *Pool) recordFailure(state *AccountState, detail string, latency int64) CheckResult {
	p.mu.Lock()
	state.Failures++
	state.Status = "error"
	state.Detail = detail
	state.LatencyMs = latency
	state.LastCheckedAt = time.Now()
	p.mu.Unlock()

	return CheckResult{
		OwnerID: state.Account.OwnerID, Endpoint: state.Account.Endpoint,
		OK: false, Status: 0, LatencyMs: latency, Error: detail,
	}
}

// Select 为一次请求挑选账号，可按请求模型过滤无权限的账号。
// SelectFor 为一次请求挑选账号；model 非空时跳过已知无权限的账号。
//
// 优先健康账号并在其中轮转；健康账号都不可用时，退回失败次数最少的那个 ——
// 宁可试一次也不要直接给调用方失败。exclude 是本请求已试过的账号，避免重复踩坑。
func (p *Pool) SelectFor(model string, exclude []string) *AccountState {
	p.mu.Lock()
	defer p.mu.Unlock()

	excluded := make(map[string]bool, len(exclude))
	for _, id := range exclude {
		excluded[id] = true
	}

	var candidates []*AccountState
	for _, item := range p.accounts {
		if excluded[item.Account.OwnerID] || item.Failures >= failureThreshold {
			continue
		}
		if !item.allowsModel(model) {
			continue // 上游 403 过该模型，别浪费尝试次数
		}
		candidates = append(candidates, item)
	}
	// 所有账号都被模型过滤掉了：放宽模型限制再选一次，
	// 宁可让上游回一句明确的 403，也不要网关层直接说没号可用。
	if len(candidates) == 0 && model != "" {
		for _, item := range p.accounts {
			if excluded[item.Account.OwnerID] || item.Failures >= failureThreshold {
				continue
			}
			candidates = append(candidates, item)
		}
	}
	if len(candidates) == 0 {
		var fallback *AccountState
		for _, item := range p.accounts {
			if excluded[item.Account.OwnerID] {
				continue
			}
			if fallback == nil || item.Failures < fallback.Failures {
				fallback = item
			}
		}
		return fallback
	}

	var healthy []*AccountState
	for _, item := range candidates {
		if item.Status == "ok" {
			healthy = append(healthy, item)
		}
	}
	target := candidates
	if len(healthy) > 0 {
		target = healthy
	}
	p.cursor = (p.cursor + 1) % len(target)
	return target[p.cursor]
}

// Select 挑选账号（无模型过滤，保留给健康检查等场景）。
func (p *Pool) Select(exclude []string) *AccountState {
	return p.SelectFor("", exclude)
}

// RecordModelDenied 记录"该账号被上游 403 拒绝了某模型"，并更新权限记忆。
//
// allowed 是 403 响应里服务端附带的可用模型清单（可能为空）。
// 这不算账号级故障：账号本身是好的，只是这个模型没订阅。
func (p *Pool) RecordModelDenied(state *AccountState, model string, allowed []string) {
	p.mu.Lock()
	defer p.mu.Unlock()

	state.LastDeniedModel = model
	if len(allowed) > 0 {
		set := make(map[string]bool, len(allowed))
		for _, id := range allowed {
			if id != "" {
				set[id] = true
			}
		}
		state.AllowedModels = set
		// 顺带把 /v1/models 的展示清单收紧为实测清单，避免前端继续展示调不通的模型
		if len(state.Models) > 0 {
			filtered := state.Models[:0:0]
			for _, id := range state.Models {
				ok := set[id]
				if !ok {
					for pattern := range set {
						if strings.HasSuffix(pattern, "/*") && strings.HasPrefix(id, strings.TrimSuffix(pattern, "*")) {
							ok = true
							break
						}
					}
				}
				if ok {
					filtered = append(filtered, id)
				}
			}
			state.Models = filtered
		}
	}
	state.Detail = fmt.Sprintf("模型 %s 无权限（已记录，后续请求自动跳过该账号）", model)
}

// MarkSuccess 请求成功后清理失败计数。
func (p *Pool) MarkSuccess(state *AccountState) {
	p.mu.Lock()
	defer p.mu.Unlock()
	state.Failures = 0
	state.Status = "ok"
	state.Detail = "可用"
	state.LastCheckedAt = time.Now()
}

// MarkFailure 请求失败后累计失败计数。
func (p *Pool) MarkFailure(state *AccountState, detail string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	state.Failures++
	state.Detail = detail
	state.LastCheckedAt = time.Now()
	if state.Failures >= failureThreshold {
		state.Status = "error"
	}
}

// Snapshot 返回脱敏后的账号池视图。
func (p *Pool) Snapshot() Snapshot {
	p.mu.Lock()
	defer p.mu.Unlock()

	view := Snapshot{Total: len(p.accounts), Accounts: make([]AccountView, 0, len(p.accounts))}
	for _, item := range p.accounts {
		if item.Status == "ok" {
			view.Available++
		}
		subs := make([]string, 0, len(item.Account.Subscriptions))
		for name := range item.Account.Subscriptions {
			subs = append(subs, name)
		}
		sort.Strings(subs)
		var checkedAt int64
		if !item.LastCheckedAt.IsZero() {
			checkedAt = item.LastCheckedAt.UnixMilli()
		}
		view.Accounts = append(view.Accounts, AccountView{
			OwnerID:       item.Account.OwnerID,
			KeyMasked:     cindyaccount.MaskKey(item.Account.APIKey),
			Endpoint:      item.Account.Endpoint,
			Profiles:      item.Account.Profiles,
			Subscriptions: subs,
			Source:        item.Account.Source,
			Status:        item.Status,
			StatusDetail:  item.Detail,
			ModelCount:    len(item.Models),
			LatencyMs:     item.LatencyMs,
			LastCheckedAt: checkedAt,
		})
	}
	return view
}

// Models 汇总所有可用账号的模型清单（去重排序）。
func (p *Pool) Models() []string {
	p.mu.Lock()
	defer p.mu.Unlock()

	seen := map[string]bool{}
	var models []string
	for _, item := range p.accounts {
		if item.Status != "ok" {
			continue
		}
		for _, id := range item.Models {
			if !seen[id] {
				seen[id] = true
				models = append(models, id)
			}
		}
	}
	sort.Strings(models)
	return models
}

// Available 返回当前可用账号数（用于 /status）。
func (p *Pool) Available() (available, total int) {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, item := range p.accounts {
		if item.Status == "ok" {
			available++
		}
	}
	return available, len(p.accounts)
}

// cindy.go —— 复刻 Cindy 桌面端的「托管回调 OAuth」授权流程。
//
// 完整链路（与 Cindy 官方客户端逐字节一致，见 cindy 仓库 packages/auth-client）：
//
//	1. pollSecret  = base64url(random 32B)                        ← 只留在本进程内
//	   clientState = base64url(sha256(pollSecret))                 ← 只有它经过浏览器
//	   codeVerifier = base64url(random 32B)
//	   codeChallenge = base64url(sha256(codeVerifier))             ← PKCE S256
//	2. 系统浏览器打开 {authBase}/api/auth/social/{provider}/authorize?...
//	3. 轮询 POST {authBase}/api/auth/desktop/callback/poll {pollSecret, deviceId}
//	     → pending / ok(code) / error / expired
//	4. POST {authBase}/api/auth/token {grantType:"authorization_code", code, codeVerifier, deviceId}
//	     → accessToken + refreshToken + membership
//	5. GET  {modelAccessBase}/api/model-access/credentials（Bearer accessToken）
//	     → {endpoint, apiKey}  ← 反代网关真正要用的凭据
//
// 设计要点：取回凭据必须是 pollSecret 而不是 clientState —— 后者会进入浏览器地址栏
// 与导航历史，任何同机进程都能抢先消费那个未鉴权接口。这一点照抄官方实现，不简化。
package oauth

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"regexp"
	"strings"
	"sync"
	"time"
)

// 区域与端点。Global 区支持 apple / google，中国大陆区仅 apple。
const (
	RegionGlobal = "global"
	RegionCN     = "cn"

	AuthBaseGlobal = "https://auth.cindy.app"
	AuthBaseCN     = "https://auth.cindy.com.cn"

	ModelAccessBaseGlobal = "https://model-access.cindy.app"
	ModelAccessBaseCN     = "https://model-access.cindy.com.cn"
)

// pollTimeout 单次轮询请求的超时（不是整次授权的超时）
const pollTimeout = 20 * time.Second

// TokenPair OAuth 完成后得到的令牌对。
type TokenPair struct {
	AccessToken  string `json:"accessToken"`
	RefreshToken string `json:"refreshToken"`
	// DisplayName / Email 用于给账号卡片起个可读的名字
	DisplayName string `json:"displayName"`
	Email       string `json:"email"`
	// MembershipID 是账号在该区域内的稳定标识
	MembershipID string `json:"membershipId"`
}

// CreditBalance 账号额度（/api/model-access/balance 的解析结果）。
//
// 注意：Cindy 的额度单位随区域不同 —— 中国大陆版按人民币计价、国际版按美元计价
// （实测 CN 赠 20、国际赠 3，正好是汇率关系），接口本身不返回货币字段，
// 货币符号由调用方按 region 决定。
type CreditBalance struct {
	Available          string `json:"available"`
	PlanCredits        string `json:"planCredits"`
	PurchasedCredits   string `json:"purchasedCredits"`
	PromotionalCredits string `json:"promotionalCredits"`
	Scale              int    `json:"scale"`
}

// CreditUsage 账号额度（/api/model-access/credit-usage 的解析结果）。
//
// 注意：Cindy 的额度单位随区域不同 —— 中国大陆版按人民币计价、国际版按美元计价
// （实测 CN 赠 20、国际赠 3，正好是汇率关系），接口本身不返回货币字段，
// 货币符号由调用方按 region 决定。
//
// 额度分三个桶：订阅计划 / 自购 / 促销赠送，Total/Used 由桶求和得出，
// null（该桶未启用）按 0 处理 —— 进度条要用。
type CreditUsage struct {
	Available   string `json:"available"`
	Total       string `json:"total"`
	Used        string `json:"used"`
	PlanTotal   string `json:"planTotal"`
	PurchTotal  string `json:"purchasedTotal"`
	PromoTotal  string `json:"promotionalTotal"`
	PromoRemain string `json:"promotionalRemaining"`
	Scale       int    `json:"scale"`
}

// creditBucket 单个额度桶；未启用的桶字段是 null。
type creditBucket struct {
	Remaining *string `json:"remaining"`
	Used      *string `json:"used"`
	Total     *string `json:"total"`
}

// bucketSum 把三个桶的 total / used 求和（null 按 0）。
func bucketSum(plan, purchased, promotional creditBucket) (total, used string) {
	sum := func(values ...*string) float64 {
		var out float64
		for _, v := range values {
			if v == nil {
				continue
			}
			var f float64
			if _, err := fmt.Sscanf(*v, "%f", &f); err == nil {
				out += f
			}
		}
		return out
	}
	return formatFloat(sum(plan.Total, purchased.Total, promotional.Total)),
		formatFloat(sum(plan.Used, purchased.Used, promotional.Used))
}

// formatFloat 输出去掉多余零的字符串（额度是字符串传输，保持可读）。
func formatFloat(value float64) string {
	return strings.TrimRight(strings.TrimRight(fmt.Sprintf("%.6f", value), "0"), ".")
}

// RefreshAccessToken 用 refreshToken 换新的令牌对。
//
// ⚠️ 服务端会**轮换** refreshToken：调用方必须把返回的新 RefreshToken 持久化，
// 否则旧的一旦失效账号就丢了登录态。
func RefreshAccessToken(ctx context.Context, client *http.Client, region, refreshToken, deviceID string) (*TokenPair, error) {
	authBase, _, err := authBaseFor(region)
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	body, err := json.Marshal(map[string]string{
		"refreshToken": refreshToken,
		"deviceId":     deviceID,
	})
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, authBase+"/api/auth/refresh", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("刷新令牌失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 32*1024))
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("刷新令牌返回 HTTP %d: %s", resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	var parsed TokenPair
	if err := json.Unmarshal(raw, &parsed); err != nil {
		return nil, fmt.Errorf("刷新响应解析失败: %w", err)
	}
	if parsed.AccessToken == "" || parsed.RefreshToken == "" {
		return nil, fmt.Errorf("刷新响应缺少令牌")
	}
	return &parsed, nil
}

// FetchCreditBalance 查询账号额度明细（credit-usage），并汇总出进度条需要的 total/used。
//
// 必须**登录令牌**（accessToken）；网关 key 不被接受 —— 实测直接 401。
func FetchCreditBalance(ctx context.Context, client *http.Client, region, accessToken string) (*CreditUsage, error) {
	_, modelAccessBase, err := authBaseFor(region)
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, modelAccessBase+"/api/model-access/credit-usage", nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+accessToken)
	req.Header.Set("Accept", "application/json")

	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("查询额度失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 32*1024))
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("查询额度返回 HTTP %d", resp.StatusCode)
	}
	var parsed struct {
		Available   string       `json:"available"`
		Plan        creditBucket `json:"plan"`
		Purchased   creditBucket `json:"purchased"`
		Promotional creditBucket `json:"promotional"`
		Scale       int          `json:"scale"`
	}
	if err := json.Unmarshal(raw, &parsed); err != nil {
		return nil, fmt.Errorf("额度解析失败: %w", err)
	}
	total, used := bucketSum(parsed.Plan, parsed.Purchased, parsed.Promotional)
	return &CreditUsage{
		Available:   parsed.Available,
		Total:       total,
		Used:        used,
		PlanTotal:   deref(parsed.Plan.Total),
		PurchTotal:  deref(parsed.Purchased.Total),
		PromoTotal:  deref(parsed.Promotional.Total),
		PromoRemain: deref(parsed.Promotional.Remaining),
		Scale:       parsed.Scale,
	}, nil
}

// deref 空指针转空串（前端不关心"未启用"与 0 的区别）。
func deref(value *string) string {
	if value == nil {
		return "0"
	}
	return *value
}

// Session 一次授权会话（一个会话对应一次「添加账号」点击）。
type Session struct {
	ID string
	// Kind "social"（Apple / Google）或 "sso"（企业单点登录）
	Kind     string
	Provider string
	Region   string
	AuthBase string
	// DeviceID 本会话**专用**的设备标识（见 StartSession 的说明）
	DeviceID     string
	PollSecret   string
	CodeVerifier string
	AuthorizeURL string
	CreatedAt    time.Time
}

// pending 会话表：会话只存在于内存，进程重启即失效（与官方一致，无需落盘）。
var (
	sessionsMu sync.Mutex
	sessions   = map[string]*Session{}
)

// sessionTTL 会话有效期。授权页停留过久（超时 5 分钟）即作废。
const sessionTTL = 10 * time.Minute

// authBaseFor 区域 → 授权服务基址。
func authBaseFor(region string) (authBase, modelAccessBase string, err error) {
	switch region {
	case RegionCN:
		return AuthBaseCN, ModelAccessBaseCN, nil
	case RegionGlobal, "":
		return AuthBaseGlobal, ModelAccessBaseGlobal, nil
	default:
		return "", "", fmt.Errorf("未知区域 %q（仅支持 global / cn）", region)
	}
}

// randomBase64URL 生成 n 字节随机数并做 base64url（无填充）编码。
func randomBase64URL(n int) (string, error) {
	buf := make([]byte, n)
	if _, err := rand.Read(buf); err != nil {
		return "", fmt.Errorf("生成随机数失败: %w", err)
	}
	return base64.RawURLEncoding.EncodeToString(buf), nil
}

// sha256Base64URL 计算 sha256 后做 base64url（无填充）编码 —— 与服务端实现必须逐字节一致。
func sha256Base64URL(value string) string {
	sum := sha256.Sum256([]byte(value))
	return base64.RawURLEncoding.EncodeToString(sum[:])
}

// StartSession 创建一次授权会话，返回会话与应当交给系统浏览器打开的地址。
//
// kind 取 "social" / "sso"（与官方 buildAuthorizeUrl 的语义一致）：
//   - social：provider 为 "google" / "apple"
//   - sso：provider 为企业组织标识（connectionId），由用户填写
func StartSession(kind, provider, region, deviceID string) (*Session, error) {
	authBase, _, err := authBaseFor(region)
	if err != nil {
		return nil, err
	}
	provider = strings.TrimSpace(provider)
	switch kind {
	case "social":
		if provider != "google" && provider != "apple" {
			return nil, fmt.Errorf("不支持的授权方式 %q（仅支持 google / apple）", provider)
		}
	case "sso":
		if provider == "" {
			return nil, fmt.Errorf("请填写企业 SSO 的组织标识")
		}
	default:
		return nil, fmt.Errorf("不支持的授权类型 %q（仅支持 social / sso）", kind)
	}

	pollSecret, err := randomBase64URL(32)
	if err != nil {
		return nil, err
	}
	codeVerifier, err := randomBase64URL(32)
	if err != nil {
		return nil, err
	}
	sessionID, err := randomBase64URL(12)
	if err != nil {
		return nil, err
	}

	// 每次授权使用**独立**的 deviceId（在基准设备标识后拼一段随机串）。
	//
	// 为什么必须这样做：服务端按 deviceId 暂存授权码，只要没被取走就会一直返回它。
	// 实测后果是「同一 deviceId 下再次发起授权会立刻拿回上一次的 code」——
	// 表现就是**永远只能登录同一个账号，换不了号**。
	// 给每次授权分配独立 deviceId 即可彻底隔离，且 poll / 兑换都用同一个值，
	// 对服务端而言仍是一次完整自洽的授权。
	sessionDeviceID := deviceID + "-" + sessionID

	// authorize 的参数顺序与名称照抄官方 buildAuthorizeUrl
	query := url.Values{}
	query.Set("client_type", "desktop")
	query.Set("device_id", sessionDeviceID)
	query.Set("redirect_uri", authBase+"/api/auth/desktop/callback")
	query.Set("code_challenge", sha256Base64URL(codeVerifier))
	query.Set("code_challenge_method", "S256")
	query.Set("client_state", sha256Base64URL(pollSecret))
	query.Set("ui_locale", "zh-CN")

	// social 与 sso 只是路径不同，其余参数完全一致
	segment := fmt.Sprintf("/api/auth/social/%s/authorize", url.PathEscape(provider))
	if kind == "sso" {
		segment = fmt.Sprintf("/api/auth/sso/%s/authorize", url.PathEscape(provider))
	}

	session := &Session{
		ID:           sessionID,
		Kind:         kind,
		Provider:     provider,
		Region:       region,
		AuthBase:     authBase,
		DeviceID:     sessionDeviceID,
		PollSecret:   pollSecret,
		CodeVerifier: codeVerifier,
		AuthorizeURL: authBase + segment + "?" + query.Encode(),
		CreatedAt:    time.Now(),
	}

	sessionsMu.Lock()
	// 顺手清理过期会话，避免内存里堆积
	for id, item := range sessions {
		if time.Since(item.CreatedAt) > sessionTTL {
			delete(sessions, id)
		}
	}
	sessions[sessionID] = session
	sessionsMu.Unlock()

	return session, nil
}

// GetSession 取出会话（不存在或已过期返回 nil）。
func GetSession(id string) *Session {
	sessionsMu.Lock()
	defer sessionsMu.Unlock()
	session, ok := sessions[id]
	if !ok || time.Since(session.CreatedAt) > sessionTTL {
		return nil
	}
	return session
}

// DropSession 删除会话（成功兑换或用户取消后调用，避免凭据常驻内存）。
func DropSession(id string) {
	sessionsMu.Lock()
	defer sessionsMu.Unlock()
	delete(sessions, id)
}

// pollResponse 轮询响应：status 决定后续，code 仅在 status=ok 时有值。
type pollResponse struct {
	Status string `json:"status"`
	Code   string `json:"code"`
	Error  string `json:"error"`
}

// Poll 轮询一次授权结果。
//
// 返回值 status 语义与官方一致：pending（继续等）/ ok（拿到授权码）/ error / expired。
func (s *Session) Poll(ctx context.Context, client *http.Client) (string, string, error) {
	body, err := json.Marshal(map[string]string{
		"pollSecret": s.PollSecret,
		// 必须用会话专属 deviceId：既与发起授权时一致，又不会串到别的会话
		"deviceId": s.DeviceID,
	})
	if err != nil {
		return "", "", err
	}
	ctx, cancel := context.WithTimeout(ctx, pollTimeout)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		s.AuthBase+"/api/auth/desktop/callback/poll", bytes.NewReader(body))
	if err != nil {
		return "", "", err
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := client.Do(req)
	if err != nil {
		return "", "", fmt.Errorf("轮询授权结果失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 8192))
	if resp.StatusCode != http.StatusOK {
		return "", "", fmt.Errorf("轮询授权结果返回 HTTP %d: %s", resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	var parsed pollResponse
	if err := json.Unmarshal(raw, &parsed); err != nil {
		return "", "", fmt.Errorf("轮询响应解析失败: %w", err)
	}
	switch parsed.Status {
	case "ok":
		return "ok", parsed.Code, nil
	case "pending":
		return "pending", "", nil
	case "expired":
		return "expired", "", nil
	default:
		message := parsed.Error
		if message == "" {
			message = "授权被拒绝或发生错误"
		}
		return "error", "", fmt.Errorf("%s", message)
	}
}

// tokenOutcome /api/auth/token 的响应（只取我们需要的字段）。
type tokenOutcome struct {
	Status       string `json:"status"`
	AccessToken  string `json:"accessToken"`
	RefreshToken string `json:"refreshToken"`
	Membership   struct {
		ID          string `json:"id"`
		DisplayName string `json:"displayName"`
		Email       string `json:"email"`
	} `json:"membership"`
	Code    string `json:"code"`
	Message string `json:"message"`
}

// Exchange 用授权码 + PKCE 兑换令牌。
func (s *Session) Exchange(ctx context.Context, client *http.Client, code string) (*TokenPair, error) {
	body, err := json.Marshal(map[string]string{
		"grantType":    "authorization_code",
		"code":         code,
		"codeVerifier": s.CodeVerifier,
		"deviceId":     s.DeviceID,
	})
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, s.AuthBase+"/api/auth/token", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("兑换令牌失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 32*1024))
	var parsed tokenOutcome
	if err := json.Unmarshal(raw, &parsed); err != nil {
		return nil, fmt.Errorf("令牌响应解析失败（HTTP %d）: %s", resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	if resp.StatusCode != http.StatusOK || parsed.Status != "ok" {
		detail := parsed.Message
		if detail == "" {
			detail = parsed.Code
		}
		if detail == "" {
			detail = strings.TrimSpace(string(raw))
		}
		return nil, fmt.Errorf("登录未完成（status=%s）: %s", parsed.Status, detail)
	}
	if parsed.AccessToken == "" || parsed.RefreshToken == "" {
		return nil, fmt.Errorf("令牌响应缺少 accessToken / refreshToken")
	}

	return &TokenPair{
		AccessToken:  parsed.AccessToken,
		RefreshToken: parsed.RefreshToken,
		DisplayName:  parsed.Membership.DisplayName,
		Email:        parsed.Membership.Email,
		MembershipID: parsed.Membership.ID,
	}, nil
}

// FetchGatewayCredentials 用 accessToken 换取反代网关真正要用的 {endpoint, apiKey}。
//
// 这一步与官方客户端取凭据走的是同一个接口，因此拿到的 endpoint 与 key 天然同租户。
func FetchGatewayCredentials(ctx context.Context, client *http.Client, region, accessToken string) (endpoint, apiKey string, err error) {
	_, modelAccessBase, baseErr := authBaseFor(region)
	if baseErr != nil {
		return "", "", baseErr
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet,
		modelAccessBase+"/api/model-access/credentials", nil)
	if err != nil {
		return "", "", err
	}
	req.Header.Set("Authorization", "Bearer "+accessToken)
	req.Header.Set("Accept", "application/json")

	resp, err := client.Do(req)
	if err != nil {
		return "", "", fmt.Errorf("获取网关凭据失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 8192))
	if resp.StatusCode != http.StatusOK {
		return "", "", fmt.Errorf("获取网关凭据返回 HTTP %d: %s", resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	var parsed struct {
		Endpoint string `json:"endpoint"`
		APIKey   string `json:"apiKey"`
	}
	if err := json.Unmarshal(raw, &parsed); err != nil {
		return "", "", fmt.Errorf("网关凭据解析失败: %w", err)
	}
	if parsed.Endpoint == "" || parsed.APIKey == "" {
		return "", "", fmt.Errorf("网关凭据为空：该账号可能未开通模型访问")
	}
	return strings.TrimRight(parsed.Endpoint, "/"), parsed.APIKey, nil
}

// FetchProviders 查询某区域支持的登录方式。
//
// 注意一处**必须修正的语义差异**：`providers` 上报的 `social` 是**客户端能力清单**
// （例如 iOS 原生的 Sign in with Apple），并不等于桌面端的 authorize 端点真的开放。
//
// 实际可用情况（按产品口径校正）：
//   - 国际版：邮箱 / Apple / Google / SSO —— 其中只有 Apple、Google 能走桌面端 OAuth
//   - 中国大陆版：**86 手机号 + 企业 SSO** —— 社交授权在桌面端不可用
//
// 所以对 CN 区域把 social 清空并给出原因，避免用户在界面上选一个注定失败的选项。
// CN 的手机号流本工具**已经支持**（见 RequestPhoneCode / VerifyPhoneCode），
// 因此引导用户改用手机号登录；企业 SSO 需要组织 connectionId，暂不支持。
func FetchProviders(ctx context.Context, client *http.Client, region string) (map[string]any, error) {
	authBase, _, err := authBaseFor(region)
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithTimeout(ctx, 20*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, authBase+"/api/auth/providers", nil)
	if err != nil {
		return nil, err
	}
	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("查询登录方式失败: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 8192))
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("查询登录方式返回 HTTP %d", resp.StatusCode)
	}
	var payload map[string]any
	if err := json.Unmarshal(raw, &payload); err != nil {
		return nil, fmt.Errorf("登录方式解析失败: %w", err)
	}

	if region == RegionCN {
		payload["social"] = []string{}
		payload["desktopAuthorizationSupported"] = false
		payload["phoneCodeLoginSupported"] = true
		payload["desktopAuthorizationHint"] =
			"中国大陆版使用 86 手机号 + 短信验证码登录，请在下方填写手机号获取验证码。"
	} else {
		payload["desktopAuthorizationSupported"] = true
		// 国际版不支持手机号（providers 里 phone: false）
		payload["phoneCodeLoginSupported"] = false
	}
	return payload, nil
}

// ── 手机号 + 短信验证码登录（中国大陆版的主要方式） ──────────────────────────
//
// 这是把桌面端自己的登录接口搬到本工具里，不是另造一条链路：
//
//	POST {authBase}/api/auth/phone/request-code  {phone, locale}          → {status:"sent"}
//	POST {authBase}/api/auth/phone/verify-code   {phone, code, deviceId,
//	                                              clientType, locale}     → tokenPair
//
// 之后与 OAuth 路径完全合流：拿 accessToken 换 {endpoint, apiKey} 再落成账号。
//
// 关于人机验证：`providers` 的 captcha.requiredFor 只包含 `email_request_code`，
// 而中国大陆版的 providers 响应里**根本没有 captcha 字段** —— 手机号流不需要
// Turnstile，所以这里不传 captchaToken。

// ── 官方验证页参数抓取 ──────────────────────────────────────────────────────
//
// 官方客户端做邮箱验证码登录时，是打开 auth 服务自带的验证页
// `{authBase}/captcha/turnstile`（页面里硬编码了 sitekey / action / cData），
// 由它渲染 Turnstile，再把 token 回传（ReactNativeWebView bridge / postMessage /
// URL hash 三种出口，见页面实现）。
//
// 我们照抄同样的参数在本机页面渲染：上游 siteverify 会校验 action/cData，
// 少传（尤其 action）会被判 CAPTCHA_INVALID —— 这是 2026-09-18 排查出的关键差异。

// CaptchaChallengeParams 官方验证页用的渲染参数。
type CaptchaChallengeParams struct {
	SiteKey string
	Action  string
	CData   string
}

// 三个参数各自独立解析：页面里 data-action 出现在 SITEKEY 之前，
// 用一个大正则串起来会匹配不到（踩过）。
var (
	captchaSiteKeyPattern = regexp.MustCompile(`SITEKEY\s*=\s*"([^"]+)"`)
	captchaActionPattern  = regexp.MustCompile(`data-action="([^"]*)"`)
	captchaCDataPattern   = regexp.MustCompile(`data-cdata="([^"]*)"`)
)

// FetchCaptchaChallengeParams 拉官方验证页并解析出 sitekey / action / cData。
func FetchCaptchaChallengeParams(ctx context.Context, client *http.Client) (*CaptchaChallengeParams, error) {
	ctx, cancel := context.WithTimeout(ctx, 20*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, AuthBaseGlobal+"/captcha/turnstile", nil)
	if err != nil {
		return nil, err
	}
	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("获取验证页失败: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 256*1024))
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("验证页返回 HTTP %d", resp.StatusCode)
	}
	siteKey := captchaSiteKeyPattern.FindSubmatch(raw)
	action := captchaActionPattern.FindSubmatch(raw)
	cData := captchaCDataPattern.FindSubmatch(raw)
	if siteKey == nil {
		return nil, fmt.Errorf("验证页结构变化，解析不到 sitekey")
	}
	params := &CaptchaChallengeParams{SiteKey: string(siteKey[1])}
	if action != nil {
		params.Action = string(action[1])
	}
	if cData != nil {
		params.CData = string(cData[1])
	}
	return params, nil
}

// ── 邮箱 + 邮箱验证码登录（国际版） ─────────────────────────────────────────
//
// 上游接口（与手机号流同构，端点从 /api/auth/phone/ 换成 /api/auth/email/）：
//
//	POST {authBase}/api/auth/email/request-code  {email, captchaToken, locale}
//	POST {authBase}/api/auth/email/verify-code   {email, code, deviceId,
//	                                              clientType, locale}      → tokenPair
//
// 与手机号流的两点差异：
//  1. request-code **强制 Turnstile 人机验证**（providers 的 captcha.requiredFor
//     只含 email_request_code），captchaToken 由前端内嵌的 Turnstile 组件取得，
//     缺失时上游报 CAPTCHA_REQUIRED，无效时报 CAPTCHA_INVALID —— 都原样透出。
//  2. verify-code 不需要 captcha；deviceId 用基础值，签发的 refreshToken
//     与刷新设备天然一致（参考 2026-09-18 修的 DEVICE_MISMATCH）。

// 人机验证（Turnstile）的两种状态。
const (
	// CaptchaRequired 上游判定本次发送需要人机验证（前端应补做后重试）
	CaptchaRequired = "captcha_required"
	// CaptchaInvalid 已带 token 但没通过（token 一次性，多半是复用/过期）
	CaptchaInvalid = "captcha_invalid"
)

// CaptchaError 邮箱验证码发送时的人机验证错误。
//
// 单独成类型是为了让网关层能把它翻译成专用 HTTP 状态码/错误码
// （见 handleEmailRequestCode），前端据此自动补做验证而不是让用户干瞪眼。
type CaptchaError struct {
	Code    string
	Message string
}

func (e *CaptchaError) Error() string { return e.Message }

// RequestEmailCode 请求发送邮箱验证码。
//
// captchaToken **可省略**：上游只在触发风控时才要求人机验证，
// 所以策略是「先不带 token 发一次」，被拒（CaptchaError）后再带上重试 ——
// 与官方客户端一致：没被风控时用户根本看不到验证组件。
//
// 返回 *CaptchaError 时调用方应据此回到前端重做一次人机验证再重试。
func RequestEmailCode(ctx context.Context, client *http.Client, region, email, captchaToken string) error {
	authBase, _, err := authBaseFor(region)
	if err != nil {
		return err
	}
	if strings.TrimSpace(email) == "" {
		return fmt.Errorf("请填写邮箱地址")
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	payload := map[string]any{
		"email":  email,
		"locale": "zh-CN",
	}
	if strings.TrimSpace(captchaToken) != "" {
		payload["captchaToken"] = captchaToken
	}

	body, err := json.Marshal(payload)
	if err != nil {
		return err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		authBase+"/api/auth/email/request-code", bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := client.Do(req)
	if err != nil {
		return fmt.Errorf("请求邮箱验证码失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 8192))
	if resp.StatusCode != http.StatusOK {
		// CAPTCHA_REQUIRED / CAPTCHA_INVALID 转成结构化错误，前端据此决定
		// 「要不要做人机验证 / 要不要重做」，而不是让用户读一段原始报文
		var parsed struct {
			Error struct {
				Code    string `json:"code"`
				Message string `json:"message"`
			} `json:"error"`
		}
		_ = json.Unmarshal(raw, &parsed)
		detail := strings.TrimSpace(string(raw))
		message := parsed.Error.Message
		if message == "" {
			message = parsed.Error.Code
		}
		if message == "" {
			message = detail
		}
		switch parsed.Error.Code {
		case "CAPTCHA_REQUIRED":
			return &CaptchaError{Code: CaptchaRequired, Message: "需要完成人机验证后才能发送验证码"}
		case "CAPTCHA_INVALID", "CAPTCHA_FAILED":
			return &CaptchaError{Code: CaptchaInvalid, Message: "人机验证未通过，请重试（" + message + "）"}
		}
		return fmt.Errorf("请求邮箱验证码返回 HTTP %d: %s", resp.StatusCode, detail)
	}
	return nil
}

// VerifyEmailCode 校验邮箱验证码，返回令牌对（结构与手机号流一致）。
func VerifyEmailCode(ctx context.Context, client *http.Client, region, email, code, deviceID string) (*TokenPair, error) {
	authBase, _, err := authBaseFor(region)
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	body, err := json.Marshal(map[string]any{
		"email":      email,
		"code":       code,
		"deviceId":   deviceID,
		"clientType": "desktop",
		"locale":     "zh-CN",
	})
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		authBase+"/api/auth/email/verify-code", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("校验邮箱验证码失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 32*1024))
	var parsed tokenOutcome
	if err := json.Unmarshal(raw, &parsed); err != nil {
		return nil, fmt.Errorf("登录响应解析失败（HTTP %d）: %s", resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	if resp.StatusCode != http.StatusOK || parsed.Status != "ok" {
		// select_account / binding_required 等非 ok 状态要如实告诉用户，
		// 不能笼统报"登录失败"让人无从下手
		switch parsed.Status {
		case "select_account":
			return nil, fmt.Errorf("该邮箱绑定了多个账号，请改用「OAuth 授权」登录后选择")
		case "binding_required":
			return nil, fmt.Errorf("该账号需要先绑定邮箱或手机号，请先在 Cindy 桌面端完成绑定")
		}
		detail := parsed.Message
		if detail == "" {
			detail = parsed.Code
		}
		if detail == "" {
			detail = strings.TrimSpace(string(raw))
		}
		return nil, fmt.Errorf("登录未完成（status=%s）: %s", parsed.Status, detail)
	}
	if parsed.AccessToken == "" || parsed.RefreshToken == "" {
		return nil, fmt.Errorf("登录响应缺少 accessToken / refreshToken")
	}

	return &TokenPair{
		AccessToken:  parsed.AccessToken,
		RefreshToken: parsed.RefreshToken,
		DisplayName:  parsed.Membership.DisplayName,
		Email:        parsed.Membership.Email,
		MembershipID: parsed.Membership.ID,
	}, nil
}

// RequestPhoneCode 请求发送短信验证码。
func RequestPhoneCode(ctx context.Context, client *http.Client, region, phone string) error {
	authBase, _, err := authBaseFor(region)
	if err != nil {
		return err
	}
	if strings.TrimSpace(phone) == "" {
		return fmt.Errorf("请填写手机号")
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	body, err := json.Marshal(map[string]any{
		"phone":  phone,
		"locale": "zh-CN",
	})
	if err != nil {
		return err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		authBase+"/api/auth/phone/request-code", bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := client.Do(req)
	if err != nil {
		return fmt.Errorf("请求短信验证码失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 8192))
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("请求短信验证码返回 HTTP %d: %s", resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	var parsed struct {
		Status string `json:"status"`
	}
	if err := json.Unmarshal(raw, &parsed); err != nil {
		return fmt.Errorf("响应解析失败: %w", err)
	}
	if parsed.Status != "sent" {
		return fmt.Errorf("短信未发送成功（status=%s）", parsed.Status)
	}
	return nil
}

// VerifyPhoneCode 用短信验证码换取令牌对。
func VerifyPhoneCode(ctx context.Context, client *http.Client, region, phone, code, deviceID string) (*TokenPair, error) {
	authBase, _, err := authBaseFor(region)
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	body, err := json.Marshal(map[string]any{
		"phone":      phone,
		"code":       code,
		"deviceId":   deviceID,
		"clientType": "desktop",
		"locale":     "zh-CN",
	})
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		authBase+"/api/auth/phone/verify-code", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("校验短信验证码失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 32*1024))
	var parsed tokenOutcome
	if err := json.Unmarshal(raw, &parsed); err != nil {
		return nil, fmt.Errorf("登录响应解析失败（HTTP %d）: %s", resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	if resp.StatusCode != http.StatusOK || parsed.Status != "ok" {
		// select_account / binding_required 等非 ok 状态要如实告诉用户，
		// 不能笼统报"登录失败"让人无从下手
		switch parsed.Status {
		case "select_account":
			return nil, fmt.Errorf("该手机号绑定了多个账号，请在 Cindy 桌面端选择后再用「本机导入」添加")
		case "binding_required":
			return nil, fmt.Errorf("该账号需要先绑定邮箱或手机号，请先在 Cindy 桌面端完成绑定")
		}
		detail := parsed.Message
		if detail == "" {
			detail = parsed.Code
		}
		if detail == "" {
			detail = strings.TrimSpace(string(raw))
		}
		return nil, fmt.Errorf("登录未完成（status=%s）: %s", parsed.Status, detail)
	}
	if parsed.AccessToken == "" || parsed.RefreshToken == "" {
		return nil, fmt.Errorf("登录响应缺少 accessToken / refreshToken")
	}

	return &TokenPair{
		AccessToken:  parsed.AccessToken,
		RefreshToken: parsed.RefreshToken,
		DisplayName:  parsed.Membership.DisplayName,
		Email:        parsed.Membership.Email,
		MembershipID: parsed.Membership.ID,
	}, nil
}

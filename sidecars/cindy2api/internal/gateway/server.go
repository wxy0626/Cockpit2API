// server.go —— HTTP 网关：对外 OpenAI 兼容，对内按账号成对转发。
//
// 管理接口的响应结构刻意与 sidecars/wb2api 对齐（/api/config、/api/status、/api/models），
// 这样 CockpitTools 前端可以复用同一套面板交互。
//
// 三条来自实测的硬约束：
//  1. 上游 key 与 endpoint 必须同租户成对使用（跨租户返回 401）。
//  2. 换号只能发生在"尚未向客户端写出任何字节"之前，否则流式响应会被撕裂。
//  3. 流式响应必须逐块 Flush，否则 net/http 会缓冲到请求结束才吐给客户端。
package gateway

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os/exec"
	"runtime"
	"strings"
	"time"

	"cindy2api/internal/cindyaccount"
	"cindy2api/internal/config"
	"cindy2api/internal/hiddenstore"
	"cindy2api/internal/manualaccounts"
	"cindy2api/internal/oauth"
	"cindy2api/internal/pool"
)

// dialTimeout 建立上游连接的超时
const dialTimeout = 15 * time.Second

// headerTimeout 等待上游响应头的超时（不含响应体传输时间）
const headerTimeout = 60 * time.Second

// Server 反代网关。
type Server struct {
	cfg    config.Config
	pool   *pool.Pool
	client *http.Client
	logger *log.Logger
	// manual 手动添加（OAuth 授权）的账号表
	manual *manualaccounts.Store
	// hidden 用户屏蔽的账号（保留给「临时禁用」场景；常规移除走 delete-local）
	hidden *hiddenstore.Store
}

// New 构造网关。
func New(
	cfg config.Config,
	p *pool.Pool,
	manual *manualaccounts.Store,
	hidden *hiddenstore.Store,
	logger *log.Logger,
) *Server {
	transport := &http.Transport{
		// 读 HTTPS_PROXY / HTTP_PROXY / ALL_PROXY / NO_PROXY ——
		// 本机实测 api.laxa.com 直连时 TLS 被断、换出口正常，所以出站代理必须可配。
		Proxy: http.ProxyFromEnvironment,
		DialContext: (&net.Dialer{
			Timeout:   dialTimeout,
			KeepAlive: 30 * time.Second,
		}).DialContext,
		TLSHandshakeTimeout:   dialTimeout,
		ResponseHeaderTimeout: headerTimeout,
		MaxIdleConns:          64,
		MaxIdleConnsPerHost:   16,
		IdleConnTimeout:       90 * time.Second,
		ForceAttemptHTTP2:     true,
	}

	return &Server{
		cfg:    cfg,
		pool:   p,
		manual: manual,
		hidden: hidden,
		logger: logger,
		// 不设整体 Timeout：流式响应可能持续很久，由客户端断开或响应头超时兜底
		client: &http.Client{Transport: transport},
	}
}

// Handler 返回完整路由（含 CORS）。
func (s *Server) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/health", s.handleHealth)
	mux.HandleFunc("/api/", s.handleAdmin)
	mux.HandleFunc("/v1/", s.handleOpenAI)
	return withCORS(mux)
}

// ── CORS ─────────────────────────────────────────────────────────────────────

// withCORS 允许本机来源访问管理接口。
//
// Tauri webview 的 origin 可能是 tauri://localhost，dev 模式是 http://localhost:1420，
// 因此只放行本机来源，不放行任意 Origin。
func withCORS(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		origin := r.Header.Get("Origin")
		if isLocalOrigin(origin) {
			w.Header().Set("Access-Control-Allow-Origin", origin)
			w.Header().Set("Vary", "Origin")
			w.Header().Set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
			w.Header().Set("Access-Control-Allow-Headers", "Content-Type, Authorization, X-Api-Key")
			w.Header().Set("Access-Control-Max-Age", "600")
		}
		if r.Method == http.MethodOptions {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		next.ServeHTTP(w, r)
	})
}

// isLocalOrigin 判断是否为可信的本机来源。
//
// Tauri webview 的 origin 因平台而异，**每个都必须放行**：
//
//	Windows      : http(s)://tauri.localhost   ← 实测最容易漏的一个，漏了前端就是 "Failed to fetch"
//	macOS / Linux: tauri://localhost
//	开发模式      : http://localhost:1420
//
// 注意用 curl 测不出来：curl 不带 Origin 头，根本不走 CORS 分支。
// 验证时必须显式带 `-H "Origin: http://tauri.localhost"`。
func isLocalOrigin(origin string) bool {
	if origin == "" {
		return false
	}
	allowed := []string{
		"tauri://localhost",
		"http://tauri.localhost", "https://tauri.localhost",
		"http://localhost", "https://localhost",
		"http://127.0.0.1", "https://127.0.0.1",
	}
	for _, prefix := range allowed {
		if origin == prefix || strings.HasPrefix(origin, prefix+":") {
			return true
		}
	}
	return false
}

// ── 工具 ─────────────────────────────────────────────────────────────────────

// writeJSON 输出 JSON 响应。
func writeJSON(w http.ResponseWriter, status int, payload any) {
	body, err := json.Marshal(payload)
	if err != nil {
		http.Error(w, `{"error":{"message":"网关内部序列化失败"}}`, http.StatusInternalServerError)
		return
	}
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	_, _ = w.Write(body)
}

// writeError 输出 OpenAI 风格的错误响应（便于各类客户端统一识别）。
func writeError(w http.ResponseWriter, status int, code, message string) {
	writeJSON(w, status, map[string]any{
		"error": map[string]any{"message": message, "type": "gateway_error", "code": code},
	})
}

// isAuthorized 校验本地 API Key。
//
// 网关对外只认自己签发的 key，上游 key 永不外泄给调用方。
func (s *Server) isAuthorized(r *http.Request) bool {
	auth := r.Header.Get("Authorization")
	if strings.HasPrefix(auth, "Bearer ") && strings.TrimSpace(auth[7:]) == s.cfg.APIKey {
		return true
	}
	return r.Header.Get("X-Api-Key") == s.cfg.APIKey
}

// clientIP 取对端地址（仅用于日志）。
func clientIP(r *http.Request) string {
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err != nil {
		return r.RemoteAddr
	}
	return host
}

// ── 路由处理 ─────────────────────────────────────────────────────────────────

// handleHealth 健康检查：返回账号计数。
func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	available, total := s.pool.Available()
	writeJSON(w, http.StatusOK, map[string]any{
		"status":   "ok",
		"accounts": fmt.Sprintf("%d/%d", available, total),
	})
}

// handleAdmin 管理接口（仅本机使用，不需要本地 key）。
func (s *Server) handleAdmin(w http.ResponseWriter, r *http.Request) {
	path := strings.SplitN(r.URL.Path, "?", 2)[0]
	switch {
	case r.Method == http.MethodGet && path == "/api/status":
		snapshot := s.pool.Snapshot()
		writeJSON(w, http.StatusOK, map[string]any{
			"total":    snapshot.Total,
			"healthy":  snapshot.Available,
			"accounts": snapshot.Accounts,
		})
	case r.Method == http.MethodGet && path == "/api/accounts":
		snapshot := s.pool.Snapshot()
		writeJSON(w, http.StatusOK, map[string]any{
			"total":     snapshot.Total,
			"available": snapshot.Available,
			"accounts":  snapshot.Accounts,
			"models":    s.pool.Models(),
		})
	case r.Method == http.MethodGet && path == "/api/models":
		writeJSON(w, http.StatusOK, s.modelsPayload())
	case r.Method == http.MethodGet && path == "/api/config":
		writeJSON(w, http.StatusOK, map[string]any{
			"config":       s.cfg,
			"baseUrl":      s.baseURL(),
			"lan_base_url": s.lanBaseURL(),
		})
	case r.Method == http.MethodPost && path == "/api/check":
		ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
		defer cancel()
		results := s.pool.CheckAll(ctx)
		snapshot := s.pool.Snapshot()
		writeJSON(w, http.StatusOK, map[string]any{
			"results":  results,
			"total":    snapshot.Total,
			"healthy":  snapshot.Available,
			"accounts": snapshot.Accounts,
		})
	case r.Method == http.MethodPost && path == "/api/refresh":
		added, removed, total := s.pool.Refresh()
		writeJSON(w, http.StatusOK, map[string]any{
			"added": added, "removed": removed, "total": total,
			"accounts": s.pool.Snapshot().Accounts,
		})
	case r.Method == http.MethodGet && path == "/api/login/providers":
		s.handleLoginProviders(w, r)
	case r.Method == http.MethodPost && path == "/api/login/oauth/start":
		s.handleLoginStart(w, r)
	case r.Method == http.MethodPost && path == "/api/login/oauth/poll":
		s.handleLoginPoll(w, r)
	case r.Method == http.MethodPost && path == "/api/login/phone/request-code":
		s.handlePhoneRequestCode(w, r)
	case r.Method == http.MethodPost && path == "/api/login/phone/verify-code":
		s.handlePhoneVerifyCode(w, r)
	case r.Method == http.MethodPost && path == "/api/accounts/remove":
		s.handleRemoveAccount(w, r)
	case r.Method == http.MethodPost && path == "/api/credits":
		s.handleCredits(w, r)
	case r.Method == http.MethodPost && path == "/api/accounts/delete-local":
		s.handleDeleteLocalAccount(w, r)
	case r.Method == http.MethodPost && path == "/api/accounts/hide":
		s.handleHideAccount(w, r)
	case r.Method == http.MethodPost && path == "/api/accounts/unhide":
		s.handleUnhideAccount(w, r)
	default:
		writeError(w, http.StatusNotFound, "not_found", "未知的管理接口路径 "+path)
	}
}

// handleOpenAI OpenAI 兼容面（需要本地 key）。
func (s *Server) handleOpenAI(w http.ResponseWriter, r *http.Request) {
	path := strings.SplitN(r.URL.Path, "?", 2)[0]

	if !s.isAuthorized(r) {
		writeError(w, http.StatusUnauthorized, "invalid_api_key",
			"无效的网关 API Key。请使用网关首页展示的 Key（Authorization: Bearer <key>）。")
		return
	}

	if path == "/v1/models" && r.Method == http.MethodGet {
		writeJSON(w, http.StatusOK, s.modelsPayload())
		return
	}

	body, err := io.ReadAll(r.Body)
	if err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "读取请求体失败: "+err.Error())
		return
	}
	// 上游对不认识的工具类型直接 400（tools[N].type is illegal）。
	// 在转发前把 Responses 专有工具转换成等价的 function 工具。
	if cleaned, changed := sanitizeTools(body); changed {
		body = cleaned
	}
	s.forward(w, r, path, body)
}

// modelsPayload 组装 OpenAI 风格的模型清单。
func (s *Server) modelsPayload() map[string]any {
	models := s.pool.Models()
	now := time.Now().Unix()
	data := make([]map[string]any, 0, len(models))
	for _, id := range models {
		data = append(data, map[string]any{
			"id": id, "object": "model", "created": now, "owned_by": "cindy-gateway",
		})
	}
	return map[string]any{"object": "list", "data": data}
}

// forward 核心转发：选号 → 透传 → 失败换号。
//
// 换号只在"上游已响应但尚未写给客户端"这一窗口内进行；一旦开始回传字节，
// 任何错误都只能如实传递给客户端。
//
// 请求体里的 model 会用于选号：上游按账号订阅做二次校验（403），
// 已知无权限的账号直接跳过，不浪费尝试次数。
func (s *Server) forward(w http.ResponseWriter, r *http.Request, path string, body []byte) {
	var (
		tried     []string
		lastError string
	)

	// 解析请求模型（解析失败不拦截请求，按"未知模型"正常选号）
	requestModel := requestBodyModel(body)

	for attempt := 0; attempt < s.cfg.MaxAccountAttempts; attempt++ {
		state := s.pool.SelectFor(requestModel, tried)
		if state == nil {
			break
		}
		tried = append(tried, state.Account.OwnerID)

		upstreamURL := state.Account.Endpoint + path
		req, err := http.NewRequestWithContext(r.Context(), r.Method, upstreamURL, bytes.NewReader(body))
		if err != nil {
			lastError = "构造上游请求失败: " + err.Error()
			s.pool.MarkFailure(state, lastError)
			continue
		}
		// 只保留必要请求头，重建鉴权：不带任何调用方身份到上游
		if ct := r.Header.Get("Content-Type"); ct != "" {
			req.Header.Set("Content-Type", ct)
		}
		if accept := r.Header.Get("Accept"); accept != "" {
			req.Header.Set("Accept", accept)
		}
		req.Header.Set("Authorization", "Bearer "+state.Account.APIKey)
		req.Header.Set("X-Api-Key", state.Account.APIKey)
		req.Header.Set("User-Agent", "cindy2api/0.1")

		resp, err := s.client.Do(req)
		if err != nil {
			lastError = "连接失败: " + pool.DescribeConnectError(err)
			s.logger.Printf("账号 %s %s", short(state.Account.OwnerID), lastError)
			s.pool.MarkFailure(state, lastError)
			continue
		}

		if resp.StatusCode == http.StatusForbidden {
			snippet, _ := io.ReadAll(io.LimitReader(resp.Body, 2048))
			_ = resp.Body.Close()
			allowed, isModelDenial := parseModelDenial(snippet)
			if isModelDenial {
				// 模型级失败：账号是好的，只是没这个模型的权限。
				// 记下服务端下发的可用清单，然后换号重放。
				s.logger.Printf("账号 %s 模型 %s 无权限（允许：%d 个）",
					short(state.Account.OwnerID), requestModel, len(allowed))
				s.pool.RecordModelDenied(state, requestModel, allowed)
				lastError = fmt.Sprintf("HTTP 403 模型 %s 无权限", requestModel)
				continue
			}
			// 不是模型权限问题（key 失效等）：按账号级失败处理
			lastError = fmt.Sprintf("HTTP %d %s", resp.StatusCode, strings.TrimSpace(string(snippet)))
			s.logger.Printf("账号 %s %s", short(state.Account.OwnerID), lastError)
			s.pool.MarkFailure(state, lastError)
			continue
		}

		if pool.IsAccountLevelFailure(resp.StatusCode) {
			snippet, _ := io.ReadAll(io.LimitReader(resp.Body, 300))
			_ = resp.Body.Close()
			lastError = fmt.Sprintf("HTTP %d %s", resp.StatusCode, strings.TrimSpace(string(snippet)))
			s.logger.Printf("账号 %s %s", short(state.Account.OwnerID), lastError)
			s.pool.MarkFailure(state, lastError)
			continue // 换下一个账号重放同一请求
		}

		s.pool.MarkSuccess(state)
		s.pipe(w, resp, state)
		return
	}

	writeError(w, http.StatusBadGateway, "no_available_account",
		fmt.Sprintf("反代网关：所有账号均不可用（已尝试 %d 个）。最后错误：%s", len(tried), lastError))
}

// requestBodyModel 从 OpenAI 风格请求体里取 model 字段；解析失败返回空串。
func requestBodyModel(body []byte) string {
	var probe struct {
		Model string `json:"model"`
	}
	if err := json.Unmarshal(body, &probe); err != nil {
		return ""
	}
	return probe.Model
}

// parseModelDenial 识别 403 响应是否为"模型无权限"。
// 服务端返回形如：user not allowed to access model. This user can only
// access models=['a', 'b', ...]。命中则返回服务端给出的可用清单；
// 被拒模型由调用方用请求体里的 model 补齐（最准确）。
func parseModelDenial(body []byte) (allowed []string, isModelDenial bool) {
	text := string(body)
	if !strings.Contains(text, "not allowed to access model") {
		return nil, false
	}
	idx := strings.Index(text, "models=[")
	if idx < 0 {
		return nil, true // 是模型拒绝，但清单被截断（日志 300 字节上限等情况）
	}
	list := text[idx+len("models=["):]
	if end := strings.Index(list, "]"); end >= 0 {
		list = list[:end]
	}
	for _, part := range strings.Split(list, ",") {
		part = strings.TrimSpace(strings.Trim(strings.TrimSpace(part), "'\""))
		if part != "" {
			allowed = append(allowed, part)
		}
	}
	return allowed, true
}

// pipe 把上游响应回传给客户端。
//
// 逐块读 + 显式 Flush：SSE 断点若不做这一步，net/http 会攒够缓冲区才发给客户端，
// 表现为"流式请求卡住不吐字"。
func (s *Server) pipe(w http.ResponseWriter, resp *http.Response, state *pool.AccountState) {
	defer resp.Body.Close()

	for name, values := range resp.Header {
		// 逐跳首部交给本地连接自己管理，不能透传
		switch strings.ToLower(name) {
		case "connection", "keep-alive", "transfer-encoding", "upgrade",
			"proxy-authenticate", "proxy-authorization", "te", "trailer", "content-length":
			continue
		}
		for _, value := range values {
			w.Header().Add(name, value)
		}
	}
	// 便于排查请求最终落到哪个账号与上游
	w.Header().Set("X-Gateway-Account", short(state.Account.OwnerID))
	w.Header().Set("X-Gateway-Upstream", hostOf(state.Account.Endpoint))

	w.WriteHeader(resp.StatusCode)

	flusher, _ := w.(http.Flusher)
	buf := make([]byte, 32*1024)
	for {
		n, err := resp.Body.Read(buf)
		if n > 0 {
			if _, writeErr := w.Write(buf[:n]); writeErr != nil {
				return // 客户端断开
			}
			if flusher != nil {
				flusher.Flush()
			}
		}
		if err != nil {
			return
		}
	}
}

// short 取 ownerID 前 8 位用于日志与响应头（不含任何凭据）。
func short(ownerID string) string {
	if len(ownerID) <= 8 {
		return ownerID
	}
	return ownerID[:8]
}

// ── 工具类型清洗 ─────────────────────────────────────────────────────────────
//
// 上游（litellm/智谱链路）对 tools[] 里的 type 做白名单校验，遇到
// local_shell / apply_patch / freeform 这类 Responses API 专有类型时直接
// 400 "tools[N].type:type is illegal"（实测逐型验证的结论）：
//
//	function / web_search / custom —— 接受
//	local_shell / apply_patch / freeform —— 400
//
// 客户端（Codex CLI）习惯发 Responses 专有工具，因此转发前把它们改写成
// 语义等价的 function 工具；上游回 function_call，网关侧无需再翻译回
// 专有格式——function_call 本就是 Responses 协议的标准输出类型。

// sanitizeTools 把请求体里上游不接受的工具类型改写为 function 工具。
// body 无法解析或没有 tools 时原样返回（changed=false）。
func sanitizeTools(body []byte) ([]byte, bool) {
	var payload map[string]json.RawMessage
	if err := json.Unmarshal(body, &payload); err != nil {
		return body, false
	}
	rawTools, ok := payload["tools"]
	if !ok {
		return body, false
	}
	var tools []map[string]any
	if err := json.Unmarshal(rawTools, &tools); err != nil {
		return body, false
	}

	changed := false
	converted := make([]map[string]any, 0, len(tools))
	for _, tool := range tools {
		kind, _ := tool["type"].(string)
		switch kind {
		case "function", "web_search", "custom":
			converted = append(converted, tool) // 上游明确接受
		case "local_shell":
			converted = append(converted, asFunctionTool(tool, "shell",
				"Runs a shell command and returns its output.",
				map[string]any{
					"type":       "object",
					"properties": map[string]any{"command": map[string]any{"type": "array", "items": map[string]any{"type": "string"}}},
					"required":   []string{"command"},
				}))
			changed = true
		case "apply_patch":
			converted = append(converted, asFunctionTool(tool, "apply_patch",
				"Applies a patch to the workspace. Input is the full patch text.",
				map[string]any{
					"type":       "object",
					"properties": map[string]any{"input": map[string]any{"type": "string", "description": "The complete patch text to apply"}},
					"required":   []string{"input"},
				}))
			changed = true
		case "freeform":
			converted = append(converted, asFunctionTool(tool, stringOf(tool["name"]),
				"Freeform tool. Input is raw text.",
				map[string]any{
					"type":       "object",
					"properties": map[string]any{"input": map[string]any{"type": "string"}},
					"required":   []string{"input"},
				}))
			changed = true
		default:
			// 未知类型：直接剔除比赌上游接受更稳，至少请求能走通
			changed = true
		}
	}
	if !changed {
		return body, false
	}

	payload["tools"] = mustJSON(converted)
	out, err := json.Marshal(payload)
	if err != nil {
		return body, false
	}
	return out, true
}

// asFunctionTool 构造与专有工具同名的 function 工具，保留 name/description。
func asFunctionTool(tool map[string]any, fallbackName, description string, parameters map[string]any) map[string]any {
	name := stringOf(tool["name"])
	if name == "" {
		name = fallbackName
	}
	desc := stringOf(tool["description"])
	if desc == "" {
		desc = description
	}
	return map[string]any{
		"type":        "function",
		"name":        name,
		"description": desc,
		"parameters":  parameters,
		"strict":      false,
	}
}

func stringOf(v any) string {
	s, _ := v.(string)
	return s
}

func mustJSON(v any) json.RawMessage {
	raw, err := json.Marshal(v)
	if err != nil {
		return json.RawMessage("[]")
	}
	return raw
}

// hostOf 取 URL 的 host 部分。
func hostOf(rawURL string) string {
	trimmed := strings.TrimPrefix(strings.TrimPrefix(rawURL, "https://"), "http://")
	if idx := strings.Index(trimmed, "/"); idx >= 0 {
		return trimmed[:idx]
	}
	return trimmed
}

// baseURL 网关的 OpenAI 兼容基址（前端"Base URL"一行展示的就是它）。
func (s *Server) baseURL() string {
	host, port, err := net.SplitHostPort(s.cfg.Listen)
	if err != nil {
		return "http://" + s.cfg.Listen + "/v1"
	}
	if host == "" || host == "0.0.0.0" || host == "::" {
		host = "127.0.0.1"
	}
	return "http://" + net.JoinHostPort(host, port) + "/v1"
}

// lanBaseURL 局域网可访问基址。只有监听非 loopback 时局域网才真的能连上，
// 所以这里如实返回空串，避免前端展示一个连不通的地址。
func (s *Server) lanBaseURL() string {
	host, port, err := net.SplitHostPort(s.cfg.Listen)
	if err != nil || (host != "0.0.0.0" && host != "::") {
		return ""
	}
	ip := lanIP()
	if ip == "" {
		return ""
	}
	return "http://" + net.JoinHostPort(ip, port) + "/v1"
}

// lanIP 找一个可用于局域网访问的 IPv4（优先 192.168 段）。
func lanIP() string {
	addrs, err := net.InterfaceAddrs()
	if err != nil {
		return ""
	}
	var fallback string
	for _, addr := range addrs {
		ipNet, ok := addr.(*net.IPNet)
		if !ok || ipNet.IP.IsLoopback() || ipNet.IP.To4() == nil {
			continue
		}
		ip := ipNet.IP.String()
		if strings.HasPrefix(ip, "192.168.") {
			return ip
		}
		if fallback == "" {
			fallback = ip
		}
	}
	return fallback
}

// ── 登录：OAuth 授权添加账号 ─────────────────────────────────────────────────

// openInBrowser 用系统默认浏览器打开授权页。
func openInBrowser(rawURL string) error {
	var cmd *exec.Cmd
	switch runtime.GOOS {
	case "windows":
		// rundll32 不依赖 cmd 的引号处理，也不会弹出控制台窗口
		cmd = exec.Command("rundll32", "url.dll,FileProtocolHandler", rawURL)
	case "darwin":
		cmd = exec.Command("open", rawURL)
	default:
		cmd = exec.Command("xdg-open", rawURL)
	}
	return cmd.Start()
}

// accountIDFromKey 由网关 key 派生稳定账号 ID。
//
// 同一账号重复授权会得到同一 ID，因此写入走覆盖语义，不会堆出重复卡片。
func accountIDFromKey(apiKey string) string {
	sum := sha256.Sum256([]byte(apiKey))
	return hex.EncodeToString(sum[:8])
}

// handleLoginProviders 查询某区域支持的登录方式，原样转发给前端展示。
func (s *Server) handleLoginProviders(w http.ResponseWriter, r *http.Request) {
	payload, err := oauth.FetchProviders(r.Context(), s.client, r.URL.Query().Get("region"))
	if err != nil {
		writeError(w, http.StatusBadGateway, "providers_unavailable", err.Error())
		return
	}
	writeJSON(w, http.StatusOK, payload)
}

// handleLoginStart 创建授权会话并拉起系统浏览器。
func (s *Server) handleLoginStart(w http.ResponseWriter, r *http.Request) {
	var body struct {
		// Kind "social"（默认，Apple/Google）或 "sso"（企业单点登录）
		Kind     string `json:"kind"`
		Provider string `json:"provider"`
		Region   string `json:"region"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "请求体解析失败："+err.Error())
		return
	}
	if body.Kind == "" {
		body.Kind = "social"
	}

	session, err := oauth.StartSession(body.Kind, body.Provider, body.Region, s.cfg.DeviceID)
	if err != nil {
		writeError(w, http.StatusBadRequest, "login_start_failed", err.Error())
		return
	}
	if err := openInBrowser(session.AuthorizeURL); err != nil {
		// 打不开浏览器不算失败：授权链接一并返回，用户可手动打开
		s.logger.Printf("打开系统浏览器失败：%v（已把授权链接返回给前端）", err)
	}
	s.logger.Printf("已发起 %s 授权（区域 %s），等待用户在浏览器完成", session.Provider, session.Region)

	writeJSON(w, http.StatusOK, map[string]any{
		"sessionId":    session.ID,
		"authorizeUrl": session.AuthorizeURL,
		"provider":     session.Provider,
		"region":       session.Region,
	})
}

// handleLoginPoll 轮询授权结果；成功后自动兑换令牌、取网关凭据并落成账号。
//
// 前端在浏览器打开期间反复调用本接口，直到拿到终态。
func (s *Server) handleLoginPoll(w http.ResponseWriter, r *http.Request) {
	var body struct {
		SessionID string `json:"sessionId"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "请求体解析失败："+err.Error())
		return
	}
	session := oauth.GetSession(body.SessionID)
	if session == nil {
		writeError(w, http.StatusGone, "session_expired", "授权会话已过期或不存在，请重新发起授权。")
		return
	}

	status, code, err := session.Poll(r.Context(), s.client)
	if err != nil {
		oauth.DropSession(session.ID)
		writeError(w, http.StatusBadRequest, "login_failed", err.Error())
		return
	}
	if status == "expired" {
		oauth.DropSession(session.ID)
		writeError(w, http.StatusGone, "session_expired", "授权已过期，请重新发起。")
		return
	}
	if status != "ok" {
		writeJSON(w, http.StatusOK, map[string]any{"status": status})
		return
	}

	// 授权码 → 令牌（PKCE 兑换）
	pair, err := session.Exchange(r.Context(), s.client, code)
	if err != nil {
		oauth.DropSession(session.ID)
		writeError(w, http.StatusBadRequest, "exchange_failed", err.Error())
		return
	}
	// 令牌 → 反代网关真正要用的 {endpoint, apiKey}
	endpoint, apiKey, err := oauth.FetchGatewayCredentials(r.Context(), s.client, session.Region, pair.AccessToken)
	if err != nil {
		oauth.DropSession(session.ID)
		writeError(w, http.StatusBadGateway, "credentials_failed", err.Error())
		return
	}

	label := pair.DisplayName
	if label == "" {
		label = pair.Email
	}
	if label == "" {
		label = "Cindy 账号"
	}
	if err := s.manual.Add(manualaccounts.Account{
		ID:           accountIDFromKey(apiKey),
		Label:        label,
		Endpoint:     endpoint,
		APIKey:       apiKey,
		RefreshToken: pair.RefreshToken,
		Region:       session.Region,
		AddedAt:      time.Now().UnixMilli(),
	}); err != nil {
		writeError(w, http.StatusInternalServerError, "persist_failed", "账号保存失败："+err.Error())
		return
	}
	oauth.DropSession(session.ID)

	// 重建账号池并后台补一次健康检查，让新账号尽快出现在列表
	s.pool.Refresh()
	go s.recheckAsync()
	s.logger.Printf("OAuth 授权成功，已添加账号：%s（%s）", label, endpoint)

	// 距发起不足 3 秒就拿到结果 → 取回的是**上一次**浏览器里已完成、当时没被取走的
	// 授权码（服务端按 deviceId 暂存）。如实告诉前端，避免用户以为"没进授权页就成功了"。
	reusedPrevious := time.Since(session.CreatedAt) < 3*time.Second

	writeJSON(w, http.StatusOK, map[string]any{
		"status":                      "ok",
		"label":                       label,
		"endpoint":                    endpoint,
		"accounts":                    s.pool.Snapshot().Accounts,
		"reusedPreviousAuthorization": reusedPrevious,
	})
}

// handleRemoveAccount 移除一个手动添加的账号。
//
// 自动发现的账号不可删 —— 它们属于 Cindy 客户端，删本地凭据文件会破坏用户的登录态。
func (s *Server) handleRemoveAccount(w http.ResponseWriter, r *http.Request) {
	var body struct {
		AccountID string `json:"accountId"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "请求体解析失败："+err.Error())
		return
	}
	const oauthPrefix = "oauth-"
	if !strings.HasPrefix(body.AccountID, oauthPrefix) {
		writeError(w, http.StatusBadRequest, "not_removable",
			"只能移除通过授权添加的账号；本机自动发现的账号属于 Cindy 客户端，请在其登录/登出流程中管理。")
		return
	}
	if err := s.manual.Remove(strings.TrimPrefix(body.AccountID, oauthPrefix)); err != nil {
		writeError(w, http.StatusInternalServerError, "persist_failed", "移除失败："+err.Error())
		return
	}
	s.pool.Refresh()
	go s.recheckAsync()
	writeJSON(w, http.StatusOK, map[string]any{
		"status":   "ok",
		"accounts": s.pool.Snapshot().Accounts,
	})
}

// ── 屏蔽 / 解除屏蔽 ─────────────────────────────────────────────────────────

// ── 额度查询 ────────────────────────────────────────────────────────────────

// handleCredits 查询所有「手动添加」账号的额度。
//
// 为什么本机账号查不了：它们手里只有网关 key，而 model-access 的额度接口只认
// 登录令牌（实测 apiKey 直接 401）—— 本机账号没有 refreshToken 可换，前端显示占位。
//
// ⚠️ refresh 会**轮换** refreshToken：每查一个号都必须把新值写回存储，
// 漏一步这个号就丢登录态。
func (s *Server) handleCredits(w http.ResponseWriter, r *http.Request) {
	ctx, cancel := context.WithTimeout(r.Context(), 120*time.Second)
	defer cancel()

	results := make([]map[string]any, 0)
	for _, item := range s.manual.List() {
		if item.RefreshToken == "" {
			continue
		}
		entry := map[string]any{
			"accountId": "oauth-" + item.ID,
			"region":    item.Region,
		}
		pair, err := oauth.RefreshAccessToken(ctx, s.client, item.Region, item.RefreshToken, s.cfg.DeviceID)
		if err != nil {
			entry["error"] = err.Error()
			results = append(results, entry)
			continue
		}
		item.RefreshToken = pair.RefreshToken
		_ = s.manual.Add(item) // Add 是覆盖语义，正好用来持久化轮换后的 refreshToken

		balance, err := oauth.FetchCreditBalance(ctx, s.client, item.Region, pair.AccessToken)
		if err != nil {
			entry["error"] = err.Error()
			results = append(results, entry)
			continue
		}
		entry["available"] = balance.Available
		entry["total"] = balance.Total
		entry["used"] = balance.Used
		entry["scale"] = balance.Scale
		results = append(results, entry)
		s.logger.Printf("额度：账号 %s（%s）可用 %s", item.Label, item.Region, balance.Available)
	}
	writeJSON(w, http.StatusOK, map[string]any{"credits": results})
}

// handleDeleteLocalAccount 删除本机账号的专属凭据文件。
//
// 语义：等价于该账号在 Cindy 客户端登出，下次需重新登录 —— 这是用户明确要求的行为
// （平台页接管多账号切换，不再依赖客户端登录态）。
//
// 安全边界见 `cindyaccount.DeleteLocalAccount`：只删 `owner_<id>_*.enc`，
// **绝不碰共用的 Local State**（主密钥，删了整个 profile 的账号都会失效）。
func (s *Server) handleDeleteLocalAccount(w http.ResponseWriter, r *http.Request) {
	var body struct {
		AccountID string `json:"accountId"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "请求体解析失败："+err.Error())
		return
	}
	// 授权添加的账号不属于 Cindy 客户端，走 /api/accounts/remove
	if strings.HasPrefix(body.AccountID, "oauth-") {
		writeError(w, http.StatusBadRequest, "invalid_request",
			"授权添加的账号请使用删除接口（/api/accounts/remove），本接口只用于移除本机登录态。")
		return
	}

	removed, err := cindyaccount.DeleteLocalAccount(body.AccountID)
	if err != nil {
		writeError(w, http.StatusInternalServerError, "delete_failed", err.Error())
		return
	}
	if len(removed) == 0 {
		writeError(w, http.StatusNotFound, "nothing_removed",
			"没有找到该账号的本机凭据文件（可能已被移除或账号标识已变化）。")
		return
	}

	s.pool.Refresh()
	s.logger.Printf("已删除本机账号 %s 的 %d 个凭据文件", body.AccountID, len(removed))
	writeJSON(w, http.StatusOK, map[string]any{
		"status":   "ok",
		"removed":  removed,
		"accounts": s.pool.Snapshot().Accounts,
	})
}

// handleHideAccount 屏蔽一个账号。
//
// 本机账号的凭据属于 Cindy 客户端（`owner_*_api_key.enc`），从网关侧删文件会破坏
// 用户的登录态、且客户端下次登录还会写回来 —— 所以本机账号走屏蔽（可恢复），
// OAuth 账号才走真正的删除（/api/accounts/remove）。
func (s *Server) handleHideAccount(w http.ResponseWriter, r *http.Request) {
	var body struct {
		AccountID string `json:"accountId"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "请求体解析失败："+err.Error())
		return
	}
	if body.AccountID == "" {
		writeError(w, http.StatusBadRequest, "invalid_request", "缺少 accountId")
		return
	}
	if err := s.hidden.Add(body.AccountID); err != nil {
		writeError(w, http.StatusInternalServerError, "persist_failed", err.Error())
		return
	}
	s.pool.Refresh()
	s.logger.Printf("已屏蔽账号：%s", body.AccountID)
	writeJSON(w, http.StatusOK, map[string]any{"status": "ok", "accounts": s.pool.Snapshot().Accounts})
}

// handleUnhideAccount 解除屏蔽（让账号重新回到池子里）。
func (s *Server) handleUnhideAccount(w http.ResponseWriter, r *http.Request) {
	var body struct {
		AccountID string `json:"accountId"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "请求体解析失败："+err.Error())
		return
	}
	if err := s.hidden.Remove(body.AccountID); err != nil {
		writeError(w, http.StatusInternalServerError, "persist_failed", err.Error())
		return
	}
	s.pool.Refresh()
	writeJSON(w, http.StatusOK, map[string]any{"status": "ok", "accounts": s.pool.Snapshot().Accounts})
}

// ── 手机号 + 短信验证码登录（中国大陆版的主要方式） ──────────────────────────

// handlePhoneRequestCode 请求发送短信验证码。
func (s *Server) handlePhoneRequestCode(w http.ResponseWriter, r *http.Request) {
	var body struct {
		Phone  string `json:"phone"`
		Region string `json:"region"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "请求体解析失败："+err.Error())
		return
	}
	if err := oauth.RequestPhoneCode(r.Context(), s.client, body.Region, body.Phone); err != nil {
		writeError(w, http.StatusBadGateway, "request_code_failed", err.Error())
		return
	}
	s.logger.Printf("已请求短信验证码（%s，区域 %s）", maskPhone(body.Phone), body.Region)
	writeJSON(w, http.StatusOK, map[string]any{"status": "sent"})
}

// handlePhoneVerifyCode 校验短信验证码并落成账号。
//
// 后续与 OAuth 路径完全合流：令牌 → 换 {endpoint, apiKey} → 写账号表 → 刷池。
func (s *Server) handlePhoneVerifyCode(w http.ResponseWriter, r *http.Request) {
	var body struct {
		Phone  string `json:"phone"`
		Code   string `json:"code"`
		Region string `json:"region"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		writeError(w, http.StatusBadRequest, "invalid_request", "请求体解析失败："+err.Error())
		return
	}
	pair, err := oauth.VerifyPhoneCode(r.Context(), s.client, body.Region, body.Phone, body.Code, s.cfg.DeviceID)
	if err != nil {
		writeError(w, http.StatusBadRequest, "login_failed", err.Error())
		return
	}
	label, endpoint, err := s.persistLoginAccount(r.Context(), pair, body.Region)
	if err != nil {
		writeError(w, http.StatusBadGateway, "credentials_failed", err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"status":   "ok",
		"label":    label,
		"endpoint": endpoint,
		"accounts": s.pool.Snapshot().Accounts,
	})
}

// persistLoginAccount 把登录得到的令牌对落成账号；OAuth 与手机号两条路径共用。
func (s *Server) persistLoginAccount(ctx context.Context, pair *oauth.TokenPair, region string) (label, endpoint string, err error) {
	gatewayEndpoint, apiKey, err := oauth.FetchGatewayCredentials(ctx, s.client, region, pair.AccessToken)
	if err != nil {
		return "", "", err
	}
	label = pair.DisplayName
	if label == "" {
		label = pair.Email
	}
	if label == "" {
		label = "Cindy 账号"
	}
	if err := s.manual.Add(manualaccounts.Account{
		ID:           accountIDFromKey(apiKey),
		Label:        label,
		Endpoint:     gatewayEndpoint,
		APIKey:       apiKey,
		RefreshToken: pair.RefreshToken,
		Region:       region,
		AddedAt:      time.Now().UnixMilli(),
	}); err != nil {
		return "", "", fmt.Errorf("账号保存失败: %w", err)
	}
	s.pool.Refresh()
	// **同步**做一次探测再返回：早先这里是 `go s.recheckAsync()`（异步），
	// 结果是登录接口立刻返回、前端拿到的状态是 "unknown"，卡片上显示"未探测"，
	// 用户得等下一轮巡检（默认 5 分钟）才能看到真实状态。
	// 探测是并发进行的，只花几秒，换来"登录完即见结果"的体验，值得。
	s.recheckAsync()
	s.logger.Printf("已添加账号：%s（%s）", label, gatewayEndpoint)
	return label, gatewayEndpoint, nil
}

// maskPhone 手机号脱敏，用于日志。
func maskPhone(phone string) string {
	if len(phone) <= 4 {
		return phone
	}
	return phone[:3] + "****" + phone[len(phone)-2:]
}

// recheckAsync 在后台对账号池做一次健康检查（不阻塞调用方响应）。
func (s *Server) recheckAsync() {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	s.pool.CheckAll(ctx)
}

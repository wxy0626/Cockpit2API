// login.go OAuth 设备授权登录：start 拿授权 URL，poll 轮询结果。
// 成功后凭证落盘 + 账号池热同步（原控制台是重启容器，现在直接 SyncToDir 即可）。
package admin

import (
	"bytes"
	"encoding/json"
	"io"
	"net/http"
	"path/filepath"
	"time"

	"workbuddy2api/internal/auth"
)

// 上游端点与常量（与 cmd/login/main.go 保持一致，CN realm only）。
const (
	upstreamBaseCN    = "https://copilot.tencent.com"
	billingBaseCN     = "https://www.codebuddy.cn"
	clientUA          = "CLI/2.63.2 CodeBuddy/2.63.2"
	originReferer     = "https://www.codebuddy.cn"
	endpointAuthState = upstreamBaseCN + "/v2/plugin/auth/state?platform=CLI"
	endpointAuthToken = upstreamBaseCN + "/v2/plugin/auth/token?state="
	endpointLoginAcct = upstreamBaseCN + "/v2/plugin/login/account?state="
)

// upstreamHeaders 登录阶段通用请求头（无凭证态）。
func upstreamHeaders(extra map[string]string) map[string]string {
	h := map[string]string{
		"Content-Type":     "application/json",
		"Accept":           "application/json, text/plain, */*",
		"X-Requested-With": "XMLHttpRequest",
		"Origin":           originReferer,
		"Referer":          originReferer + "/",
		"User-Agent":       clientUA,
	}
	for k, v := range extra {
		h[k] = v
	}
	return h
}

// upstreamEnvelope 上游 {code,msg,data} 信封。
type upstreamEnvelope struct {
	Code int             `json:"code"`
	Msg  string          `json:"msg"`
	Data json.RawMessage `json:"data"`
}

// upstreamJSON 调用上游信封接口：code=0 返回 data，业务失败返回错误。
func upstreamJSON(method, url string, extra map[string]string, body []byte) (json.RawMessage, error) {
	var reader io.Reader
	if body != nil {
		reader = bytes.NewReader(body)
	}
	req, err := http.NewRequest(method, url, reader)
	if err != nil {
		return nil, err
	}
	for k, v := range upstreamHeaders(extra) {
		req.Header.Set(k, v)
	}
	client := &http.Client{Timeout: 30 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	var env upstreamEnvelope
	if json.Unmarshal(raw, &env) != nil {
		return nil, errLoginPending
	}
	if env.Code != 0 {
		return nil, errLoginPending
	}
	return env.Data, nil
}

// errLoginPending 登录未完成（上游 pending 时业务 code 非 0）。
var errLoginPending = &pendingError{}

type pendingError struct{}

func (*pendingError) Error() string { return "login pending" }

// handleLoginStart POST /api/login/start：申请 state 与授权 URL。
func (s *Server) handleLoginStart(w http.ResponseWriter) {
	data, err := upstreamJSON(http.MethodPost, endpointAuthState, nil, []byte("{}"))
	if err != nil {
		if _, pending := err.(*pendingError); !pending {
			s.writeJSON(w, 200, map[string]any{"error": shortMsg(err.Error(), 100)})
			return
		}
		s.writeJSON(w, 200, map[string]any{"error": "上游未返回授权状态"})
		return
	}
	var st struct {
		State   string `json:"state"`
		AuthURL string `json:"authUrl"`
	}
	if json.Unmarshal(data, &st) != nil || st.State == "" || st.AuthURL == "" {
		s.writeJSON(w, 200, map[string]any{"error": "上游未返回 state/authUrl"})
		return
	}
	s.mu.Lock()
	s.loginState = st.State
	s.mu.Unlock()
	s.writeJSON(w, 200, map[string]any{"authUrl": st.AuthURL})
}

// handleLoginPoll POST /api/login/poll：轮询登录结果。
// 成功：凭证落盘 + 初始签到 + 账号池热同步。
func (s *Server) handleLoginPoll(w http.ResponseWriter) {
	s.mu.Lock()
	state := s.loginState
	s.mu.Unlock()
	if state == "" {
		s.writeJSON(w, 200, map[string]any{"status": "error", "message": "请先点击「获取登录链接」"})
		return
	}

	// auth/token 是权威登录状态端点：pending 时业务 code 非 0，完成时 code=0 + token bundle
	tokRaw, err := upstreamJSON(http.MethodGet, endpointAuthToken+state, nil, nil)
	if err != nil {
		s.writeJSON(w, 200, map[string]any{"status": "pending"})
		return
	}
	var tok struct {
		AccessToken  string `json:"accessToken"`
		RefreshToken string `json:"refreshToken"`
		ExpiresIn    int64  `json:"expiresIn"`
		Domain       string `json:"domain"`
	}
	if json.Unmarshal(tokRaw, &tok) != nil || tok.AccessToken == "" {
		s.writeJSON(w, 200, map[string]any{"status": "pending"})
		return
	}

	// login/account 拿 uid/nickname（带 Bearer）
	var acct struct {
		UID          string `json:"uid"`
		EnterpriseID string `json:"enterpriseId"`
		Nickname     string `json:"nickname"`
	}
	if acctRaw, err := upstreamJSON(http.MethodGet, endpointLoginAcct+state,
		map[string]string{"Authorization": "Bearer " + tok.AccessToken}, nil); err == nil {
		_ = json.Unmarshal(acctRaw, &acct)
	}
	if acct.UID == "" {
		s.writeJSON(w, 200, map[string]any{"status": "error", "message": "无法获取 uid，token 可能无效"})
		return
	}

	// 凭证落盘（嵌套形，与插件输出一致）
	record := map[string]any{
		"account": map[string]any{"uid": acct.UID, "enterpriseId": acct.EnterpriseID, "nickname": acct.Nickname},
		"auth": map[string]any{
			"accessToken":  tok.AccessToken,
			"refreshToken": tok.RefreshToken,
			"expiresAt":    time.Now().Unix() + tok.ExpiresIn,
			"domain":       tok.Domain,
		},
	}
	fp := filepath.Join(s.Opt.AuthDir, "workbuddy-"+acct.UID+".json")
	if err := writeJSONAtomic(fp, record); err != nil {
		s.writeJSON(w, 200, map[string]any{"status": "error", "message": shortMsg("落盘失败: "+err.Error(), 80)})
		return
	}

	// 初始签到（失败不阻断登录结果）
	checkinMsg := "签到成功"
	a := &auth.Auth{
		AccessToken:  tok.AccessToken,
		RefreshToken: tok.RefreshToken,
		ExpiresAt:    time.Now().Unix() + tok.ExpiresIn,
		Domain:       tok.Domain,
		UID:          acct.UID,
		EnterpriseID: acct.EnterpriseID,
		Nickname:     acct.Nickname,
		FilePath:     fp,
	}
	if err := s.Opt.Up.DailyCheckin(a); err != nil {
		checkinMsg = shortMsg(err.Error(), 40)
	}

	// 账号池热同步：新账号直接入池，无需重启进程
	if s.Opt.OnAuthsChanged != nil {
		s.Opt.OnAuthsChanged()
	}

	s.mu.Lock()
	s.loginState = ""
	s.mu.Unlock()

	s.writeJSON(w, 200, map[string]any{
		"status":  "ok",
		"account": map[string]any{"uid": acct.UID, "nickname": acct.Nickname},
		"checkin": checkinMsg,
		"restart": "账号池已热加载，无需重启",
	})
}

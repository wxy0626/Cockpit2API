// Package admin 本机管理 API（默认 127.0.0.1:7864）。
//
// 网关由桌面 App 以 sidecar 直接拉起后，原 Python 控制台（webadmin.py）退役，
// 其全部管理端点由本包在网关进程内提供，语义与返回结构保持兼容：
//
//	GET  /api/status            网关池状态（代理本进程 /status）
//	GET  /api/accounts          账号列表（含缓存积分与今日签到态）
//	GET  /api/models            模型列表（代理 /v1/models）
//	GET  /api/config            读配置
//	POST /api/config            写配置（保存后进程优雅退出，由 App 重拉生效）
//	GET  /api/logs?n=200        进程日志环形缓冲最近 N 行
//	GET  /api/backups           备份列表
//	POST /api/backup            创建备份
//	GET  /api/backup/download?f= 下载备份
//	POST /api/login/start       OAuth 登录第一步（拿授权 URL）
//	POST /api/login/poll        OAuth 登录轮询（成功落盘并热加载入池）
//	POST /api/signin            一键签到
//	POST /api/signin/one        单账号签到
//	POST /api/credits           全账号积分查询
//	POST /api/credits/one       单账号积分查询
//	POST /api/keepalive         全账号 token 保活
//	POST /api/account/delete    删除账号（删文件 + 热同步池）
//	POST /api/account/max-in-flight {uid, limit} 设置单账号并发上限（limit<=0 回落全局）
//	POST /api/chat              对话测试（走本进程 /v1 完整链路）
//
// 安全：仅监听回环地址；能操作凭证与配置，不对局域网暴露。
package admin

import (
	"context"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"regexp"
	"strings"
	"sync"
	"time"

	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/upstream"
)

// Options 管理端构造参数。
type Options struct {
	Listen      string // 监听地址，默认 "127.0.0.1:7864"
	BaseDir     string // 项目根目录（auths/ data/ backups/ config.json 所在）
	AuthDir     string // 凭证目录
	DataDir     string // 池状态目录（state.json / credits.json / checkin.json）
	BackupDir   string // 备份输出目录
	ConfigPath  string // config.json 路径
	DisplayBase string // 对外展示的接入地址（App 页面显示用）
	GatewayBase string // 本进程 API 地址（自调用），默认 http://127.0.0.1:7863
	APIKey      string // 网关 API Key（自调用时携带）
	Pool        *pool.Pool
	Up          *upstream.Client

	// OnAuthsChanged 账号文件变化（登录落盘/删除）后由处理方调用，用于热同步账号池。
	OnAuthsChanged func()
	// OnConfigSaved 配置保存后由处理方调用（异步），触发进程优雅退出以便 App 重拉生效。
	OnConfigSaved func()
}

// Server 管理端 HTTP 服务器。
type Server struct {
	Opt Options // 构造参数（由 main 注入）

	srv *http.Server

	mu         sync.Mutex
	loginState string // 最近一次 OAuth 登录会话的 state
}

// reLoopbackOrigin 允许的跨域来源：本机回环 + Tauri 桌面 App webview。
var (
	reLoopback = regexp.MustCompile(`^https?://(localhost|127\.0\.0\.1|\[::1\])(:\d+)?$`)
	reTauri    = regexp.MustCompile(`^https?://tauri\.localhost(:\d+)?$`)
)

// corsOrigin 返回允许的 Origin（不做通配放行——管理端能操作凭证与配置）。
func corsOrigin(origin string) string {
	if origin == "" {
		return ""
	}
	if reLoopback.MatchString(origin) || reTauri.MatchString(origin) || origin == "tauri://localhost" {
		return origin
	}
	return ""
}

// Serve 启动管理端（阻塞直至 ctx 取消）。
func (s *Server) Serve(ctx context.Context) error {
	if s.Opt.Listen == "" {
		s.Opt.Listen = "127.0.0.1:7864"
	}
	if s.Opt.GatewayBase == "" {
		s.Opt.GatewayBase = "http://127.0.0.1:7863"
	}
	mux := http.NewServeMux()
	mux.HandleFunc("/api/", s.handleAPI)
	s.srv = &http.Server{
		Addr:              s.Opt.Listen,
		Handler:           mux,
		ReadHeaderTimeout: 10 * time.Second,
	}
	go func() {
		<-ctx.Done()
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
		defer cancel()
		_ = s.srv.Shutdown(shutdownCtx)
	}()
	ln, err := net.Listen("tcp", s.Opt.Listen)
	if err != nil {
		return fmt.Errorf("admin listen %s: %w", s.Opt.Listen, err)
	}
	if err := s.srv.Serve(ln); err != nil && err != http.ErrServerClosed {
		return err
	}
	return nil
}

// handleAPI 管理端路由总入口（/api/* 全部走这里，再按方法分发）。
func (s *Server) handleAPI(w http.ResponseWriter, r *http.Request) {
	// CORS：预检与响应头
	if origin := corsOrigin(r.Header.Get("Origin")); origin != "" {
		w.Header().Set("Access-Control-Allow-Origin", origin)
		w.Header().Set("Vary", "Origin")
		w.Header().Set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
		w.Header().Set("Access-Control-Allow-Headers", "Content-Type")
		w.Header().Set("Access-Control-Max-Age", "600")
	}
	if r.Method == http.MethodOptions {
		w.WriteHeader(http.StatusNoContent)
		return
	}

	path := strings.SplitN(r.URL.Path, "?", 2)[0]
	switch {
	case r.Method == http.MethodGet && path == "/api/status":
		s.proxyJSON(w, "GET", "/status", nil)
	case r.Method == http.MethodGet && path == "/api/models":
		s.proxyJSON(w, "GET", "/v1/models", nil)
	case r.Method == http.MethodGet && path == "/api/accounts":
		s.handleAccounts(w)
	case r.Method == http.MethodGet && path == "/api/config":
		s.handleConfigGet(w)
	case r.Method == http.MethodGet && path == "/api/logs":
		s.handleLogs(w, r)
	case r.Method == http.MethodGet && path == "/api/backups":
		s.handleBackupList(w)
	case r.Method == http.MethodGet && path == "/api/backup/download":
		s.handleBackupDownload(w, r)
	case r.Method == http.MethodPost && path == "/api/config":
		s.handleConfigSave(w, r)
	case r.Method == http.MethodPost && path == "/api/backup":
		s.handleBackupCreate(w)
	case r.Method == http.MethodPost && path == "/api/login/start":
		s.handleLoginStart(w)
	case r.Method == http.MethodPost && path == "/api/login/poll":
		s.handleLoginPoll(w)
	case r.Method == http.MethodPost && path == "/api/signin":
		s.handleSigninAll(w)
	case r.Method == http.MethodPost && path == "/api/signin/one":
		s.handleSigninOne(w, r)
	case r.Method == http.MethodPost && path == "/api/credits":
		s.handleCreditsAll(w)
	case r.Method == http.MethodPost && path == "/api/credits/one":
		s.handleCreditsOne(w, r)
	case r.Method == http.MethodPost && path == "/api/keepalive":
		s.handleKeepalive(w)
	case r.Method == http.MethodPost && path == "/api/account/delete":
		s.handleAccountDelete(w, r)
	case r.Method == http.MethodPost && path == "/api/account/max-in-flight":
		s.handleAccountMaxInFlight(w, r)
	case r.Method == http.MethodPost && path == "/api/chat":
		s.handleChat(w, r)
	case r.Method == http.MethodPost && path == "/api/container":
		// 网关已改为进程直跑（sidecar），容器操作退役；保留端点避免旧前端报 404。
		s.writeJSON(w, 200, map[string]any{"ok": false, "message": "网关已改为随 App 进程运行，无需容器操作"})
	default:
		s.writeJSON(w, 404, map[string]string{"error": "not found"})
	}
}

// writeJSON 统一 JSON 响应。
func (s *Server) writeJSON(w http.ResponseWriter, code int, obj any) {
	raw, err := json.Marshal(obj)
	if err != nil {
		code = 500
		raw, _ = json.Marshal(map[string]string{"error": err.Error()})
	}
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(code)
	_, _ = w.Write(raw)
}

// decodeBody 读取请求体 JSON 到 map（空体返回空 map）。
func (s *Server) decodeBody(r *http.Request) map[string]any {
	out := map[string]any{}
	defer func() { _ = r.Body.Close() }()
	if err := json.NewDecoder(r.Body).Decode(&out); err != nil {
		return map[string]any{}
	}
	return out
}

// proxyJSON 把请求转发到本进程 API 端口（/status、/v1/models 等，自动带鉴权）。
func (s *Server) proxyJSON(w http.ResponseWriter, method, path string, body []byte) {
	req, err := http.NewRequest(method, s.Opt.GatewayBase+path, nil)
	if err != nil {
		s.writeJSON(w, 500, map[string]string{"error": err.Error()})
		return
	}
	if s.Opt.APIKey != "" {
		req.Header.Set("Authorization", "Bearer "+s.Opt.APIKey)
	}
	req.Header.Set("Content-Type", "application/json")
	client := &http.Client{Timeout: 30 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		s.writeJSON(w, 200, map[string]any{"error": err.Error(), "total": 0, "healthy": 0})
		return
	}
	defer resp.Body.Close()
	raw := make([]byte, 0, 4096)
	buf := make([]byte, 4096)
	for {
		n, rerr := resp.Body.Read(buf)
		raw = append(raw, buf[:n]...)
		if rerr != nil {
			break
		}
	}
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(resp.StatusCode)
	_, _ = w.Write(raw)
}

// tailString 取字符串尾部最多 max 字节（多字节安全按字节截断，日志展示可接受）。
func tailString(s string, max int) string {
	if len(s) <= max {
		return s
	}
	return s[len(s)-max:]
}

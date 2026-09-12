// ops.go 运维类端点：配置读写 / 进程日志 / 备份 / 对话测试。
package admin

import (
	"archive/zip"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"

	"workbuddy2api/internal/logbuf"
)

// handleConfigGet GET /api/config：读配置 + 展示用接入地址。
func (s *Server) handleConfigGet(w http.ResponseWriter) {
	cfg := readJSONFile(s.Opt.ConfigPath)
	if cfg == nil {
		cfg = map[string]any{}
	}
	s.writeJSON(w, 200, map[string]any{"config": cfg, "baseUrl": s.Opt.DisplayBase})
}

// handleConfigSave POST /api/config {config}: 原子写配置后触发进程优雅退出，
// 由桌面 App 的 sidecar 守护重拉新进程，新配置即生效。
func (s *Server) handleConfigSave(w http.ResponseWriter, r *http.Request) {
	body := s.decodeBody(r)
	cfg, ok := body["config"].(map[string]any)
	if !ok {
		s.writeJSON(w, 200, map[string]any{"ok": false, "message": "配置格式错误"})
		return
	}
	if err := writeJSONAtomic(s.Opt.ConfigPath, cfg); err != nil {
		s.writeJSON(w, 200, map[string]any{"ok": false, "message": shortMsg("保存失败: "+err.Error(), 100)})
		return
	}
	// 延迟触发退出：先让本响应完整送达前端，再优雅停机（App 守护线程会在 1s 内重拉）
	if s.Opt.OnConfigSaved != nil {
		time.AfterFunc(500*time.Millisecond, s.Opt.OnConfigSaved)
	}
	s.writeJSON(w, 200, map[string]any{"ok": true, "message": "已保存，网关正在重启使配置生效"})
}

// handleLogs GET /api/logs?n=200：进程日志环形缓冲最近 N 行（替代原 docker logs）。
func (s *Server) handleLogs(w http.ResponseWriter, r *http.Request) {
	n := 200
	if v := r.URL.Query().Get("n"); v != "" {
		if parsed := parseInt(v); parsed > 0 {
			n = parsed
		}
	}
	lines := logbuf.Global().Snapshot(n)
	out := strings.Join(lines, "\n")
	s.writeJSON(w, 200, map[string]any{"ok": true, "logs": tailString(out, 8000)})
}

// handleBackupCreate POST /api/backup：打包 auths/ 与 data/state.json 为 zip。
func (s *Server) handleBackupCreate(w http.ResponseWriter) {
	if err := os.MkdirAll(s.Opt.BackupDir, 0o755); err != nil {
		s.writeJSON(w, 200, map[string]any{"ok": false, "message": shortMsg(err.Error(), 80)})
		return
	}
	name := "backup-" + time.Now().Format("20060102-150405") + ".zip"
	target := filepath.Join(s.Opt.BackupDir, name)
	count := 0
	zf, err := os.Create(target)
	if err != nil {
		s.writeJSON(w, 200, map[string]any{"ok": false, "message": shortMsg(err.Error(), 80)})
		return
	}
	defer zf.Close()
	zw := zip.NewWriter(zf)
	for _, fp := range s.authFiles() {
		if raw, err := os.ReadFile(fp); err == nil {
			if entry, err := zw.Create("auths/" + filepath.Base(fp)); err == nil {
				_, _ = entry.Write(raw)
				count++
			}
		}
	}
	if raw, err := os.ReadFile(filepath.Join(s.Opt.DataDir, "state.json")); err == nil {
		if entry, err := zw.Create("data/state.json"); err == nil {
			_, _ = entry.Write(raw)
		}
	}
	_ = zw.Close()
	info, err := os.Stat(target)
	if err != nil {
		s.writeJSON(w, 200, map[string]any{"ok": false, "message": shortMsg(err.Error(), 80)})
		return
	}
	s.writeJSON(w, 200, map[string]any{
		"ok": true, "file": name, "accounts": count,
		"size": fmt.Sprintf("%.1f KB", float64(info.Size())/1024),
	})
}

// handleBackupList GET /api/backups：历史备份列表（新→旧）。
func (s *Server) handleBackupList(w http.ResponseWriter) {
	out := []map[string]any{}
	matches, _ := filepath.Glob(filepath.Join(s.Opt.BackupDir, "backup-*.zip"))
	// 倒序：文件名含时间戳，按名倒序即新→旧
	for i := len(matches) - 1; i >= 0; i-- {
		fp := matches[i]
		info, err := os.Stat(fp)
		if err != nil {
			continue
		}
		out = append(out, map[string]any{
			"file": filepath.Base(fp),
			"size": fmt.Sprintf("%.1f KB", float64(info.Size())/1024),
			"time": info.ModTime().Format("2006-01-02 15:04"),
		})
	}
	s.writeJSON(w, 200, out)
}

// handleBackupDownload GET /api/backup/download?f=xxx.zip：下载备份文件。
func (s *Server) handleBackupDownload(w http.ResponseWriter, r *http.Request) {
	name := safeUID(strings.SplitN(r.URL.Query().Get("f"), ".", 2)[0])
	// 保留扩展名：safeUID 会剥掉点，手动拼回 .zip 白名单后缀
	fp := filepath.Join(s.Opt.BackupDir, name+".zip")
	raw, err := os.ReadFile(fp)
	if err != nil {
		s.writeJSON(w, 404, map[string]string{"error": "文件不存在"})
		return
	}
	w.Header().Set("Content-Type", "application/zip")
	w.WriteHeader(200)
	_, _ = w.Write(raw)
}

// handleChat POST /api/chat {model,message}：对话测试。
// 走本进程 /v1 完整链路（鉴权 + 池调度 + 会话粘性），与外部客户端行为一致。
func (s *Server) handleChat(w http.ResponseWriter, r *http.Request) {
	body := s.decodeBody(r)
	model := str(body["model"])
	if model == "" {
		model = "deepseek-v4-flash"
	}
	message := str(body["message"])
	payload, _ := json.Marshal(map[string]any{
		"model":  model,
		"stream": false,
		"messages": []map[string]string{
			{"role": "user", "content": message},
		},
	})

	started := time.Now()
	req, err := http.NewRequest(http.MethodPost, s.Opt.GatewayBase+"/v1/chat/completions", strings.NewReader(string(payload)))
	if err != nil {
		s.writeJSON(w, 200, map[string]any{"ok": false, "content": err.Error(), "elapsed": 0, "usage": map[string]any{}})
		return
	}
	req.Header.Set("Content-Type", "application/json")
	if s.Opt.APIKey != "" {
		req.Header.Set("Authorization", "Bearer "+s.Opt.APIKey)
	}
	client := &http.Client{Timeout: 120 * time.Second}
	resp, err := client.Do(req)
	elapsed := time.Since(started).Seconds()
	if err != nil {
		msg := err.Error()
		if strings.Contains(strings.ToLower(msg), "timed out") || strings.Contains(msg, "timeout") {
			msg = "等待超时无响应。若多个模型都这样，通常上游连接异常，重启 App 内网关进程即可恢复。"
		}
		s.writeJSON(w, 200, map[string]any{"ok": false, "content": msg,
			"elapsed": round2(elapsed), "usage": map[string]any{}})
		return
	}
	defer resp.Body.Close()
	var out map[string]any
	_ = json.NewDecoder(resp.Body).Decode(&out)
	if resp.StatusCode >= 400 {
		msg := ""
		if errObj, ok := out["error"].(map[string]any); ok {
			msg, _ = errObj["message"].(string)
		}
		if msg == "" {
			msg = fmt.Sprintf("HTTP %d", resp.StatusCode)
		}
		s.writeJSON(w, 200, map[string]any{"ok": false, "content": fmt.Sprintf("HTTP %d: %s", resp.StatusCode, msg),
			"elapsed": round2(elapsed), "usage": map[string]any{}})
		return
	}
	content := ""
	if choices, ok := out["choices"].([]any); ok && len(choices) > 0 {
		if choice, ok := choices[0].(map[string]any); ok {
			if msgObj, ok := choice["message"].(map[string]any); ok {
				content, _ = msgObj["content"].(string)
			}
		}
	}
	if content == "" {
		content = "(空回复)"
	}
	s.writeJSON(w, 200, map[string]any{"ok": true, "content": content,
		"elapsed": round2(elapsed), "usage": out["usage"]})
}

// parseInt 简单正整数解析。
func parseInt(v string) int {
	n := 0
	for _, ch := range v {
		if ch < '0' || ch > '9' {
			return 0
		}
		n = n*10 + int(ch-'0')
	}
	return n
}

// round2 保留两位小数。
func round2(v float64) float64 {
	return float64(int(v*100+0.5)) / 100
}

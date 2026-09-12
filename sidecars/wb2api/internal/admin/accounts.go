// accounts.go 账号类端点：列表 / 签到 / 积分 / 保活 / 删除。
// 状态字符串（签到成功/今日已签到/签到失败/凭证失效/读取失败）与原 Python 控制台保持一致。
package admin

import (
	"encoding/json"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

var reSafeUID = regexp.MustCompile(`[^0-9a-zA-Z\-]`)

// safeUID 清洗 uid，防路径穿越。
func safeUID(uid string) string {
	return reSafeUID.ReplaceAllString(uid, "")
}

// readJSONFile 读 JSON 文件到 map，失败返回 nil。
func readJSONFile(path string) map[string]any {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil
	}
	var out map[string]any
	if json.Unmarshal(raw, &out) != nil {
		return nil
	}
	return out
}

// writeJSONAtomic 原子写 JSON 文件（tmp + replace）。
func writeJSONAtomic(path string, data any) error {
	raw, err := json.MarshalIndent(data, "", "  ")
	if err != nil {
		return err
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, raw, 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

// cachePath 通用缓存路径（data/credits.json、data/checkin.json）。
func (s *Server) cachePath(name string) string {
	return filepath.Join(s.Opt.DataDir, name)
}

// loadMapCache 读 {uid: value} 形式的 JSON 缓存。
func (s *Server) loadMapCache(name string) map[string]any {
	out := map[string]any{}
	raw, err := os.ReadFile(s.cachePath(name))
	if err != nil || json.Unmarshal(raw, &out) != nil {
		return map[string]any{}
	}
	return out
}

// saveMapCache 合并写入 {uid: value} 缓存。
func (s *Server) saveMapCache(name string, patch map[string]any) {
	merged := s.loadMapCache(name)
	for k, v := range patch {
		if v != nil {
			merged[k] = v
		}
	}
	raw, err := json.Marshal(merged)
	if err != nil {
		return
	}
	_ = os.MkdirAll(s.Opt.DataDir, 0o755)
	_ = os.WriteFile(s.cachePath(name), raw, 0o600)
}

// authFiles 返回排序后的凭证文件路径。
func (s *Server) authFiles() []string {
	matches, _ := filepath.Glob(filepath.Join(s.Opt.AuthDir, "workbuddy-*.json"))
	sort.Strings(matches)
	return matches
}

// today 判断签到缓存用的日期字符串。
func today() string { return time.Now().Format("2006-01-02") }

// handleAccounts GET /api/accounts：全部账号（含缓存积分与今日签到态）。
func (s *Server) handleAccounts(w http.ResponseWriter) {
	credits := s.loadMapCache("credits.json")
	checkins := s.loadMapCache("checkin.json")
	result := []map[string]any{}
	for _, fp := range s.authFiles() {
		acct := readJSONFile(fp)
		if acct == nil {
			continue
		}
		account, _ := acct["account"].(map[string]any)
		uid, _ := account["uid"].(string)
		nickname, _ := account["nickname"].(string)
		checked := false
		if v, ok := checkins[uid].(string); ok && v == today() {
			checked = true
		}
		row := map[string]any{
			"uid":      uid,
			"nickname": nickname,
			"file":     filepath.Base(fp),
			"checked":  checked,
		}
		if remain, ok := credits[uid].(float64); ok {
			row["remain"] = int64(remain)
		} else {
			row["remain"] = nil
		}
		result = append(result, row)
	}
	s.writeJSON(w, 200, result)
}

// refreshIfNeeded token 即将过期（2h 内）时刷新并原子写回。
// 返回是否已刷新；失败时给出状态与明细。
func (s *Server) refreshIfNeeded(a *auth.Auth) (refreshed bool, status, detail string) {
	if !a.NeedsRefresh(2 * time.Hour) {
		return false, "", ""
	}
	if err := s.Opt.Up.RefreshToken(a); err != nil {
		if ue, ok := err.(*upstream.Error); ok && ue.Kind == upstream.ErrSessionDead {
			return false, "凭证失效", shortMsg(err.Error(), 60)
		}
		return false, "凭证失效", shortMsg("refresh: "+err.Error(), 60)
	}
	if err := a.SaveAtomic(); err != nil {
		return true, "凭证失效", shortMsg("save: "+err.Error(), 60)
	}
	return true, "", ""
}

// checkinOne 对单个账号执行签到并查余额（与 webadmin.py signin_one 同语义）。
func (s *Server) checkinOne(fp string) map[string]any {
	resp := map[string]any{"uid": "?", "nickname": "", "status": "读取失败",
		"detail": "凭证文件损坏", "remain": nil}
	raw, err := os.ReadFile(fp)
	if err != nil {
		return resp
	}
	a, err := auth.Parse(raw)
	if err != nil {
		return resp
	}
	a.FilePath = fp
	resp["uid"], resp["nickname"] = a.UID, a.Nickname

	if _, status, detail := s.refreshIfNeeded(a); status != "" {
		resp["status"], resp["detail"] = status, detail
		return resp
	}

	status, detail := "签到失败", ""
	err = s.Opt.Up.DailyCheckin(a)
	switch {
	case err == nil:
		status = "签到成功"
	default:
		msg := err.Error()
		lowered := strings.ToLower(msg)
		if strings.Contains(lowered, "已签到") || strings.Contains(lowered, "already") ||
			strings.Contains(lowered, "checkin") || strings.Contains(lowered, "code=400") {
			status, detail = "今日已签到", shortMsg(msg, 40)
		} else {
			status, detail = "签到失败", shortMsg(msg, 40)
		}
	}
	resp["status"], resp["detail"] = status, detail

	if remain, err := s.Opt.Up.UserResource(a); err == nil {
		resp["remain"] = remain
	}

	if status == "签到成功" || status == "今日已签到" {
		s.saveMapCache("checkin.json", map[string]any{a.UID: today()})
		resp["checked"] = true
	} else {
		resp["checked"] = false
	}
	return resp
}

// handleSigninAll POST /api/signin：一键签到全部账号。
func (s *Server) handleSigninAll(w http.ResponseWriter) {
	results := []map[string]any{}
	creditPatch := map[string]any{}
	for _, fp := range s.authFiles() {
		r := s.checkinOne(fp)
		results = append(results, r)
		if uid, ok := r["uid"].(string); ok {
			creditPatch[uid] = r["remain"]
		}
	}
	s.saveMapCache("credits.json", creditPatch)
	s.writeJSON(w, 200, map[string]any{"results": results})
}

// handleSigninOne POST /api/signin/one {uid}：单账号签到。
func (s *Server) handleSigninOne(w http.ResponseWriter, r *http.Request) {
	body := s.decodeBody(r)
	uid := safeUID(str(body["uid"]))
	fp := filepath.Join(s.Opt.AuthDir, "workbuddy-"+uid+".json")
	if _, err := os.Stat(fp); err != nil {
		s.writeJSON(w, 200, map[string]any{"ok": false, "message": "凭证文件不存在"})
		return
	}
	result := s.checkinOne(fp)
	if v, ok := result["remain"].(int64); ok {
		s.saveMapCache("credits.json", map[string]any{result["uid"].(string): v})
	}
	status, _ := result["status"].(string)
	result["ok"] = status == "签到成功" || status == "今日已签到"
	s.writeJSON(w, 200, result)
}

// queryRemain 查询单个账号剩余积分（上游聚合逻辑在 upstream.UserResource 内）。
func (s *Server) queryRemain(fp string) (string, string, int64, error) {
	raw, err := os.ReadFile(fp)
	if err != nil {
		return "", "", 0, err
	}
	a, err := auth.Parse(raw)
	if err != nil {
		return "", "", 0, err
	}
	remain, err := s.Opt.Up.UserResource(a)
	return a.UID, a.Nickname, remain, err
}

// handleCreditsAll POST /api/credits：全账号积分查询。
func (s *Server) handleCreditsAll(w http.ResponseWriter) {
	rows := []map[string]any{}
	creditPatch := map[string]any{}
	for _, fp := range s.authFiles() {
		uid, nickname, remain, err := s.queryRemain(fp)
		if uid == "" {
			continue
		}
		if err != nil {
			rows = append(rows, map[string]any{"uid": uid, "nickname": nickname, "remain": nil, "error": shortMsg(err.Error(), 60)})
			continue
		}
		creditPatch[uid] = remain
		rows = append(rows, map[string]any{"uid": uid, "nickname": nickname, "remain": remain, "error": ""})
	}
	s.saveMapCache("credits.json", creditPatch)
	s.writeJSON(w, 200, map[string]any{"rows": rows})
}

// handleCreditsOne POST /api/credits/one {uid}：单账号积分查询。
func (s *Server) handleCreditsOne(w http.ResponseWriter, r *http.Request) {
	body := s.decodeBody(r)
	uid := safeUID(str(body["uid"]))
	fp := filepath.Join(s.Opt.AuthDir, "workbuddy-"+uid+".json")
	if _, err := os.Stat(fp); err != nil {
		s.writeJSON(w, 200, map[string]any{"ok": false, "message": "凭证文件不存在"})
		return
	}
	gotUID, _, remain, err := s.queryRemain(fp)
	if err != nil {
		s.writeJSON(w, 200, map[string]any{"ok": false, "message": shortMsg(err.Error(), 60)})
		return
	}
	s.saveMapCache("credits.json", map[string]any{gotUID: remain})
	s.writeJSON(w, 200, map[string]any{"ok": true, "uid": gotUID, "remain": remain})
}

// handleKeepalive POST /api/keepalive：全账号强制刷新凭证（对应每日 22 点定时任务）。
func (s *Server) handleKeepalive(w http.ResponseWriter) {
	rows := []map[string]any{}
	for _, fp := range s.authFiles() {
		raw, err := os.ReadFile(fp)
		if err != nil {
			continue
		}
		a, err := auth.Parse(raw)
		if err != nil {
			continue
		}
		a.FilePath = fp
		if err := s.Opt.Up.RefreshToken(a); err != nil {
			rows = append(rows, map[string]any{"uid": a.UID, "nickname": a.Nickname, "ok": false, "detail": shortMsg(err.Error(), 80)})
			continue
		}
		if err := a.SaveAtomic(); err != nil {
			rows = append(rows, map[string]any{"uid": a.UID, "nickname": a.Nickname, "ok": false, "detail": shortMsg("save: "+err.Error(), 80)})
			continue
		}
		rows = append(rows, map[string]any{"uid": a.UID, "nickname": a.Nickname, "ok": true, "detail": "凭证已刷新"})
	}
	s.writeJSON(w, 200, map[string]any{"rows": rows})
}

// handleAccountDelete POST /api/account/delete {uid}：删凭证文件并热同步池。
func (s *Server) handleAccountDelete(w http.ResponseWriter, r *http.Request) {
	body := s.decodeBody(r)
	uid := safeUID(str(body["uid"]))
	fp := filepath.Join(s.Opt.AuthDir, "workbuddy-"+uid+".json")
	if _, err := os.Stat(fp); err != nil {
		s.writeJSON(w, 200, map[string]any{"ok": false, "message": "文件不存在"})
		return
	}
	if err := os.Remove(fp); err != nil {
		s.writeJSON(w, 200, map[string]any{"ok": false, "message": shortMsg(err.Error(), 80)})
		return
	}
	if s.Opt.OnAuthsChanged != nil {
		s.Opt.OnAuthsChanged()
	}
	s.writeJSON(w, 200, map[string]any{"ok": true, "message": "已删除，账号池已同步"})
}

// shortMsg 截断错误消息便于前端展示。
func shortMsg(msg string, n int) string {
	msg = strings.ReplaceAll(msg, "\n", " ")
	if len(msg) > n {
		return msg[:n]
	}
	return msg
}

// str 取 map 中字符串字段（缺省空串）。
func str(v any) string {
	s, _ := v.(string)
	return s
}

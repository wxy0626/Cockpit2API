// store.go —— 被用户屏蔽的账号 id 集合。
//
// 为什么是「屏蔽」而不是「删除」：
//   本机账号的凭据属于 Cindy 客户端（`owner_*_api_key.enc`），
//   从网关侧删文件会破坏用户的登录态，而且客户端下次登录还会写回来。
//   所以只做「网关不再使用该账号」—— 可逆、不碰别人的数据。
//
// 对 OAuth 添加的账号，删除走 manualaccounts（那是我们自己的数据，可以真删）；
// 本机账号则走这里。两者在界面上都表现为「移除」。
//
// 落盘位置：<runtime>/hidden_accounts.json
package hiddenstore

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"sync"
)

// Store 屏蔽名单。
type Store struct {
	mu   sync.Mutex
	path string
	ids  map[string]bool
}

// NewStore 创建屏蔽名单（不立即读盘）。
func NewStore(path string) *Store {
	return &Store{path: path, ids: map[string]bool{}}
}

// Load 从磁盘读取；文件不存在视为空名单。
func (s *Store) Load() error {
	s.mu.Lock()
	defer s.mu.Unlock()

	raw, err := os.ReadFile(s.path)
	if os.IsNotExist(err) {
		s.ids = map[string]bool{}
		return nil
	}
	if err != nil {
		return fmt.Errorf("读取屏蔽名单失败: %w", err)
	}
	var list []string
	if err := json.Unmarshal(raw, &list); err != nil {
		return fmt.Errorf("屏蔽名单解析失败（%s）: %w", s.path, err)
	}
	next := make(map[string]bool, len(list))
	for _, id := range list {
		if id != "" {
			next[id] = true
		}
	}
	s.ids = next
	return nil
}

// List 返回全部被屏蔽的账号 id。
func (s *Store) List() []string {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]string, 0, len(s.ids))
	for id := range s.ids {
		out = append(out, id)
	}
	sort.Strings(out)
	return out
}

// Contains 判断某账号是否被屏蔽。
func (s *Store) Contains(id string) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.ids[id]
}

// Add 屏蔽一个账号（幂等）。
func (s *Store) Add(id string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.ids[id] = true
	return s.saveLocked()
}

// Remove 解除屏蔽（幂等）。
func (s *Store) Remove(id string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	delete(s.ids, id)
	return s.saveLocked()
}

// saveLocked 落盘（调用方须持锁）。
func (s *Store) saveLocked() error {
	if err := os.MkdirAll(filepath.Dir(s.path), 0o755); err != nil {
		return fmt.Errorf("创建屏蔽名单目录失败: %w", err)
	}
	list := make([]string, 0, len(s.ids))
	for id := range s.ids {
		list = append(list, id)
	}
	sort.Strings(list)
	raw, err := json.MarshalIndent(list, "", "  ")
	if err != nil {
		return err
	}
	if err := os.WriteFile(s.path, append(raw, '\n'), 0o600); err != nil {
		return fmt.Errorf("写入屏蔽名单失败: %w", err)
	}
	return nil
}

// store.go —— 手动添加的 Cindy 账号（OAuth 授权得到）的持久化。
//
// 为什么要独立一份：自动发现的账号来自本机 Cindy 桌面端数据目录（owner_*_api_key.enc），
// 生命周期由 Cindy 客户端管；而通过 OAuth 添加的账号是本工具自己拿到的，
// 必须自己保存，且刷新令牌只能由我们自己维护。
//
// 落盘位置：<runtime>/accounts.json，权限 0600（内含 apiKey 与 refreshToken）。
package manualaccounts

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"sync"
	"time"
)

// Account 一个手动添加的账号。
type Account struct {
	// ID 稳定标识：优先用 auth-server 返回的 membershipId，缺失时退化为随机串
	ID string `json:"id"`
	// Label 账号卡片上展示的名字（displayName，缺失时用 email）
	Label string `json:"label"`
	// Endpoint 该账号专属的推理入口（与 APIKey 同租户，成对取用）
	Endpoint string `json:"endpoint"`
	// APIKey 上游网关 key
	APIKey string `json:"apiKey"`
	// RefreshToken 用于在凭据轮换后重新拉取 endpoint + apiKey
	RefreshToken string `json:"refreshToken"`
	// Region 该账号所属区域（global / cn），决定刷新时打哪个端点
	Region string `json:"region"`
	// DeviceID 签发 refreshToken 时使用的设备标识。
	//
	// 为什么必须单独存：OAuth 社交/SSO 授权为了让「换号登录」可用，每次授权都用
	// 会话专属 deviceId（基础值 + 随机后缀）兑换令牌；上游校验 refreshToken 与
	// 签发设备一致，刷新时必须回传**同一个**值，否则 401 DEVICE_MISMATCH。
	// 手机验证码路径签发时用的就是基础 deviceId，此时留空、刷新时回退基础值。
	DeviceID string `json:"deviceID,omitempty"`
	// AddedAt 添加时间（Unix 毫秒）
	AddedAt int64 `json:"addedAt"`
}

// Store 手动账号表。
type Store struct {
	mu       sync.Mutex
	path     string
	accounts []Account
}

// NewStore 创建账号表（不立即读盘，由 Load 显式触发）。
func NewStore(path string) *Store {
	return &Store{path: path}
}

// Load 从磁盘读取；文件不存在视为空表。
func (s *Store) Load() error {
	s.mu.Lock()
	defer s.mu.Unlock()

	raw, err := os.ReadFile(s.path)
	if os.IsNotExist(err) {
		s.accounts = nil
		return nil
	}
	if err != nil {
		return fmt.Errorf("读取手动账号表失败: %w", err)
	}
	var list []Account
	if err := json.Unmarshal(raw, &list); err != nil {
		return fmt.Errorf("手动账号表解析失败（%s）: %w", s.path, err)
	}
	s.accounts = list
	return nil
}

// List 返回当前全部手动账号（副本，调用方可安全使用）。
func (s *Store) List() []Account {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]Account, len(s.accounts))
	copy(out, s.accounts)
	return out
}

// Add 新增或按 ID 覆盖一个账号，并落盘。
//
// 覆盖语义：同一个 membershipId 重复授权时视为更新凭据（轮换后重新授权很常见），
// 而不是插入重复卡片。
func (s *Store) Add(account Account) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	if account.AddedAt == 0 {
		account.AddedAt = time.Now().UnixMilli()
	}
	replaced := false
	for i, item := range s.accounts {
		if item.ID == account.ID {
			s.accounts[i] = account
			replaced = true
			break
		}
	}
	if !replaced {
		s.accounts = append(s.accounts, account)
	}
	// 稳定的展示顺序：先按添加时间，再按 ID
	sort.SliceStable(s.accounts, func(i, j int) bool {
		if s.accounts[i].AddedAt != s.accounts[j].AddedAt {
			return s.accounts[i].AddedAt < s.accounts[j].AddedAt
		}
		return s.accounts[i].ID < s.accounts[j].ID
	})
	return s.saveLocked()
}

// Remove 按 ID 删除账号并落盘；不存在视为成功（幂等）。
func (s *Store) Remove(id string) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	next := s.accounts[:0]
	for _, item := range s.accounts {
		if item.ID != id {
			next = append(next, item)
		}
	}
	s.accounts = next
	return s.saveLocked()
}

// saveLocked 落盘（调用方必须已持锁）。
func (s *Store) saveLocked() error {
	if err := os.MkdirAll(filepath.Dir(s.path), 0o755); err != nil {
		return fmt.Errorf("创建账号表目录失败: %w", err)
	}
	raw, err := json.MarshalIndent(s.accounts, "", "  ")
	if err != nil {
		return err
	}
	// 0600：文件内含 apiKey 与 refreshToken
	if err := os.WriteFile(s.path, append(raw, '\n'), 0o600); err != nil {
		return fmt.Errorf("写入手动账号表失败: %w", err)
	}
	return nil
}

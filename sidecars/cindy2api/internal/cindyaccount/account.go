// account.go —— Cindy 账号发现：从桌面端数据目录解出网关凭据。
//
// 凭据链路（Windows 上 Electron safeStorage 的 Chromium OSCrypt 格式）：
//
//	{userData}/Local State
//	    os_crypt.encrypted_key = base64("DPAPI" + CryptProtectData(aesKey))
//	      └─ 去 5 字节 "DPAPI" 前缀 → DPAPI 解出 32 字节主密钥
//
//	{userData}/safe-storage/owner_<ownerId>_api_key.enc
//	    = base64("v10" + nonce(12B) + AES-256-GCM 密文 + tag(16B))
//	      └─ 用主密钥解开
//
//	{userData}/owners/<ownerId>/model-access-credentials.json   ← 明文，含 endpoint
//
// 结论：只要 Cindy 登录过，网关就能自动发现账号，不需要 Cindy 在运行，
// 也不需要重新登录。key 与 endpoint 必须成对使用（跨租户调用会 401）。
package cindyaccount

import (
	"bytes"
	"crypto/aes"
	"crypto/cipher"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

const (
	localStateFile   = "Local State"
	safeStorageDir   = "safe-storage"
	endpointMetadata = "model-access-credentials.json"
	ownersDir        = "owners"

	v10Prefix   = "v10"
	dpapiPrefix = "DPAPI"
)

// Account 一个可被网关消费的 Cindy 账号（凭据已解密，仅在内存中流转）。
type Account struct {
	// OwnerID 账号唯一标识。实测同一 owner 的 key 跨 profile 完全一致，
	// 因此用它做账号去重主键。
	OwnerID string `json:"ownerId"`
	// Endpoint 该账号专属的推理入口（服务端随凭据成对下发，不同账号可能不同）
	Endpoint string `json:"endpoint"`
	// APIKey 上游网关 key —— 不外泄给调用方，故不参与序列化
	APIKey string `json:"-"`
	// Profiles 该账号出现在哪些 userData 目录（诊断用）
	Profiles []string `json:"profiles"`
	// Subscriptions 订阅账号凭证（claude-code / codex / pi），保留供后续扩展
	Subscriptions map[string]string `json:"-"`
	// Source 来源：local = 本机 Cindy 登录态自动发现；oauth = 本工具授权添加
	Source string `json:"source,omitempty"`
}

// loadMasterKey 从 Local State 读取并解开 OSCrypt 主密钥。
func loadMasterKey(userDataDir string) ([]byte, error) {
	raw, err := os.ReadFile(filepath.Join(userDataDir, localStateFile))
	if err != nil {
		return nil, fmt.Errorf("读取 Local State 失败: %w", err)
	}
	var state struct {
		OSCrypt struct {
			EncryptedKey string `json:"encrypted_key"`
		} `json:"os_crypt"`
	}
	if err := json.Unmarshal(raw, &state); err != nil {
		return nil, fmt.Errorf("解析 Local State 失败: %w", err)
	}
	if state.OSCrypt.EncryptedKey == "" {
		return nil, fmt.Errorf("Local State 缺少 os_crypt.encrypted_key")
	}
	blob, err := base64.StdEncoding.DecodeString(state.OSCrypt.EncryptedKey)
	if err != nil {
		return nil, fmt.Errorf("encrypted_key 不是合法 base64: %w", err)
	}
	if !bytes.HasPrefix(blob, []byte(dpapiPrefix)) {
		return nil, fmt.Errorf("encrypted_key 前缀不是 DPAPI（该应用可能启用了 App-Bound 加密，外部进程无法解密）")
	}
	return dpapiUnprotect(blob[len(dpapiPrefix):])
}

// decryptV10 解密一个 v10 块（AES-256-GCM）。
//
// 布局：v10(3) + nonce(12) + 密文 + tag(16)；Go 的 cipher.AEAD 期望 tag 紧跟在密文尾部，
// 正好与这个布局一致，可以直接把尾部整体交给 Open。
func decryptV10(masterKey, raw []byte) (string, error) {
	if !bytes.HasPrefix(raw, []byte(v10Prefix)) {
		return "", fmt.Errorf("密文前缀不是 v10")
	}
	body := raw[len(v10Prefix):]
	const nonceSize, tagSize = 12, 16
	if len(body) < nonceSize+tagSize {
		return "", fmt.Errorf("密文长度不足（%d 字节），不是合法的 v10 块", len(body))
	}
	block, err := aes.NewCipher(masterKey)
	if err != nil {
		return "", fmt.Errorf("构造 AES 失败: %w", err)
	}
	gcm, err := cipher.NewGCM(block)
	if err != nil {
		return "", fmt.Errorf("构造 GCM 失败: %w", err)
	}
	plain, err := gcm.Open(nil, body[:nonceSize], body[nonceSize:], nil)
	if err != nil {
		return "", fmt.Errorf("GCM 解密失败（主密钥不匹配或数据损坏）: %w", err)
	}
	return string(plain), nil
}

// readSecret 读取并解密 safe-storage 下的一个凭据文件；文件不存在返回空串。
func readSecret(userDataDir, name string, masterKey []byte) (string, error) {
	path := filepath.Join(userDataDir, safeStorageDir, name+".enc")
	raw, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		return "", nil
	}
	if err != nil {
		return "", err
	}
	blob, err := base64.StdEncoding.DecodeString(strings.TrimSpace(string(raw)))
	if err != nil {
		return "", fmt.Errorf("凭据 %s 不是合法 base64: %w", name, err)
	}
	return decryptV10(masterKey, blob)
}

// readEndpoint 读取某账号的推理入口（明文元数据，服务端下发）。
func readEndpoint(userDataDir, ownerID string) string {
	path := filepath.Join(userDataDir, ownersDir, ownerID, endpointMetadata)
	raw, err := os.ReadFile(path)
	if err != nil {
		return ""
	}
	var meta struct {
		Endpoint string `json:"endpoint"`
	}
	if err := json.Unmarshal(raw, &meta); err != nil {
		return ""
	}
	return strings.TrimRight(meta.Endpoint, "/")
}

// DiscoverProfiles 列出本机所有含 safe-storage 的 Cindy userData 目录。
//
// 覆盖正式版、隔离 dev profile 与历史 profile（同一台机器上可能并存多份）。
func DiscoverProfiles() []string {
	roaming := os.Getenv("APPDATA")
	if roaming == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return nil
		}
		roaming = filepath.Join(home, "AppData", "Roaming")
	}
	entries, err := os.ReadDir(roaming)
	if err != nil {
		return nil
	}
	var dirs []string
	for _, entry := range entries {
		if !entry.IsDir() || !strings.HasPrefix(entry.Name(), "Cindy") {
			continue
		}
		dir := filepath.Join(roaming, entry.Name())
		if info, err := os.Stat(filepath.Join(dir, safeStorageDir)); err == nil && info.IsDir() {
			dirs = append(dirs, dir)
		}
	}
	sort.Strings(dirs)
	return dirs
}

// loadProfileAccounts 读出一个 userData 目录下的全部账号（一个 owner 一条）。
func loadProfileAccounts(userDataDir string) ([]Account, error) {
	masterKey, err := loadMasterKey(userDataDir)
	if err != nil {
		return nil, err
	}
	entries, err := os.ReadDir(filepath.Join(userDataDir, safeStorageDir))
	if err != nil {
		return nil, err
	}

	var accounts []Account
	profile := filepath.Base(userDataDir)
	for _, entry := range entries {
		name := entry.Name()
		// 匹配 owner_<ownerId>_api_key.enc
		if !strings.HasPrefix(name, "owner_") || !strings.HasSuffix(name, "_api_key.enc") {
			continue
		}
		ownerID := strings.TrimSuffix(strings.TrimPrefix(name, "owner_"), "_api_key.enc")

		apiKey, err := readSecret(userDataDir, "owner_"+ownerID+"_api_key", masterKey)
		if err != nil || apiKey == "" {
			continue
		}
		subscriptions := map[string]string{}
		for _, agent := range []string{"claude-code", "codex", "pi"} {
			if value, err := readSecret(userDataDir, "owner_"+ownerID+"_provider_key_sub_"+agent, masterKey); err == nil && value != "" {
				subscriptions[agent] = value
			}
		}
		accounts = append(accounts, Account{
			OwnerID:       ownerID,
			Endpoint:      readEndpoint(userDataDir, ownerID),
			APIKey:        apiKey,
			Profiles:      []string{profile},
			Subscriptions: subscriptions,
		})
	}
	return accounts, nil
}

// LoadAll 扫描全部 profile，返回按 ownerID 去重后的账号列表。
//
// 去重主键是 OwnerID：实测同一 owner 的 key 跨 profile 完全一致，属账号级而非 profile 级。
// 缺少 endpoint 的账号会被跳过 —— 没有配套入口的 key 无法使用（同租户不变量）。
func LoadAll() ([]Account, []string) {
	var (
		merged  = map[string]*Account{}
		order   []string
		skipped []string
	)
	for _, dir := range DiscoverProfiles() {
		accounts, err := loadProfileAccounts(dir)
		if err != nil {
			skipped = append(skipped, fmt.Sprintf("%s: %v", filepath.Base(dir), err))
			continue
		}
		for _, account := range accounts {
			if account.Endpoint == "" {
				skipped = append(skipped, fmt.Sprintf("%s/%s: 缺少 endpoint 元数据",
					filepath.Base(dir), ShortID(account.OwnerID)))
				continue
			}
			if existing, ok := merged[account.OwnerID]; ok {
				existing.Profiles = append(existing.Profiles, account.Profiles...)
				continue
			}
			copied := account
			merged[account.OwnerID] = &copied
			order = append(order, account.OwnerID)
		}
	}

	result := make([]Account, 0, len(order))
	for _, ownerID := range order {
		result = append(result, *merged[ownerID])
	}
	return result, skipped
}

// MaskKey 脱敏展示：只留前 6 位与总长度。
func MaskKey(key string) string {
	if key == "" {
		return "-"
	}
	if len(key) <= 6 {
		return key
	}
	return fmt.Sprintf("%s…(%d)", key[:6], len(key))
}

// ShortID 安全截取 ownerID 前 8 位，用于日志（直接切片会在短 ID 上 panic）。
func ShortID(ownerID string) string {
	if len(ownerID) <= 8 {
		return ownerID
	}
	return ownerID[:8]
}

// delete.go —— 删除本机 Cindy 账号的专属凭据文件。
//
// 语义：删掉凭据 = 该账号在 Cindy 客户端也退出登录，下次需要重新登录。
// 这是**用户明确要求**的行为（平台页会接管多账号切换，不需要保留客户端登录态）。
//
// ⚠️ 仍然必须遵守的两条红线（违反后果不可逆，且与"是否保留凭据"无关）：
//  1. **只删文件名匹配 `owner_<ownerId>_` 前缀的 .enc 文件**。
//     `Local State` 是 OSCrypt 主密钥、被同一 profile 下**所有**账号共用，
//     删它会让整个 profile 的所有账号一起失效 —— 这不是"删一个账号"，绝不能碰。
//  2. **不删除任何目录、不递归**。只对具体文件做删除。
//  3. ownerId 必须来自我们自己发现的账号，不接受任意字符串（防路径穿越）。
package cindyaccount

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// 删除目标文件名前缀：owner_<ownerId>_
const ownerFilePrefix = "owner_"

// DeleteLocalAccount 删除某本机账号的专属凭据文件，返回被删除的文件路径。
func DeleteLocalAccount(accountID string) ([]string, error) {
	ownerID := strings.TrimSpace(accountID)
	if ownerID == "" {
		return nil, fmt.Errorf("账号标识为空")
	}
	// 防路径穿越：ownerId 只允许字母数字（实测是 16~20 位 hex）
	for _, r := range ownerID {
		isDigit := r >= '0' && r <= '9'
		isAlpha := (r >= 'a' && r <= 'z') || (r >= 'A' && r <= 'Z')
		if !isDigit && !isAlpha {
			return nil, fmt.Errorf("账号标识包含非法字符，已拒绝操作")
		}
	}

	profiles := DiscoverProfiles()
	if len(profiles) == 0 {
		return nil, fmt.Errorf("未找到本机 Cindy 数据目录")
	}

	prefix := ownerFilePrefix + ownerID + "_"
	var deleted []string

	for _, profileDir := range profiles {
		storageDir := filepath.Join(profileDir, safeStorageDir)
		entries, err := os.ReadDir(storageDir)
		if err != nil {
			continue // 该 profile 读不到就跳过，不影响其它 profile
		}
		for _, entry := range entries {
			name := entry.Name()
			if entry.IsDir() {
				continue // 红线 2：不碰目录
			}
			// 红线 1：只处理本账号的专属文件（_api_key / _provider_key_sub_* 等）
			if !strings.HasPrefix(name, prefix) || !strings.HasSuffix(name, ".enc") {
				continue
			}
			source := filepath.Join(storageDir, name)
			if err := os.Remove(source); err != nil {
				return deleted, fmt.Errorf("删除 %s 失败: %w", source, err)
			}
			deleted = append(deleted, source)
		}
	}
	return deleted, nil
}

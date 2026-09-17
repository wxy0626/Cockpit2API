// config.go —— sidecar 自身配置（与 Cindy 凭据无关）。
//
// 只存"网关怎么跑"的参数；上游 endpoint/key 一律实时从 Cindy 数据目录发现，不落配置。
package config

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

// Config 网关运行参数。
type Config struct {
	// Listen 监听地址，例如 ":7865"。默认绑 127.0.0.1，避免把额度端点暴露到网络。
	Listen string `json:"listen"`
	// APIKey 网关对外签发的本地 key（与上游 key 完全隔离）
	APIKey string `json:"api_key"`
	// DeviceID 授权流程使用的设备标识（authorize 与令牌兑换都要带），首次运行自动生成
	DeviceID string `json:"device_id"`
	// MaxAccountAttempts 单个请求最多尝试几个账号
	MaxAccountAttempts int `json:"max_account_attempts"`
	// CheckIntervalSeconds 账号巡检间隔（秒）
	CheckIntervalSeconds int `json:"check_interval_seconds"`
	// RefreshIntervalSeconds 重新读取本机凭据的间隔（秒），用于跟随 key 轮换
	RefreshIntervalSeconds int `json:"refresh_interval_seconds"`
	// UpstreamTimeoutSeconds 单次上游请求的整体超时（秒）；0 表示不限制（流式场景）
	UpstreamTimeoutSeconds int `json:"upstream_timeout_seconds"`
	// Proxy 强制上行请求走的代理（如 http://127.0.0.1:7890）。
	//
	// 为什么需要：本机代理软件（Clash 等）按自己的规则分流，可能把某个上游域名
	// 判成直连，导致 TLS 握手被断开（表现为 EOF / schannel handshake failed）。
	// 填上这个地址即强制该域名走代理，不再依赖代理软件的判断。留空则跟随系统代理设置。
	Proxy string `json:"proxy"`
}

// Default 返回默认配置。
func Default() Config {
	return Config{
		Listen:                 "127.0.0.1:7865",
		MaxAccountAttempts:     3,
		CheckIntervalSeconds:   300,
		RefreshIntervalSeconds: 60,
		UpstreamTimeoutSeconds: 0,
	}
}

// Load 从指定路径读取配置；文件不存在则生成默认配置并落盘。
//
// 首次启动自动生成随机的本地 API Key —— 与 wb2api 的 api_key 语义一致，
// 便于前端面板用同一套交互展示。
func Load(path string) (Config, error) {
	cfg := Default()

	raw, err := os.ReadFile(path)
	switch {
	case err == nil:
		if err := json.Unmarshal(raw, &cfg); err != nil {
			return cfg, fmt.Errorf("配置文件 %s 解析失败: %w", path, err)
		}
	case os.IsNotExist(err):
		// 首次运行：生成 key 并写回
	default:
		return cfg, fmt.Errorf("读取配置失败: %w", err)
	}

	if cfg.APIKey == "" {
		buf := make([]byte, 24)
		if _, err := rand.Read(buf); err != nil {
			return cfg, fmt.Errorf("生成本地 API Key 失败: %w", err)
		}
		cfg.APIKey = "sk-cindy-" + hex.EncodeToString(buf)
	}
	if cfg.DeviceID == "" {
		// 设备标识必须稳定：授权会话与令牌兑换都带它，重启后变化会导致服务端校验不一致
		buf := make([]byte, 16)
		if _, err := rand.Read(buf); err != nil {
			return cfg, fmt.Errorf("生成设备标识失败: %w", err)
		}
		cfg.DeviceID = hex.EncodeToString(buf)
	}
	if cfg.MaxAccountAttempts <= 0 {
		cfg.MaxAccountAttempts = Default().MaxAccountAttempts
	}

	if err := Save(path, cfg); err != nil {
		return cfg, err
	}
	return cfg, nil
}

// Save 落盘配置（自动建目录）。
func Save(path string, cfg Config) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return fmt.Errorf("创建配置目录失败: %w", err)
	}
	raw, err := json.MarshalIndent(cfg, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, append(raw, '\n'), 0o600)
}

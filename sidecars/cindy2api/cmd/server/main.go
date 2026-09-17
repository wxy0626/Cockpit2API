// main.go —— Cindy 反代网关 sidecar 入口。
//
// 启动顺序：读配置 → 发现本机 Cindy 账号 → 起 HTTP 服务 → 后台首次巡检 → 挂定时器。
// 定时器做两件事：跟随 Cindy 的 key 轮换重新发现账号、周期性巡检可用性。
package main

import (
	"context"
	"flag"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"cindy2api/internal/config"
	"cindy2api/internal/gateway"
	"cindy2api/internal/hiddenstore"
	"cindy2api/internal/manualaccounts"
	"cindy2api/internal/parent"
	"cindy2api/internal/pool"
)

// 日志格式：与 wb2api 保持风格一致，便于在同一日志目录里检索
var logger = log.New(os.Stdout, "", log.LstdFlags)

func main() {
	configPath := flag.String("config", "", "配置文件路径（默认：可执行文件同级的 runtime/config.json）")
	parentPid := flag.Int("parent-pid", 0, "父进程 PID；父进程退出后本进程自动退出（由 CockpitTools 托管时传入）")
	flag.Parse()

	// 托管模式下跟随父进程生命周期：主程序退出即收摊，避免残留占用网关端口
	parent.Watch(*parentPid)

	if err := run(*configPath); err != nil {
		logger.Printf("启动失败: %v", err)
		os.Exit(1)
	}
}

// run 承载全部启动逻辑，便于 defer 生效与测试。
func run(configPath string) error {
	resolved, err := resolveConfigPath(configPath)
	if err != nil {
		return err
	}
	// 自写日志：不管由谁拉起（应用托管 / 手动 / WMI），stdout 都可能丢失，
	// 这里补一份落盘日志保证可诊断。stdout 保留，应用托管时仍进应用日志。
	if logFile, err := os.OpenFile(
		filepath.Join(filepath.Dir(resolved), "cindy2api.log"),
		os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644,
	); err == nil {
		logger.SetOutput(io.MultiWriter(os.Stdout, logFile))
	}
	cfg, err := config.Load(resolved)
	if err != nil {
		return err
	}
	logger.Printf("配置: %s", resolved)

	// 显式代理优先：上行 HTTP 客户端读的是环境变量，这里在创建客户端之前设好。
	// 用于绕开本机代理软件把某个上游域名误判为直连导致的 TLS 握手失败。
	if cfg.Proxy != "" {
		if err := os.Setenv("HTTPS_PROXY", cfg.Proxy); err == nil {
			_ = os.Setenv("HTTP_PROXY", cfg.Proxy)
			logger.Printf("已按配置强制上行走代理：%s", cfg.Proxy)
		} else {
			logger.Printf("警告：设置代理失败：%v", err)
		}
	}

	manualStore := manualaccounts.NewStore(filepath.Join(filepath.Dir(resolved), "accounts.json"))
	if err := manualStore.Load(); err != nil {
		logger.Printf("警告：%v", err)
	}
	hiddenStore := hiddenstore.NewStore(filepath.Join(filepath.Dir(resolved), "hidden_accounts.json"))
	if err := hiddenStore.Load(); err != nil {
		logger.Printf("警告：%v", err)
	}

	accountPool := pool.New(nil, manualStore, hiddenStore)
	added, removed, total := accountPool.Refresh()
	logger.Printf("账号发现：共 %d 个（新增 %d，失效 %d）", total, added, removed)
	if total == 0 {
		logger.Printf("提示：本机未发现 Cindy 登录态，可在管理页用「添加账号」授权登录")
	}

	server := gateway.New(cfg, accountPool, manualStore, hiddenStore, logger)
	httpServer := &http.Server{
		Addr:    cfg.Listen,
		Handler: server.Handler(),
		// 不设 WriteTimeout：流式响应可能长时间持续
		ReadHeaderTimeout: 20 * time.Second,
	}

	listenerErr := make(chan error, 1)
	go func() {
		logger.Printf("管理页/网关：http://%s/", cfg.Listen)
		logger.Printf("OpenAI 基址：http://%s/v1", cfg.Listen)
		logger.Printf("本地 API Key：%s", cfg.APIKey)
		if err := httpServer.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			listenerErr <- err
		}
	}()

	// 首次巡检放后台，避免拖慢启动
	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
		defer cancel()
		accountPool.CheckAll(ctx)
		logAccountStates(accountPool)
	}()

	// 定时任务
	stopTicker := make(chan struct{})
	go func() {
		checkTicker := time.NewTicker(time.Duration(cfg.CheckIntervalSeconds) * time.Second)
		refreshTicker := time.NewTicker(time.Duration(cfg.RefreshIntervalSeconds) * time.Second)
		defer checkTicker.Stop()
		defer refreshTicker.Stop()
		for {
			select {
			case <-stopTicker:
				return
			case <-checkTicker.C:
				ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
				accountPool.CheckAll(ctx)
				cancel()
			case <-refreshTicker.C:
				// 跟随 Cindy 侧 key 轮换：只有账号集合变化时才重新巡检
				if added, removed, _ := accountPool.Refresh(); added > 0 || removed > 0 {
					logger.Printf("账号变更：新增 %d，失效 %d", added, removed)
					ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
					accountPool.CheckAll(ctx)
					cancel()
				}
			}
		}
	}()

	signals := make(chan os.Signal, 1)
	signal.Notify(signals, os.Interrupt, syscall.SIGTERM)

	select {
	case err := <-listenerErr:
		close(stopTicker)
		return fmt.Errorf("监听失败（端口可能被占用）: %w", err)
	case sig := <-signals:
		logger.Printf("收到信号 %s，正在关闭…", sig)
		close(stopTicker)
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = httpServer.Shutdown(ctx)
		return nil
	}
}

// resolveConfigPath 决定配置文件位置。
//
// 依次尝试：
//  1. 显式参数
//  2. 可执行文件同级的 runtime/config.json（`go run` / 直接放根目录时）
//  3. 可执行文件上一级的 runtime/config.json（产物落在 bin/ 下的标准布局）
//  4. 当前目录的 runtime/config.json（兜底）
//
// 这样无论从 CockpitTools 内拉起、`go run` 还是双击 exe，都能找到同一份配置。
func resolveConfigPath(explicit string) (string, error) {
	if explicit != "" {
		return explicit, nil
	}
	if exe, err := os.Executable(); err == nil {
		exeDir := filepath.Dir(exe)
		candidates := []string{
			filepath.Join(exeDir, "runtime", "config.json"),
			filepath.Join(filepath.Dir(exeDir), "runtime", "config.json"),
		}
		for _, candidate := range candidates {
			if _, statErr := os.Stat(candidate); statErr == nil {
				return candidate, nil
			}
		}
		// 都不存在时用标准布局（上一级），首次运行会在那里生成
		return candidates[1], nil
	}
	return filepath.Join("runtime", "config.json"), nil
}

// logAccountStates 打印账号池现状（key 一律脱敏）。
func logAccountStates(accountPool *pool.Pool) {
	snapshot := accountPool.Snapshot()
	logger.Printf("健康检查完成：%d/%d 可用", snapshot.Available, snapshot.Total)
	for _, account := range snapshot.Accounts {
		if account.Status == "ok" {
			logger.Printf("  可用 %s（%s） %d 个模型 %dms",
				account.Endpoint, account.KeyMasked, account.ModelCount, account.LatencyMs)
			continue
		}
		logger.Printf("  不可用 %s（%s） — %s", account.Endpoint, account.KeyMasked, account.StatusDetail)
	}
}

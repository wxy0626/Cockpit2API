//go:build windows

// watch_windows.go —— 父进程守卫：CockpitTools 退出后本 sidecar 自动退出。
//
// 为什么需要：sidecar 由主程序拉起，若主程序退出而 sidecar 残留，会一直占着网关
// 端口与账号额度，用户下次启动应用时还会误以为"端口被占用"。
// 这与 cockpit-cliproxy 的 --parent-pid 语义保持一致。
package parent

import (
	"os"
	"syscall"
	"time"
)

// stillActiveProcessExitCode 是 Win32 的 STILL_ACTIVE（259）：
// GetExitCodeProcess 返回它表示进程仍在运行。
const stillActiveProcessExitCode = 259

// processQueryLimitedInformation 是最小查询权限，不需要 PROCESS_ALL_ACCESS
const processQueryLimitedInformation = 0x1000

// watchInterval 检查间隔。3 秒足够及时，又不会造成可感知的 CPU 开销。
const watchInterval = 3 * time.Second

// Watch 在后台监视父进程；父进程消失即自行退出。
func Watch(pid int) {
	if pid <= 0 {
		return
	}
	go func() {
		for {
			time.Sleep(watchInterval)
			if !alive(pid) {
				os.Exit(0)
			}
		}
	}()
}

// alive 判断给定 pid 的进程是否仍在运行。
func alive(pid int) bool {
	handle, err := syscall.OpenProcess(processQueryLimitedInformation, false, uint32(pid))
	if err != nil {
		return false // 打不开句柄 = 进程已不存在（或权限不足，按已退出处理）
	}
	defer syscall.CloseHandle(handle)

	var code uint32
	if err := syscall.GetExitCodeProcess(handle, &code); err != nil {
		return false
	}
	return code == stillActiveProcessExitCode
}

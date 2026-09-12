// Package logbuf 进程内日志环形缓冲。
//
// 网关由桌面 App 以 sidecar 方式直接拉起（不再跑 Docker），管理端 /api/logs
// 无法再借助 docker logs 取日志，因此把关键输出（启动日志 + 每请求表格行）
// 同步存入内存环形缓冲，按需吐出最近 N 行。
package logbuf

import (
	"strings"
	"sync"
)

// Ring 固定容量的按行环形缓冲。
type Ring struct {
	mu    sync.Mutex
	lines []string // 最多 max 行，旧行被覆盖
	max   int
	pend  strings.Builder // 行内未凑齐的半行（Write 可能按字节切片到达）
}

// global 进程级单例：一个网关进程只有一份日志流，无需多实例。
var global = New(1000)

// New 创建容量为 maxLines 行的环形缓冲。
func New(maxLines int) *Ring {
	if maxLines < 100 {
		maxLines = 100
	}
	return &Ring{max: maxLines}
}

// Global 返回进程级单例。
func Global() *Ring { return global }

// Add 追加一行（自动补换行语义；空行忽略）。
func (r *Ring) Add(line string) {
	line = strings.TrimRight(line, "\r\n")
	if line == "" {
		return
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	if len(r.lines) >= r.max {
		r.lines = r.lines[len(r.lines)-r.max+1:]
	}
	r.lines = append(r.lines, line)
}

// Write 实现 io.Writer：按换行切行入环形缓冲，半行缓存到下次凑齐。
// 典型用法：log.SetOutput(io.MultiWriter(os.Stderr, logbuf.Global()))。
func (r *Ring) Write(p []byte) (int, error) {
	total := len(p)
	r.mu.Lock()
	defer r.mu.Unlock()
	for len(p) > 0 {
		idx := indexByte(p, '\n')
		if idx < 0 {
			r.pend.Write(p)
			break
		}
		r.pend.Write(p[:idx])
		r.emitLocked()
		p = p[idx+1:]
	}
	return total, nil
}

// Snapshot 返回最近 n 行（不足 n 返回全部），按时间正序。
func (r *Ring) Snapshot(n int) []string {
	if n <= 0 {
		n = 200
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	// 先把未成行的半行也吐出去（通常是仍在打印中的最新一行）
	tail := r.pend.String()
	out := make([]string, 0, n+1)
	start := 0
	if len(r.lines) > n {
		start = len(r.lines) - n
	}
	out = append(out, r.lines[start:]...)
	if tail != "" {
		out = append(out, tail)
	}
	return out
}

// emitLocked 在持锁状态下把累积的半行落为一行。
func (r *Ring) emitLocked() {
	line := r.pend.String()
	r.pend.Reset()
	if len(r.lines) >= r.max {
		r.lines = r.lines[len(r.lines)-r.max+1:]
	}
	r.lines = append(r.lines, line)
}

// indexByte 独立实现避免重复 import bytes。
func indexByte(p []byte, b byte) int {
	for i := range p {
		if p[i] == b {
			return i
		}
	}
	return -1
}

//go:build windows

// dpapi_windows.go —— Windows DPAPI 调用，解密 Cindy 落盘凭据的第一步。
//
// 为什么需要它：Cindy 桌面端（Electron）用 safeStorage 写凭据，其中的主密钥由
// DPAPI(CurrentUser) 保护。以同一 Windows 用户运行的本进程可以直接解开。
package cindyaccount

import (
	"fmt"
	"syscall"
	"unsafe"
)

// dataBlob 对应 Win32 的 DATA_BLOB：长度 + 裸指针
type dataBlob struct {
	cbData uint32
	pbData *byte
}

var (
	crypt32DLL             = syscall.NewLazyDLL("crypt32.dll")
	procCryptUnprotectData = crypt32DLL.NewProc("CryptUnprotectData")
	kernel32DLL            = syscall.NewLazyDLL("kernel32.dll")
	procLocalFree          = kernel32DLL.NewProc("LocalFree")
)

// cryptProtectUIForbidden 禁止弹出任何 UI，与 Electron safeStorage 行为保持一致
const cryptProtectUIForbidden = 0x01

// dpapiUnprotect 解密一段 CurrentUser 作用域的 DPAPI 密文。
//
// 返回的是拷贝：DPAPI 分配的内存在 LocalFree 后即失效，不能直接引用。
func dpapiUnprotect(data []byte) ([]byte, error) {
	if len(data) == 0 {
		return nil, fmt.Errorf("DPAPI 输入为空")
	}
	in := dataBlob{cbData: uint32(len(data)), pbData: &data[0]}
	var out dataBlob
	ret, _, callErr := procCryptUnprotectData.Call(
		uintptr(unsafe.Pointer(&in)),
		0, // ppszDataDescr
		0, // pOptionalEntropy
		0, // pvReserved
		0, // pPromptStruct
		cryptProtectUIForbidden,
		uintptr(unsafe.Pointer(&out)),
	)
	if ret == 0 {
		return nil, fmt.Errorf("CryptUnprotectData 失败（密文无效，或不属于当前 Windows 用户）: %v", callErr)
	}
	defer procLocalFree.Call(uintptr(unsafe.Pointer(out.pbData)))

	plain := make([]byte, out.cbData)
	copy(plain, unsafe.Slice(out.pbData, out.cbData))
	return plain, nil
}

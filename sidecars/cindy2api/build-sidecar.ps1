# 构建 cindy2api sidecar（Cindy 反代网关）。
# 用法：powershell -File build-sidecar.ps1
#
# 与 wb2api 同族：产物落在本模块的 bin/ 下，独立部署，不随 CockpitTools 打包
# （src-tauri/tauri.conf.json 的 externalBin 只注册了 cockpit-cliproxy）。
$ErrorActionPreference = "Stop"

$bin = Join-Path $PSScriptRoot "bin"
New-Item -ItemType Directory -Force -Path $bin | Out-Null

$out = Join-Path $bin "cindy2api.exe"
Push-Location $PSScriptRoot
try {
    go build -ldflags "-s -w" -o $out ./cmd/server
    if ($LASTEXITCODE -ne 0) { throw "go build 失败" }
} finally {
    Pop-Location
}

Write-Host "已构建 $out"
Write-Host "启动：& '$out'   （首次运行会自动生成 runtime/config.json 与本地 API Key）"

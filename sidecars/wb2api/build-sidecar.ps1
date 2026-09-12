# Build the wb2api gateway sidecar binary from this folder.
# Usage:  powershell -File build-sidecar.ps1
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot   # wb-switch-app root
$bin  = Join-Path $root "src-tauri\binaries"
New-Item -ItemType Directory -Force -Path $bin | Out-Null
foreach ($name in @("wb2api-x86_64-pc-windows-gnullvm.exe", "wb2api-x86_64-pc-windows-msvc.exe")) {
    $out = Join-Path $bin $name
    go build -ldflags "-s -w" -o $out ./cmd/server
    if ($LASTEXITCODE -ne 0) { throw "go build failed for $name" }
    Write-Host "built $out"
}
Write-Host "Done. Rebuild the App with: npx tauri build --bundles nsis"

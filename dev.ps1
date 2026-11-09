$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

if (-not (Get-Command npm -ErrorAction SilentlyContinue)) {
    Write-Host '未找到 npm，请先安装 Node.js LTS。' -ForegroundColor Red
    exit 1
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host '未找到 cargo，请先安装 Rust。' -ForegroundColor Red
    exit 1
}

npm install
npm run tauri dev

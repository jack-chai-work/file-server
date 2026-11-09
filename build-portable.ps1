$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

function Require-Command($Name, $Hint) {
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        Write-Host "未找到 $Name。$Hint" -ForegroundColor Red
        exit 1
    }
}

Write-Host '=== 轻型文件服务器：Portable 单文件构建 ===' -ForegroundColor Cyan
Require-Command 'node' '请安装 Node.js LTS。'
Require-Command 'npm' '请安装 npm（通常随 Node.js 提供）。'
Require-Command 'cargo' '请安装 Rust，并确保使用 stable-msvc 工具链。'

npm install
Write-Host '`n[1/2] 构建 Tauri 前端和 Release 可执行文件…' -ForegroundColor Yellow
npm run build
cargo build --release --manifest-path src-tauri/Cargo.toml

$exe = Join-Path $PSScriptRoot 'src-tauri\target\release\tauri-ftp-server.exe'
if (-not (Test-Path $exe)) {
    throw "没有找到构建结果：$exe"
}

$outDir = Join-Path $PSScriptRoot 'portable'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$outExe = Join-Path $outDir '轻型文件服务器.exe'
Copy-Item $exe $outExe -Force

Write-Host '`n[2/2] Portable 构建完成' -ForegroundColor Green
Write-Host "单文件程序：$outExe"
Write-Host '注意：这是单个应用 EXE；Windows 仍需要 WebView2 Runtime。'

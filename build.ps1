$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

function Require-Command($Name, $Hint) {
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        Write-Host "未找到 $Name。$Hint" -ForegroundColor Red
        exit 1
    }
}

Write-Host '=== 轻型文件服务器：Windows 发布构建 ===' -ForegroundColor Cyan
Require-Command 'node' '请安装 Node.js LTS。'
Require-Command 'npm' '请安装 npm（通常随 Node.js 提供）。'
Require-Command 'cargo' '请安装 Rust，并确保使用 stable-msvc 工具链。'

Write-Host "Node: $(node --version)"
Write-Host "npm : $(npm --version)"
Write-Host "Rust: $(rustc --version)"

Write-Host '`n[1/3] 安装前端依赖…' -ForegroundColor Yellow
npm install

Write-Host '`n[2/4] 构建前端并打包 Tauri 安装包…' -ForegroundColor Yellow
npm run tauri build

Write-Host '`n[3/4] 生成 Portable 单文件版本…' -ForegroundColor Yellow
$exe = Join-Path $PSScriptRoot 'src-tauri\target\release\tauri-ftp-server.exe'
$outDir = Join-Path $PSScriptRoot 'portable'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
if (Test-Path $exe) { Copy-Item $exe (Join-Path $outDir '轻型文件服务器.exe') -Force } else { throw "没有找到 Release 可执行文件：$exe" }

Write-Host '`n[4/4] 构建完成' -ForegroundColor Green
Write-Host '安装包位置：'
Write-Host '  src-tauri\target\release\bundle\nsis\'
Write-Host '  src-tauri\target\release\bundle\msi\'
Write-Host '  portable\轻型文件服务器.exe'

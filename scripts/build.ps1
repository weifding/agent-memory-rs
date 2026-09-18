# =============================================================================
# build.ps1 —— Windows (MSVC) 构建 agent-memory-server（含 Kùzu 图谱 feature）
#
# Windows 适配要点：
#   - 工具链：MSVC（VS Build Tools 2022，含 C++ 桌面开发负载 + CMake）
#   - kuzu 0.11.x 在 Windows 上由 cmake + MSVC 编译，无需 GCC
#   - 产物为 agent-memory-server.exe
#
# 用法:
#   .\scripts\build.ps1              # 构建图谱版 release
#   .\scripts\build.ps1 -NoGraph     # 仅基础版（无 kuzu）
#   .\scripts\build.ps1 -Target x86_64-pc-windows-gnu   # MinGW 交叉（不推荐）
# =============================================================================
param(
    [switch]$NoGraph,
    [string]$Target = ""
)

$ErrorActionPreference = "Stop"
$ProjectRoot = Split-Path -Parent $PSScriptRoot
Set-Location $ProjectRoot

# ---- 工具链检查 ----
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoPath = "$env:USERPROFILE\.cargo\bin\cargo.exe"
    if (Test-Path $cargoPath) { $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH" }
    else { Write-Error "未找到 cargo，请先安装 Rust: https://rustup.rs"; exit 1 }
}

# MSVC 环境：若未在 VS Developer Prompt 中，尝试定位 vcvars
$vsPath = $null
if (-not $env:VCToolsInstallDir) {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        $vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if ($vsPath) { Write-Host "[INFO] 检测到 VS: $vsPath（建议在 Developer Prompt 中运行以获取完整 MSVC 环境）" }
        else { Write-Warning "未检测到 VS C++ 工具集，kuzu 编译可能失败；请安装 VS Build Tools 2022（含 C++ 负载）" }
    }
}

# CMake/Ninja：kuzu 的 build.rs 在 Windows 固定 -G Ninja，两个工具都必须在 PATH；
# 普通终端里 VS 自带的不进 PATH，这里自动定位补齐（Build Tools 2019/2022 目录布局一致）
if (-not (Get-Command cmake -ErrorAction SilentlyContinue) -or -not (Get-Command ninja -ErrorAction SilentlyContinue)) {
    if (-not $vsPath -and $env:VCToolsInstallDir) {
        # VCToolsInstallDir 形如 <VS 根>\VC\Tools\MSVC\<版本>\，回溯 4 级到 VS 安装根
        $vsPath = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $env:VCToolsInstallDir)))
    }
    if ($vsPath) {
        $cmakeDir = Join-Path $vsPath "Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"
        $ninjaDir = Join-Path $vsPath "Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja"
        $add = @()
        if (-not (Get-Command cmake -ErrorAction SilentlyContinue) -and (Test-Path (Join-Path $cmakeDir "cmake.exe"))) { $add += $cmakeDir }
        if (-not (Get-Command ninja -ErrorAction SilentlyContinue) -and (Test-Path (Join-Path $ninjaDir "ninja.exe"))) { $add += $ninjaDir }
        if ($add) {
            $env:PATH = "$($add -join ';');$env:PATH"
            Write-Host "[INFO] 已补齐 VS 自带 CMake/Ninja 到 PATH: $($add -join '; ')"
        }
    }
    if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) { Write-Warning "PATH 中未找到 cmake，kuzu 原生编译会失败（安装 VS 组件“适用于 Windows 的 C++ CMake 工具”，或 winget install Kitware.CMake）" }
    if (-not (Get-Command ninja -ErrorAction SilentlyContinue)) { Write-Warning "PATH 中未找到 ninja，kuzu 原生编译会失败（VS 组件“适用于 Windows 的 C++ CMake 工具”自带）" }
}

# ---- 构建 ----
$cmd = @("build", "--release")
if (-not $NoGraph) { $cmd += @("--features", "graph") }
if ($Target) { $cmd += @("--target", $Target) }

Write-Host "[INFO] cargo $($cmd -join ' '))"
& cargo @cmd
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$binary = if ($Target) { "target\$Target\release\agent-memory-server.exe" } else { "target\release\agent-memory-server.exe" }
Write-Host "[OK] 构建完成: $ProjectRoot\$binary"

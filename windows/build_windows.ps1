# PowerShell build script for CineFS on Windows (x86_64 / ARM64)
# Usage: powershell -ExecutionPolicy Bypass -File .\windows\build_windows.ps1

$ErrorActionPreference = "Stop"
$RootDir = Split-Path -Parent $PSScriptRoot
Set-Location $RootDir

Write-Host "==> [1/4] Building CineFS Rust workspace (cinefs-win.exe & cinefs-cli.exe) in release mode..." -ForegroundColor Cyan
cargo build --release --workspace

$DepsDir = Join-Path $PSScriptRoot "deps"
New-Item -ItemType Directory -Force -Path $DepsDir | Out-Null
$WinFspMsi = Join-Path $DepsDir "winfsp.msi"

if (-not (Test-Path $WinFspMsi)) {
    Write-Host "==> [2/4] Downloading WinFsp 2.0 runtime MSI for bundling..." -ForegroundColor Cyan
    $WinFspUrl = "https://github.com/winfsp/winfsp/releases/download/v2.0/winfsp-2.0.23075.msi"
    Invoke-WebRequest -Uri $WinFspUrl -OutFile $WinFspMsi
} else {
    Write-Host "==> [2/4] Found cached WinFsp MSI at $WinFspMsi" -ForegroundColor Green
}

$BuildDir = Join-Path $PSScriptRoot "build"
New-Item -ItemType Directory -Force -Path $BuildDir | Out-Null
Copy-Item "target\release\cinefs-win.exe" (Join-Path $BuildDir "CineFS.exe") -Force
Copy-Item "target\release\cinefs-win.exe" (Join-Path $BuildDir "CineFS-Setup.exe") -Force
Copy-Item "target\release\cinefs-cli.exe" (Join-Path $BuildDir "cinefs-cli.exe") -Force

$IsccPaths = @(
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles}\Inno Setup 6\ISCC.exe"
)
$Iscc = $IsccPaths | Where-Object { Test-Path $_ } | Select-Object -First 1

if ($Iscc) {
    Write-Host "==> [3/4] Compiling CineFS-Setup.exe via Inno Setup ($Iscc)..." -ForegroundColor Cyan
    & $Iscc (Join-Path $PSScriptRoot "CineFS.iss")
    # Native tools do not trip $ErrorActionPreference; without this check a failed compile
    # would leave the plain CineFS.exe copy in place as "CineFS-Setup.exe".
    if ($LASTEXITCODE -ne 0) { throw "Inno Setup compilation failed (exit code $LASTEXITCODE)" }
    Write-Host "==> [4/4] Done! Installer created at: windows\build\CineFS-Setup.exe" -ForegroundColor Green
} elseif ($env:CINEFS_REQUIRE_INNO -eq "1") {
    # Release builds (CI) must ship the Inno Setup installer (uninstaller, Apps & features
    # entry, clean upgrades), never the fallback below.
    throw "Inno Setup 6 (ISCC.exe) not found, but CINEFS_REQUIRE_INNO=1. Install Inno Setup 6 and retry."
} else {
    Write-Host "==> [3/4] Built-in self-contained Setup Wizard created at: windows\build\CineFS-Setup.exe (and portable windows\build\CineFS.exe)" -ForegroundColor Green
}

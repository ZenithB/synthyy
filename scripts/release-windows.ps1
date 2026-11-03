<#
.SYNOPSIS
  Build the Synthyy Windows installer on this PC (NSIS, per-user install).

.DESCRIPTION
  Windows counterpart of scripts/release-macos.sh. Produces
  target\release\bundle\nsis\Synthyy_<version>_x64-setup.exe, which installs
  for the current user without administrator rights.

  By default the build includes ASIO support. That needs LLVM (libclang) for
  the ASIO bindings; the Steinberg ASIO SDK is downloaded automatically during
  the build. Use -NoAsio for a WASAPI-only build without LLVM.

  The installer is unsigned: on first launch Windows SmartScreen shows
  "Windows protected your PC" -> More info -> Run anyway. See docs/RELEASE.md.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\release-windows.ps1

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\release-windows.ps1 -NoAsio
#>
param(
    [switch]$NoAsio
)

$ErrorActionPreference = 'Stop'

# Native programs (npm, cargo) report failure through their exit code, which
# PowerShell does not turn into an error by itself - check it every time.
function Invoke-Checked {
    param([string]$Exe, [string[]]$Arguments)
    Write-Host "> $Exe $($Arguments -join ' ')" -ForegroundColor Cyan
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Exe failed with exit code $LASTEXITCODE"
    }
}

# Run from the repository root whatever the current folder is.
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

# ---- prerequisites (docs/WINDOWS.md section 1) ----
$missing = @()
foreach ($cmd in @('cargo', 'npm', 'npx')) {
    if (-not (Get-Command $cmd -ErrorAction SilentlyContinue)) { $missing += $cmd }
}
if ($missing.Count -gt 0) {
    throw "Not found on PATH: $($missing -join ', '). Install Rust (rustup.rs) and Node.js 22 - see docs/WINDOWS.md section 1."
}

if (-not $NoAsio) {
    if (-not $env:LIBCLANG_PATH) {
        $llvm = 'C:\Program Files\LLVM\bin'
        if (Test-Path (Join-Path $llvm 'libclang.dll')) {
            $env:LIBCLANG_PATH = $llvm
        } else {
            throw "ASIO build needs LLVM: run 'winget install LLVM.LLVM', or build without ASIO using -NoAsio."
        }
    }
    Write-Host "ASIO: on (libclang from $env:LIBCLANG_PATH)"
} else {
    Write-Host "ASIO: off (WASAPI only)"
}

# ---- build ----
Invoke-Checked 'npm' @('ci')

$tauriArgs = @('tauri', 'build', '--bundles', 'nsis')
if (-not $NoAsio) { $tauriArgs += @('--features', 'asio') }
# tauri build runs `npm run build` (web UI) itself, then the release Rust build
Invoke-Checked 'npx' $tauriArgs

# ---- result ----
$installer = Get-ChildItem -Path (Join-Path $repo 'target\release\bundle\nsis') -Filter '*-setup.exe' |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 1
if (-not $installer) {
    throw 'Build finished but no *-setup.exe was found under target\release\bundle\nsis.'
}
$sizeMb = [math]::Round($installer.Length / 1MB, 1)
Write-Host ''
Write-Host "Installer: $($installer.FullName) ($sizeMb MB)" -ForegroundColor Green
Write-Host 'Double-click it to install for this user (no admin needed).'

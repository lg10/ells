# ells installer for Windows PowerShell
# Usage:  irm https://raw.githubusercontent.com/lg10/ells/main/install.ps1 | iex
# Env vars:
#   ELLS_VERSION      pin a version (e.g. v0.1.0); defaults to latest GitHub release
#   ELLS_INSTALL_DIR  install dir; defaults to %LOCALAPPDATA%\ells\bin
# NOTE: keep this file ASCII-only; irm|iex pipelines are decoded without a BOM.
$ErrorActionPreference = 'Stop'

$Repo = 'lg10/ells'
$InstallDir = if ($env:ELLS_INSTALL_DIR) { $env:ELLS_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'ells\bin' }

function Info($msg) { Write-Host "[ells] $msg" -ForegroundColor Cyan }
# NOTE: never exit 1 here -- under `irm | iex` that would close the user's shell session.
function Fail($msg) { Write-Host "[ells] install failed: $msg" -ForegroundColor Red; throw "ells install failed: $msg" }

$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
if ($arch -ne 'X64') { Fail "unsupported Windows architecture: $arch (only x86_64 is published for now)" }

$version = $env:ELLS_VERSION
if (-not $version) {
  try {
    $latest = Invoke-RestMethod 'https://api.github.com/repos/lg10/ells/releases/latest'
    $version = $latest.tag_name
  } catch { Fail 'cannot resolve latest version; set $env:ELLS_VERSION="vX.Y.Z" and retry' }
}
if (-not $version) { Fail 'cannot resolve latest version; set $env:ELLS_VERSION="vX.Y.Z" and retry' }

$base = "https://github.com/$Repo/releases/download/$version"
$asset = 'ells-windows-x86_64.exe'
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ('ells-install-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  Info "downloading ells $version ($asset)..."
  $exePath = Join-Path $tmp $asset
  Invoke-WebRequest -Uri "$base/$asset" -OutFile $exePath -UseBasicParsing

  $sumsPath = Join-Path $tmp 'SHA256SUMS.txt'
  try { Invoke-WebRequest -Uri "$base/SHA256SUMS.txt" -OutFile $sumsPath -UseBasicParsing } catch { Fail 'cannot fetch SHA256SUMS.txt (release may be incomplete)' }
  $line = (Get-Content $sumsPath | Where-Object { $_ -match [regex]::Escape($asset) }) | Select-Object -First 1
  if (-not $line) { Fail "no checksum entry for $asset in SHA256SUMS.txt" }
  $expected = ($line -split '\s+')[0]
  $actual = (Get-FileHash -Algorithm SHA256 -Path $exePath).Hash.ToLower()
  if ($actual -ne $expected.ToLower()) { Fail 'SHA256 mismatch, the download may be corrupted' }
  Info 'SHA256 verified'

  New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
  Copy-Item $exePath (Join-Path $InstallDir 'ells.exe') -Force
  Copy-Item $exePath (Join-Path $InstallDir 's.exe') -Force
  Info "installed: $InstallDir\ells.exe and $InstallDir\s.exe"

  $current = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (-not ($current -split ';' | Where-Object { $_ -eq $InstallDir })) {
    [Environment]::SetEnvironmentVariable('Path', (($current, $InstallDir) -join ';'), 'User')
    Info 'added install dir to user PATH (takes effect in new terminals)'
  }

  # PowerShell resolves functions before PATH executables, and `s` is a built-in alias
  # (Set-Variable) -- register a global function in the user profile so `s` works there too.
  $sExe = Join-Path $InstallDir 's.exe'
  $profilePath = $PROFILE.CurrentUserAllHosts
  $profileDir = Split-Path $profilePath -Parent
  if (-not (Test-Path $profileDir)) { New-Item -ItemType Directory -Force -Path $profileDir | Out-Null }
  if (-not (Test-Path $profilePath)) { New-Item -ItemType File -Path $profilePath | Out-Null }
  $marker = '# ells: short command s'
  if (-not ((Get-Content $profilePath -Raw) -match [regex]::Escape($marker))) {
    Add-Content $profilePath @"

$marker
if (Test-Path '$sExe') {
    function global:s { & '$sExe' @args }
}
"@
    Info "registered the 's' command in the PowerShell profile ($profilePath)"
  }

  Info "done ($version). Reopen cmd / PowerShell, then run: ells   or   s"
} finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

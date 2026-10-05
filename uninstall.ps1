# ells uninstaller for Windows PowerShell
# Usage: irm https://raw.githubusercontent.com/lg10/ells/main/uninstall.ps1 | iex
#        $env:ELLS_PURGE='1'; irm https://raw.githubusercontent.com/lg10/ells/main/uninstall.ps1 | iex
# Env vars:
#   ELLS_INSTALL_DIR  install dir to clean; defaults to %LOCALAPPDATA%\ells\bin
#   ELLS_PURGE        1 = also delete ~\.ells (vault + known_hosts + settings)
#
# ~\.ells holds the encrypted vault with every host address, account and
# password. It is KEPT by default -- removing the binaries must not destroy
# credentials by accident. There is no recovery once the vault is deleted.
# NOTE: keep this file ASCII-only; irm|iex pipelines are decoded without a BOM.
$ErrorActionPreference = 'Stop'

$InstallDir = if ($env:ELLS_INSTALL_DIR) { $env:ELLS_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'ells\bin' }

function Info($msg) { Write-Host "[ells] $msg" -ForegroundColor Cyan }
function Warn($msg) { Write-Host "[ells] $msg" -ForegroundColor Yellow }

foreach ($name in @('ells.exe', 's.exe')) {
  $binPath = Join-Path $InstallDir $name
  if (Test-Path -LiteralPath $binPath) {
    Remove-Item -LiteralPath $binPath -Force -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $binPath) {
      Warn "cannot remove $binPath -- close any running ells/s window and retry"
    } else {
      Info "removed $binPath"
    }
  }
}

# Take the install dir out of the user PATH (install.ps1 put it there).
$current = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($current) {
  $all = @($current -split ';')
  $kept = @($all | Where-Object { $_ -ne $InstallDir })
  if ($kept.Count -lt $all.Count) {
    [Environment]::SetEnvironmentVariable('Path', ($kept -join ';'), 'User')
    Info "removed $InstallDir from your user PATH (reopen terminals to apply)"
  }
}

if (Test-Path -LiteralPath $InstallDir) {
  if (-not (Get-ChildItem -LiteralPath $InstallDir -Force)) {
    Remove-Item -LiteralPath $InstallDir -Force
    Info "removed the now empty directory $InstallDir"
  }
}

# Drop the 's' helper block install.ps1 appended to the PowerShell profile.
$profilePath = $PROFILE.CurrentUserAllHosts
if (Test-Path -LiteralPath $profilePath) {
  $text = Get-Content -LiteralPath $profilePath -Raw
  if ($text -and $text.Contains('# ells: short command s')) {
    $cleaned = [regex]::Replace($text, '(?ms)^# ells: short command s\r?\n.*?\r?\n\}\r?\n', '')
    if ($cleaned -ne $text) {
      Set-Content -LiteralPath $profilePath -Value $cleaned -NoNewline
      Info "removed the 's' command from $profilePath"
    }
  }
}

$vaultDir = Join-Path $env:USERPROFILE '.ells'
if ($env:ELLS_PURGE -eq '1') {
  if (Test-Path -LiteralPath $vaultDir) {
    Remove-Item -LiteralPath $vaultDir -Recurse -Force
    Warn "deleted $vaultDir -- the vault and all stored credentials are gone for good"
  }
} else {
  Warn "kept $vaultDir (vault, known_hosts, settings)"
  Info 'set $env:ELLS_PURGE="1" and rerun, or delete that folder yourself, to wipe it'
}

Info 'uninstall finished'

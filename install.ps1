# ells installer for Windows PowerShell
# Usage:  irm https://raw.githubusercontent.com/lg10/ells/main/install.ps1 | iex
# Env vars:
#   ELLS_VERSION         pin a version (e.g. v0.1.0); defaults to latest GitHub release
#   ELLS_INSTALL_DIR     install dir; defaults to %LOCALAPPDATA%\ells\bin
#   ELLS_UPDATE_SOURCES  comma/semicolon/space separated sources: `github` or a
#                        mirror prefix (e.g. https://gh-proxy.com). Empty = built-ins.
#   ELLS_API_URL / ELLS_DOWNLOAD_URL are deprecated and ignored.
# NOTE: keep this file ASCII-only; irm|iex pipelines are decoded without a BOM.
$ErrorActionPreference = 'Stop'

$Repo = 'lg10/ells'
$InstallDir = if ($env:ELLS_INSTALL_DIR) { $env:ELLS_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'ells\bin' }

function Info($msg) { Write-Host "[ells] $msg" -ForegroundColor Cyan }
# NOTE: never exit 1 here -- under `irm | iex` that would close the user's shell session.
function Fail($msg) { Write-Host "[ells] install failed: $msg" -ForegroundColor Red; throw "ells install failed: $msg" }

# GitHub needs TLS 1.2; Windows PowerShell 5.1 does not always negotiate it by default.
try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch { }

# Architecture detection: RuntimeInformation can be missing/empty on old .NET
# Framework (PowerShell 5.1 without 4.7.1+), so fall back to
# PROCESSOR_ARCHITECTURE. ARM64 Windows has no native asset yet -- the x86_64
# build runs through Windows' built-in emulation, so we accept both.
$archRaw = $null
try { $archRaw = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture } catch { }
if (-not "$archRaw") { $archRaw = $env:PROCESSOR_ARCHITECTURE }
$arch = switch -Regex ("$archRaw") {
  '^(x64|amd64)$'   { 'x86_64' }
  '^(arm64)$'       { 'arm64' }
  default           { "$archRaw" }
}
if ($arch -eq 'arm64') {
  Info 'Windows ARM64 detected: installing the x86_64 build (runs via built-in emulation)'
} elseif ($arch -ne 'x86_64') {
  Fail "unsupported Windows architecture: '$archRaw' (only x86_64 and ARM64-via-emulation are published for now)"
}
Info "target architecture: $arch"

$asset = 'ells-windows-x86_64.exe'

if ($env:ELLS_API_URL -or $env:ELLS_DOWNLOAD_URL) {
  Info 'ELLS_API_URL / ELLS_DOWNLOAD_URL are deprecated and ignored; use ELLS_UPDATE_SOURCES instead'
}

if ($env:ELLS_UPDATE_SOURCES) {
  $rawSources = @($env:ELLS_UPDATE_SOURCES -split '[,;\s]+' | Where-Object { $_ })
} else {
  $rawSources = @(
    'github',
    'https://gh-proxy.com',
    'https://ghproxy.net',
    'https://ghfast.top',
    'https://gh.llkk.cc'
  )
}
$sourceList = New-Object System.Collections.ArrayList
foreach ($src in $rawSources) {
  if ($src -eq 'github' -or $src -match '^https?://') {
    if (-not $sourceList.Contains($src)) { [void]$sourceList.Add($src) }
  } else {
    Info "ignoring invalid update source: $src"
  }
}
$sources = @($sourceList)
if ($sources.Count -eq 0) { Fail 'no usable update source; check ELLS_UPDATE_SOURCES' }

function Source-Url($src, $url) {
  if ($src -eq 'github') { return $url }
  return ($src.TrimEnd('/') + '/' + $url)
}

function Source-Label($src) {
  if ($src -eq 'github') { return 'github' }
  try { return ([Uri]$src).Host } catch { return $src }
}

$version = $env:ELLS_VERSION
if (-not $version) {
  foreach ($src in $sources) {
    try {
      $url = Source-Url $src "https://api.github.com/repos/$Repo/releases/latest"
      $latest = Invoke-RestMethod -Uri $url -TimeoutSec 8
      if ($latest.tag_name) {
        $version = "$($latest.tag_name)"
        Info "version from $(Source-Label $src)"
        break
      }
    } catch {
      Info "cannot query $(Source-Label $src); trying next source"
    }
  }
}
if (-not $version) { Fail 'cannot resolve latest version; set $env:ELLS_VERSION="vX.Y.Z" and retry' }
if ($version -notmatch '^v[0-9]') { Fail "unexpected version string: $version" }

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ('ells-install-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
$exePath = Join-Path $tmp $asset

try {
  # First collect checksums from every source. Official GitHub wins; otherwise at
  # least two mirrors must agree, so one stale/corrupt mirror cannot define truth.
  Info 'collecting checksums from update sources...'
  $records = New-Object System.Collections.ArrayList
  foreach ($src in $sources) {
    $sumsUrl = Source-Url $src "https://github.com/$Repo/releases/download/$version/SHA256SUMS.txt"
    $sumsPath = Join-Path $tmp ('sums-' + [guid]::NewGuid().ToString('N') + '.txt')
    try {
      Invoke-WebRequest -Uri $sumsUrl -OutFile $sumsPath -UseBasicParsing -TimeoutSec 10
      $line = (Get-Content $sumsPath | Where-Object { $_ -match [regex]::Escape($asset) }) | Select-Object -First 1
      if (-not $line) {
        Info "no $asset entry from $(Source-Label $src); skipping"
        continue
      }
      $sum = ($line -split '\s+')[0].ToLower()
      if ($sum -notmatch '^[0-9a-f]{64}$') {
        Info "invalid checksum from $(Source-Label $src); skipping"
        continue
      }
      [void]$records.Add([pscustomobject]@{ Source = $src; Sum = $sum })
    } catch {
      Info "cannot fetch checksums from $(Source-Label $src); skipping"
    }
  }
  if ($records.Count -eq 0) { Fail 'no update source provided a usable checksum file' }

  $strict = $null
  $githubRecord = $records | Where-Object { $_.Source -eq 'github' } | Select-Object -First 1
  if ($githubRecord) {
    $strict = $githubRecord.Sum
  } else {
    $group = $records | Group-Object Sum | Where-Object { $_.Count -ge 2 } | Select-Object -First 1
    if ($group) { $strict = $group.Name }
  }

  $ordered = New-Object System.Collections.ArrayList
  if ($strict) {
    foreach ($record in $records) {
      if ($record.Sum -eq $strict -and -not $ordered.Contains($record.Source)) {
        [void]$ordered.Add($record.Source)
      }
    }
  } else {
    foreach ($record in $records) {
      if (-not $ordered.Contains($record.Source)) { [void]$ordered.Add($record.Source) }
    }
    Info 'no cross-source checksum consensus; verifying each source against its own checksum'
  }
  if ($ordered.Count -eq 0) { Fail 'no update source matches the trusted checksum' }

  $installed = $false
  foreach ($src in $ordered) {
    $record = $records | Where-Object { $_.Source -eq $src } | Select-Object -First 1
    if (-not $record) { continue }
    $want = $record.Sum
    if ($strict -and $want -ne $strict) { continue }
    for ($attempt = 1; $attempt -le 2 -and -not $installed; $attempt++) {
      Info "downloading ells $version from $(Source-Label $src) (attempt $attempt/2)..."
      try {
        $assetUrl = Source-Url $src "https://github.com/$Repo/releases/download/$version/$asset"
        Invoke-WebRequest -Uri $assetUrl -OutFile $exePath -UseBasicParsing -TimeoutSec 300
        $actual = (Get-FileHash -Algorithm SHA256 -Path $exePath).Hash.ToLower()
        if ($actual -ne $want) { throw 'SHA256 mismatch' }
        $size = (Get-Item $exePath).Length
        if ($size -lt 65536) { throw "download too small ($size bytes)" }
        $magic = New-Object byte[] 2
        $n = 0
        $fs = [System.IO.File]::OpenRead($exePath)
        try { $n = $fs.Read($magic, 0, 2) } finally { $fs.Dispose() }
        if ($n -lt 2 -or $magic[0] -ne 0x4D -or $magic[1] -ne 0x5A) {
          throw 'download is not a Windows executable'
        }
        Info 'SHA256 verified'
        $installed = $true
      } catch {
        Info "source $(Source-Label $src) failed: $($_.Exception.Message)"
        if (Test-Path $exePath) { Remove-Item $exePath -Force -ErrorAction SilentlyContinue }
        Start-Sleep -Milliseconds 700
      }
    }
    if ($installed) { break }
  }
  if (-not $installed) { Fail 'all update sources failed; try again later or set ELLS_VERSION' }

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

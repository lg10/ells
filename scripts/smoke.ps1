param([switch]$PrepareOnly)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# 1. build if needed
if (-not (Test-Path "target\debug\ells.exe")) {
    Write-Host "[ells-smoke] building ells (first run takes a bit)..."
    cargo build --bin ells
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}

# 2. dev hosts file
$dir = Join-Path $env:USERPROFILE ".ells"
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$hostsFile = Join-Path $dir "hosts.dev.toml"
if (-not (Test-Path $hostsFile)) {
    @"
[[hosts]]
alias = "smoke"
hostname = "127.0.0.1"
port = 2222
user = "tester"
auth = { type = "password" }
password = "test123"

"@ | ForEach-Object { [System.IO.File]::WriteAllText($hostsFile, $_, (New-Object System.Text.UTF8Encoding($false))) }
    Write-Host "[ells-smoke] created $hostsFile"
}

# 3. start fake sshd on 2222 unless something is already listening
$srvPid = $null
$listening = Get-NetTCPConnection -LocalPort 2222 -State Listen -ErrorAction SilentlyContinue
if (-not $listening) {
    $log = Join-Path $env:TEMP "ells-fake-sshd.log"
    $p = Start-Process -FilePath "python" -ArgumentList "tests\fake_sshd.py" -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput $log -RedirectStandardError (Join-Path $env:TEMP "ells-fake-sshd.err.log")
    $srvPid = $p.Id
    # wait up to 5s for the port to actually listen
    $up = $false
    foreach ($i in 1..10) {
        Start-Sleep -Milliseconds 500
        if (Get-NetTCPConnection -LocalPort 2222 -State Listen -ErrorAction SilentlyContinue) { $up = $true; break }
        if ($p.HasExited) { break }
    }
    if (-not $up) {
        Write-Warning "fake sshd did not come up; see $log and $env:TEMP\ells-fake-sshd.err.log"
        Stop-Process -Id $srvPid -ErrorAction SilentlyContinue
        $srvPid = $null
    } else {
        Write-Host "[ells-smoke] fake sshd started (pid $srvPid)"
    }
}

try {
    if ($PrepareOnly) {
        Write-Host "[ells-smoke] environment ready; run scripts\smoke.bat for the interactive experience"
    } else {
        Write-Host "[ells-smoke] launching ells --dev  (list -> Enter connect -> 'echo COLOR' for colors)"
        & ".\target\debug\ells.exe" --dev
    }
} finally {
    if ($srvPid) {
        Stop-Process -Id $srvPid -ErrorAction SilentlyContinue
        Write-Host "[ells-smoke] fake sshd stopped"
    }
}

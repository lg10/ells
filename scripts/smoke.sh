#!/usr/bin/env bash
# One-shot ells M1 experience on macOS/Linux (x86 + ARM).
set -e
cd "$(dirname "$0")/.."

[ -f target/debug/ells ] || { echo "[ells-smoke] building..."; cargo build --bin ells; }

mkdir -p ~/.ells
if [ ! -f ~/.ells/hosts.dev.toml ]; then
cat > ~/.ells/hosts.dev.toml <<'EOF'
[[hosts]]
alias = "smoke"
hostname = "127.0.0.1"
port = 2222
user = "tester"
auth = { type = "password" }
password = "test123"
EOF
echo "[ells-smoke] created ~/.ells/hosts.dev.toml"
fi

SRV=""
if ! (exec 3<>/dev/tcp/127.0.0.1/2222) 2>/dev/null; then
  python3 tests/fake_sshd.py >/tmp/ells-fake-sshd.log 2>&1 &
  SRV=$!
  sleep 1
  kill -0 "$SRV" || { echo "[ells-smoke] fake sshd failed: see /tmp/ells-fake-sshd.log"; exit 1; }
  echo "[ells-smoke] fake sshd started (pid $SRV)"
fi

cleanup() { [ -n "$SRV" ] && kill "$SRV" 2>/dev/null || true; }
trap cleanup EXIT

echo "[ells-smoke] launching ells --dev (list -> Enter connect -> 'echo COLOR' for colors)"
./target/debug/ells --dev

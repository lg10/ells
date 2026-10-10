[简体中文](README_zh.md) | **English**

<div align="center">

# ells

**A lightweight, cross-platform SSH client that lives in your terminal.**

`Rust` `TUI` `SSH` `SFTP` `ZMODEM` `port forwarding` `headless CLI`

Homepage: [https://ells.cn](https://ells.cn) · Repo: [GitHub](https://github.com/lg10/ells) · [中文 README](README_zh.md) · [CHANGELOG](CHANGELOG.md) · [SECURITY](SECURITY.md)

![CI](https://github.com/lg10/ells/actions/workflows/ci.yml/badge.svg) ![license](https://img.shields.io/badge/license-MIT-green) ![rust](https://img.shields.io/badge/rust-1.85%2B-orange) ![platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue) [![release](https://img.shields.io/github/v/release/lg10/ells?label=release&color=blue)](https://github.com/lg10/ells/releases) ![downloads](https://img.shields.io/github/downloads/lg10/ells/total?label=downloads) [![changelog](https://img.shields.io/badge/changelog-keep%20a%20changelog-informational)](CHANGELOG.md) [![security](https://img.shields.io/badge/security-policy-red)](SECURITY.md)

</div>

> ⚠️ **Early access.** ells is in active development (v0.2.x). Expect bugs and rough edges; feedback via [Issues](https://github.com/lg10/ells/issues) is very welcome.

## What is ells

ells is an all-in-one terminal SSH client written in pure Rust. It embeds a real terminal emulator (vt100) inside a fast TUI, so remote sessions keep their colors and programs like `vim`/`htop` render correctly — while adding the workflow features a plain `ssh` lacks: an encrypted credential vault, SFTP file browsing, Xshell-style `sz`/`rz` transfer interception, local port forwarding with a tunnel manager, an operation log and session transcripts you can actually read back, and a headless CLI so the same vault works from scripts and CI. All of it driven with a few keystrokes or the mouse.

## Features

- 🔐 **Encrypted vault** — hosts and passwords stored in `~/.ells/vault.bin`, protected by a master password (Argon2id KDF + XChaCha20Poly1305 AEAD). Optional auto-unlock, in-app master-password change.
- 🗂 **Host manager** — aliases, per-host auth (password / private key / **ssh-agent & Pageant**), notes, and one-alias-per-line **ProxyJump bastion** support (`ells` → jump → target, loop detection included).
- ⌨️ **Auth formats that just work** — PKCS#8, PKCS#1 RSA (`BEGIN RSA PRIVATE KEY`), OpenSSH keys, and legacy **DEK-Info / 3DES-encrypted PEM** decrypted natively.
- 🖥 **Embedded terminal** — vt100 emulator with full ANSI/256/true-color output; `Ctrl-Q` toggles a raw **passthrough mode** as an escape hatch for anything the emulator can't handle; `Ctrl-L` redraws.
- 📦 **`sz` / `rz` interception** — ells watches the remote output stream for ZMODEM handshakes (protocol-level detection tuned against real `lrzsz` byte streams) and reroutes them over the SFTP channel with progress bars and native Save-As / file-picker dialogs. No ZMODEM daemon required.
- 📁 **SFTP browser** — `Ctrl-S` opens a remote file browser: Enter to enter/download, `u` upload, `d` download, mouse click + wheel supported, aggregated transfer progress.
- 🎨 **Output highlighting** — `docker ps` is colored column by column (container ID / image name / version tag / name each get their own colour, the command fades back), `kubectl`, log levels (ERROR/WARN/INFO/...), HTTP methods, IPv4 addresses and percentages are colorized on the fly — without overriding programs' own ANSI colors.
- 🔌 **Keepalive** — configurable SSH keepalive interval (5–3600 s) survives NAT idle-timeout on cloud providers.
- 🖱 **Mouse friendly** — drag-select to copy (OSC 52), wheel scrollback with preserved colors, click targets on every screen; the terminal tab title follows the current page (`ells-<alias>` while connected).
- 🔑 **Host-key TOFU** — the first connection is confirmed and recorded in `~/.ells/known_hosts` (OpenSSH-compatible; existing `~/.ssh/known_hosts` entries are honoured too), and a later key change blocks the session with a warning. `-y` / `ELLS_YES=1` auto-accepts first-seen keys for scripting, while key *changes* are still refused.
- 🪟 **Multi-session tabs** — one process, several servers: `F2` new tab, `F5`/`F6` or a mouse click to switch, `Ctrl-]` to close. Background tabs keep their own output, transfers and state. The host list shows which machines are already connected (`●`), and `Ctrl-G` drops you back to that list without touching the connection — Enter or a click on the tab returns.
- 🔍 **Scrollback search** — `F3` searches the history buffer, `n`/`N` jump between hits.
- 🛠 **Remote file operations** — `m` mkdir, `n` rename, `c` chmod (`644` / `0644` / `0o644` / `000` all parse; permission bits only, owner and mtime stay put), `D` delete (confirmed twice) inside the browser, `Ctrl-C` cancels in-flight transfers, and a dropped connection can be re-established straight from the vault — or let ells retry it on a backoff curve (see below).
- 🔄 **In-app updates** — ells checks for a new release at every start (one HEAD request, no anonymous API quota), shows a badge, and on confirm downloads the right asset, stream-verifies it against `SHA256SUMS.txt` and swaps itself in place. A checksum mismatch never touches the running binary.
- 📥 **Two-way `~/.ssh/config`** — `i` pulls your existing OpenSSH hosts in (only aliases ells doesn't have yet; hosts you already filled in are never overwritten). `x` goes the other way: it writes an ssh_config snippet to `~/.ells/ssh_config.export` containing alias / host / port / user / `IdentityFile` / `ProxyJump` / forwards — **never a password** — and it does not touch your `~/.ssh/config`.
- 🧭 **Groups · tags · favorites · fuzzy filter** — the list draws one section per group (`▾ prod (3)`), sorted by group → favorites inside the group → recently used → alias; `o` cycles four sort modes (default · recent · alias · group) and remembers the choice in `settings.ini`; `Space` folds the section under the cursor, `z` folds or unfolds everything; `/` then just type, matching alias, host, user, group, tag and note — the header shows `hits n/m` and the matched characters light up inside the alias; `f` stars a host, and every row ends with a right-aligned "last used" stamp (`now / 5m / 3h / yesterday / 5d / never`).
- 🔀 **Local port forwarding** — forward rules live in the host form (`-L 8080:127.0.0.1:5432`, `-D 1080`, or `-L db:5432` when you don't care which local port); one SSH connection carries every rule for that host. `t` opens the tunnel manager with live per-host state: `space`/`s` starts or stops, `Enter` opens a **table editor** (kind · bind · local port · destination host · destination port) — `p` on the host list opens the same table. Leave the local port **blank and the OS picks one** (same as `ssh -L 0:db:5432`), so two hosts reaching for 8080 no longer fight; `m` shows the **port map** with the ports actually bound (auto ones marked). When a fixed port really is taken, the failure names who holds it. The local listener is bound before dialing, so a dropped session doesn't take the tunnel down — it backoffs and reconnects on its own.
- 🔁 **Auto-reconnect** — after a drop you didn't cause, ells retries with exponential backoff (from 1s, doubling, capped at 60s, ±20% jitter). The status line says how long until the next try and which attempt it is; `Ctrl-]` cancels. A refused host key is never auto-retried — it stops and asks. What counts as "you caused it": typing `exit`/`logout` on the remote, the peer shutting the channel down the orderly way (bastions and gateways often close the channel without ever sending an exit status), or `Ctrl-]` — all of those end the session quietly. Only a dead link (TCP death, keepalive timeout) reconnects. The attempt limit is a settings row (`0` turns it off).
- 🖧 **Headless CLI** — work without the UI: `ells list`, `ells exec <alias> -- <cmd>`, `ells sftp ls/get/put/rm/chmod`, `ells tunnel <alias>` (forwards only, stays resident; `--all` starts every host that has rules), `ells export`, `ells completions bash`. Exit codes are a contract: 0 OK · 1 failure · 2 usage or safety gate · 124 timeout. One red line: no password, private key material or master password is ever printed. Feed the master password through `ELLS_MASTER` or a stdin pipe.
- 🪪 **Known-hosts panel** — `h` lists what ells remembers: entries in `~/.ells/known_hosts` can be deleted (next connect asks for the fingerprint again); `~/.ssh/known_hosts` is displayed read-only and never modified.
- 📜 **Audit log** — `l` reads `~/.ells/audit.log`: connects, disconnects, key trusted/changed, transfers, tunnels, vault saves, imports/exports, master-password changes (Beijing time, append-only, rotated past 1 MiB, never containing a password or a private key).
- 💾 **Session transcripts** — every connection saves the raw terminal output to `~/.ells/logs/<alias>-<timestamp>.log` (ANSI kept, so `less -R` still shows colors; the in-app `v` viewer strips control sequences). 8 MiB per file, 30 days of history, toggle in settings.
- 📊 **Remote host metrics** — a fixed bottom row on the session page with CPU / memory / disk bars and
  percentages (disk is the fullest real mount point, and the number is `df`'s own Capacity column, next to
  the mount point and used/total, plus the 1/5/15-minute load average whenever the row is wide enough for
  them — both of those notes are parked at the **end of the whole row**, never inside their own cell, so the
  three bars stay the same width and nothing looks "wider" for carrying text). The data comes from **the connection you already authenticated**:
  ells first reads `/proc/stat`, `/proc/meminfo`, `/proc/loadavg` and `/proc/mounts` over that connection's
  existing SFTP session and asks the disks via `statvfs@openssh.com` — local filesystems only, because one
  `statvfs` stuck on a dead NFS mount would wedge the whole SFTP session, transfers included. With no SFTP
  session, no extension support or nothing readable it falls back to a one-shot read-only `exec` (`df -Pk`),
  filling only the cells that are still blank. No second SSH connection, no touching the PTY you are typing
  into, no process left behind.
  The cadence is split: the `/proc` numbers refresh every 5 seconds and so does the disk cell — but a middle
  round only asks the one mount that was fullest at the last survey (a single request), while the survey itself
  (re-read `/proc/mounts`, ask every candidate, re-rank) runs once a minute: asking every disk is the expensive
  leg, and the mount list barely moves within a minute. On a server without `statvfs@openssh.com` a follow round
  asks nothing at all and keeps the previous number, and `df` only fills that cell at the 60-second survey
  point — never every 5 seconds. (The very first round is a survey, so nobody stares at an empty slot for a
  minute.) Tabs you are not
  looking for drop to one round a minute and re-collect immediately when you switch back to them.
  The first paint is not delayed either: the SFTP subsystem is already open during the handshake, so the very
  first round runs the moment you connect and memory, disk and load are on screen right away. CPU needs two
  snapshots to difference, so that cell arrives about two seconds later (the round that only brings a baseline
  hands over a short 2-second relay — a machine without `/proc/stat` never enters it, so nothing spins) and
  then follows the normal cadence. Machines without
  `/proc` (FreeBSD, slim containers) keep showing `—` there while disk stays real. A missing reading is
  drawn as `—`, never as `0%`; after three rounds that return nothing (about 15 seconds) the host is marked
  unsupported, polling stops and the row goes back to the terminal. A *transport* failure (timeout, channel
  refused, exec error) is different — it only backs the cadence off 5 → 10 → 30 → 60 seconds, and any round
  that brings data goes straight back to 5. Toggle in Settings (「主机指标」, `metrics=`).
- 🌏 **Chinese-first UI**, native OS file dialogs, zero telemetry.

## Install

**macOS / Linux** (bash / zsh):

```bash
curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/install.sh | sh
```

**Windows** (PowerShell):

```powershell
irm https://raw.githubusercontent.com/lg10/ells/main/install.ps1 | iex
```

Reopen your terminal afterwards, and both `ells` and the short command `s` are ready (the installer sets up PATH; on PowerShell it also registers an `s` function in your profile — cmd needs nothing extra). Both installers verify `SHA256SUMS.txt` and refuse to run on a mismatch.

### Pin a version / custom dir / mirror

macOS / Linux:

```bash
ELLS_VERSION=v0.2.1 curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/install.sh | sh
ELLS_INSTALL_DIR=/usr/local/bin curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/install.sh | sh
```

Windows PowerShell:

```powershell
$env:ELLS_VERSION="v0.2.1"; irm https://raw.githubusercontent.com/lg10/ells/main/install.ps1 | iex
$env:ELLS_INSTALL_DIR="$env:USERPROFILE\bin"; irm https://raw.githubusercontent.com/lg10/ells/main/install.ps1 | iex
```

`ELLS_API_URL` / `ELLS_DOWNLOAD_URL` point the release lookup and download at a mirror or an intranet. If installation misbehaves, run `sh diagnose.sh` — it prints the shell, curl, architecture and proxy decisions the installer is about to make.

### Updating in place

Once installed you rarely need the installer again. ells asks GitHub for a newer release in the background on **every start** — a single HEAD request whose redirected URL carries the version, so nothing is downloaded and no anonymous API quota is spent. When there is something newer, the host list grows a `【vX.Y.Z 可更新】` badge; click it (or press Enter on the 「自动更新」 row in settings) and the confirm dialog then does four things:

1. downloads the asset for your platform (`ells-linux-x86_64` / `ells-macos-universal` / `ells-windows-x86_64.exe`) into a temp file next to ells;
2. hashes it with SHA256 **while streaming**, and compares against the release's `SHA256SUMS.txt` — on a mismatch, or a short body, the temp file is deleted and **nothing is replaced**;
3. swaps the new binary in. A running image on Windows cannot be deleted but can be renamed, so `ells.exe` becomes `ells.exe.old` and the new file takes its place; your current session keeps running, and a failed swap is renamed back;
4. reports "the new version is in place" — the file on disk is new, the process is still old. Press Enter / 【立即重启】 to restart into it. The confirm dialog says up front how many live sessions that would drop.

`Esc` or 【取消下载】 aborts a download in progress and cleans the temp file up. The automatic check at startup fails **silently** (no network should not block you); only explicit checks and updates print a reason, in Chinese. The next start also sweeps leftover `.old` files and temp files older than a day.

- Prefer not to be checked: toggle 「自动更新」 off with `←` in settings (`auto_update=false` in `settings.ini`). You can still press Enter on that row to check by hand.
- Scripting and troubleshooting: `ells --check-update` prints the result and exits — no download, no TUI, no terminal state touched.
- The last check is cached in `~/.ells/update.cache` (a version and a timestamp, nothing about your hosts).
- Mirrors / intranet: `ELLS_DOWNLOAD_URL` (asset base) and `ELLS_API_URL` (release lookup) — the same contract the install scripts use.
- Platforms with no prebuilt package (e.g. Linux aarch64) are pointed at the install scripts instead of being overwritten with something that cannot run.

### Uninstall

```bash
curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/uninstall.sh | sh
# also delete ~/.ells (irreversible):
curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/uninstall.sh | sh -s -- --purge
```

```powershell
irm https://raw.githubusercontent.com/lg10/ells/main/uninstall.ps1 | iex
$env:ELLS_PURGE="1"; irm https://raw.githubusercontent.com/lg10/ells/main/uninstall.ps1 | iex
```

Uninstalling removes the binaries, the PATH entry and the PowerShell `s` function, but **keeps `~/.ells`** (vault, `known_hosts`, settings) — removing a binary must not destroy credentials. ells writes no registry keys and installs no services, so removing the `ells` / `s` binaries by hand is just as clean. Use `--purge` / `ELLS_PURGE=1` only once you are sure: a master password can never be recovered.

<details>
<summary>Build from source</summary>

```bash
cargo build --release -p ells
./target/release/ells            # host list
./target/release/ells myserver   # connect by alias directly
```

</details>

First run asks you to create a master password and a vault. Add a host with `a`, then press Enter to connect.

### Usage

```
ells [ALIAS] [--dev] [-y] [--check-update]
ells <subcommand> …    headless, for scripts and CI: list · exec · sftp · tunnel · export · completions

  ALIAS           connect to this host alias right after unlock
                  (the installed short command `s <alias>` does the same)
  --dev           load plaintext hosts from ~/.ells/hosts.dev.toml (development only,
                  nothing is persisted)
  -y, --yes       auto-accept and record a first-seen host key; key *changes* are
                  still refused
  --check-update  print whether a newer release exists and exit — no download, no TUI

Headless subcommands (run `ells <subcommand> --help` for every flag):

  ells list [--format table|json] [--filter text]
  ells exec <alias> [--tty] [--timeout sec] [--stdin] -- <remote command>
  ells sftp ls|get|put|rm|chmod <alias> …    overwrite needs --yes, dirs need --recursive
  ells tunnel <alias> | --all               just run the configured forwards, Ctrl-C stops
                                            every status line carries the local ports actually
                                            bound (auto-allocated ones marked)
  ells export [--out path]                  stdout by default

  Exit codes are part of the contract: 0 OK · 1 failure (connect, auth or host-key
  refusal, or the remote command returned non-zero) · 2 usage or safety gate (a missing
  --yes, with the reason in Chinese on stderr) · 124 --timeout hit. `ells exec` passes
  the remote command's own exit code through. One red line: no password, private key
  material or master password is ever printed — `--format json` only carries public
  metadata.

Environment: ELLS_LOG=1 logs to stderr; ELLS_ZMODEM_LOG=1 additionally writes
sz/rz interception diagnostics to ~/.ells/zmodem.log; ELLS_YES=1 == -y;
ELLS_MASTER feeds the master password to the headless subcommands (the first stdin
line works too when you pipe); ELLS_API_URL / ELLS_DOWNLOAD_URL point update checks
at a mirror or intranet.

Config lives in ~/.ells/: vault.bin (credentials), settings.ini, known_hosts,
audit.log (operation log), logs/ (session transcripts), ssh_config.export
(the `x` output), update.cache (last update check).
```

### Key bindings (essentials)

| Screen   | Keys |
|----------|------|
| Host list | `↑↓/j k` select · `Enter` connect (a host already connected just switches back to its tab) · `a` add (with the selected host as template: user/port/auth/key/jump/group/tags/forwards carry over, alias, hostname and passwords stay blank) · `e` edit · `d` delete (with confirm) · `s` settings · `i` import `~/.ssh/config` · `x` export ssh_config (never a password) · `p` edit the selected host's forward rules as a table · `m` port map · `/` filter (header shows `hits n/m`, matched letters light up) · `f` favorite · `Space` fold/unfold the current group · `z` fold or unfold every group · `o` cycle sort mode (default · recent · alias · group) · `t` tunnels · `h` known hosts · `l` audit log · `v` session transcripts · `?`/`F1` help · `q`/`Ctrl-C` quit; group headers read `▾ prod (3)`, rows are prefixed `●` connected / `○` dialing, each row ends with its last-used stamp, and the tab bar on top clicks you back into a session |
| Panels     | `t` tunnel manager: `space`/`s` start or stop the selected host · `Enter` edit its rules as a table · `m` port map (the button on top is clickable too) · `x` stop all (`▲` ready `◐` dialing/reconnecting `▼` failed) · `h` known keys: `d` forget an entry in `~/.ells/known_hosts` (`~/.ssh/known_hosts` is read-only and never touched) · `l` audit log: `↑↓`/`PgUp`/`PgDn` scroll `~/.ells/audit.log` · `v` session transcripts: `↑↓` pick a file, `Enter` read it, `d` delete it. All close with `Esc`/`q`/their own key |
| Rule table | Opened by `p` on the host list or `Enter` in the tunnel manager: `↑↓` rows · `←→`/`Tab` cells (on the first column `←→` cycles the kind `L`/`D`/`R`) · `Ctrl-N` add row · `Ctrl-D` delete row · `Ctrl-C` clear the cell · `Enter` save · `Esc` discard everything; an empty **local port** cell means "let the OS pick one", and a row that collides with another rule gets a `⚠` plus a line naming which host shares that port. Saving while the tunnel runs restarts it with the new rules |
| Port map   | `m` (host list or tunnel manager): four columns — `host / rule / actually listening / state`. The ports an auto-allocated rule landed on are only visible here (marked `（自动）`); when two hosts want the same fixed port the button on the tunnel manager turns into `⚠ 8080 shared by several hosts`. Close with `m`/`Esc`/`q` |
| Tabs      | `F2` new tab · `F5` next · `F6` previous · click the tab bar to switch, `+` to create · `Ctrl-]` close current tab (back to the list when it is the only one; press twice while a transfer runs) · `Ctrl-G` hop to the host list without dropping the connection, press again to return |
| Session   | any key → remote · `Ctrl-Q` embedded/passthrough · `Ctrl-S` SFTP browser · `Ctrl-L` redraw · `Ctrl-]` close tab · `Ctrl-G` host list (stays connected) · wheel = scrollback · drag = select & copy (OSC 52) · `F3` search scrollback (`/` works while scrolled, `n`/`N` step through hits) · `F1` help; the fixed bottom row shows the remote host's CPU/memory/disk percentages (auto-hidden on hosts that yield nothing) |
| Browser   | `Enter` open/download · `u` upload file · `U` upload a whole directory · `d` download · `m` mkdir · `n` rename · `c` chmod (octal `600`/`0644`, permission bits only) · `D` delete (recursive, confirmed) · `Ctrl-C` cancel all transfers · `r` refresh · `Backspace` up · `Esc` back · the tab bar up top works here too |
| Form      | `Tab/↑↓` move · `←→` switch auth method · `Ctrl-F` pick private key · `Ctrl-J` pick bastion host · `Ctrl-R` reveal the password / key passphrase while the cursor is on it · `Enter` on those two fields opens the picker directly · save via the **Save** button or Enter when focused; the three `＊` fields (alias, host, user) are required and a rejected save names only what is actually missing; while the cursor sits on 「分组」 the line below lists every existing group with its host count — type a known name to join it, a new one to create it; 「标签」 comma-separated tags the `/` filter can hit, 「转发」 holds ssh-style `-L`/`-D` rules separated by spaces (a rule may drop its local port: `-L db:5432`); rather than counting spaces, cancel the form and press `p` on the host list to fill the same rules cell by cell |
| Dialogs   | `←→/Tab` switch option · `Enter` confirm · `Esc` cancel |

`?` (list / browser) and `F1` (session) open the full in-app help page.

The tab and session chords above (`F2`/`F5`/`F6`/`Ctrl-]`/`Ctrl-G`/`Ctrl-S`/`Ctrl-Q`/`Ctrl-L`/`F3`) are defaults
only. Remap them from the host list via `s` → settings → 「快捷键设置」 (keybindings): focus a row, press
Enter, then hit the new key. F2–F9 and Ctrl/Alt combinations are accepted — `F1` stays on the help page.
A chord already taken by another action swaps the two, and every change is written to
`~/.ells/settings.ini` right away.

Chords are normalized to the same byte on all three platforms: terminals on macOS and Linux send `Ctrl-]` as
the control byte crossterm reports as `Ctrl-5`, so ells treats the two as one binding and the defaults now fire
on Windows, macOS and Linux alike. `Ctrl+digits`, `Ctrl+Space` and `Ctrl-/` are rejected because they collide
with other bytes or with Windows Terminal's own shortcuts, and the panel footer prints the caveats for the
platform you are running on.

Colors are a setting too: the host list `s` → settings → 「界面主题」 cycles four palettes with `Enter` or `←→` —
**跟随终端 / follow-terminal** (ells paints
no background at all and reuses your terminal's own theme — fits macOS Terminal, iTerm2, WezTerm and Windows
Terminal alike, and **the default on every platform**), **dark** (painted black/dark-gray backgrounds, a safe
fallback when you don't know what the terminal's palette is), **高对比 / high contrast** (no small gray text;
hierarchy comes from bold and
reverse) and **浅色底 / light** (dark text for light-background terminals). A switch previews immediately and is
only written to `theme=` in `~/.ells/settings.ini` when you hit 【保 存】; 【取 消】 reverts the preview.

Three more settings rows decide what you can look up afterwards, whether ells keeps the bottom row for
itself, and whether a drop heals itself:

- **会话记录 / session transcripts** — `Enter` toggles writing `~/.ells/logs` files (`session_log=`,
  on by default). Turning it off only affects connections made afterwards; existing transcripts stay
  readable in `v` and are still pruned after 30 days.
- **主机指标 / host metrics** — `Enter` toggles the CPU/memory/disk row at the bottom of the session
  page (`metrics=`, on by default). Off — or a host that yields nothing — gives that row back to the
  terminal: viewport height, the PTY `SIGWINCH` and mouse hit-testing all read the same row count, so
  the remote screen is never clipped by one row. Turning it on retro-arms sessions that are already up.
- **自动重连 / auto-reconnect** — `Enter` cycles `off → 1 → 2 → 3 → 5 → unlimited`, `←→` steps;
  written as `reconnect_attempts=` (`0` = ask me each time like before, `9999` is shown as unlimited).
  The backoff curve itself is tuned in `settings.ini`: `reconnect_initial_ms` (first wait, default
  1000), `reconnect_max_ms` (ceiling, default 60000), `reconnect_stable_secs` (how long a session has
  to survive before the next drop restarts the ladder at attempt 1, default 30), `reconnect_jitter`
  (0–0.5, default 0.2 — it only ever pushes a retry later, never earlier, so a fleet of connections
  doesn't retry in the same second). While a retry is queued the status line reads
  "reconnecting in N s (attempt n/m) · Ctrl-] to stop". Two cases never auto-retry: a session you ended
  yourself, and a refused host key — that is a security event, so it stops and asks.

## Building from source

Requires **Rust 1.85+** (edition 2024).

```bash
cargo build --release -p ells   # binary at target/release/ells(.exe)
cargo test                      # unit tests (vault crypto, zmodem, highlight…)
```

On Linux, install the system packages `pkg-config libwayland-dev` first (rfd's XDG-portal stack links against wayland). Prebuilt binaries / the one-line installer need none of this.

Local integration testing uses `tests/fake_sshd.py`, a paramiko-based throwaway SSH/SFTP server (Python 3 + `pip install paramiko`).

### Platform notes

| Platform | Status | Notes |
|----------|--------|-------|
| Windows 10/11 (x86_64) | ✅ primary dev target | Use **Windows Terminal**: legacy conhost has poor mouse / OSC 52 support |
| macOS (x86_64 / Apple Silicon) | ✅ prebuilt universal binary | The one-line installer picks the right build. Prefer **iTerm2** — Terminal.app ignores OSC 52 clipboard |
| Linux (x86_64 / aarch64) | ✅ prebuilt x86_64 binary | Native file dialogs need a desktop session (X11/Wayland); headless TTY can't open pickers |

Nothing in the codebase is platform-specific by design (`ring` crypto backend, pure-Rust terminal stack), so CI cross-builds all four targets — Linux x86_64, Windows x86_64, and macOS both aarch64 and x86_64, lipo'd into one universal binary.

## Project layout

```
crates/
  ells-core        SSH (russh) client, vault crypto, host model, keepalive, host-key
                   TOFU, port forwarding + tunnel manager, ssh_config both directions,
                   audit log, session transcripts, atomic writes, reconnect backoff,
                   fuzzy filter
  ells-term        vt100 emulator wrapper (screen, scrollback, DSR/DA answers)
  ells-transfer    SFTP upload/download engine with throttled progress + chmod parsing
  ells             the TUI application and headless CLI (ratatui + crossterm): UI,
                   zmodem interception, settings, keybindings
install.sh / install.ps1      one-line install: latest release, SHA256 verify,
                              PATH setup, `s` registration
uninstall.sh / uninstall.ps1  removal; keeps ~/.ells unless --purge / ELLS_PURGE=1
diagnose.sh        one-shot install-environment report (shell, curl, arch, proxy)
scripts/smoke.*    build the debug binary, seed hosts.dev.toml, launch the UI
tools/             repo self-checks (e.g. the bash-3.2 expansion-adjacency lint)
tests/fake_sshd.py headless SSH+SFTP+ZMODEM test server
.github/workflows/ ci.yml (3-platform build + test + install smoke),
                   release.yml (tag → cross-platform artifacts + notes from CHANGELOG)
```

## Roadmap

- `-R` remote port forwarding (such rules are stored, editable in the table and exported to
  `ssh_config`, but ells refuses to start them with an explicit "not supported" instead of
  silently doing nothing).
- Multi-select and batch queues in the SFTP browser.

The headless CLI (`list` / `exec` / `sftp` / `tunnel` / `export` / `completions`), local
port forwarding with a tunnel manager, the rule table editor and the port map,
auto-allocated local ports (leave one blank and two hosts stop fighting over it),
groups · tags · favorites · fuzzy filter, the audit
log and session transcripts, the known-hosts panel, remote `chmod`, auto-reconnect backoff,
host-key TOFU, ssh-agent/Pageant authentication, prebuilt packages for all three platforms
and multi-session tabs have all landed; per-release details live in [CHANGELOG.md](CHANGELOG.md).

## Contributing & security

- Bugs and feature requests use the repo's [issue templates](.github/ISSUE_TEMPLATE) —
  please update to the latest release first, and scrub hostnames, paths and usernames
  from any log you attach.
- **Security vulnerabilities go through a private report**, never a public issue:
  see [SECURITY.md](SECURITY.md).

## License

ells is released under the [MIT License](LICENSE).

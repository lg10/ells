[简体中文](README_zh.md) | **English**

<div align="center">

# ells

**A lightweight, cross-platform SSH client that lives in your terminal.**

`Rust` `TUI` `SSH` `SFTP` `ZMODEM`

Homepage: [https://ells.cn](https://ells.cn) · Repo: [GitHub](https://github.com/lg10/ells) · [中文 README](README_zh.md) · [CHANGELOG](CHANGELOG.md) · [SECURITY](SECURITY.md)

![CI](https://github.com/lg10/ells/actions/workflows/ci.yml/badge.svg) ![license](https://img.shields.io/badge/license-MIT-green) ![rust](https://img.shields.io/badge/rust-1.85%2B-orange) ![platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue) [![release](https://img.shields.io/github/v/release/lg10/ells?label=release&color=blue)](https://github.com/lg10/ells/releases) ![downloads](https://img.shields.io/github/downloads/lg10/ells/total?label=downloads) [![changelog](https://img.shields.io/badge/changelog-keep%20a%20changelog-informational)](CHANGELOG.md) [![security](https://img.shields.io/badge/security-policy-red)](SECURITY.md)

</div>

> ⚠️ **Early access.** ells is in active development (v0.1.x). Expect bugs and rough edges; feedback via [Issues](https://github.com/lg10/ells/issues) is very welcome.

## What is ells

ells is an all-in-one terminal SSH client written in pure Rust. It embeds a real terminal emulator (vt100) inside a fast TUI, so remote sessions keep their colors and programs like `vim`/`htop` render correctly — while adding the workflow features a plain `ssh` lacks: an encrypted credential vault, SFTP file browsing, and Xshell-style `sz`/`rz` transfer interception, all driven with a few keystrokes or the mouse.

## Features

- 🔐 **Encrypted vault** — hosts and passwords stored in `~/.ells/vault.bin`, protected by a master password (Argon2id KDF + XChaCha20Poly1305 AEAD). Optional auto-unlock, in-app master-password change.
- 🗂 **Host manager** — aliases, per-host auth (password / private key / agent on the roadmap), notes, and one-alias-per-line **ProxyJump bastion** support (`ells` → jump → target, loop detection included).
- ⌨️ **Auth formats that just work** — PKCS#8, PKCS#1 RSA (`BEGIN RSA PRIVATE KEY`), OpenSSH keys, and legacy **DEK-Info / 3DES-encrypted PEM** decrypted natively.
- 🖥 **Embedded terminal** — vt100 emulator with full ANSI/256/true-color output; `Ctrl-Q` toggles a raw **passthrough mode** as an escape hatch for anything the emulator can't handle; `Ctrl-L` redraws.
- 📦 **`sz` / `rz` interception** — ells watches the remote output stream for ZMODEM handshakes (protocol-level detection tuned against real `lrzsz` byte streams) and reroutes them over the SFTP channel with progress bars and native Save-As / file-picker dialogs. No ZMODEM daemon required.
- 📁 **SFTP browser** — `Ctrl-S` opens a remote file browser: Enter to enter/download, `u` upload, `d` download, mouse click + wheel supported, aggregated transfer progress.
- 🎨 **Output highlighting** — `docker ps`, `kubectl`, log levels (ERROR/WARN/INFO/...), HTTP methods, IPv4 addresses and percentages are colorized on the fly — without overriding programs' own ANSI colors.
- 🔌 **Keepalive** — configurable SSH keepalive interval (15–300 s) survives NAT idle-timeout on cloud providers.
- 🖱 **Mouse friendly** — drag-select to copy (OSC 52), wheel scrollback with preserved colors, click targets on every screen; the terminal tab title follows the current page (`ells-<alias>` while connected).
- 🔑 **Host-key TOFU** — the first connection is confirmed and recorded in `~/.ells/known_hosts` (OpenSSH-compatible; existing `~/.ssh/known_hosts` entries are honoured too), and a later key change blocks the session with a warning. `-y` / `ELLS_YES=1` auto-accepts first-seen keys for scripting, while key *changes* are still refused.
- 🪟 **Multi-session tabs** — one process, several servers: `F2` new tab, `F5`/`F6` or a mouse click to switch, `Ctrl-]` to close. Background tabs keep their own output, transfers and state. The host list shows which machines are already connected (`●`), and `Ctrl-G` drops you back to that list without touching the connection — Enter or a click on the tab returns.
- 🔍 **Scrollback search** — `F3` searches the history buffer, `n`/`N` jump between hits.
- 🛠 **Remote file operations** — `m` mkdir, `n` rename, `D` delete (confirmed twice) inside the browser, `Ctrl-C` cancels in-flight transfers, and a dropped connection can be re-established straight from the vault.
- 🔄 **In-app updates** — ells checks for a new release at every start (one HEAD request, no anonymous API quota), shows a badge, and on confirm downloads the right asset, stream-verifies it against `SHA256SUMS.txt` and swaps itself in place. A checksum mismatch never touches the running binary.
- 📥 **`~/.ssh/config` import** — press `i` in the host list to pull in your existing OpenSSH hosts.
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
ELLS_VERSION=v0.1.5 curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/install.sh | sh
ELLS_INSTALL_DIR=/usr/local/bin curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/install.sh | sh
```

Windows PowerShell:

```powershell
$env:ELLS_VERSION="v0.1.5"; irm https://raw.githubusercontent.com/lg10/ells/main/install.ps1 | iex
$env:ELLS_INSTALL_DIR="$env:USERPROFILE\bin"; irm https://raw.githubusercontent.com/lg10/ells/main/install.ps1 | iex
```

`ELLS_API_URL` / `ELLS_DOWNLOAD_URL` point the release lookup and download at a mirror or an intranet. If installation misbehaves, run `sh diagnose.sh` — it prints the shell, curl, architecture and proxy decisions the installer is about to make.

### Updating in place

Once installed you rarely need the installer again. ells asks GitHub for a newer release in the background on **every start** — a single HEAD request whose redirected URL carries the version, so nothing is downloaded and no anonymous API quota is spent. When there is something newer, the host list grows a `【v0.1.6 可更新】` badge; click it (or press Enter on the 「自动更新」 row in settings) and the confirm dialog then does four things:

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

  ALIAS           connect to this host alias right after unlock
                  (the installed short command `s <alias>` does the same)
  --dev           load plaintext hosts from ~/.ells/hosts.dev.toml (development only,
                  nothing is persisted)
  -y, --yes       auto-accept and record a first-seen host key; key *changes* are
                  still refused
  --check-update  print whether a newer release exists and exit — no download, no TUI

Environment: ELLS_LOG=1 logs to stderr; ELLS_ZMODEM_LOG=1 additionally writes
sz/rz interception diagnostics to ~/.ells/zmodem.log; ELLS_YES=1 == -y;
ELLS_API_URL / ELLS_DOWNLOAD_URL point update checks at a mirror or intranet.

Config lives in ~/.ells/: vault.bin (credentials), settings.ini, known_hosts,
update.cache (last update check).
```

### Key bindings (essentials)

| Screen   | Keys |
|----------|------|
| Host list | `↑↓/j k` select · `Enter` connect (a host already connected just switches back to its tab) · `a` add · `e` edit · `d` delete (with confirm) · `s` settings · `i` import `~/.ssh/config` · `?`/`F1` help · `q`/`Ctrl-C` quit; rows are prefixed `●` connected / `○` dialing, and the tab bar on top clicks you back into a session |
| Tabs      | `F2` new tab · `F5` next · `F6` previous · click the tab bar to switch, `+` to create · `Ctrl-]` close current tab (back to the list when it is the only one; press twice while a transfer runs) · `Ctrl-G` hop to the host list without dropping the connection, press again to return |
| Session   | any key → remote · `Ctrl-Q` embedded/passthrough · `Ctrl-S` SFTP browser · `Ctrl-L` redraw · `Ctrl-]` close tab · `Ctrl-G` host list (stays connected) · wheel = scrollback · drag = select & copy (OSC 52) · `F3` search scrollback (`/` works while scrolled, `n`/`N` step through hits) · `F1` help |
| Browser   | `Enter` open/download · `u` upload file · `U` upload a whole directory · `d` download · `m` mkdir · `n` rename · `D` delete (recursive, confirmed) · `Ctrl-C` cancel all transfers · `r` refresh · `Backspace` up · `Esc` back · the tab bar up top works here too |
| Form      | `Tab/↑↓` move · `←→` switch auth method · `Ctrl-F` pick private key · `Ctrl-J` pick bastion host · `Enter` on those two fields opens the picker directly · save via the **Save** button or Enter when focused |
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
**dark** (painted black/dark-gray backgrounds, the look on Windows), **跟随终端 / follow-terminal** (ells paints
no background at all and reuses your terminal's own theme — recommended for macOS Terminal, iTerm2 and WezTerm,
and the default on macOS), **高对比 / high contrast** (no small gray text; hierarchy comes from bold and
reverse) and **浅色底 / light** (dark text for light-background terminals). A switch previews immediately and is
only written to `theme=` in `~/.ells/settings.ini` when you hit 【保 存】; 【取 消】 reverts the preview.

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
  ells-core        SSH (russh) client, vault crypto, host model, keepalive
  ells-term        vt100 emulator wrapper (screen, scrollback, DSR/DA answers)
  ells-transfer    SFTP upload/download engine with throttled progress
  ells             the TUI application (ratatui + crossterm): UI, zmodem
                   interception, settings, keybindings
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

- ssh-agent authentication (password and private-key auth, incl. key passphrases,
  already work)

Host-key TOFU, prebuilt packages for all three platforms and multi-session tabs
have landed; per-release details live in [CHANGELOG.md](CHANGELOG.md).

## Contributing & security

- Bugs and feature requests use the repo's [issue templates](.github/ISSUE_TEMPLATE) —
  please update to the latest release first, and scrub hostnames, paths and usernames
  from any log you attach.
- **Security vulnerabilities go through a private report**, never a public issue:
  see [SECURITY.md](SECURITY.md).

## License

ells is released under the [MIT License](LICENSE).

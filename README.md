[简体中文](README_zh.md) | **English**

<div align="center">

# ells

**A lightweight, cross-platform SSH client that lives in your terminal.**

`Rust` `TUI` `SSH` `SFTP` `ZMODEM`

Homepage: [https://ells.cn](https://ells.cn) · Repo: [GitHub](https://github.com/lg10/ells)

![CI](https://github.com/lg10/ells/actions/workflows/ci.yml/badge.svg) ![license](https://img.shields.io/badge/license-MIT-green) ![rust](https://img.shields.io/badge/rust-edition%202024-orange) ![platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue)

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

Reopen your terminal afterwards — both `ells` and the short command `s` will be available.

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
ells [ALIAS] [--dev]

  ALIAS   connect to this host alias right after unlock
  --dev   load plaintext hosts from ~/.ells/hosts.dev.toml (development only,
          nothing is persisted)
```

### Key bindings (essentials)

| Screen   | Keys |
|----------|------|
| Host list | `↑↓/j k` select · `Enter` connect · `a` add · `e` edit · `d` delete (with confirm) · `s` settings · `q` quit |
| Session   | any key → remote · `Ctrl-Q` embedded/passthrough · `Ctrl-S` SFTP browser · `Ctrl-L` redraw · `Ctrl-]` detach back to list · wheel = scrollback · drag = select & copy |
| Browser   | `Enter` open/download · `u` upload · `d` download · `Backspace` up · `r` refresh · `Esc` back |
| Form      | `Tab/↑↓` move · `Ctrl-F` pick private key · `Ctrl-J` pick bastion host · save only via the **Save** button (Enter or click) |

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
| macOS (x86_64 / Apple Silicon) | ✅ supported in code | Build on the Mac. Prefer **iTerm2** — Terminal.app ignores OSC 52 clipboard |
| Linux (x86_64 / aarch64) | ✅ supported in code | Native file dialogs need a desktop session (X11/Wayland); headless TTY can't open pickers |

Cross-target builds (ARM / Mac) are on the roadmap; nothing in the codebase is platform-specific by design (`ring` crypto backend, pure-Rust terminal stack).

## Project layout

```
crates/
  ells-core        SSH (russh) client, vault crypto, host model, keepalive
  ells-term        vt100 emulator wrapper (screen, scrollback, DSR/DA answers)
  ells-transfer    SFTP upload/download engine with throttled progress
  ells             the TUI application (ratatui + crossterm): UI, zmodem
                   interception, settings, keybindings
tests/fake_sshd.py headless SSH+SFTP+ZMODEM test server
```

## Roadmap

- ssh-agent authentication
- TOFU host-key verification prompts
- prebuilt binaries for Windows / macOS (x86_64 + arm64) / Linux

## License

ells is released under the [MIT License](LICENSE).

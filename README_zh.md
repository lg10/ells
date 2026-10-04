**简体中文** | [English](README.md)

<div align="center">

# ells

**一个活在终端里的轻量级跨平台 SSH 客户端。**

`Rust` `TUI` `SSH` `SFTP` `ZMODEM`

官网：[https://ells.cn](https://ells.cn) · 仓库：[Gitee](https://gitee.com/lg10/ells)

![license](https://img.shields.io/badge/license-MIT-green) ![rust](https://img.shields.io/badge/rust-edition%202024-orange) ![platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue)

</div>

> ⚠️ **早期版本。** ells 处于活跃开发阶段（v0.1.x），可能存在 Bug、功能尚不完善。欢迎通过 [Issue](https://gitee.com/lg10/ells/issues) 反馈问题。

## 什么是 ells

ells 是一个纯 Rust 编写的一体化终端 SSH 客户端。它在高速 TUI 中内嵌了一个真正的终端模拟器（vt100），远程会话的颜色、`vim`/`htop` 等全屏程序都能正确渲染；同时补齐了原生 `ssh` 命令缺少的工作流能力：加密凭据保险库、SFTP 文件浏览、以及 Xshell 风格的 `sz`/`rz` 传输拦截——全部只需几个按键或一次鼠标点击。

## 功能特性

- 🔐 **加密保险库** — 主机与密码存于 `~/.ells/vault.bin`，由主密码保护（Argon2id 派生 + XChaCha20Poly1305 认证加密）；支持免密自动解锁、应用内修改主密码。
- 🗂 **主机管理** — 别名、按主机的认证方式（密码 / 私钥，agent 规划中）、备注，以及 **跳板机（ProxyJump）**：`ells` → 跳板 → 目标，内置循环检测。
- ⌨️ **广泛兼容的私钥格式** — PKCS#8、PKCS#1 RSA（`BEGIN RSA PRIVATE KEY`）、OpenSSH 格式，以及旧式 **DEK-Info / 3DES 加密 PEM**（ells 原生解密，无需转换）。
- 🖥 **内嵌终端** — vt100 模拟器完整支持 ANSI/256 色/真彩色；`Ctrl-Q` 一键切换**直通模式**作为兜底逃生门；`Ctrl-L` 重绘。
- 📦 **`sz` / `rz` 拦截** — ells 在远端输出流做协议级 ZMODEM 握手检测（针对真实 `lrzsz` 字节流反复调校），自动改走 SFTP 通道传输：进度条、系统原生"另存为/选择文件"对话框，远端无需 ZMODEM 服务端配合。
- 📁 **SFTP 文件浏览器** — 会话内 `Ctrl-S` 打开：`Enter` 进入/下载、`u` 上传、`d` 下载，支持鼠标点击与滚轮，顶部聚合进度条 + 传输详情弹窗。
- 🎨 **输出高亮** — `docker ps`、`kubectl`、日志级别（ERROR/WARN/INFO…）、HTTP 方法、IPv4 地址、百分比即时着色，且绝不覆盖程序自己发出的 ANSI 颜色。
- 🔌 **空闲保活** — 可配置 SSH keepalive（15–300 秒），扛住云服务器 NAT 空闲断连。
- 🖱 **鼠标友好** — 拖选复制（OSC 52）、滚轮回看历史且保留颜色、各界面元素可点击；终端标签页标题随页面联动（连接后显示 `ells-别名`）。
- 🌏 **全中文界面**、系统原生文件对话框、零遥测。

## 快速开始

```bash
cargo build --release -p ells
./target/release/ells            # 主机列表
./target/release/ells myserver   # 按别名直接连接
```

首次运行会引导你设置主密码并创建保险库；按 `a` 添加主机，回车即可连接。

### 用法

```
ells [ALIAS] [--dev]

  ALIAS   解锁后立即连接该别名对应的主机
  --dev   从 ~/.ells/hosts.dev.toml 读取明文主机表（仅开发调试，不做持久化）
```

### 快捷键一览

| 界面 | 按键 |
|------|------|
| 主机列表 | `↑↓/j k` 选择 · `Enter` 连接 · `a` 新增 · `e` 编辑 · `d` 删除（二级确认） · `s` 设置 · `q` 退出 |
| 会话 | 任意按键直达远端 · `Ctrl-Q` 内嵌/直通切换 · `Ctrl-S` 打开 SFTP 浏览器 · `Ctrl-L` 重绘 · `Ctrl-]` 断开会话返回列表 · 滚轮=回看历史 · 拖选=复制 |
| 文件浏览器 | `Enter` 进入目录/下载 · `u` 上传 · `d` 下载 · `Backspace` 上级 · `r` 刷新 · `Esc` 返回终端 |
| 主机表单 | `Tab/↑↓` 切换输入项 · `Ctrl-F` 选择私钥 · `Ctrl-J` 选择跳板机 · 仅通过【保 存】按钮（回车或点击）提交 |

## 从源码构建

需要 **Rust 1.85+**（edition 2024）。

```bash
cargo build --release -p ells   # 产物在 target/release/ells(.exe)
cargo test                      # 单元测试（保险库加密、zmodem 检测、高亮规则…）
```

本地集成测试依赖 `tests/fake_sshd.py`——一个基于 paramiko 的一次性 SSH/SFTP/ZMODEM 测试服务器（`pip install paramiko`）。

### 平台说明

| 平台 | 状态 | 说明 |
|------|------|------|
| Windows 10/11 (x86_64) | ✅ 主力开发平台 | 建议使用 **Windows Terminal**：旧版 conhost 对鼠标 / OSC 52 支持不佳 |
| macOS (x86_64 / Apple Silicon) | ✅ 代码已支持 | 需在 Mac 上构建；推荐 **iTerm2**（系统自带终端不支持 OSC 52 剪贴板） |
| Linux (x86_64 / aarch64) | ✅ 代码已支持 | 原生文件对话框需要桌面会话（X11/Wayland），纯无头 TTY 无法弹出 |

代码库不含平台特定实现（crypto 用 `ring`、终端栈全 Rust），交叉编译产物在规划中。

## 项目结构

```
crates/
  ells-core        SSH（russh）客户端、保险库加密、主机模型、keepalive
  ells-term        vt100 终端模拟器封装（屏幕、回看缓冲、DSR/DA 应答）
  ells-transfer    SFTP 上传/下载引擎（节流进度回调）
  ells             TUI 应用本体（ratatui + crossterm）：界面、ZMODEM
                   拦截、设置、键位处理
tests/fake_sshd.py 无头 SSH+SFTP+ZMODEM 测试服务器
```

## 路线图

- ssh-agent 认证
- TOFU 主机密钥校验提示
- Windows / macOS（x86_64 + arm64）/ Linux 预编译包

## 许可证

ells 以 [MIT 许可证](LICENSE) 开源。

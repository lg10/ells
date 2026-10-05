**简体中文** | [English](README.md)

<div align="center">

# ells

**一个活在终端里的轻量级跨平台 SSH 客户端。**

`Rust` `TUI` `SSH` `SFTP` `ZMODEM`

官网：[https://ells.cn](https://ells.cn) · 仓库：[GitHub](https://github.com/lg10/ells) · [English README](README.md) · 更新日志：[CHANGELOG](CHANGELOG.md) · 安全策略：[SECURITY](SECURITY.md)

![CI](https://github.com/lg10/ells/actions/workflows/ci.yml/badge.svg) ![license](https://img.shields.io/badge/license-MIT-green) ![rust](https://img.shields.io/badge/rust-1.85%2B-orange) ![platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue) [![release](https://img.shields.io/github/v/release/lg10/ells?label=release&color=blue)](https://github.com/lg10/ells/releases) ![downloads](https://img.shields.io/github/downloads/lg10/ells/total?label=downloads) [![changelog](https://img.shields.io/badge/changelog-keep%20a%20changelog-informational)](CHANGELOG.md) [![security](https://img.shields.io/badge/security-policy-red)](SECURITY.md)

</div>

> ⚠️ **早期版本。** ells 处于活跃开发阶段（v0.1.x），可能存在 Bug、功能尚不完善。欢迎通过 [Issue](https://github.com/lg10/ells/issues) 反馈问题。

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
- 🔑 **主机密钥 TOFU** — 首次连接确认并记入 `~/.ells/known_hosts`（OpenSSH 兼容格式，也读 `~/.ssh/known_hosts`），密钥变化时阻断告警；批量场景用 `-y` 自动接受首次密钥，密钥变更仍然拒绝。
- 🪟 **多会话标签页** — 一个进程同时挂多台服务器：`F2` 新建、`F5`/`F6` 或鼠标点标签切换、`Ctrl-]` 关闭；后台标签的输出与传输继续跑，切回来还在。主机列表会标出哪几台已经连着（`●`），`Ctrl-G` 保持连接回到列表、Enter 或点标签即切回那一格。
- 🔍 **回看搜索** — `F3` 在历史输出里查关键词，`n`/`N` 跳上下条。
- 🛠 **远端目录操作** — 浏览器内 `m` 新建目录、`n` 重命名、`D` 删除（二级确认），`Ctrl-C` 取消在途传输；断线后可一键用保险库里的凭据重连。
- 📥 **`~/.ssh/config` 导入** — 主机列表按 `i` 一键把已有的 OpenSSH 主机搬进来。
- 🌏 **全中文界面**、系统原生文件对话框、零遥测。

## 安装

**macOS / Linux**（bash / zsh）：

```bash
curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/install.sh | sh
```

**Windows**（PowerShell）：

```powershell
irm https://raw.githubusercontent.com/lg10/ells/main/install.ps1 | iex
```

安装完成后**重新打开终端**，`ells` 与短命令 `s` 即可直接使用（脚本会自动配置 PATH；PowerShell 下会在配置文件里注册 `s` 函数，cmd 无需额外处理）。脚本会强制校验 `SHA256SUMS.txt`，校验不过直接退出。

### 指定版本 / 安装目录 / 镜像

macOS / Linux：

```bash
ELLS_VERSION=v0.1.5 curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/install.sh | sh
ELLS_INSTALL_DIR=/usr/local/bin curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/install.sh | sh
```

Windows PowerShell：

```powershell
$env:ELLS_VERSION='v0.1.5'; irm https://raw.githubusercontent.com/lg10/ells/main/install.ps1 | iex
$env:ELLS_INSTALL_DIR="$env:USERPROFILE\bin"; irm https://raw.githubusercontent.com/lg10/ells/main/install.ps1 | iex
```

`ELLS_API_URL` / `ELLS_DOWNLOAD_URL` 可把发布查询与下载地址换成内网或镜像源。安装出问题时先跑 `sh diagnose.sh`，它会打印本机 shell / curl / 架构 / 代理等判定结果。

### 卸载

```bash
curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/uninstall.sh | sh
# 连 ~/.ells 一起删（不可恢复）：
curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/uninstall.sh | sh -s -- --purge
```

```powershell
irm https://raw.githubusercontent.com/lg10/ells/main/uninstall.ps1 | iex
$env:ELLS_PURGE='1'; irm https://raw.githubusercontent.com/lg10/ells/main/uninstall.ps1 | iex
```

卸载只清理二进制、PATH 条目和 PowerShell 里的 `s` 函数，**默认保留 `~/.ells`**（保险库、`known_hosts`、设置）——删二进制不该顺手毁掉凭据。ells 不写注册表，手工删掉安装目录里的 `ells` / `s` 也一样干净。确认不再需要这些主机时才用 `--purge` / `ELLS_PURGE=1`：主密码没有任何找回途径。

<details>
<summary>从源码构建</summary>

```bash
cargo build --release -p ells
./target/release/ells            # 主机列表
./target/release/ells myserver   # 按别名直接连接
```

</details>

首次运行会引导你设置主密码并创建保险库；按 `a` 添加主机，回车即可连接。

### 用法

```
ells [ALIAS] [--dev] [-y]

  ALIAS      解锁后立即连接该别名对应的主机（安装后的短命令 `s <别名>` 同义）
  --dev      从 ~/.ells/hosts.dev.toml 读取明文主机表（仅开发调试，不做持久化）
  -y, --yes  首次见到的主机密钥自动接受并记录；密钥变更时仍然拒绝

环境变量：ELLS_LOG=1 把内部日志写到 stderr；ELLS_ZMODEM_LOG=1 额外把 sz/rz
拦截诊断写入 ~/.ells/zmodem.log；ELLS_YES=1 等价于 -y。

配置文件都在 ~/.ells/：vault.bin（保险库）、settings.ini（设置）、
known_hosts（主机密钥记录）。
```

### 快捷键一览

| 界面 | 按键 |
|------|------|
| 主机列表 | `↑↓/j k` 选择 · `Enter` 连接（这台已连着就切回它那一格标签） · `a` 新增 · `e` 编辑 · `d` 删除（二级确认） · `s` 设置 · `i` 导入 `~/.ssh/config` · `?`/`F1` 帮助 · `q`/`Ctrl-C` 退出；行首 `●` 已连接 / `○` 正在连接，顶部标签条点一下就切回去 |
| 多标签 | `F2` 新建标签 · `F5` 下一个 · `F6` 上一个 · 鼠标点顶部标签条切换、点末尾 `+` 新建 · `Ctrl-]` 关闭当前标签（只剩一个时退回主机列表；有传输在跑时要按两次） · `Ctrl-G` 保持连接回到主机列表，再按一次回到原来那一页 |
| 会话 | 任意按键直达远端 · `Ctrl-Q` 内嵌/直通切换 · `Ctrl-S` 打开 SFTP 浏览器 · `Ctrl-L` 重绘 · `Ctrl-]` 关闭标签 · `Ctrl-G` 回主机列表（连接不断） · 滚轮=回看历史 · 拖选=复制（OSC 52） · `F3` 搜索历史输出（回看时按 `/` 同样可用，`n`/`N` 跳上下条） · `F1` 帮助 |
| 文件浏览器 | `Enter` 进入目录/下载 · `u` 上传文件 · `U` 上传整个目录 · `d` 下载 · `m` 新建目录 · `n` 重命名 · `D` 删除（递归，先确认） · `Ctrl-C` 取消全部在途传输 · `r` 刷新 · `Backspace` 上级 · `Esc` 返回终端 · 顶部标签条同样可点 |
| 主机表单 | `Tab/↑↓` 切换输入项 · `←→` 切换认证方式 · `Ctrl-F` 选私钥 · `Ctrl-J` 选跳板机 · `Enter` 在"私钥路径/跳板机"上直接打开选择器 · 保存点【保 存】按钮或回车 |
| 确认弹窗 | `←→/Tab` 切换选项 · `Enter` 确认 · `Esc` 取消 |

`?`（列表/浏览器）与 `F1`（会话）随时打开完整键位帮助页。

表里的标签与会话组合键（`F2`/`F5`/`F6`/`Ctrl-]`/`Ctrl-G`/`Ctrl-S`/`Ctrl-Q`/`Ctrl-L`/`F3`）都只是默认值，可在
「主机列表 `s` → 设置 → 快捷键设置」里改成自己的：选中一项按 Enter 录制，只接受 F2–F9 或带 Ctrl/Alt
的组合键，撞上已占用的键时两个动作自动互换，改完即时生效并写入 `~/.ells/settings.ini`。

这些组合键在三个平台都按同一个字节归一：mac/Linux 终端会把 `Ctrl-]` 发成与 `Ctrl-5` 相同的控制字符，
ells 现在认出它们是同一条快捷键，所以默认键在 Windows、macOS、Linux 上都能触发。面板底部会按当前平台
提示该终端自己的占用情况（Windows Terminal 的 `Ctrl+1–8` 与 `F11`；macOS 默认把功能键当媒体键，需按 `fn`
或在系统设置里改；把 Option 设为 Meta 才能用 `Alt` 组合；GNOME/KDE 终端占用 `F10`/`F11`）。因此可绑范围
收窄为 F2–F9，`Ctrl+数字`/`Ctrl+空格`/`Ctrl-/` 与同码键一律拒绝并给出中文原因。

界面配色也是可以选的：「主机列表 `s` → 设置 → 界面主题」按 Enter 或 `←→` 循环四套——
**深色**（画死黑底灰条，Windows 上最贴）、**跟随终端**（ells 一处底色都不画，底与正文色都用终端
自己的主题，macOS 终端 / iTerm2 / WezTerm 推荐，也是 mac 上的默认）、**高对比**（去掉灰色小字，
层级靠粗体与反显）、**浅色底**（给白底终端的深字配色）。改完立刻预览，点【保 存】才写入 `settings.ini`
的 `theme=`，【取消】还原。mac 上觉得"灰底/黑块和终端底色不搭"就换「跟随终端」。

## 从源码构建

需要 **Rust 1.85+**（edition 2024）。

```bash
cargo build --release -p ells   # 产物在 target/release/ells(.exe)
cargo test                      # 单元测试（保险库加密、zmodem 检测、高亮规则…）
```

Linux 源码构建需先安装系统依赖 `pkg-config libwayland-dev`（文件选择对话框 rfd 的 XDG 门户链路依赖 wayland）；预编译二进制/一行安装无需此步骤。

本地集成测试依赖 `tests/fake_sshd.py`——一个基于 paramiko 的一次性 SSH/SFTP/ZMODEM 测试服务器（`pip install paramiko`）。

### 平台说明

| 平台 | 状态 | 说明 |
|------|------|------|
| Windows 10/11 (x86_64) | ✅ 主力开发平台 | 建议使用 **Windows Terminal**：旧版 conhost 对鼠标 / OSC 52 支持不佳 |
| macOS (x86_64 / Apple Silicon) | ✅ 预编译通用二进制 | 一行安装脚本会自动挑对架构；推荐 **iTerm2**（系统自带终端不支持 OSC 52 剪贴板） |
| Linux (x86_64 / aarch64) | ✅ 预编译 x86_64 二进制 | 原生文件对话框需要桌面会话（X11/Wayland），纯无头 TTY 无法弹出 |

代码库不含平台特定实现（crypto 统一用 `ring`、终端栈纯 Rust），CI 按 tag 交叉产出
四个目标：Linux x86_64、Windows x86_64、macOS aarch64 + x86_64（lipo 合成通用二进制）。

## 项目结构

```
crates/
  ells-core        SSH（russh）客户端、保险库加密、主机模型、keepalive
  ells-term        vt100 终端模拟器封装（屏幕、回看缓冲、DSR/DA 应答）
  ells-transfer    SFTP 上传/下载引擎（节流进度回调）
  ells             TUI 应用本体（ratatui + crossterm）：界面、ZMODEM
                   拦截、设置、键位处理
install.sh / install.ps1      一行安装：查最新版、SHA256 校验、配 PATH、注册 s
uninstall.sh / uninstall.ps1  卸载：默认保留 ~/.ells，--purge / ELLS_PURGE=1 才删
diagnose.sh        安装环境一键诊断（shell / curl / 架构 / 代理）
scripts/smoke.*    本地一键冒烟：构建 debug 产物、写入示例 hosts.dev.toml 并启动界面
tools/             仓库自检脚本（如 bash 3.2 展开相邻多字节字符 lint）
tests/fake_sshd.py 无头 SSH+SFTP+ZMODEM 测试服务器
.github/workflows/  ci.yml（三平台 build + test + 安装冒烟）、release.yml（tag 出包）
```

## 路线图

- ssh-agent 认证（目前支持密码与私钥，含私钥口令）

主机密钥 TOFU、三平台预编译安装包、多会话标签页等已落地；每个版本的细项见
[CHANGELOG.md](CHANGELOG.md)。

## 贡献与安全

- 提交 bug / 功能建议：用仓库自带的 [issue 模板](.github/ISSUE_TEMPLATE)，请先升级到最新版并抹掉日志里的主机地址、路径与用户名。
- **发现安全漏洞请走私密渠道**，不要开公开 issue：见 [SECURITY.md](SECURITY.md)。

## 许可证

ells 以 [MIT 许可证](LICENSE) 开源。

**简体中文** | [English](README.md)

<div align="center">

# ells

**一个活在终端里的轻量级跨平台 SSH 客户端。**

`Rust` `TUI` `SSH` `SFTP` `ZMODEM` `端口转发` `无头 CLI`

官网：[https://ells.cn](https://ells.cn) · 仓库：[GitHub](https://github.com/lg10/ells) · [English README](README.md) · 更新日志：[CHANGELOG](CHANGELOG.md) · 安全策略：[SECURITY](SECURITY.md)

![CI](https://github.com/lg10/ells/actions/workflows/ci.yml/badge.svg) ![license](https://img.shields.io/badge/license-MIT-green) ![rust](https://img.shields.io/badge/rust-1.85%2B-orange) ![platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue) [![release](https://img.shields.io/github/v/release/lg10/ells?label=release&color=blue)](https://github.com/lg10/ells/releases) ![downloads](https://img.shields.io/github/downloads/lg10/ells/total?label=downloads) [![changelog](https://img.shields.io/badge/changelog-keep%20a%20changelog-informational)](CHANGELOG.md) [![security](https://img.shields.io/badge/security-policy-red)](SECURITY.md)

</div>

> ⚠️ **早期版本。** ells 处于活跃开发阶段（v0.2.x），可能存在 Bug、功能尚不完善。欢迎通过 [Issue](https://github.com/lg10/ells/issues) 反馈问题。

## 什么是 ells

ells 是一个纯 Rust 编写的一体化终端 SSH 客户端。它在高速 TUI 中内嵌了一个真正的终端模拟器（vt100），远程会话的颜色、`vim`/`htop` 等全屏程序都能正确渲染；同时补齐了原生 `ssh` 命令缺少的工作流能力：加密凭据保险库、SFTP 文件浏览、Xshell 风格的 `sz`/`rz` 传输拦截、本地端口转发与隧道管理、出事之后还能回看的操作记录与会话落盘，以及一套能让同一份保险库进脚本和 CI 的无头 CLI——全部只需几个按键或一次鼠标点击。

## 功能特性

- 🔐 **加密保险库** — 主机与密码存于 `~/.ells/vault.bin`，由主密码保护（Argon2id 派生 + XChaCha20Poly1305 认证加密）；支持免密自动解锁、应用内修改主密码。
- 🗂 **主机管理** — 别名、按主机的认证方式（密码 / 私钥 / **ssh-agent 与 Pageant**）、备注，以及 **跳板机（ProxyJump）**：`ells` → 跳板 → 目标，内置循环检测。
- ⌨️ **广泛兼容的私钥格式** — PKCS#8、PKCS#1 RSA（`BEGIN RSA PRIVATE KEY`）、OpenSSH 格式，以及旧式 **DEK-Info / 3DES 加密 PEM**（ells 原生解密，无需转换）。
- 🖥 **内嵌终端** — vt100 模拟器完整支持 ANSI/256 色/真彩色；`Ctrl-Q` 一键切换**直通模式**作为兜底逃生门；`Ctrl-L` 重绘。
- 📦 **`sz` / `rz` 拦截** — ells 在远端输出流做协议级 ZMODEM 握手检测（针对真实 `lrzsz` 字节流反复调校），自动改走 SFTP 通道传输：进度条、系统原生"另存为/选择文件"对话框，远端无需 ZMODEM 服务端配合。
- 📁 **SFTP 文件浏览器** — 会话内 `Ctrl-S` 打开：`Enter` 进入/下载、`u` 上传、`d` 下载，支持鼠标点击与滚轮，顶部聚合进度条 + 传输详情弹窗。
- 🎨 **输出高亮** — `docker ps` 按列分色（容器 ID / 镜像名 / 版本标签 / 容器名各一档，命令弱化）、`kubectl`、日志级别（ERROR/WARN/INFO…）、HTTP 方法、IPv4 地址、百分比即时着色，且绝不覆盖程序自己发出的 ANSI 颜色。
- 🔌 **空闲保活** — 可配置 SSH keepalive（5–3600 秒），扛住云服务器 NAT 空闲断连。
- 🖱 **鼠标友好** — 拖选复制（OSC 52）、滚轮回看历史且保留颜色、各界面元素可点击；终端标签页标题随页面联动（连接后显示 `ells-别名`）。
- 🔑 **主机密钥 TOFU** — 首次连接确认并记入 `~/.ells/known_hosts`（OpenSSH 兼容格式，也读 `~/.ssh/known_hosts`），密钥变化时阻断告警；批量场景用 `-y` 自动接受首次密钥，密钥变更仍然拒绝。
- 🪟 **多会话标签页** — 一个进程同时挂多台服务器：`F2` 新建、`F5`/`F6` 或鼠标点标签切换、`Ctrl-]` 关闭；后台标签的输出与传输继续跑，切回来还在。主机列表会标出哪几台已经连着（`●`），`Ctrl-G` 保持连接回到列表、Enter 或点标签即切回那一格。
- 🔍 **回看搜索** — `F3` 在历史输出里查关键词，`n`/`N` 跳上下条。
- 🛠 **远端目录操作** — 浏览器内 `m` 新建目录、`n` 重命名、`c` 改权限（`644` / `0644` / `0o644` / `000` 都认，只动权限位，属主和时间戳不碰）、`D` 删除（二级确认），`Ctrl-C` 取消在途传输；断线后可一键用保险库里的凭据重连，也可以让 ells 自己按退避接回去（见下）。
- 🔄 **应用内更新** — 启动时后台查一次新版（一个 HEAD 请求，不占匿名 API 限额），顶部出可更新徽标；点一下确认就下载、按 `SHA256SUMS.txt` 流式校验后原地替换自身，重启即生效（校验不过绝不替换）。
- 📥 **`~/.ssh/config` 双向搬砖** — 主机列表按 `i` 一键把已有的 OpenSSH 主机搬进来（只新增库里没有的别名，绝不覆盖你填好的密码）；按 `x` 反向导出一段 ssh_config 到 `~/.ells/ssh_config.export`，只写别名 / 主机 / 端口 / 用户 / `IdentityFile` / `ProxyJump` / 转发，**密码一条都不写**，`~/.ssh/config` 本体 ells 从不动笔。
- 🧭 **分组 · 标签 · 收藏 · 模糊过滤** — 列表画段头（`▾ 生产 (3)`），默认按 分组 → 组内收藏 → 最近使用 → 别名 排序；`o` 循环四档排序（默认 / 最近使用 / 别名 / 分组，选中的写进 `settings.ini`），`Space` 折叠光标所在那一组、`z` 一键收起全部；`/` 之后直接打字，别名、主机、用户、分组、标签、备注都算命中，标题写 `命中 n/m`，别名里命中的字符点亮；`f` 加星置顶，行尾右对齐一列「最近使用」（`刚刚 / 5分 / 3小时 / 昨天 / 5天 / 从未`）。
- 🔀 **本地端口转发** — 转发规则写在主机表单的「转发」栏（`-L 8080:127.0.0.1:5432`、`-D 1080`、只写目标的 `-L db:5432`），一条 SSH 承载这台机器的全部规则；`t` 打开隧道面板看每台实况，`空格` 启停、`Enter` 进**表格编辑器**一格一格改（类型 / 绑定 / 本地端口 / 目标主机 / 目标端口）。本地端口**留空就交给系统分配**（同 `ssh -L 0:db:5432`），两台主机想占同一个口也不会互挤，实际落在哪个口按 `m` 看「端口映射」总表；写死的端口真撞上时，失败原因会点名是谁占着。本地监听口在拨号前就占好，会话断开不影响隧道，它自己按退避重连。
- 🔁 **自动重连** — 非人为断开时按指数退避自动重试（1s 起、翻倍、封顶 60s、±20% 抖动），状态行写清"几秒后重连（第 n/m 次）"，`Ctrl-]` 随时取消；主机密钥被拒**绝不**自动重试，而是停下来问人。次数上限在「设置 → 自动重连」里调，`0` 就是关掉。
- 🖧 **无头 CLI** — 不开界面也能干活：`ells list`、`ells exec <别名> -- <命令>`、`ells sftp ls/get/put/rm/chmod`、`ells tunnel <别名>`（只起转发并常驻，`--all` 一次拉起所有配了规则的机器，Ctrl-C 结束）、`ells export`、`ells completions bash`。退出码固定 0/1/2/124（成功 / 失败 / 用法与安全闸门 / 超时），可直接进脚本；**任何密码、私钥内容、主密码都不会被打印**。主密码走 `ELLS_MASTER` 环境变量或 stdin 管道。
- 🪪 **已知主机密钥管理** — `h` 打开面板：`~/.ells/known_hosts` 里的记录可删（删掉下次连重新问指纹），`~/.ssh/known_hosts` 只显示、永不改动。
- 📜 **操作记录** — `l` 查看 `~/.ells/audit.log`：连接/断开、密钥被信任或变更、传输、隧道、保险库保存、导入导出、改主密码全部留痕（北京时间、只追加、超 1 MiB 自动转存，记录里不会出现任何密码或私钥）。
- 💾 **会话落盘** — 每连上一台机器把终端原始输出另存一份 `~/.ells/logs/<别名>-<时间>.log`（原样存 ANSI，事后 `less -R` 照样带色；界面里按 `v` 直接看，控制序列已剥掉）。单份 8 MiB 封顶、只留最近 30 天，开关在「设置 → 会话记录」。
- 📊 **远端主机指标** — 会话页最底下固定一行 CPU / 内存 / 磁盘的进度条与百分比（磁盘取用得最满的那个真实挂载点，比例照抄 `df` 的 Capacity 列，旁边写挂载点和已用/总量，1/5/15 分钟平均负载同排，摆得下才写——这两句补充都排在**整行末尾**，三格里只有标签、进度条和百分比，谁也不会因为带了说明被撑宽）。数据走的是**那条已经认证好的连接**：先在这条连接已有的 SFTP 会话上读 `/proc/stat`、`/proc/meminfo`、`/proc/loadavg`、`/proc/mounts`，磁盘用 `statvfs@openssh.com` 问本地盘（网络盘一律不问——一次卡死的 statvfs 会把整条 SFTP 会话拖住，连传输面板一起停）；没有 SFTP、服务器不支持那个扩展或读不到东西时，才回落一条一次性 `exec` 只读探针（`df -Pk`），并且只填还空着的那几格。不另开 SSH 连接、不碰你正在敲的那个 PTY、不在远端留进程。首帧不等：SFTP 子系统本来就在握手阶段先于终端开好，所以连上立刻跑第一轮，内存、磁盘、负载当场有数。节奏是分开的：`/proc` 那三样 5 秒一轮，磁盘那一格也是 5 秒一换，但中间那些轮只对「上次普查里最满那块」问一次 statvfs（一个往返），而普查（重读 `/proc/mounts`、逐块问一遍、重新决定谁最满）60 秒才走一次 —— 逐块问盘代价最大，挂载点集合一分钟里又几乎不动。服务器没有 statvfs 扩展时，跟单轮一个请求都不发、那一格沿用上次的数，只在 60 秒的普查点补一条 `df` 兜底，绝不会退化成每 5 秒一条 df。（连上的第一轮就是普查，所以不用对着空格等一分钟。）没在看的标签降到一分钟一轮，切回它那一格当场重采一轮。CPU 要两次快照做差才是瞬时值，所以它比别人晚一步——拿到基线的那一轮之后单独两秒接力，进去约两秒见到第一个数，之后跟着 5 秒一轮（没有 `/proc/stat` 的机器不会两秒空转）；FreeBSD、精简容器那一格一直是 `—`，磁盘照常。采不到就画 `—`，绝不画 0%；连续三轮什么都拿不到就判定这台不支持，停止轮询并把这一行还给终端——但"通道级失败"（超时、开不了通道、执行报错）不算采不到，只是把节奏退到 5 → 10 → 30 → 60 秒，任意一轮拿到数就自动回到 5 秒。开关在「设置 → 主机指标」（`metrics=`）。
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
ELLS_VERSION=v0.2.0 curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/install.sh | sh
ELLS_INSTALL_DIR=/usr/local/bin curl -fsSL https://raw.githubusercontent.com/lg10/ells/main/install.sh | sh
```

Windows PowerShell：

```powershell
$env:ELLS_VERSION='v0.2.0'; irm https://raw.githubusercontent.com/lg10/ells/main/install.ps1 | iex
$env:ELLS_INSTALL_DIR="$env:USERPROFILE\bin"; irm https://raw.githubusercontent.com/lg10/ells/main/install.ps1 | iex
```

`ELLS_API_URL` / `ELLS_DOWNLOAD_URL` 可把发布查询与下载地址换成内网或镜像源。安装出问题时先跑 `sh diagnose.sh`，它会打印本机 shell / curl / 架构 / 代理等判定结果。

### 应用内更新

装好之后一般不用再跑安装脚本。ells **每次启动**都在后台问一次"有没有新版本"——只发一个 HEAD 请求、从 GitHub 的重定向地址读版本号，不下载任何内容，也不占匿名 API 限额。有新版本时主机列表顶部会出现 `【vX.Y.Z 可更新】` 徽标，点它（或进设置把光标移到「自动更新」按 Enter）弹确认框，确认后按顺序做四件事：

1. 下载本平台对应的发布资产（`ells-linux-x86_64` / `ells-macos-universal` / `ells-windows-x86_64.exe`）到 ells 所在目录的临时文件；
2. **边下边算 SHA256**，和发布里的 `SHA256SUMS.txt` 比对，校验不过（或没下完）就直接删掉临时文件，**绝不替换**；
3. 原地换上新二进制。Windows 上正在运行的镜像删不掉但能改名，于是先把 `ells.exe` 改成 `ells.exe.old` 再把新文件换进去，本次会话照常跑完；换失败会原样改回来；
4. 提示"新版本已就位"——盘上是新的了，进程还是旧的，点【立即重启】或按 Enter 换新安。确认框里会先说明这会断开手上几路会话。

下载途中按 `Esc` 或点【取消下载】随时中止，临时文件不留。启动那次自动检查失败是**静默**的（没网不该拦人），只有你主动检查/更新才会看到中文的失败原因；下次启动会顺手清掉残留的 `.old` 和超过一天的临时文件。

- 不想让它自己查：进设置把「自动更新」按 `←` 关掉（写入 `settings.ini` 的 `auto_update=false`），之后仍可在同一行按 Enter 手动检查。
- 脚本 / 排障：`ells --check-update` 只查版本、打印结果就退出，不启动界面、不碰终端。
- 上次检查的时间与版本号缓存于 `~/.ells/update.cache`（只有这两行，没有任何主机信息）。
- 镜像 / 内网：`ELLS_DOWNLOAD_URL`（资产基址）与 `ELLS_API_URL`（发布查询），和安装脚本同一套约定。
- 没有预编译包的平台（例如 Linux aarch64）不会尝试自替换，而是提示改用安装脚本。

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
ells [ALIAS] [--dev] [-y] [--check-update]
ells <子命令> …            无界面，脚本 / CI 用：list · exec · sftp · tunnel · export · completions

  ALIAS          解锁后立即连接该别名对应的主机（安装后的短命令 `s <别名>` 同义）
  --dev          从 ~/.ells/hosts.dev.toml 读取明文主机表（仅开发调试，不做持久化）
  -y, --yes      首次见到的主机密钥自动接受并记录；密钥变更时仍然拒绝
  --check-update 只检查有没有新版本，打印结果就退出（不下载、不启动界面）

无头子命令（`ells <子命令> --help` 看全部参数）：

  ells list [--format table|json] [--filter 关键词]
  ells exec <别名> [--tty] [--timeout 秒] [--stdin] -- <远端命令>
  ells sftp ls|get|put|rm|chmod <别名> …        覆盖要 --yes，删目录要 --recursive
  ells tunnel <别名> | --all                    只起端口转发并常驻，Ctrl-C 结束
                                                每行状态都带着实际绑上的本地端口，
                                                留空（自动分配）的标「（自动）」
  ells export [--out 路径]                      默认打到标准输出
  ells completions bash|zsh|fish

  退出码是对外契约：0 成功 · 1 失败（连不上 / 认证被拒 / 主机密钥被拒 / 远端命令非 0）
  · 2 用法或安全闸门（差一个 --yes 之类，原因用中文打在 stderr）· 124 --timeout 到点。
  `ells exec` 里远端命令的退出码原样透传。红线只有一条：任何密码、私钥内容、主密码
  都不会被打印，`--format json` 里能出现的只有可公开的元数据。

环境变量：ELLS_LOG=1 把内部日志写到 stderr；ELLS_ZMODEM_LOG=1 额外把 sz/rz
拦截诊断写入 ~/.ells/zmodem.log；ELLS_YES=1 等价于 -y；ELLS_MASTER 给无头子命令
喂主密码（管道里也可以把主密码作为 stdin 第一行）；ELLS_API_URL /
ELLS_DOWNLOAD_URL 把更新检查指到镜像或内网。

配置文件都在 ~/.ells/：vault.bin（保险库）、settings.ini（设置）、
known_hosts（主机密钥记录）、audit.log（操作记录）、logs/（会话记录）、
ssh_config.export（列表页 `x` 的导出件）、update.cache（上次更新检查）。
```

### 快捷键一览

| 界面 | 按键 |
|------|------|
| 主机列表 | `↑↓/j k` 选择 · `Enter` 连接（这台已连着就切回它那一格标签） · `a` 新增（选中那台时预填结构性字段，别名/主机/密码留空） · `e` 编辑 · `d` 删除（二级确认） · `s` 设置 · `i` 导入 `~/.ssh/config` · `x` 导出 ssh_config（永不含密码） · `p` 用表格编辑选中那台的转发规则 · `m` 端口映射总表 · `/` 过滤（标题写 `命中 n/m`，别名里命中的字符点亮） · `f` 收藏置顶 · `Space` 折叠/展开当前分组 · `z` 一键全折叠/全展开 · `o` 循环排序（默认 · 最近使用 · 别名 · 分组） · `t` 端口转发 · `h` 已知主机密钥 · `l` 操作记录 · `v` 会话记录 · `?`/`F1` 帮助 · `q`/`Ctrl-C` 退出；分组段头 `▾ 生产 (3)`，行首 `●` 已连接 / `○` 正在连接，行尾右对齐一列最近使用时间，顶部标签条点一下就切回去 |
| 列表页面板 | `t` 隧道面板：`空格`/`s` 启停选中那台 · `Enter` 进表格改这台的规则 · `m` 端口映射总表（顶部「端口映射」可点） · `x` 全停（`▲` 已就绪 `◐` 连/重连中 `▼` 失败） · `h` 已知主机密钥：`d` 删 `~/.ells/known_hosts` 里的记录（`~/.ssh/known_hosts` 只读，永不改） · `l` 操作记录：`↑↓`/`PgUp`/`PgDn` 翻看 `~/.ells/audit.log` · `v` 会话记录：`↑↓` 选文件、`Enter` 看正文、`d` 删单份、`Esc` 返回；面板都用 `Esc`/`q` 关闭 |
| 转发规则表格 | 列表页 `p` 或隧道面板 `Enter` 打开：`↑↓` 移行 · `←→`/`Tab` 移格（第 1 列「类型」上 `←→` 是 `L`/`D`/`R` 循环） · `Ctrl-N` 加一行 · `Ctrl-D` 删一行 · `Ctrl-C` 清空当前格 · `Enter` 保存 · `Esc` 放弃全部改动；「本地端口」一格留空（或写 `0`）＝系统分配空闲口，撞上别的规则时行尾标 `⚠` 并在表下点名同用的是哪一台；保存后隧道在跑就按新规则自动重启 |
| 端口映射总表 | `m` 打开（列表页、隧道面板都行）：`主机 / 规则 / 实际监听 / 状态` 四列，自动分配的端口在这里才看得见，标「（自动）」；撞口时隧道面板顶部的按钮会先变成 `⚠ 8080 被多台主机共用`。`m`/`Esc`/`q` 关闭 |
| 多标签 | `F2` 新建标签 · `F5` 下一个 · `F6` 上一个 · 鼠标点顶部标签条切换、点末尾 `+` 新建 · `Ctrl-]` 关闭当前标签（只剩一个时退回主机列表；有传输在跑时要按两次） · `Ctrl-G` 保持连接回到主机列表，再按一次回到原来那一页 |
| 会话 | 任意按键直达远端 · `Ctrl-Q` 内嵌/直通切换 · `Ctrl-S` 打开 SFTP 浏览器 · `Ctrl-L` 重绘 · `Ctrl-]` 关闭标签 · `Ctrl-G` 回主机列表（连接不断） · 滚轮=回看历史 · 拖选=复制（OSC 52） · `F3` 搜索历史输出（回看时按 `/` 同样可用，`n`/`N` 跳上下条） · `F1` 帮助；底部固定一行远端主机的 CPU/内存/磁盘百分比（采不到的机器自动收起，开关在设置） |
| 文件浏览器 | `Enter` 进入目录/下载 · `u` 上传文件 · `U` 上传整个目录 · `d` 下载 · `m` 新建目录 · `n` 重命名 · `c` 改权限（八进制 `600`/`0644`，只动权限位） · `D` 删除（递归，先确认） · `Ctrl-C` 取消全部在途传输 · `r` 刷新 · `Backspace` 上级 · `Esc` 返回终端 · 顶部标签条同样可点 |
| 主机表单 | `Tab/↑↓` 切换输入项 · `←→` 切换认证方式 · `Ctrl-F` 选私钥 · `Ctrl-J` 选跳板机 · `Ctrl-R` 让「密码/私钥口令」显形（再按遮回去） · `Enter` 在"私钥路径/跳板机"上直接打开选择器 · 保存点【保 存】按钮或回车；带 `＊` 的别名/主机/用户是必填项，提交失败只点缺的那几项的名；光标停在「分组」时行下列出已有分组与各自台数，敲同名即归入、敲新名即新建；「标签」逗号分隔可被 `/` 过滤、「转发」写 ssh 风格的 `-L`/`-D` 规则（一条一空格分隔，本地端口可留成 `-L db:5432` 交给系统分配），不想数空格就取消表单、在列表页按 `p` 用表格逐格填 |
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

设置页另外三行管的是"事后能不能查"、"要不要占掉底下那一行"和"断了要不要自己接"：

- **会话记录（终端输出落盘）**：Enter 开关，写 `settings.ini` 的 `session_log=`（默认开）。
  关掉只影响之后的新连接，已落盘的文件照旧能在 `v` 面板里看、照旧 30 天后被清掉。
- **主机指标（CPU/内存/磁盘）**：Enter 开关，写 `metrics=`（默认开）。关掉或这台机器采不到，
  会话页底部那一行就整个还给终端——视口高度、PTY 的 `SIGWINCH` 和鼠标命中用的是同一个行数，
  不会出现"画面被裁掉一行"。开着则给已经连上的标签当场补上，不必重连。
- **自动重连（会话断开后）**：Enter 在 `关 → 1 → 2 → 3 → 5 → 无限` 之间循环，`←→` 微调，
  写 `reconnect_attempts=`（`0` = 关掉自动重连、回到断开后弹窗问人；`9999` 显示为"无限"）。
  退避曲线的细项直接在 `settings.ini` 里写：`reconnect_initial_ms`（首次等待，默认 1000）、
  `reconnect_max_ms`（封顶，默认 60000）、`reconnect_stable_secs`（连着撑过这么久才算"稳了"，
  下次掉线从第 1 次重头计，默认 30）、`reconnect_jitter`（抖动比例 0–0.5，默认 0.2，
  只会把等待往后推、不会提前，避免一群连接同一秒一起重试）。排队期间状态行写着
  "N 秒后自动重连（第 n/m 次）· 按 Ctrl-] 停止"，随时可以按掉。两种情况绝不自动重试：
  你自己退出/关闭的会话，以及主机密钥被拒——后者是安全事件，必须停下来问人。

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
  ells-core        SSH（russh）客户端、保险库加密、主机模型、keepalive、主机密钥
                   TOFU、端口转发与隧道管理、ssh_config 双向读写、审计日志、
                   会话落盘、原子写、自动重连退避、模糊过滤
  ells-term        vt100 终端模拟器封装（屏幕、回看缓冲、DSR/DA 应答）
  ells-transfer    SFTP 上传/下载引擎（节流进度回调、chmod 八进制解析）
  ells             TUI 应用本体 + 无头 CLI（ratatui + crossterm）：界面、ZMODEM
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

- `-R` 远端端口转发（现在存得进来、表格改得了、`ssh_config` 导出也带上，但 ells 起不来它，隧道会明确报「这类远程转发暂不支持」而不是静默失效）。
- SFTP 浏览器多选与批量传输队列。

无头 CLI（`list` / `exec` / `sftp` / `tunnel` / `export` / `completions`）、本地端口转发与隧道管理、
转发规则表格编辑器与端口映射总表、本地端口自动分配（撞口时留空就不互挤）、
分组标签收藏与模糊过滤、审计日志与会话落盘、已知主机密钥面板、远端 `chmod`、断线自动重连退避、
主机密钥 TOFU、ssh-agent / Pageant 认证、三平台预编译安装包、多会话标签页等已落地；
每个版本的细项见 [CHANGELOG.md](CHANGELOG.md)。

## 贡献与安全

- 提交 bug / 功能建议：用仓库自带的 [issue 模板](.github/ISSUE_TEMPLATE)，请先升级到最新版并抹掉日志里的主机地址、路径与用户名。
- **发现安全漏洞请走私密渠道**，不要开公开 issue：见 [SECURITY.md](SECURITY.md)。

## 许可证

ells 以 [MIT 许可证](LICENSE) 开源。

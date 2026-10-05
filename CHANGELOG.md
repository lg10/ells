# 更新日志 / Changelog

本文件遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 与
[语义化版本](https://semver.org/lang/zh-CN/)。0.x 阶段允许在不通知的情况下调整
内部实现，但保险库文件格式（`ELLSVAULT` v1）保持兼容。

English summary lives in each GitHub Release note; this file is the full history.

发版流程：把下面的 `[未发布 / Unreleased]` 小节改名为 `[x.y.z] - 日期`，再打 tag。
CI 的 release 作业会按 tag 从本文件截取对应小节，作为 GitHub Release 说明。

## [未发布 / Unreleased]

### 新增
- **"已连接"可见可回**：主机列表现在能看见并回到正在连的会话。列表顶部加入
  与会话页同一套标签条（点标签即切过去、点末尾 `+` 新建），主机行首用
  `●` 已连接 / `○` 正在连接 / `·` 空闲标记；列表里对已连着的主机按 Enter
  不再重复拨号，而是直接切回它那一格标签。会话页新增【列表】按钮与 `Ctrl-G`：
  保持连接回到主机列表，再按一次回到原来那一页（连接、画面、传输都不动）。
  文件浏览器顶部同样有标签条，随时切到别的标签。
- **快捷键自定义**：设置页新增「快捷键设置」，`F2` 新建标签、`F5`/`F6` 切换、
  `Ctrl-]` 关闭标签、`Ctrl-G` 回主机列表、`Ctrl-S` 文件浏览器、`Ctrl-Q` 直通模式、
  `Ctrl-L` 重绘、`F3` 搜索这 9 项都可以改成自己的组合键。进入后选中一项按 Enter 开始录制，
  按新键即时生效并写入 `~/.ells/settings.ini`；撞上已占用的键时两个动作自动互换，
  也可一键「恢复默认」。可绑范围收窄为 F2–F9 或带 Ctrl/Alt 的组合（F1 留给帮助页，
  F10–F12 常被终端菜单/全屏/媒体键吃掉），避免把普通输入吞掉；
  终端里同码的 `Ctrl-H`/`Ctrl-I`/`Ctrl-M`/`Ctrl-[`/`Ctrl-C` 会被明确拒绝并说明原因。
- **组合键跨平台归一**：mac/Linux 终端把 `Ctrl-]` 这类组合发成裸字节 `0x1C–0x1F`，
  crossterm 解出来是 `Ctrl-5`，所以在 mac/Linux 上默认 `Ctrl-]` 曾经根本触发不了。
  现在在输入入口按字节归一（`Ctrl-5` ⇄ `Ctrl-]`、`Ctrl-4` ⇄ `Ctrl-\` 等），录制、保存、
  派发用同一种写法，默认键在 Windows/macOS/Linux 上都能命中；`Ctrl+数字`/`Ctrl+空格`/`Ctrl-/`
  因为与别的组合同码或被 Windows Terminal 占用，改为拒绝并说明。快捷键面板与帮助页会按
  当前平台显示对应的注意点（Windows Terminal 的 `Ctrl+1–8`/`F11`、mac 的功能键需按 `fn`
  或把 Option 设为 Meta、Linux 的 `F10` 菜单/`F11` 全屏）。

### 修复
- 发布流水线：`release.yml` 里重复的 `fail_on_unmatched_files` 键让整个 workflow 被
  GitHub 判定无效，v0.1.4 第一次打 tag 时零作业直接失败、没有产出任何产物。

### 内部
- CI 新增 `workflows-lint` 作业（`tools/check-workflow-keys.py`）：用会拒绝重复映射键
  的严格 YAML loader 校验 workflow 与 issue 表单——这类错误在 GitHub 侧是静默失败，
  必须在本机拦下。

## [0.1.4] - 2026-10-05

### 新增
- **主机密钥校验（TOFU）**：首次连接时确认并记录到 `~/.ells/known_hosts`
  （OpenSSH 兼容格式，同时读 `~/.ssh/known_hosts` 里已有的记录），之后密钥变化会
  阻断并告警，避免中间人劫持。批量/脚本场景可用 `-y`（`ELLS_YES=1`）自动接受
  首次见到的密钥，密钥变更仍然拒绝。
- **多会话标签页**：一个进程同时挂多台服务器。`F2` 新建、`F5`/`F6` 切换、
  `Ctrl-]` 关闭当前标签，也可用鼠标点击标签条。后台标签的输出、传输与状态
  都留在自己那一页，切回来还在。
- **历史输出搜索**：`F3` 在回看缓冲里查关键词，`n`/`N` 跳命中。
- **断线重连**：非用户主动断开时提示重连，凭据直接从保险库取用。
- **远端目录操作**：文件浏览器内 `m` 新建目录、`n` 改名、`D` 删除（二级确认）。
- **`~/.ssh/config` 导入**：主机列表按 `i` 一键导入已有 OpenSSH 主机配置。
- **全键位帮助页**：`?`（列表/浏览器）或 `F1`（会话）查看。
- **卸载脚本**：`uninstall.sh` / `uninstall.ps1` 清理二进制、PATH 与 PowerShell
  里的 `s` 函数；默认保留 `~/.ells`，只有显式 `--purge` / `ELLS_PURGE=1` 才删保险库。
- **发布物料**：新增本 `CHANGELOG.md`、`SECURITY.md`（私密漏洞上报 + 报告禁忌清单）、
  GitHub issue 模板（反馈 / 功能建议 + 关闭空白 issue）；README 双语补齐徽章、
  卸载说明与最新键位表；release 作业改为按 tag 从本文件截取对应小节生成
  GitHub Release 说明。

### 变更
- 传输目标已存在时不再静默覆盖：提供覆盖 / 改名 / 取消三选。
- `sz` 传目录改为整目录递归下载；上传/下载都可用系统目录选择框选整个目录。
- 传输进行中可按 `Ctrl-C` 取消，不再只能等它跑完。
- 首启设置主密码时增加"不可找回"警示页（黄色边框 + 红字说明后果），与
  普通解锁页视觉区分。
- 关闭标签或退出程序时若仍有传输在跑，需要再按一次确认，避免半截文件。
- ZMODEM 诊断日志默认关闭（只有 `ELLS_ZMODEM_LOG=1` 才写 `~/.ells/zmodem.log`），
  不再往临时目录留排障垃圾文件。

### 安全
- 明文主密码改用 `Zeroizing` 承载，派生密钥在 KDF/解密失败的早退路径上也会清零。
- `~/.ells` 目录创建后立刻收紧权限：Unix 下 0700，Windows 下用 `icacls` 去掉
  继承项、只留给当前用户（写法与 `ssh-keygen` 收紧私钥一致，失败不影响启动）。

## [0.1.3] - 2026-10-05

### 修复
- `ells <别名>` / `s <别名>` 解锁后立即直连，不再停在主机列表等一次按键。

## [0.1.2] - 2026-10-05

### 新增
- 主机表单里 `Enter` 直接触发"选私钥 / 选跳板机"，不再顺带下移输入项
  （换行与移焦只用上下方向键）。

### 修复
- macOS 上系统文件对话框改由主线程服务，修复
  `Fallback Sync Dialog Must Be Spawned On Main Thread` 导致的崩溃。
- `install.sh` 兼容 macOS 自带的 bash 3.2：变量全部预绑定、展开一律加花括号、
  展开相邻标点改 ASCII，修复 `VERSION?: unbound variable`；新增 `diagnose.sh`
  一键诊断安装环境，并在 CI 用 `LC_ALL=C` 真跑一遍安装冒烟。

## [0.1.1] - 2026-10-05

### 新增
- 安装脚本在 PowerShell 配置文件里注册 `s` 短命令（函数优先于 PATH 解析，
  绕开 PowerShell 内置别名 `s=Set-Variable` 的冲突）。

## [0.1.0] - 2026-10-05

首个公开版本：纯 Rust 终端 SSH 客户端。

### 新增
- 加密保险库（Argon2id + XChaCha20Poly1305）保存主机、账号、私钥口令，支持
  主密码、自动解锁与应用内改密。
- 内嵌 vt100 终端（真彩色 / 256 色 / ANSI 保留），`Ctrl-Q` 切直通模式兜底，
  `Ctrl-L` 重绘。
- SFTP 文件浏览器（`Ctrl-S`）：上传/下载、聚合进度条与传输详情弹窗、鼠标点选
  与滚轮。
- `sz` / `rz` 劫持：识别远端 lrzsz 的 ZMODEM 握手，改走 SFTP 通道，无需服务器
  装 ZMODEM 守护进程。
- 私钥格式兼容：PKCS#8、PKCS#1 RSA、OpenSSH、以及 DEK-Info/3DES 老式加密 PEM
  的原地解密。
- 跳板机（ProxyJump）别名链，含环路检测。
- 输出高亮：`docker ps`、`kubectl`、日志级别、HTTP 方法、IPv4、百分比。
- SSH keepalive（15–300 秒可配），扛住云厂商 NAT 空闲断连。
- 鼠标：拖选复制（OSC 52）、滚轮回看保留颜色、各界面元素可点击；终端标签名
  随页面联动（`ells-<别名>`）。
- 一行命令安装：`install.sh`（macOS/Linux）与 `install.ps1`（Windows），装出
  `ells` 与短命令 `s`，强制 SHA256 校验并自动配置 PATH。
- GitHub Actions 构建矩阵（Linux x86_64、macOS universal、Windows x86_64）与
  tag 触发的 Release。
- 全中文界面、系统原生文件对话框、零遥测。

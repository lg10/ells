//! clap 帮助文本（派生宏里写死的中文）的英文对照。
//!
//! 这些是整段文字，只能整串精确命中，不参与 `{}` 模板匹配。

pub static ENTRIES: &[(&str, &str)] = &[
    (
        "ells — 跨平台终端 SSH 客户端（内嵌终端 / rz-sz 自动转 SFTP / 主密码保险库）",
        "ells — cross-platform terminal SSH client (built-in terminal / rz-sz to SFTP / master-password vault)",
    ),
    (
        "用法示例：\n  ells            打开主机列表\n  ells <别名>      直连指定主机（安装后的短命令 `s <别名>` 同义）\n  ells list        无界面列出主机（--format json 给脚本用）\n  ells exec web -- uname -a   跑一条命令就退出\n  ells sftp get web /var/log/app.log ./\n  ells tunnel web  只起这台配好的端口转发\n  ells export      导出 ~/.ssh/config 片段（永不含密码）\n  ells --help      查看完整帮助",
        "Usage examples:\n  ells            open the host list\n  ells <alias>    connect straight to a host (the installed short command `s <alias>` is the same)\n  ells list       list hosts without the UI (--format json for scripts)\n  ells exec web -- uname -a   run one command and exit\n  ells sftp get web /var/log/app.log ./\n  ells tunnel web  start only this host's configured port forwards\n  ells export      export a ~/.ssh/config fragment (never contains passwords)\n  ells --help      show the full help",
    ),
    (
        "快捷键（列表页）：↑↓ 选择 · Enter 连接 · a 新增 · e 编辑 · d 删除 · i 导入 ~/.ssh/config · x 导出 ssh_config · p 转发规则表格 · m 端口映射 · / 过滤 · f 收藏 · Space 折叠当前分组 · z 全折叠 · o 循环排序 · t 隧道 · h 密钥 · l 操作记录 · v 会话记录 · s 设置 · ? 帮助 · q 退出\n  隧道面板（t）里：空格/s 启停 · Enter 用表格改这台机器的规则 · m 端口映射总表 · x 全部停止\n\n端口转发：本地端口留空（或写 0）＝交给系统分配，两台主机想要同一个口就不会互挤；\n  表单里可以只写目标（`-L db:5432`），实际绑上的端口在「端口映射」里看，启动失败会点名是谁占着。\n\n主机指标：会话页底部固定一行 CPU / 内存 / 磁盘百分比（磁盘取用得最满的那个挂载点，比例照抄 df 的 Capacity 列；三格里只有标签、条和百分比，1/5/15 分钟负载与挂载点已用/总量都排在整行末尾，摆得下才写）。优先在那条已认证连接已有的 SFTP 会话上读 /proc、用 statvfs 问本地盘，服务器不配合才回落一条一次性 exec 只读探针（只填空着的那几格）。连上立刻跑第一轮，内存 / 磁盘 / 负载当场就有数；CPU 是两次快照做差，那一格约两秒后填上。节奏是分开的：/proc 那三样 5 秒一轮，磁盘那一格 5 秒也跟着换数、但每轮只对「当前最满那块」问一次 statvfs（挂载点表缓存在这条连接上），每 60 秒才重做一次普查 —— 重读挂载点、逐块问一遍、重新决定哪块最满；服务器没有 statvfs 扩展时跟单轮什么都不问，只在 60 秒的普查点补一条 df，绝不让 df 退化成每 5 秒一次；没在看的标签降到 60 秒一轮，切回去当场重采。不另开连接、不碰你正在敲的 PTY；采不到画 —，连续三轮什么都拿不到就停止轮询、把这一行还给终端；通道级失败（超时、开不了通道）只把节奏退到 5 → 10 → 30 → 60 秒，拿到数就回到 5 秒。开关：设置 → 主机指标（settings.ini 的 metrics=）。\n\n子命令（无界面，脚本 / CI 用）：list · exec · sftp · tunnel · export · completions\n  注意：子命令名优先于别名位置参数，所以名为 list/exec/sftp/tunnel/export/completions 的主机别名不能被 `ells <别名>` 直连，请改名或进列表连接。\n\n配置文件：\n  ~/.ells/vault.bin     加密后的主机与凭据（argon2id + XChaCha20-Poly1305）\n  ~/.ells/settings.ini  全局设置（含列表排序 list_sort=、主机指标 metrics=）\n  ~/.ells/known_hosts   主机密钥记录（首次连接时确认，之后校验）\n  ~/.ells/audit.log     操作记录（连接、传输、密钥变更；绝不含密码）\n  ~/.ells/logs/         会话记录（终端原始输出，单份 8 MiB、留 30 天）\n  ~/.ells/update.cache  上次更新检查（只有版本号和时刻）\n\n环境变量：\n  ELLS_LOG=1            把内部日志写到 stderr（排障用）\n  ELLS_YES=1            首次见到的主机密钥自动接受并记录（密钥变更仍会拒绝）\n  ELLS_MASTER=…         无头子命令用的主密码（管道里跑时也可用 stdin 第一行）\n  ELLS_ZMODEM_LOG=1     额外把 sz/rz 拦截诊断写入 ~/.ells/zmodem.log\n  ELLS_UPDATE_SOURCES=… 自更新源列表（逗号/分号分隔）：github 或镜像前缀（如 https://gh-proxy.com）。\n                        空则用内置的官方 + 多个加速镜像；每次并行测速、失败自动换源。\n                        旧的 ELLS_API_URL / ELLS_DOWNLOAD_URL / ELLS_RELEASE_URL 已废弃且不再生效。",
        "Keys (host list): ↑↓ select · Enter connect · a add · e edit · d delete · i import ~/.ssh/config · x export ssh_config · p forwarding rules table · m port map · / filter · f favorite · Space collapse this group · z collapse all · o cycle sort · t tunnels · h keys · l activity log · v session logs · s settings · ? help · q quit\n  In the tunnels panel (t): Space/s start/stop · Enter edit this host's rules in a table · m port map overview · x stop all\n\nPort forwarding: leaving the local port empty (or writing 0) lets the system pick one, so two hosts asking for the same port never collide;\n  a form may give just the target (`-L db:5432`); the port actually bound is shown under \"port map\", and a failed start names whoever holds it.\n\nHost metrics: one fixed line at the bottom of the session page shows CPU / memory / disk percentages (disk takes the fullest mount point and copies df's Capacity column; the three cells hold only a label, a bar and a percentage; 1/5/15-minute load plus used/total of the mount point go at the end of the line and are written only when they fit). The preferred path reads /proc over the SFTP session the authenticated connection already has and asks local disks via statvfs; only if the server won't cooperate does it fall back to a one-shot read-only exec probe (which fills only the empty cells). The first round runs right after connecting, so memory / disk / load have numbers immediately; CPU is the difference of two snapshots and fills in about two seconds later. The cadence is split: the three /proc values refresh every 5 seconds, the disk cell changes every 5 seconds as well but each round asks statvfs only about \"the current fullest mount\" (the mount table is cached on this connection), and only every 60 seconds does it redo a full survey — re-read mounts, ask each one, decide again which is fullest; without the statvfs extension the routine rounds ask nothing, and a single df is added only at the 60-second survey point, so df never degrades to once every 5 seconds; tabs you are not looking at drop to once every 60 seconds and are re-sampled the moment you switch back. No extra connection, no touching the PTY you are typing in; unreadable values show —, three consecutive rounds with nothing at all stop polling and give the line back to the terminal; channel-level failures (timeout, can't open a channel) only back the cadence off 5 → 10 → 30 → 60 seconds and return to 5 as soon as a value arrives. Toggle: Settings → host metrics (metrics= in settings.ini).\n\nSubcommands (headless, for scripts / CI): list · exec · sftp · tunnel · export · completions\n  Note: a subcommand name wins over the alias positional, so a host aliased list/exec/sftp/tunnel/export/completions cannot be reached with `ells <alias>`; rename it or connect from the list.\n\nConfig files:\n  ~/.ells/vault.bin     encrypted hosts and credentials (argon2id + XChaCha20-Poly1305)\n  ~/.ells/settings.ini  global settings (including list sort list_sort= and host metrics metrics=)\n  ~/.ells/known_hosts   host key records (confirmed on first connect, verified afterwards)\n  ~/.ells/audit.log     activity log (connects, transfers, key changes; never contains passwords)\n  ~/.ells/logs/         session logs (raw terminal output, 8 MiB per file, kept 30 days)\n  ~/.ells/update.cache  last update check (version and time only)\n\nEnvironment variables:\n  ELLS_LOG=1            write internal logs to stderr (troubleshooting)\n  ELLS_YES=1            accept and record a host key on first sight (key changes are still rejected)\n  ELLS_MASTER=…         master password for headless subcommands (the first line of stdin also works in a pipe)\n  ELLS_ZMODEM_LOG=1     also write sz/rz interception diagnostics to ~/.ells/zmodem.log\n  ELLS_UPDATE_SOURCES=… update source list (comma/semicolon separated): github or a mirror prefix (e.g. https://gh-proxy.com).\n                        empty means the built-in official site plus several accelerators; each check races them and moves on when one fails.\n                        The old ELLS_API_URL / ELLS_DOWNLOAD_URL / ELLS_RELEASE_URL are deprecated and no longer take effect.",
    ),
    (
        "跳过主密码解锁，改从明文文件 ~/.ells/hosts.dev.toml 读取主机（仅开发调试用）",
        "Skip the master-password unlock and read hosts from the plaintext file ~/.ells/hosts.dev.toml (development and debugging only)",
    ),
    (
        "首次连接新主机时自动接受并记录主机密钥；密钥变更时仍然拒绝",
        "Accept and record the host key on first sight; key changes are still rejected",
    ),
    (
        "首次连接新主机时自动接受并记录主机密钥；密钥变更时仍然拒绝\n\n不用 clap 的 `env`：它只认 true/false，而我们对外文档写的是 `ELLS_YES=1`， 脚本里 `export ELLS_YES=1` 会直接被 clap 判成用法错误。",
        "Accept and record the host key on first sight; key changes are still rejected\n\nNo clap `env` here: it only accepts true/false, while our docs say `ELLS_YES=1`, so `export ELLS_YES=1` in a script would be rejected by clap as invalid usage.",
    ),
    (
        "只检查有没有新版本：打印结果就退出，不下载、不启动界面（脚本 / 排障用）",
        "Only check for a new version: print the result and exit, no download and no UI (for scripts / troubleshooting)",
    ),
    (
        "列出主机（表格或 JSON；绝不输出密码与私钥内容）",
        "List hosts (table or JSON; never prints passwords or private keys)",
    ),
    (
        "按别名/主机/用户/分组/标签过滤（子串，不区分大小写）",
        "Filter by alias/host/user/group/tag (substring, case-insensitive)",
    ),
    (
        "在远端执行一条命令就退出：ells exec web -- uname -a",
        "Run one command on the remote and exit: ells exec web -- uname -a",
    ),
    (
        "主机别名",
        "host alias",
    ),
    (
        "申请伪终端：给 top/systemctl 这类要 TTY 的命令；此时 stdout 含控制序列",
        "Request a pseudo-terminal: for commands that need a TTY such as top/systemctl; stdout then contains control sequences",
    ),
    (
        "整体超时秒数（含连接与认证）；到点退出码 124",
        "overall timeout in seconds (connection and auth included); exits with code 124 when hit",
    ),
    (
        "把本进程的标准输入整段送进远端命令",
        "send this process's whole stdin to the remote command",
    ),
    (
        "不界面的文件操作：ells sftp ls web /var/log",
        "Headless file operations: ells sftp ls web /var/log",
    ),
    (
        "允许覆盖已存在的目标（不给就报错退出）\n\n`global` 不是好看：`--yes` 声明在这一层，写在 `get`/`put` 后面会被 clap 当成子命令不认识的参数，而\"ells sftp get web a b --yes\"才是人会打的字。",
        "overwrite an existing target (otherwise it errors out)\n\n`global` is not cosmetic: `--yes` is declared at this level, so writing it after `get`/`put` would be an argument the subcommand doesn't know, while \"ells sftp get web a b --yes\" is what people actually type.",
    ),
    (
        "列远端目录",
        "list a remote directory",
    ),
    (
        "下载远端文件",
        "download a remote file",
    ),
    (
        "上传本地文件",
        "upload a local file",
    ),
    (
        "改远端权限：ells sftp chmod web 600 /home/app/.env",
        "Change remote permissions: ells sftp chmod web 600 /home/app/.env",
    ),
    (
        "八进制，如 600 / 0644",
        "octal, e.g. 600 / 0644",
    ),
    (
        "只起端口转发并常驻：ells tunnel web（Ctrl-C 结束）",
        "Start only port forwarding and stay running: ells tunnel web (Ctrl-C to stop)",
    ),
    (
        "只起端口转发并常驻：ells tunnel web（Ctrl-C 结束）\n\n每一行状态都带着这条隧道实际绑上的本地端口，留空（自动分配）的会标出\"（自动）\"， 所以 `ells tunnel web` 也是查\"这个 db 到底开在哪个口\"的命令。 绑定失败会点名是谁占着同一个口（另一台 ells 主机，还是外部程序）。",
        "Start only port forwarding and stay running: ells tunnel web (Ctrl-C to stop)\n\nEvery status line carries the local port this tunnel actually bound; an auto-assigned one is marked \"(auto)\", so `ells tunnel web` is also how you ask \"which port is db really on\". A failed bind names whoever holds the same port (another ells host or an outside program).",
    ),
    (
        "拉起所有配了转发规则的主机",
        "start every host that has forwarding rules",
    ),
    (
        "写到文件而不是标准输出",
        "write to a file instead of stdout",
    ),
    (
        "打印 shell 补全脚本：ells completions bash",
        "Print a shell completion script: ells completions bash",
    ),
    ("要直连的主机别名（省略则打开主机列表）", "alias of the host to connect to (omit it to open the host list)"),
    ("远端命令（用 `--` 与 ells 自己的选项分开）", "remote command (separate it from ells's own options with `--`)"),
    ("允许覆盖已存在的目标（不给就报错退出）", "overwrite an existing target (otherwise it errors out)"),
    ("本地目录或文件名（默认当前目录同名）", "local directory or file name (defaults to the same name in the current directory)"),
    ("远端路径（默认同名落到远端家目录）", "remote path (defaults to the same name under the remote home)"),
    ("删远端文件（目录要 --recursive）", "delete a remote file (add --recursive for a directory)"),
    ("主机别名（配合 --all 时可省略）", "host alias (may be omitted with --all)"),
    ("导出成 ~/.ssh/config 片段（永不含密码）", "export as a ~/.ssh/config fragment (never contains passwords)"),
];

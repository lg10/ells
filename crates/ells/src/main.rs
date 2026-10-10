mod app;
mod cli;
mod dialog;
mod events;
mod highlight;
mod keybinds;
mod session;
mod settings;
mod term;
mod theme;
mod ui;
mod update;
mod zmodem;

use anyhow::Result;
use clap::Parser;

use crate::term::restore_terminal;

#[derive(Parser, Debug)]
#[command(
    name = "ells",
    version,
    about = "ells — 跨平台终端 SSH 客户端（内嵌终端 / rz-sz 自动转 SFTP / 主密码保险库）",
    before_help = "用法示例：\n  ells            打开主机列表\n  ells <别名>      直连指定主机（安装后的短命令 `s <别名>` 同义）\n  ells list        无界面列出主机（--format json 给脚本用）\n  ells exec web -- uname -a   跑一条命令就退出\n  ells sftp get web /var/log/app.log ./\n  ells tunnel web  只起这台配好的端口转发\n  ells export      导出 ~/.ssh/config 片段（永不含密码）\n  ells --help      查看完整帮助",
    after_help = "快捷键（列表页）：↑↓ 选择 · Enter 连接 · a 新增 · e 编辑 · d 删除 · i 导入 ~/.ssh/config · x 导出 ssh_config · p 转发规则表格 · m 端口映射 · / 过滤 · f 收藏 · Space 折叠当前分组 · z 全折叠 · o 循环排序 · t 隧道 · h 密钥 · l 操作记录 · v 会话记录 · s 设置 · ? 帮助 · q 退出\n  隧道面板（t）里：空格/s 启停 · Enter 用表格改这台机器的规则 · m 端口映射总表 · x 全部停止\n\n端口转发：本地端口留空（或写 0）＝交给系统分配，两台主机想要同一个口就不会互挤；\n  表单里可以只写目标（`-L db:5432`），实际绑上的端口在「端口映射」里看，启动失败会点名是谁占着。\n\n主机指标：会话页底部固定一行 CPU / 内存 / 磁盘百分比（磁盘取用得最满的那个挂载点，比例照抄 df 的 Capacity 列；三格里只有标签、条和百分比，1/5/15 分钟负载与挂载点已用/总量都排在整行末尾，摆得下才写）。优先在那条已认证连接已有的 SFTP 会话上读 /proc、用 statvfs 问本地盘，服务器不配合才回落一条一次性 exec 只读探针（只填空着的那几格）。连上立刻跑第一轮，内存 / 磁盘 / 负载当场就有数；CPU 是两次快照做差，那一格约两秒后填上。节奏是分开的：/proc 那三样 5 秒一轮，磁盘那一格 5 秒也跟着换数、但每轮只对「当前最满那块」问一次 statvfs（挂载点表缓存在这条连接上），每 60 秒才重做一次普查 —— 重读挂载点、逐块问一遍、重新决定哪块最满；服务器没有 statvfs 扩展时跟单轮什么都不问，只在 60 秒的普查点补一条 df，绝不让 df 退化成每 5 秒一次；没在看的标签降到 60 秒一轮，切回去当场重采。不另开连接、不碰你正在敲的 PTY；采不到画 —，连续三轮什么都拿不到就停止轮询、把这一行还给终端；通道级失败（超时、开不了通道）只把节奏退到 5 → 10 → 30 → 60 秒，拿到数就回到 5 秒。开关：设置 → 主机指标（settings.ini 的 metrics=）。\n\n子命令（无界面，脚本 / CI 用）：list · exec · sftp · tunnel · export · completions\n  注意：子命令名优先于别名位置参数，所以名为 list/exec/sftp/tunnel/export/completions 的主机别名不能被 `ells <别名>` 直连，请改名或进列表连接。\n\n配置文件：\n  ~/.ells/vault.bin     加密后的主机与凭据（argon2id + XChaCha20-Poly1305）\n  ~/.ells/settings.ini  全局设置（含列表排序 list_sort=、主机指标 metrics=）\n  ~/.ells/known_hosts   主机密钥记录（首次连接时确认，之后校验）\n  ~/.ells/audit.log     操作记录（连接、传输、密钥变更；绝不含密码）\n  ~/.ells/logs/         会话记录（终端原始输出，单份 8 MiB、留 30 天）\n  ~/.ells/update.cache  上次更新检查（只有版本号和时刻）\n\n环境变量：\n  ELLS_LOG=1            把内部日志写到 stderr（排障用）\n  ELLS_YES=1            首次见到的主机密钥自动接受并记录（密钥变更仍会拒绝）\n  ELLS_MASTER=…         无头子命令用的主密码（管道里跑时也可用 stdin 第一行）\n  ELLS_ZMODEM_LOG=1     额外把 sz/rz 拦截诊断写入 ~/.ells/zmodem.log\n  ELLS_API_URL=…        更新检查用的发布查询地址（镜像 / 内网）\n  ELLS_DOWNLOAD_URL=…   更新下载用的资产基址（镜像 / 内网）"
)]
struct Cli {
    /// 要直连的主机别名（省略则打开主机列表）
    #[arg(value_name = "别名")]
    alias: Option<String>,

    /// 无头子命令：不进界面也能用 ells（详见 `ells list --help`）
    #[command(subcommand)]
    cmd: Option<cli::Command>,

    /// 跳过主密码解锁，改从明文文件 ~/.ells/hosts.dev.toml 读取主机（仅开发调试用）
    #[arg(long)]
    dev: bool,

    /// 首次连接新主机时自动接受并记录主机密钥；密钥变更时仍然拒绝
    ///
    /// 不用 clap 的 `env`：它只认 true/false，而我们对外文档写的是 `ELLS_YES=1`，
    /// 脚本里 `export ELLS_YES=1` 会直接被 clap 判成用法错误。
    #[arg(short = 'y', long)]
    yes: bool,

    /// 只检查有没有新版本：打印结果就退出，不下载、不启动界面（脚本 / 排障用）
    #[arg(long)]
    check_update: bool,
}

fn main() -> Result<()> {
    install_panic_hook();
    if std::env::var("ELLS_LOG").is_ok() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .try_init()
            .ok();
    }
    let Cli {
        alias,
        cmd,
        dev,
        yes,
        check_update,
    } = Cli::parse();
    // 帮助里承诺的是 `ELLS_YES=1`，所以 1/true/yes/on 都算数（clap 的 env 只认真写字面量）
    let yes = yes || env_yes();
    if check_update {
        return check_update_only();
    }

    // 无头子命令：不进界面、不装对话框服务，退出码就是对外契约
    if let Some(cmd) = cmd {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let code = rt.block_on(cli::dispatch(cmd, dev, yes))?;
        // exit 之前必须冲刷：输出被重定向到文件时 stdout 是全缓冲的
        use std::io::Write;
        std::io::stdout().flush().ok();
        std::process::exit(code);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let (tx, rx) = std::sync::mpsc::channel();
    dialog::install(tx);

    let app_thread = std::thread::Builder::new()
        .name("ells-app".into())
        .spawn(move || {
            let _service = dialog::ShutdownGuard;
            let result = rt.block_on(app::run(alias, dev, yes));
            // 故意不回收 runtime：未完成的对话框任务会让 drop 永久等待，进程退出时内核自会回收
            std::mem::forget(rt);
            result
        })?;

    // 主线程只服务系统文件对话框：macOS 的 AppKit 面板不允许在其他线程上创建
    dialog::run_service(rx);

    let restart = match app_thread.join() {
        Ok(result) => result?,
        Err(_) => anyhow::bail!(
            "ells 主循环异常退出，屏幕已恢复；详情见上方错误信息"
        ),
    };
    if restart {
        // 必须在主线程执行：unix 的 exec 原地替换进程映像，pid/进程组/控制终端
        // 全部保留，新版本才能直接接管屏幕；Windows 是 spawn 后正常退出
        update::restart().map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

/// `ELLS_YES`：文档和 CI 里习惯写 `=1`，所以 1/true/yes/on 都算放行首次密钥。
fn env_yes() -> bool {
    matches!(
        std::env::var("ELLS_YES")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "y" | "on"
    )
}

/// `ells --check-update`：只查版本就退出。界面里能不能提示更新，取决于这条网络路径
/// 走不走得通，而在无头环境里唯一可验证的入口就是它。
fn check_update_only() -> Result<()> {
    match update::check() {
        Ok(Some(tag)) => println!("发现新版本 {tag}（当前 v{}），运行 ells 后点顶部徽标即可更新", update::current_version()),
        Ok(None) => println!("已是最新版本 v{}", update::current_version()),
        Err(msg) => anyhow::bail!("检查更新失败：{msg}"),
    }
    Ok(())
}

/// panic 默认不做任何终端收尾：raw mode / 备用屏 / 鼠标捕获会留在 shell 里
/// （表现为看不见输入、残留 TUI 画面），且错误信息被打成乱码。
/// 这里先把终端还原，再给一句可读的中文结论。
fn install_panic_hook() {
    let verbose = std::env::var("ELLS_LOG").is_ok();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        let detail = panic_detail(info);
        eprintln!("\x1b[31mells 遇到未预期的错误：{detail}\x1b[0m");
        eprintln!("终端已恢复。若认为这是 ells 的问题，请到 https://github.com/lg10/ells/issues 附上以上信息。");
        if verbose {
            tracing::error!(%detail, "panic");
        }
    }));
}

fn panic_detail(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "未知错误".to_string()
    };
    match info.location() {
        Some(loc) => format!("{payload}（{}:{}）", loc.file(), loc.line()),
        None => payload,
    }
}

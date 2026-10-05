mod app;
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
    before_help = "用法示例：\n  ells            打开主机列表\n  ells <别名>      直连指定主机（安装后的短命令 `s <别名>` 同义）\n  ells --help      查看完整帮助",
    after_help = "快捷键（列表页）：↑↓ 选择 · Enter 连接 · a 新增 · e 编辑 · d 删除 · i 导入 ~/.ssh/config · ? 帮助 · q 退出\n\n配置文件：\n  ~/.ells/vault.bin     加密后的主机与凭据（argon2id + XChaCha20-Poly1305）\n  ~/.ells/settings.ini  全局设置\n  ~/.ells/known_hosts   主机密钥记录（首次连接时确认，之后校验）\n  ~/.ells/update.cache  上次更新检查（只有版本号和时刻）\n\n环境变量：\n  ELLS_LOG=1            把内部日志写到 stderr（排障用）\n  ELLS_YES=1            首次见到的主机密钥自动接受并记录（密钥变更仍会拒绝）\n  ELLS_ZMODEM_LOG=1     额外把 sz/rz 拦截诊断写入 ~/.ells/zmodem.log\n  ELLS_API_URL=…        更新检查用的发布查询地址（镜像 / 内网）\n  ELLS_DOWNLOAD_URL=…   更新下载用的资产基址（镜像 / 内网）"
)]
struct Cli {
    /// 要直连的主机别名（省略则打开主机列表）
    #[arg(value_name = "别名")]
    alias: Option<String>,

    /// 跳过主密码解锁，改从明文文件 ~/.ells/hosts.dev.toml 读取主机（仅开发调试用）
    #[arg(long)]
    dev: bool,

    /// 首次连接新主机时自动接受并记录主机密钥；密钥变更时仍然拒绝
    #[arg(short = 'y', long, env = "ELLS_YES")]
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
    let cli = Cli::parse();
    if cli.check_update {
        return check_update_only();
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let (tx, rx) = std::sync::mpsc::channel();
    dialog::install(tx);

    let yes = cli.yes;
    let app_thread = std::thread::Builder::new()
        .name("ells-app".into())
        .spawn(move || {
            let _service = dialog::ShutdownGuard;
            let result = rt.block_on(app::run(cli.alias, cli.dev, yes));
            // 故意不回收 runtime：未完成的对话框任务会让 drop 永久等待，进程退出时内核自会回收
            std::mem::forget(rt);
            result
        })?;

    // 主线程只服务系统文件对话框：macOS 的 AppKit 面板不允许在其他线程上创建
    dialog::run_service(rx);

    match app_thread.join() {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "ells 主循环异常退出，屏幕已恢复；详情见上方错误信息"
        )),
    }
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

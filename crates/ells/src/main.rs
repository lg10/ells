mod app;
mod dialog;
mod events;
mod highlight;
mod session;
mod settings;
mod ui;
mod zmodem;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "ells", version, about = "TUI SSH client with rz/sz -> SFTP interception")]
struct Cli {
    /// Host alias to connect to directly (skips the list)
    alias: Option<String>,

    /// Skip the master-password unlock and read hosts from a plaintext file (dev only)
    #[arg(long)]
    dev: bool,
}

fn main() -> Result<()> {
    if std::env::var("ELLS_LOG").is_ok() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .try_init()
            .ok();
    }
    let cli = Cli::parse();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let (tx, rx) = std::sync::mpsc::channel();
    dialog::install(tx);

    let app_thread = std::thread::Builder::new()
        .name("ells-app".into())
        .spawn(move || {
            let _service = dialog::ShutdownGuard;
            let result = rt.block_on(app::run(cli.alias, cli.dev));
            // 故意不回收 runtime：未完成的对话框任务会让 drop 永久等待，进程退出时内核自会回收
            std::mem::forget(rt);
            result
        })?;

    // 主线程只服务系统文件对话框：macOS 的 AppKit 面板不允许在其他线程上创建
    dialog::run_service(rx);

    match app_thread.join() {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("ells 主循环异常退出")),
    }
}

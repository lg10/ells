mod app;
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

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("ELLS_LOG").is_ok() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .try_init()
            .ok();
    }
    let cli = Cli::parse();
    app::run(cli.alias, cli.dev).await
}

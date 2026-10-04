//! SFTP transfer engine: directory listing plus upload/download with
//! throttled progress events. The Zmodem handshake sniffer (M3) will reuse
//! this module to reroute rz/sz traffic through SFTP.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Upload,
    Download,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub label: String,
    pub transferred: u64,
    pub total: Option<u64>,
    pub bytes_per_sec: f64,
}

impl Progress {
    pub fn ratio(&self) -> Option<f64> {
        self.total.map(|t| if t == 0 { 1.0 } else { self.transferred as f64 / t as f64 })
    }
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

pub fn remote_join(dir: &str, name: &str) -> String {
    if dir == "/" || dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

pub fn remote_parent(dir: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(pos) => trimmed[..pos].to_string(),
        None => "/".to_string(),
    }
}

pub async fn list(sftp: &SftpSession, dir: &str) -> Result<Vec<FileEntry>> {
    let rd = sftp
        .read_dir(dir)
        .await
        .with_context(|| format!("无法读取远端目录 {dir}"))?;
    let mut out: Vec<FileEntry> = rd
        .map(|entry| {
            let name = entry.file_name();
            FileEntry {
                path: entry.path(),
                is_dir: entry.metadata().is_dir(),
                size: entry.metadata().len(),
                name,
            }
        })
        .collect();
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

pub async fn upload(
    sftp: &SftpSession,
    local: &Path,
    remote: String,
    tx: UnboundedSender<Progress>,
) -> Result<()> {
    let label = file_label(local);
    let total = tokio::fs::metadata(local)
        .await
        .with_context(|| format!("无法读取本地文件 {}", local.display()))?
        .len();
    let src = tokio::fs::File::open(local)
        .await
        .with_context(|| format!("无法打开本地文件 {}", local.display()))?;
    let dst = sftp
        .create(&remote)
        .await
        .with_context(|| format!("无法创建远端文件 {remote}"))?;
    pump(src, dst, Some(total), label, tx).await
}

pub async fn download(
    sftp: &SftpSession,
    remote: String,
    local_dir: &Path,
    tx: UnboundedSender<Progress>,
    name: &str,
) -> Result<PathBuf> {
    let total = sftp
        .metadata(&remote)
        .await
        .map(|md| md.len())
        .ok();
    let src = sftp
        .open(&remote)
        .await
        .with_context(|| format!("无法打开远端文件 {remote}"))?;
    tokio::fs::create_dir_all(local_dir)
        .await
        .with_context(|| format!("无法创建本地目录 {}", local_dir.display()))?;
    let dest = local_dir.join(name);
    let dst = tokio::fs::File::create(&dest)
        .await
        .with_context(|| format!("无法创建本地文件 {}", dest.display()))?;
    pump(src, dst, total, name.to_string(), tx).await?;
    Ok(dest)
}

async fn pump<R, W>(
    mut read: R,
    mut write: W,
    total: Option<u64>,
    label: String,
    tx: UnboundedSender<Progress>,
) -> Result<()>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    const CHUNK: usize = 32 * 1024;
    const EMIT_INTERVAL: Duration = Duration::from_millis(150);

    let mut buf = vec![0u8; CHUNK];
    let mut done: u64 = 0;
    let start = Instant::now();
    let mut last_emit = start;
    loop {
        let n = read.read(&mut buf).await.context("读取数据失败")?;
        if n == 0 {
            break;
        }
        write.write_all(&buf[..n]).await.context("写入数据失败")?;
        done += n as u64;
        if last_emit.elapsed() >= EMIT_INTERVAL {
            emit(&tx, &label, done, total, start.elapsed());
            last_emit = Instant::now();
        }
    }
    write.flush().await.ok();
    write.shutdown().await.ok();
    emit(&tx, &label, done, total, start.elapsed());
    Ok(())
}

fn emit(tx: &UnboundedSender<Progress>, label: &str, done: u64, total: Option<u64>, elapsed: Duration) {
    let secs = elapsed.as_secs_f64().max(0.001);
    let _ = tx.send(Progress {
        label: label.to_string(),
        transferred: done,
        total,
        bytes_per_sec: done as f64 / secs,
    });
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

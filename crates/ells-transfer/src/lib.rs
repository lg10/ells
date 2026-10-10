//! SFTP 传输引擎：目录浏览、上传/下载（含递归目录）、进度事件、取消、远端改名/删除。
//!
//! 三条硬规则：
//! 1. 覆盖必须先由调用方确认（`remote_meta`/本地存在性检查），引擎本身不猜；
//! 2. 任何一次读写之间都检查取消位，Ctrl-C 能在一个块内停下；
//! 3. 递归传输对外只报一条累计进度，用户看到的是一根条而不是刷屏的条。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
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

/// 传输目标的存在性/类型预检结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub is_dir: bool,
    pub size: u64,
}

/// 一次传输的可取消位（多个并发传输可共享同一个 Cancel：一次 Ctrl-C 全停）。
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// 取消导致的失败（调用方用 `err.downcast_ref::<Cancelled>()` 区分）。
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("传输已取消")
    }
}

impl std::error::Error for Cancelled {}

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

/// 在 `dir` 下为 `name` 找一个不冲突的名字：`a.txt` → `a (1).txt` → `a (2).txt`。
/// `taken` 由调用方提供（远端查 sftp、本地查 fs），引擎不关心冲突的来源。
pub fn unique_in<T: Fn(&str) -> bool>(name: &str, taken: T) -> String {
    if !taken(name) {
        return name.to_string();
    }
    let (stem, ext) = split_name(name);
    for i in 1..1000 {
        let candidate = match ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{stem}-{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0))
}

/// `a.tar.gz` 按最后一段扩展名切分（够用且不 surprises 用户）。
fn split_name(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(pos) if pos > 0 => (&name[..pos], Some(&name[pos + 1..])),
        _ => (name, None),
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

/// 远端路径的类型；不存在时返回 None。
pub async fn remote_meta(sftp: &SftpSession, path: &str) -> Result<Option<Meta>> {
    match sftp.metadata(path).await {
        Ok(md) => Ok(Some(Meta {
            is_dir: md.is_dir(),
            size: md.len(),
        })),
        Err(russh_sftp::client::error::Error::Status(status))
            if status.status_code == russh_sftp::protocol::StatusCode::NoSuchFile =>
        {
            Ok(None)
        }
        Err(err) => Err(anyhow!("无法读取远端路径 {path}: {err}")),
    }
}

pub async fn mkdir(sftp: &SftpSession, path: &str) -> Result<()> {
    sftp.create_dir(path)
        .await
        .with_context(|| format!("无法创建远端目录 {path}"))
}

/// 把用户写的八进制权限串解析成 `chmod` 要的数值。
///
/// 界面里手打、CLI 里参数、脚本里拼字符串都会走到这里，所以规则只有一份：
/// `644` / `0644` / `0o644` / `0` / `0000` 都合法；`7778`、`abc`、空串是错的。
/// 特别留意全零：`trim_start_matches('0')` 会把它削成空串，那是一次合法的
/// `chmod 000`，不能当成"没填"。
pub fn parse_mode(text: &str) -> Option<u32> {
    let trimmed = text.trim();
    let cleaned = trimmed.strip_prefix("0o").unwrap_or(trimmed);
    if cleaned.is_empty() || !cleaned.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // 全零会被 trim_start_matches('0') 削成空串：那是合法的 0000，不是"没填"
    let bits = if cleaned.chars().all(|c| c == '0') {
        0
    } else {
        u32::from_str_radix(cleaned.trim_start_matches('0'), 8).ok()?
    };
    (bits <= 0o7777).then_some(bits)
}

/// 改远端权限（八进制数字，如 `0o600`）。
///
/// 只送权限位：属主、大小、时间全部留 `None`，SFTP 的 `setstat` 对缺省字段不动，
/// 一次 chmod 不该顺手把文件改成别人的或把 mtime 抹平。
pub async fn chmod(sftp: &SftpSession, path: &str, mode: u32) -> Result<()> {
    if mode > 0o7777 {
        bail!("权限超出 07777：{mode:o}");
    }
    let attrs = russh_sftp::protocol::FileAttributes {
        permissions: Some(mode),
        ..Default::default()
    };
    sftp.set_metadata(path, attrs)
        .await
        .with_context(|| format!("无法修改 {path} 的权限"))
}

pub async fn rename(sftp: &SftpSession, from: &str, to: &str) -> Result<()> {
    sftp.rename(from, to)
        .await
        .with_context(|| format!("无法重命名 {from}"))
}

/// 删除单个文件或空目录。
pub async fn remove_one(sftp: &SftpSession, path: &str, is_dir: bool) -> Result<()> {
    if is_dir {
        sftp.remove_dir(path)
            .await
            .with_context(|| format!("无法删除远端目录 {path}（非空目录请先清空）"))
    } else {
        sftp.remove_file(path)
            .await
            .with_context(|| format!("无法删除远端文件 {path}"))
    }
}

/// 递归删除：先收集整棵树（前序），删完全部文件后按前序逆序删目录，
/// 保证任何父目录都在其所有子项之后删除。软链目录不会被展开（listing 里不是 dir），
/// 因此不存在环路风险。
pub async fn remove_tree(sftp: &SftpSession, path: &str, cancel: &Cancel) -> Result<usize> {
    let Some(meta) = remote_meta(sftp, path).await? else {
        bail!("{path} 不存在");
    };
    if !meta.is_dir {
        remove_one(sftp, path, false).await?;
        return Ok(1);
    }
    let mut dirs: Vec<String> = vec![path.to_string()];
    let mut files: Vec<String> = Vec::new();
    let mut i = 0;
    while i < dirs.len() {
        if cancel.is_cancelled() {
            return Err(Cancelled.into());
        }
        for entry in list(sftp, &dirs[i]).await? {
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            if entry.is_dir {
                dirs.push(entry.path);
            } else {
                files.push(entry.path);
            }
        }
        i += 1;
    }
    let mut count = 0;
    for file in &files {
        if cancel.is_cancelled() {
            return Err(Cancelled.into());
        }
        remove_one(sftp, file, false).await?;
        count += 1;
    }
    for dir in dirs.iter().rev() {
        remove_one(sftp, dir, true).await?;
        count += 1;
    }
    Ok(count)
}

pub async fn upload(
    sftp: &SftpSession,
    local: &Path,
    remote: String,
    tx: UnboundedSender<Progress>,
    cancel: &Cancel,
) -> Result<()> {
    // 进度条用远端名字：调用方按它登记条目，"改名保留双方"后本地名与远端名会不同
    let label = remote
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| file_label(local));
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
    pump(src, dst, Some(total), label, tx, cancel).await
}

/// 递归上传：保持目录结构，同名远端文件被覆盖（调用方已就"覆盖"取得用户同意）。
/// 目录树落在 `remote_dir/name`（`name` 可与本地目录名不同，用于"改名保留双方"）。
/// 对外报一条累计进度。
pub async fn upload_tree(
    sftp: &SftpSession,
    local: &Path,
    remote_dir: &str,
    name: &str,
    tx: UnboundedSender<Progress>,
    cancel: &Cancel,
) -> Result<u64> {
    let label = name.to_string();
    let mut plan: Vec<(PathBuf, String, u64)> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    collect_local(
        local,
        &remote_join(remote_dir, name),
        &mut plan,
        &mut dirs,
    )?;
    let total: u64 = plan.iter().map(|(_, _, s)| *s).sum();
    for dir in dirs {
        if cancel.is_cancelled() {
            return Err(Cancelled.into());
        }
        if remote_meta(sftp, &dir).await?.is_none() {
            mkdir(sftp, &dir).await?;
        }
    }
    let mut done = 0u64;
    for (src, dst, _) in plan {
        if cancel.is_cancelled() {
            return Err(Cancelled.into());
        }
        let reader = tokio::fs::File::open(&src)
            .await
            .with_context(|| format!("无法打开本地文件 {}", src.display()))?;
        let writer = sftp
            .create(&dst)
            .await
            .with_context(|| format!("无法创建远端文件 {dst}"))?;
        // 每个文件单独计进度，但都归到同一条 label 上（累计 = 已传 + 当前文件内）
        done += pump_at(reader, writer, &label, &tx, cancel, done, total).await?;
    }
    Ok(total)
}

/// 递归下载：远端目录树落到本地 `local_dir/name`（`name` 可与远端目录名不同，
/// 用于"改名保留双方"），同名本地文件被覆盖（调用方已确认）。
pub async fn download_tree(
    sftp: &SftpSession,
    remote: &str,
    local_dir: &Path,
    name: &str,
    tx: UnboundedSender<Progress>,
    cancel: &Cancel,
) -> Result<PathBuf> {
    let dest_root = local_dir.join(name);
    let mut plan: Vec<(String, PathBuf, u64)> = Vec::new();
    let mut dirs: Vec<PathBuf> = vec![dest_root.clone()];
    collect_remote(sftp, remote, &dest_root, &mut plan, &mut dirs, cancel).await?;
    let total: u64 = plan.iter().map(|(_, _, s)| *s).sum();
    for dir in dirs {
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("无法创建本地目录 {}", dir.display()))?;
    }
    let mut done = 0u64;
    for (src, dst, _) in plan {
        if cancel.is_cancelled() {
            return Err(Cancelled.into());
        }
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let reader = sftp
            .open(&src)
            .await
            .with_context(|| format!("无法打开远端文件 {src}"))?;
        let writer = tokio::fs::File::create(&dst)
            .await
            .with_context(|| format!("无法创建本地文件 {}", dst.display()))?;
        done += pump_at(reader, writer, name, &tx, cancel, done, total).await?;
    }
    Ok(dest_root)
}

/// 把本地路径 `local` 规划到远端 `target`：目录递归展开，软链接整个跳过
/// （跟随软链接可能形成环路，且远端也丢了链接语义）。
fn collect_local(
    local: &Path,
    target: &str,
    plan: &mut Vec<(PathBuf, String, u64)>,
    dirs: &mut Vec<String>,
) -> Result<()> {
    let md = std::fs::metadata(local)
        .with_context(|| format!("无法读取本地路径 {}", local.display()))?;
    if md.is_dir() {
        dirs.push(target.to_string());
        for entry in std::fs::read_dir(local)
            .with_context(|| format!("无法读取本地目录 {}", local.display()))?
        {
            let entry = entry?;
            if entry.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            collect_local(&entry.path(), &remote_join(target, &name), plan, dirs)?;
        }
    } else {
        plan.push((local.to_path_buf(), target.to_string(), md.len()));
    }
    Ok(())
}

fn collect_remote<'a>(
    _sftp: &'a SftpSession,
    _remote: &'a str,
    _local: &'a Path,
    _plan: &'a mut Vec<(String, PathBuf, u64)>,
    _dirs: &'a mut Vec<PathBuf>,
    _cancel: &'a Cancel,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    let sftp = _sftp;
    let remote = _remote;
    let local = _local;
    let plan = _plan;
    let dirs = _dirs;
    let cancel = _cancel;
    Box::pin(async move {
        if cancel.is_cancelled() {
            return Err(Cancelled.into());
        }
        for entry in list(sftp, remote).await? {
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            let dest = local.join(&entry.name);
            if entry.is_dir {
                dirs.push(dest.clone());
                collect_remote(sftp, &entry.path, &dest, plan, dirs, cancel).await?;
            } else {
                plan.push((entry.path, dest, entry.size));
            }
        }
        Ok(())
    })
}

pub async fn download(
    sftp: &SftpSession,
    remote: String,
    local_dir: &Path,
    tx: UnboundedSender<Progress>,
    name: &str,
    cancel: &Cancel,
) -> Result<PathBuf> {
    let total = sftp.metadata(&remote).await.map(|md| md.len()).ok();
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
    pump(src, dst, total, name.to_string(), tx, cancel).await?;
    Ok(dest)
}

async fn pump<R, W>(
    read: R,
    write: W,
    total: Option<u64>,
    label: String,
    tx: UnboundedSender<Progress>,
    cancel: &Cancel,
) -> Result<()>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let grand = total.unwrap_or(0);
    pump_at(read, write, &label, &tx, cancel, 0, grand).await?;
    Ok(())
}

/// 搬一个文件：`base`/`grand` 用于把当前文件的字节数折算进整棵树的累计进度。
/// 返回实际搬动的字节数。
async fn pump_at<R, W>(
    mut read: R,
    mut write: W,
    label: &str,
    tx: &UnboundedSender<Progress>,
    cancel: &Cancel,
    base: u64,
    grand: u64,
) -> Result<u64>
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
        if cancel.is_cancelled() {
            return Err(Cancelled.into());
        }
        let n = read.read(&mut buf).await.context("读取数据失败")?;
        if n == 0 {
            break;
        }
        write.write_all(&buf[..n]).await.context("写入数据失败")?;
        done += n as u64;
        if last_emit.elapsed() >= EMIT_INTERVAL {
            let _ = tx.send(Progress {
                label: label.to_string(),
                transferred: base + done,
                total: if grand > 0 { Some(grand) } else { None },
                bytes_per_sec: (base + done) as f64 / start.elapsed().as_secs_f64().max(0.001),
            });
            last_emit = Instant::now();
        }
    }
    write.flush().await.ok();
    write.shutdown().await.ok();
    let _ = tx.send(Progress {
        label: label.to_string(),
        transferred: base + done,
        total: if grand > 0 { Some(grand) } else { None },
        bytes_per_sec: (base + done) as f64 / start.elapsed().as_secs_f64().max(0.001),
    });
    Ok(done)
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// 本地是否已有同名文件（下载/上传前的冲突预检）。
pub fn local_exists(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_the_ways_people_type_them() {
        assert_eq!(parse_mode("644"), Some(0o644));
        assert_eq!(parse_mode(" 0644 "), Some(0o644));
        assert_eq!(parse_mode("0o600"), Some(0o600));
        assert_eq!(parse_mode("755"), Some(0o755));
        assert_eq!(parse_mode("4755"), Some(0o4755), "setuid 位要留得住");
    }

    #[test]
    fn all_zeros_is_a_real_mode_not_an_empty_field() {
        assert_eq!(parse_mode("0"), Some(0));
        assert_eq!(parse_mode("000"), Some(0));
        assert_eq!(parse_mode("0000"), Some(0));
    }

    #[test]
    fn nonsense_modes_are_rejected() {
        for bad in ["", "  ", "abc", "64x", "7778", "0o", "6.4", "-1", "177777"] {
            assert_eq!(parse_mode(bad), None, "{bad} 不该被接受");
        }
    }

    #[test]
    fn join_and_parent_roundtrip() {
        assert_eq!(remote_join("/srv", "app"), "/srv/app");
        assert_eq!(remote_join("/", "srv"), "/srv");
        assert_eq!(remote_parent("/srv/app"), "/srv");
        assert_eq!(remote_parent("/srv"), "/");
        assert_eq!(remote_parent("/"), "/");
    }

    #[test]
    fn unique_name_avoids_conflicts() {
        let taken = |n: &str| n == "report.pdf" || n == "report (1).pdf";
        assert_eq!(unique_in("readme.md", |_| false), "readme.md");
        assert_eq!(unique_in("report.pdf", taken), "report (2).pdf");
        assert_eq!(unique_in("noext", |n| n == "noext"), "noext (1)");
    }

    #[test]
    fn split_name_keeps_multi_dot_stem() {
        assert_eq!(split_name("a.tar.gz"), ("a.tar", Some("gz")));
        assert_eq!(split_name("Makefile"), ("Makefile", None));
        assert_eq!(split_name(".hidden"), (".hidden", None));
    }

    #[test]
    fn cancel_flag_is_shared() {
        let a = Cancel::default();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled());
    }
}

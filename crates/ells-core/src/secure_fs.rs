//! 凭据文件的安全写入：同目录临时文件 → 写全 → fsync → rename 覆盖。
//!
//! `create(true).truncate(true)` 的原地覆写在崩溃或断电后会留下**半截文件**，
//! 而 `vault.bin` 损坏是无法挽回的——主密码不可能重建。rename 是目录项的原子
//! 替换，因此 `vault.bin` 永远要么完整的旧内容、要么完整的新内容。
//!
//! 临时文件必须与目标同目录：跨设备 rename 会失败（EXDEV），Windows 上更是
//! 直接报错，所以不能放 `std::env::temp_dir()`。

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// 原子写入 `path`。Unix 下把权限收到 0600（含临时文件），Windows 依赖
/// `harden_config_dir` 给 `~/.ells` 打的继承 ACL。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = temp_path(path);
    let result = write_then_rename(path, bytes, &tmp);
    if result.is_err() {
        // 失败不留半成品：下次启动不该看到一份来历不明的密钥副本
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn write_then_rename(path: &Path, bytes: &[u8], tmp: &Path) -> Result<()> {
    if let Some(dir) = parent_dir(path) {
        std::fs::create_dir_all(dir).with_context(|| format!("无法创建目录 {}", dir.display()))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(tmp)
        .with_context(|| format!("无法写入临时文件 {}", tmp.display()))?;
    f.write_all(bytes)?;
    // 数据必须先落盘，rename 才有意义：否则重启后目录项指向一块空临时文件
    f.sync_all()
        .with_context(|| format!("临时文件 {} 未落盘", tmp.display()))?;
    drop(f);

    std::fs::rename(tmp, path).with_context(|| {
        format!("无法把 {} 换成新内容（文件正被占用？）", path.display())
    })?;
    sync_parent(path);
    Ok(())
}

/// rename 本身只是目录项改动，Unix 上要 fsync 父目录才保证重启后还在。
#[cfg(unix)]
fn sync_parent(path: &Path) {
    if let Some(dir) = parent_dir(path) {
        if let Ok(f) = std::fs::File::open(dir) {
            let _ = f.sync_all();
        }
    }
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) {}

fn parent_dir(path: &Path) -> Option<&Path> {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => Some(p),
        _ => Some(Path::new(".")),
    }
}

/// `<原名>.tmp-<pid>-<纳秒>`：同目录、绝不与目标同名，并发写也互不覆盖。
fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ells-secret".to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    path.with_file_name(format!("{name}.tmp-{}-{nanos}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("ells-fs-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn overwrite_keeps_old_content_intact_when_write_fails() {
        // 目标写不进去时（这里让父路径是一个普通文件），旧内容必须原样留着
        let dir = temp_dir("crash");
        let path = dir.join("vault.bin");
        std::fs::write(&path, b"good vault").unwrap();

        std::fs::write(dir.join("not-a-dir"), b"").unwrap();
        let broken = dir.join("not-a-dir").join("vault.bin");
        assert!(write_atomic(&broken, b"half written").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"good vault");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_file() {
        let dir = temp_dir("round");
        let path = dir.join("settings.ini");
        write_atomic(&path, b"theme=dark\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theme=dark\n");
        write_atomic(&path, b"theme=light\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theme=light\n");
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件应被改名走: {leftovers:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = temp_dir("nested");
        let path = dir.join("deep").join("deeper").join("known_hosts");
        write_atomic(&path, b"h1 ssh-ed25519 AAAA\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "h1 ssh-ed25519 AAAA\n");
        std::fs::remove_dir_all(&dir).ok();
    }
}

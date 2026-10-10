//! 会话落盘：一次连接一个文件，`~/.ells/logs/<别名>-<北京时间>.log`。
//!
//! 记的是**远端打回来的字节**（与 `script(1)` 同），所以里面带着控制序列：
//! `less -R` 打开是带颜色的，界面里的查看器用 [`plain`] 去掉序列再显示。
//! 单个文件写满 `MAX_SESSION_BYTES` 就停（末尾留一句说明），进程启动时清理
//! `KEEP_DAYS` 天以前的旧文件——日志不该把用户磁盘吃干净。
//!
//! 红线：密码不会回显，所以正常输密码不会进日志；但屏幕上出现过的东西就会进文件，
//! 不想记就在设置里关掉（`session_log=false`）。

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, SecondsFormat, Utc};

/// 单次会话的体积上限：够装下一整天的构建输出，又不至于无限膨胀。
const MAX_SESSION_BYTES: u64 = 8 * 1024 * 1024;
/// 查看器一次最多读这么多：再大就该交给 `less` 了。
pub const PREVIEW_BYTES: u64 = 256 * 1024;
/// 保留天数。
pub const KEEP_DAYS: i64 = 30;

const CAP_NOTE: &str = "\n——— ells：已达单会话 8 MiB 上限，后续输出不再记录 ———\n";

/// `~/.ells/logs`。
pub fn dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ells").join("logs"))
}

/// 北京时间：日志名和页面上的时刻都要能和别人说的"下午那次"对上。
fn now() -> String {
    beijing(Utc::now())
}

fn beijing(instant: DateTime<Utc>) -> String {
    let offset = FixedOffset::east_opt(8 * 3600).expect("+08:00 恒有效");
    instant.with_timezone(&offset).to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// 文件名用的时间戳：`20261009-140322`（冒号和加号在文件名里都是麻烦）。
fn stamp() -> String {
    let offset = FixedOffset::east_opt(8 * 3600).expect("+08:00 恒有效");
    Utc::now()
        .with_timezone(&offset)
        .format("%Y%m%d-%H%M%S")
        .to_string()
}

/// 把别名压成安全文件名：路径分隔符、`..`、控制字符一律换掉。
///
/// 别名是用户输入的，而日志目录只有 `.ells/logs` 一层——不能让一个叫
/// `../../x` 的别名把文件写到别处去。中文别名保留（`is_alphanumeric` 认 Unicode
/// 字母），否则中文机器名全变成一串横线，回看时根本认不出是哪台。
fn sanitize(alias: &str) -> String {
    let mut out: String = alias
        .chars()
        .map(|c| {
            // is_alphanumeric 认 Unicode：中文别名照样能认出是哪台，
            // 而分隔符、空格、控制字符全换成横线
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    // 连续的点或纯点会被当成相对路径成分
    while out.starts_with('.') {
        out.remove(0);
    }
    if out.is_empty() {
        out.push_str("host");
    }
    out.truncate(40);
    out
}

/// 一次会话的记录器。 dropping 就关闭文件，所以重连会自然开一个新文件。
pub struct Recorder {
    path: PathBuf,
    file: Option<std::fs::File>,
    written: u64,
}

impl Recorder {
    /// 开一个会话文件；目录建不出来或打不开就返回 `None`（记录是附加能力，不该拖垮连接）。
    pub fn start(alias: &str) -> Option<Self> {
        let dir = dir()?;
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{}-{}.log", sanitize(alias), stamp()));
        let mut out = Self::at_path(&path)?;
        let header = format!("# ells 会话记录 · {} · {}\n", sanitize(alias), now());
        out.record(header.as_bytes());
        Some(out)
    }

    /// 在指定路径上续写一个会话文件（同秒重名就接着写，不覆盖）。
    fn at_path(path: &Path) -> Option<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Some(Self {
            path: path.to_path_buf(),
            file: Some(file),
            written,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 原样追加远端输出（含控制序列），到顶就停。
    pub fn record(&mut self, bytes: &[u8]) {
        let Some(f) = self.file.as_mut() else {
            return;
        };
        if self.written >= MAX_SESSION_BYTES {
            return;
        }
        let room = (MAX_SESSION_BYTES - self.written) as usize;
        let take = bytes.len().min(room);
        if f.write_all(&bytes[..take]).is_err() {
            // 磁盘满 / 文件被删：放弃这次记录，连接本身不该因此断掉
            self.file = None;
            return;
        }
        let _ = f.flush();
        self.written += take as u64;
        if self.written >= MAX_SESSION_BYTES {
            let _ = f.write_all(CAP_NOTE.as_bytes());
        }
    }

    /// 插一行带时刻的说明（会话建立、断开、被远端关闭）。
    pub fn note(&mut self, text: &str) {
        let line = format!("\n——— {} {} ———\n", now(), text.replace(['\n', '\r'], " · "));
        self.record(line.as_bytes());
    }
}

/// 目录里的一个日志文件。
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    /// 北京时间的修改时刻
    pub mtime: String,
    pub path: PathBuf,
}

/// 最近 `limit` 个会话文件，按修改时间从新到旧。
pub fn list(limit: usize) -> Vec<Entry> {
    let Some(dir) = dir() else { return Vec::new() };
    entries_in(&dir, limit)
}

fn entries_in(dir: &Path, limit: usize) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = read
        .flatten()
        .filter_map(|item| {
            let path = item.path();
            if path.extension().and_then(|e| e.to_str()) != Some("log") {
                return None;
            }
            let meta = item.metadata().ok()?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| DateTime::<Utc>::try_from(t).ok())
                .map(beijing)
                .unwrap_or_default();
            Some(Entry {
                name: path.file_name()?.to_string_lossy().into_owned(),
                size: meta.len(),
                mtime,
                path,
            })
        })
        .collect();
    // 文件名里的别名排在时间前面，排序只能按修改时间；同秒的按名字兜底
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| b.name.cmp(&a.name)));
    out.truncate(limit);
    out
}

/// 读文件尾部并去掉控制序列，供 TUI 查看器显示。
pub fn read_plain(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let start = bytes.len().saturating_sub(PREVIEW_BYTES as usize);
    Some(plain(String::from_utf8_lossy(&bytes[start..]).as_ref()))
}

/// 去掉 ANSI/VT 转义序列，留下人能读的文本。
///
/// 保留 `\n` 与 `\t`，扔掉 `\r`（进度条靠它回退，落到屏幕上只会重复一行）。
pub fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => {
                i = skip_escape(bytes, i);
            }
            b'\r' => i += 1,
            b'\n' | b'\t' => {
                out.push(bytes[i] as char);
                i += 1;
            }
            b if b < 0x20 => i += 1,
            // 非 ASCII 交给下面的 UTF-8 续字节：逐字节只保证不吞掉控制序列，
            // 中文由 from_utf8_lossy 已经处理成合法 char 序列
            _ => {
                let start = i;
                i += 1;
                while i < bytes.len() && (bytes[i] & 0b1100_0000) == 0b1000_0000 {
                    i += 1;
                }
                if let Ok(chunk) = std::str::from_utf8(&bytes[start..i]) {
                    out.push_str(chunk);
                }
            }
        }
    }
    out
}

/// 从一个 ESC 开始吃掉整条序列，返回下一个普通字节的下标。
fn skip_escape(bytes: &[u8], mut i: usize) -> usize {
    i += 1; // ESC
    let Some(&next) = bytes.get(i) else {
        return i;
    };
    match next {
        // CSI：参数直到一个最终字节（0x40..=0x7E）
        b'[' => {
            i += 1;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            i + 1
        }
        // OSC：到 BEL 或 ST（ESC \）为止，超长的调色板定义也算在内
        b']' => {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    return i + 1;
                }
                if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                    return i + 2;
                }
                i += 1;
            }
            i
        }
        // 其它两字节序列（ESC ( B、ESC = 之类）
        _ => i + 2,
    }
}

/// 清理过期文件，返回删掉的数量。进程启动时调一次即可。
pub fn prune() -> usize {
    let Some(dir) = dir() else { return 0 };
    prune_at(&dir, KEEP_DAYS)
}

fn prune_at(dir: &Path, keep_days: i64) -> usize {
    let Ok(read) = std::fs::read_dir(dir) else {
        return 0;
    };
    let deadline = Utc::now() - chrono::Duration::days(keep_days);
    let mut removed = 0;
    for item in read.flatten() {
        let Ok(meta) = item.metadata() else { continue };
        if item.path().extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let expired = meta
            .modified()
            .ok()
            .and_then(|t| DateTime::<Utc>::try_from(t).ok())
            .is_some_and(|t| t < deadline);
        if expired && std::fs::remove_file(item.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "ells-session-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn alias_cannot_escape_the_log_dir() {
        for evil in [
            "../../etc/passwd",
            "..",
            "...abc",
            "web:443",
            "a\\b\\c",
            "带 空格 的别名",
            "",
        ] {
            let name = sanitize(evil);
            assert!(!name.is_empty(), "{evil} 不能压成空名");
            assert!(!name.contains('/') && !name.contains('\\'), "{evil} → {name}");
            assert!(
                !name.starts_with('.') && !name.split('.').all(|p| p.is_empty()),
                "{evil} → {name} 不能是相对路径成分"
            );
            assert!(name.chars().count() <= 40, "{name}");
        }
        assert_eq!(sanitize(".."), "host");
        assert_eq!(sanitize("web-prod"), "web-prod");
    }

    #[test]
    fn plain_strips_sequences_but_keeps_line_breaks() {
        let raw = "\x1b[31mRED\x1b[0m done\r\n第二行\x1b]0;标题\x07尾巴\n";
        assert_eq!(plain(raw), "RED done\n第二行尾巴\n");
        // 进度条那种整行覆盖：只剩最后一次的内容
        assert_eq!(plain("50%\r100%\r"), "50%100%");
    }

    #[test]
    fn plain_survives_utf8_and_a_truncated_escape() {
        assert_eq!(plain("中文 ✅"), "中文 ✅");
        // 连接断开时最常见：最后一个字节只剩半个序列
        assert_eq!(plain("ok\x1b[3"), "ok");
    }

    #[test]
    fn entries_sort_newest_first_and_ignore_non_logs() {
        let dir = temp_dir("list");
        std::fs::write(dir.join("web-20261008-101010.log"), "a").unwrap();
        std::fs::write(dir.join("web-20261009-101010.log"), "bb").unwrap();
        std::fs::write(dir.join("notes.txt"), "skip").unwrap();
        let rows = entries_in(&dir, 10);
        assert_eq!(rows.len(), 2, "只认 .log");
        assert_eq!(rows[0].size, 2);
        assert!(rows[0].mtime.ends_with("+08:00"), "时间要带北京时区: {}", rows[0].mtime);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prune_only_touches_expired_logs() {
        let dir = temp_dir("prune");
        let fresh = dir.join("web-20261009-101010.log");
        std::fs::write(&fresh, "x").unwrap();
        std::fs::write(dir.join("notes.txt"), "别删我").unwrap();
        std::fs::write(dir.join("web-20200101-000000.log"), "x").unwrap();
        // 把老文件的时间拨回 40 天前
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86400);
        file_times_set(&dir.join("web-20200101-000000.log"), old);
        assert_eq!(prune_at(&dir, KEEP_DAYS), 1);
        assert!(fresh.exists() && dir.join("notes.txt").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recording_stops_at_the_size_cap() {
        let dir = temp_dir("cap");
        let path = dir.join("web.log");
        std::fs::write(&path, vec![b'x'; MAX_SESSION_BYTES as usize]).unwrap();
        // 到顶之后 record 必须是空操作：日志不能把磁盘吃穿
        let mut rec = Recorder::at_path(&path).expect("能续写已存在的文件");
        rec.record(b"more");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), MAX_SESSION_BYTES);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn file_times_set(path: &Path, when: std::time::SystemTime) {
        let f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }
}

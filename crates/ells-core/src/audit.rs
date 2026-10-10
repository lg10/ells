//! 审计日志：`~/.ells/audit.log`，一行一条，只追加、不覆写。
//!
//! 记的是"谁在什么时候动了什么"：连了哪台机、认证是否失败、主机密钥是否被换、
//! 保险库何时被写、传了哪些文件。这些正是排障和事后追责最需要、又最容易在
//! 终端滚动 buffer 里丢掉的信息。
//!
//! 三条硬约束：
//! - **不含密钥**：永不写入密码、私钥内容或主密码；调用方只给别名与原因。
//! - **防注入**：所有自由文本先压掉换行，避免一条 `note` 伪造出第二行审计记录。
//! - **有上限**：超过 `MAX_BYTES` 就丢掉前半，日志不能无限吃掉用户磁盘。

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, SecondsFormat, Utc};

/// 单条日志里自由文本的长度上限：够写清原因，又不至于让一行变成半份转储。
const MAX_DETAIL: usize = 200;

/// 日志文件大小上限；超过后保留后半（最近的记录更有价值）。
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditKind {
    /// 连接建立成功
    Connect,
    /// 连接或认证失败
    ConnectFailed,
    /// 会话结束
    Disconnect,
    /// 主机密钥首次被信任
    HostKeyTrusted,
    /// 主机密钥变更（用户确认接受）
    HostKeyChanged,
    /// 一次文件传输
    Transfer,
    /// 隧道状态变化
    Tunnel,
    /// 保险库被写盘
    VaultSaved,
    /// 从 ~/.ssh/config 导入
    Import,
    /// 导出 ssh_config 片段（只含结构，不含凭据）
    Export,
    /// 设置被改
    Settings,
    /// 主密码被换
    MasterPassword,
}

impl AuditKind {
    fn tag(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::ConnectFailed => "connect-failed",
            Self::Disconnect => "disconnect",
            Self::HostKeyTrusted => "hostkey-trusted",
            Self::HostKeyChanged => "hostkey-changed",
            Self::Transfer => "transfer",
            Self::Tunnel => "tunnel",
            Self::VaultSaved => "vault-saved",
            Self::Import => "import",
            Self::Export => "export",
            Self::Settings => "settings",
            Self::MasterPassword => "master-password",
        }
    }
}

/// `~/.ells/audit.log`。
pub fn audit_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ells").join("audit.log"))
}

/// 记一条审计。写失败不影响主流程——调用方 `let _ = record(...)`。
pub fn record(kind: AuditKind, subject: &str, detail: &str) -> std::io::Result<()> {
    let path = audit_path().ok_or_else(|| std::io::Error::other("无法定位用户主目录"))?;
    record_at(&path, kind, &now(), subject, detail)
}

fn record_at(
    path: &Path,
    kind: AuditKind,
    when: &str,
    subject: &str,
    detail: &str,
) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let line = format!("{when} {} {} {}\n", kind.tag(), clean(subject), clean(detail));
    let mut f = std::fs::OpenOptions::new().append(true).create(true).open(path)?;
    f.write_all(line.as_bytes())?;
    rotate_if_huge(path)
}

/// 读最近 `count` 条（原始行，已经是北京时间 + 中文原因）。
pub fn read_last(count: usize) -> Vec<String> {
    let Some(path) = audit_path() else { return Vec::new() };
    read_last_at(&path, count)
}

fn read_last_at(path: &Path, count: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    if lines.len() > count {
        lines.drain(..lines.len() - count);
    }
    lines
}

/// 压掉换行与首尾空白、限长：一条记录只能占一行。
///
/// 换成 ` · ` 而不是直接删：`note` 里真的可能有换行，全删会把两个字段粘成一句
/// 读不懂的话，而这才是最容易看漏的那类内容。
fn clean(text: &str) -> String {
    let flat = text.replace(['\n', '\r'], " · ");
    let trimmed = flat.trim();
    if trimmed.is_empty() {
        return "-".to_string();
    }
    // 按字符截断，不按字节：中文原因按字节切会在多字节中间断开
    let mut out: String = trimmed.chars().take(MAX_DETAIL).collect();
    if trimmed.chars().count() > MAX_DETAIL {
        out.push('…');
    }
    out
}

/// 超限就丢掉前半：`read_to_string` + 重写，日志本来就小，不值得为它写流式截断。
fn rotate_if_huge(path: &Path) -> std::io::Result<()> {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len <= MAX_BYTES {
        return Ok(());
    }
    let text = std::fs::read_to_string(path)?;
    let keep: String = text
        .lines()
        .skip(text.lines().count() / 2)
        .map(|l| format!("{l}\n"))
        .collect();
    // 日志不是凭据：这里可以原地覆写，半截只会少几行历史
    std::fs::write(path, keep)?;
    Ok(())
}

/// 北京时间（+08:00）的 `2026-10-09T14:03:22+08:00`。
///
/// 固定 +08:00 而不是本地时区：日志要能和别人说的"下午那次"对上，
/// 也要在跨时区的机器之间拼成一条连续时间线。
fn now() -> String {
    beijing(Utc::now())
}

fn beijing(instant: DateTime<Utc>) -> String {
    let offset = FixedOffset::east_opt(8 * 3600).expect("+08:00 恒有效");
    instant.with_timezone(&offset).to_rfc3339_opts(SecondsFormat::Secs, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("ells-audit-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("audit.log")
    }

    #[test]
    fn one_record_is_always_one_line() {
        let path = temp_path("inject");
        // 恶意 note：换行后面伪造一条 vault-saved
        let evil = "web\n2026-10-09T14:00:01+08:00 vault-saved evil -";
        record_at(&path, AuditKind::Import, "2026-10-09T14:00:00+08:00", evil, "").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1, "换行必须被压掉: {text}");
        // 只有一条记录，且记录头是调用方给的时间与类型，不是被注入的那个
        let line = text.lines().next().unwrap();
        assert!(line.starts_with("2026-10-09T14:00:00+08:00 import "), "记录头被篡改: {line}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn keeps_recent_records_when_reading_back() {
        let path = temp_path("tail");
        for i in 0..5 {
            record_at(
                &path,
                AuditKind::Connect,
                &format!("2026-10-09T14:00:0{i}+08:00"),
                &format!("h{i}"),
                "ok",
            )
            .unwrap();
        }
        let last2 = read_last_at(&path, 2);
        assert_eq!(last2.len(), 2);
        assert!(last2[0].contains("connect h3"), "实际: {}", last2[0]);
        assert!(last2[1].contains("connect h4"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn detail_is_truncated_by_chars_not_bytes() {
        // 300 个汉字：按字节切会切断多字节字符
        let long = "结".repeat(300);
        let out = clean(&long);
        assert_eq!(out.chars().count(), MAX_DETAIL + 1, "200 字 + 省略号");
        assert!(out.ends_with('…'));
        assert_eq!(clean("   "), "-");
        assert_eq!(clean("密码 · 已改"), "密码 · 已改");
    }

    #[test]
    fn timestamp_is_beijing_time() {
        let utc = Utc.with_ymd_and_hms(2026, 10, 9, 6, 0, 0).unwrap();
        assert_eq!(beijing(utc), "2026-10-09T14:00:00+08:00");
    }
}

//! 全局设置：`~/.ells/settings.ini`（key=value 行，无需序列化依赖）。

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Settings {
    /// 终端输出高亮（docker ps / 日志级别）
    pub highlight: bool,
    /// SSH 空闲保活间隔（秒），作用于新建连接
    pub keepalive_secs: u64,
    /// 主密码保护：关闭后主密码存入本地凭据文件、启动自动解锁（安全性降为文件权限级）
    pub master_password_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { highlight: true, keepalive_secs: 30, master_password_enabled: true }
    }
}

fn settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ells").join("settings.ini"))
}

/// 自动解锁凭据文件：仅在主密码保护关闭时存在。
fn master_backup_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ells").join(".master"))
}

pub fn read_master_backup() -> Option<String> {
    let path = master_backup_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let master = text.trim_end_matches(['\r', '\n']).to_string();
    (!master.is_empty()).then_some(master)
}

pub fn write_master_backup(master: &str) -> std::io::Result<()> {
    let path = master_backup_path().ok_or_else(|| {
        std::io::Error::other("无法定位用户主目录")
    })?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path)?;
    std::io::Write::write_all(&mut f, master.as_bytes())?;
    Ok(())
}

pub fn clear_master_backup() {
    if let Some(path) = master_backup_path() {
        let _ = std::fs::remove_file(path);
    }
}

impl Settings {
    pub fn load() -> Self {
        let Some(path) = settings_path() else { return Self::default() };
        let Ok(text) = std::fs::read_to_string(&path) else { return Self::default() };
        let mut s = Self::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "highlight" => s.highlight = v.trim() == "true",
                "keepalive_secs" => {
                    if let Ok(n) = v.trim().parse::<u64>() {
                        s.keepalive_secs = n.clamp(5, 3600);
                    }
                }
                "master_password_enabled" => {
                    s.master_password_enabled = v.trim() != "false"
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let Some(path) = settings_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!(
            "highlight={}\nkeepalive_secs={}\nmaster_password_enabled={}\n",
            self.highlight, self.keepalive_secs, self.master_password_enabled
        );
        let _ = std::fs::write(&path, text);
    }
}

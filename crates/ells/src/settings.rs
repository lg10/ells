//! 全局设置：`~/.ells/settings.ini`（key=value 行，无需序列化依赖）。

use std::path::PathBuf;

use crate::keybinds::{Action, Chord, KeyBinds};
use crate::theme;

#[derive(Debug, Clone)]
pub struct Settings {
    /// 终端输出高亮（docker ps / 日志级别）
    pub highlight: bool,
    /// SSH 空闲保活间隔（秒），作用于新建连接
    pub keepalive_secs: u64,
    /// 主密码保护：关闭后主密码存入本地凭据文件、启动自动解锁（安全性降为文件权限级）
    pub master_password_enabled: bool,
    /// 界面快捷键（设置里可改，改完即时写盘）
    pub keybinds: KeyBinds,
    /// 界面配色主题：设置里循环切换，预览即时生效，点【保 存】才写盘
    pub theme: theme::Theme,
    /// 启动时自动查一次新版本（只在后台问一次 GitHub，不下载）
    pub auto_update: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            highlight: true,
            keepalive_secs: 30,
            master_password_enabled: true,
            keybinds: KeyBinds::default(),
            theme: theme::Theme::platform_default(),
            auto_update: true,
        }
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
    /// 读盘并把全局调色板对齐到盘上的主题（包括"根本没有配置文件"的首启）。
    pub fn load() -> Self {
        let s = Self::read_from_disk();
        theme::apply(s.theme);
        s
    }

    fn read_from_disk() -> Self {
        let Some(path) = settings_path() else { return Self::default() };
        let Ok(text) = std::fs::read_to_string(&path) else { return Self::default() };
        let mut s = Self::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let k = k.trim();
            match k {
                "highlight" => s.highlight = v.trim() == "true",
                "keepalive_secs" => {
                    if let Ok(n) = v.trim().parse::<u64>() {
                        s.keepalive_secs = n.clamp(5, 3600);
                    }
                }
                "master_password_enabled" => {
                    s.master_password_enabled = v.trim() != "false"
                }
                "theme" => {
                    // 写错的值忽略，留着默认（按平台）的那套
                    if let Some(t) = theme::Theme::parse(v) {
                        s.theme = t;
                    }
                }
                "auto_update" => s.auto_update = v.trim() != "false",
                _ => {
                    // key_new_tab=F2 之类：非法/未知键名直接忽略，保留默认值
                    if let Some(action) = Action::from_ini_key(k) {
                        if let Some(chord) = Chord::parse(v.trim()) {
                            s.keybinds.assign(action, chord);
                        }
                    }
                }
            }
        }
        s
    }

    pub fn save(&self) {
        let Some(path) = settings_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut text = format!(
            "highlight={}\nkeepalive_secs={}\nmaster_password_enabled={}\ntheme={}\nauto_update={}\n",
            self.highlight,
            self.keepalive_secs,
            self.master_password_enabled,
            self.theme.ini_value(),
            self.auto_update
        );
        for action in Action::ALL {
            text.push_str(&format!("{}={}\n", action.ini_key(), self.keybinds.display(action)));
        }
        let _ = std::fs::write(&path, text);
        theme::apply(self.theme);
    }
}

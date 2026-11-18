//! 全局设置：`~/.ells/settings.ini`（key=value 行，无需序列化依赖）。

use ells_lang::{t, Lang};
use std::path::PathBuf;

use crate::keybinds::{Action, Chord, KeyBinds};
use crate::theme;

/// 界面语言：默认跟随系统（`ELLS_LANG` > 系统语言 > 英文），也可以在这里钉死。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LangSetting {
    /// 启动时按环境变量 / 系统语言探测（现状行为）
    Auto,
    Zh,
    En,
}

impl LangSetting {
    pub const ALL: [LangSetting; 3] = [Self::Auto, Self::Zh, Self::En];

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "auto" => Some(Self::Auto),
            "zh" => Some(Self::Zh),
            "en" => Some(Self::En),
            _ => None,
        }
    }

    pub fn ini_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Zh => "zh",
            Self::En => "en",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => t("跟随系统"),
            Self::Zh => t("简体中文"),
            Self::En => "English",
        }
    }

    /// 设置面板里按 ←（backwards）或 →/Enter 循环。
    pub fn shift(self, backwards: bool) -> Self {
        let at = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        let len = Self::ALL.len();
        let next = if backwards { at + len - 1 } else { at + 1 };
        Self::ALL[next % len]
    }

    pub fn explicit(self) -> Option<Lang> {
        match self {
            Self::Auto => None,
            Self::Zh => Some(Lang::Zh),
            Self::En => Some(Lang::En),
        }
    }

    /// 让全局语言立刻对齐到这一项；`Auto` 重新走一次探测，
    /// 这样从"英文"切回"跟随系统"不用重启。
    pub fn apply(self) {
        ells_lang::set_lang(self.explicit().unwrap_or_else(ells_lang::detect));
    }
}

/// 解析参数之前先认盘上的 `lang=`：clap 的 `--help` 也得跟着设置走。
/// 只读这一行，不碰主题、不改任何东西。
pub fn apply_saved_language() {
    let Some(path) = settings_path() else { return };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        if k.trim() == "lang"
            && let Some(setting) = LangSetting::parse(v)
        {
            setting.apply();
            return;
        }
    }
}

/// 会话里手敲 `vi 文件` / 文件浏览器按 `e` 时怎么打开。默认用内置编辑器——
/// 文件取回到临时文件后直接在 ells 自己的终端面板里改（行号 + 语法高亮），
/// 不依赖本机装没装 vim，也不拉起外部窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOpen {
    /// ells 内置面板（终端内 vim 风格编辑，Ctrl-S 保存回传）
    Builtin,
    /// 本机外部程序（`$ELLS_EDITOR` > `$VISUAL` > `$EDITOR` > 平台默认）
    Local,
    /// 交回远端原生 vi（只做原样转发，不拦截）
    Native,
}

impl EditOpen {
    pub const ALL: [EditOpen; 3] = [Self::Builtin, Self::Local, Self::Native];

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "builtin" => Some(Self::Builtin),
            "local" => Some(Self::Local),
            "native" => Some(Self::Native),
            _ => None,
        }
    }

    pub fn ini_value(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Local => "local",
            Self::Native => "native",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => t("内置编辑器"),
            Self::Local => t("本地程序"),
            Self::Native => t("原生（远端 vi）"),
        }
    }

    /// 设置面板里按 ←（backwards）或 →/Enter 循环。
    pub fn shift(self, backwards: bool) -> Self {
        let at = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        let len = Self::ALL.len();
        let next = if backwards { at + len - 1 } else { at + 1 };
        Self::ALL[next % len]
    }

    /// 终端里手敲 vi 要不要拦截下来取回文件。原生档不碰用户输入。
    pub fn intercepts_terminal_vi(self) -> bool {
        !matches!(self, Self::Native)
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// 界面语言：默认跟随系统，可在设置里钉死中文 / 英文
    pub lang: LangSetting,
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
    /// 把远端打回来的输出落盘到 `~/.ells/logs/<别名>-<时间>.log`
    pub session_log: bool,
    /// 会话意外断开后的自动重连退避（`max_attempts = 0` 就回到"弹窗问你"）
    pub reconnect: ells_core::ReconnectParams,
    /// 主机列表的排序方式（列表页 `o` 循环，切换时即刻写盘）
    pub list_sort: crate::app::ListSort,
    /// 会话页底部那排 CPU / 内存 / 磁盘指标。关掉就不开第二条采集通道，
    /// 那一行还给终端。
    pub metrics: bool,
    /// 终端里手敲 `vi 文件` / 浏览器按 `e` 时的打开方式。
    pub edit_open: EditOpen,
    /// 自更新源列表（逗号 / 分号 / 空白分隔）：`github` 或镜像前缀。
    /// 空 = 内置列表（官方 + 几个公开加速镜像）。
    pub update_sources: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // 默认跟随系统：以前怎么判断现在还怎么判断
            lang: LangSetting::Auto,
            highlight: true,
            keepalive_secs: 30,
            master_password_enabled: true,
            keybinds: KeyBinds::default(),
            theme: theme::Theme::platform_default(),
            auto_update: true,
            // 默认开：会话记录的价值恰恰在"出事后还能回看"，事前关掉就没了。
            // 单文件 8 MiB 封顶、30 天清理，写的是远端输出而不是任何凭据。
            session_log: true,
            reconnect: ells_core::ReconnectParams::default(),
            list_sort: crate::app::ListSort::Grouped,
            // 默认开：这排条是"连上就能看见远端负载"，等用户去设置里找就失去意义了。
            // 采不到的机器会自己降级成一行提示并停止轮询，不会一直占着那一行。
            metrics: true,
            // 默认内置编辑器：不依赖本机装没装 vim，也不用离开终端；
            // 想用外部程序或远端原生 vi 在设置里切。
            edit_open: EditOpen::Builtin,
            // 空 = update.rs 的内置源列表；用户可用环境变量或 settings.ini 覆盖
            update_sources: String::new(),
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
        std::io::Error::other(t("无法定位用户主目录"))
    })?;
    // 原子写：这个文件是明文主密码副本，半截文件等于保险库彻底打不开
    ells_core::write_atomic(&path, master.as_bytes())
        .map_err(|err| std::io::Error::other(err.to_string()))
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
        // 语言不在这里落地：update.rs 的源列表解析也会调 load()，从盘上回写
        // 会把设置面板里刚切换的预览语言打回去。启动与「取消」两处显式 apply。
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
                "lang" => {
                    // 写错的值忽略，保留「跟随系统」
                    if let Some(m) = LangSetting::parse(v) {
                        s.lang = m;
                    }
                }
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
                "session_log" => s.session_log = v.trim() != "false",
                "metrics" => s.metrics = v.trim() != "false",
                "update_sources" => s.update_sources = v.trim().to_string(),
                // 编辑方式：写错的值忽略，保留默认（内置编辑器）。
                // 旧版本写的是 vi_open，继续认，升级后不用重设。
                "edit_open" | "vi_open" => {
                    if let Some(mode) = EditOpen::parse(v) {
                        s.edit_open = mode;
                    }
                }
                // 列表排序：写错的值忽略，回到「默认」
                "list_sort" => {
                    if let Some(m) = crate::app::ListSort::parse(v) {
                        s.list_sort = m;
                    }
                }
                // 自动重连：写错的值一律忽略，保留默认，绝不因为一个错数字变成疯狂重连
                "reconnect_attempts" => {
                    if let Ok(n) = v.trim().parse::<u32>() {
                        s.reconnect.max_attempts = n.min(ells_core::reconnect::MAX_ATTEMPTS_CAP);
                    }
                }
                "reconnect_initial_ms" => {
                    if let Ok(n) = v.trim().parse::<u64>() {
                        s.reconnect.initial_delay_ms = n.clamp(100, 60_000);
                    }
                }
                "reconnect_max_ms" => {
                    if let Ok(n) = v.trim().parse::<u64>() {
                        s.reconnect.max_delay_ms = n.clamp(1_000, 600_000);
                    }
                }
                "reconnect_stable_secs" => {
                    if let Ok(n) = v.trim().parse::<u64>() {
                        s.reconnect.stable_secs = n.clamp(1, 86_400);
                    }
                }
                "reconnect_jitter" => {
                    if let Ok(n) = v.trim().parse::<f64>() {
                        s.reconnect.jitter_ratio = n.clamp(0.0, 0.5);
                    }
                }
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
            "lang={}\nhighlight={}\nkeepalive_secs={}\nmaster_password_enabled={}\ntheme={}\nauto_update={}\nsession_log={}\nmetrics={}\nupdate_sources={}\nedit_open={}\nlist_sort={}\nreconnect_attempts={}\nreconnect_initial_ms={}\nreconnect_max_ms={}\nreconnect_stable_secs={}\nreconnect_jitter={}\n",
            self.lang.ini_value(),
            self.highlight,
            self.keepalive_secs,
            self.master_password_enabled,
            self.theme.ini_value(),
            self.auto_update,
            self.session_log,
            self.metrics,
            self.update_sources.replace(['\n', '\r'], ","),
            self.edit_open.ini_value(),
            self.list_sort.ini_value(),
            self.reconnect.max_attempts,
            self.reconnect.initial_delay_ms,
            self.reconnect.max_delay_ms,
            self.reconnect.stable_secs,
            self.reconnect.jitter_ratio
        );
        for action in Action::ALL {
            text.push_str(&format!("{}={}\n", action.ini_key(), self.keybinds.display(action)));
        }
        let _ = ells_core::write_atomic(&path, text.as_bytes());
        theme::apply(self.theme);
    }
}

#[cfg(test)]
mod tests {
    use super::{EditOpen, LangSetting};

    #[test]
    fn lang_setting_parses_only_known_values() {
        assert_eq!(LangSetting::parse("auto"), Some(LangSetting::Auto));
        assert_eq!(LangSetting::parse(" zh \n"), Some(LangSetting::Zh));
        assert_eq!(LangSetting::parse("en"), Some(LangSetting::En));
        assert_eq!(LangSetting::parse("fr"), None);
        assert_eq!(LangSetting::Auto.ini_value(), "auto");
        assert_eq!(LangSetting::Zh.ini_value(), "zh");
        assert_eq!(LangSetting::En.ini_value(), "en");
    }

    #[test]
    fn lang_setting_cycles_both_ways() {
        assert_eq!(LangSetting::Auto.shift(false), LangSetting::Zh);
        assert_eq!(LangSetting::Zh.shift(false), LangSetting::En);
        assert_eq!(LangSetting::En.shift(false), LangSetting::Auto);
        assert_eq!(LangSetting::Auto.shift(true), LangSetting::En);
        assert_eq!(LangSetting::En.shift(true), LangSetting::Zh);
    }

    #[test]
    fn only_auto_follows_the_system() {
        assert_eq!(LangSetting::Auto.explicit(), None);
        assert_eq!(LangSetting::Zh.explicit(), Some(ells_lang::Lang::Zh));
        assert_eq!(LangSetting::En.explicit(), Some(ells_lang::Lang::En));
    }

    #[test]
    fn edit_open_parses_only_known_values() {
        assert_eq!(EditOpen::parse("builtin"), Some(EditOpen::Builtin));
        assert_eq!(EditOpen::parse("local"), Some(EditOpen::Local));
        assert_eq!(EditOpen::parse(" native \n"), Some(EditOpen::Native));
        assert_eq!(EditOpen::parse("remote"), None);
        assert_eq!(EditOpen::Builtin.ini_value(), "builtin");
        assert_eq!(EditOpen::Local.ini_value(), "local");
        assert_eq!(EditOpen::Native.ini_value(), "native");
    }

    #[test]
    fn edit_open_cycles_all_three_modes_both_ways() {
        assert_eq!(EditOpen::Builtin.shift(false), EditOpen::Local);
        assert_eq!(EditOpen::Local.shift(false), EditOpen::Native);
        assert_eq!(EditOpen::Native.shift(false), EditOpen::Builtin);
        assert_eq!(EditOpen::Builtin.shift(true), EditOpen::Native);
        assert_eq!(EditOpen::Native.shift(true), EditOpen::Local);
    }

    #[test]
    fn only_native_skips_terminal_vi_interception() {
        assert!(EditOpen::Builtin.intercepts_terminal_vi());
        assert!(EditOpen::Local.intercepts_terminal_vi());
        assert!(!EditOpen::Native.intercepts_terminal_vi());
    }
}

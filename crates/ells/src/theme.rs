//! 界面配色（主题）。ells 自身绘制的每一块颜色都从这里取，不再在 `ui.rs` 里写死颜色名。
//!
//! 为什么要这一层：同一份代码在 Windows Terminal 里好看、在 mac 终端里"差点意思"，根因是
//! 我们把 `Black`/`DarkGray` 当背景画死了——终端主题里的这两个色和它自己的底色不是一回事，
//! 于是标签条、表头像贴了一块灰布。ANSI 16 色本来是"语义槽位"，该由终端主题去翻译。
//! 所以主题之间真正的差别只有两类：**要不要画底色**、**强调色用哪几档**。
//!
//! 当前主题存在进程级的一份全局状态里：绘制侧近 140 处取色，逐个把 `&Palette` 传进每个
//! 函数签名代价太高，而写入点只有"启动读设置"和"设置面板改主题"两处，且 TUI 循环本身
//! 单线程绘制，不会和谁打架。

use std::sync::atomic::{AtomicU8, Ordering};

use ratatui::style::Color;

/// 内置主题。`Terminal` 是"跟随终端"：ells 只给前景色，不再画任何底色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    /// 现在的样子：黑底 + 灰条，Windows Terminal 上最贴。
    Dark,
    /// 不画底色，底和默认字色都交给终端主题（mac 终端默认用这套）。
    Terminal,
    /// 少用灰色小字，靠粗体/反显区分层级，弱视或投影仪下更清楚。
    HighContrast,
    /// 给白底终端（Terminal.app 的 Basic、浅色 iTerm2 配色）准备的深字浅底。
    Light,
}

/// 一套主题的具体取色结果。字段名就是它在界面上的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// 面板/列表的底色
    pub bg: Color,
    /// 顶部条、标签条、弹窗标题带的底色
    pub band: Color,
    /// 进度条空档那一截自己的底色。
    /// 条不能靠"色带透出来"画：`Dark` 的色带是浅灰，`░` 网点透出来的还是浅灰，
    /// 于是整根条和背景一个亮度，看起来像被背景吃掉了。不画底色的主题一律 `Reset`
    /// （跟随终端时终端自己的底色就是轨道，画反而脏）。
    pub track: Color,
    /// 压在条带上的刻线（标签之间的 `│`）：必须和 band 不同档，否则跟随终端时线条会消失
    pub rule: Color,
    /// 正文
    pub text: Color,
    /// 次要说明（比正文淡）
    pub dim: Color,
    /// 更淡的一层注释
    pub muted: Color,
    /// 强调前景：标题、目录、可点元素
    pub accent: Color,
    /// 强调底色：当前标签、选中项
    pub accent_bg: Color,
    /// 压在亮底上的字色（徽章、当前标签）
    pub on_accent: Color,
    /// 按钮被选中时的底色
    pub select_bg: Color,
    /// 提醒/待确认
    pub warn: Color,
    /// 错误、危险操作
    pub err: Color,
    /// 成功、已连接
    pub ok: Color,
    /// 备用强调（进度、第二种状态）
    pub alt: Color,
}

impl Theme {
    pub const ALL: [Theme; 4] = [Theme::Dark, Theme::Terminal, Theme::HighContrast, Theme::Light];

    /// settings.ini 里 `theme=` 的取值。
    pub fn ini_value(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Terminal => "terminal",
            Theme::HighContrast => "contrast",
            Theme::Light => "light",
        }
    }

    /// 读 ini 取值；未知/手写错的值返回 None，由调用方回落平台默认。
    pub fn parse(text: &str) -> Option<Theme> {
        match text.trim().to_ascii_lowercase().as_str() {
            "dark" => Some(Theme::Dark),
            "terminal" => Some(Theme::Terminal),
            "contrast" | "highcontrast" => Some(Theme::HighContrast),
            "light" => Some(Theme::Light),
            _ => None,
        }
    }

    /// 设置面板里显示的名字。
    pub fn label(self) -> &'static str {
        match self {
            Theme::Dark => "深色（画死底色）",
            Theme::Terminal => "跟随终端（不画底色）",
            Theme::HighContrast => "高对比",
            Theme::Light => "浅色底",
        }
    }

    /// 没配过 theme 时一律跟随终端：ells 只给前景色，底色交给终端自己的主题。
    /// 画死底色的那套（`Dark`）留给显式选它的人 —— 它把色带刷成浅灰之后，压在上面的
    /// 进度条要靠自己的轨道色才看得见（见 `Palette::track`）。
    pub fn platform_default() -> Theme {
        Theme::Terminal
    }

    /// 循环取色：`backwards` 为真时往前一套（设置面板的 ← / Enter）。
    pub fn shift(self, backwards: bool) -> Theme {
        let len = Self::ALL.len();
        let at = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        let next = if backwards { at + len - 1 } else { at + 1 };
        Self::ALL[next % len]
    }

    pub fn palette(self) -> Palette {
        match self {
            Theme::Dark => Palette {
                bg: Color::Black,
                band: Color::DarkGray,
                track: Color::Black,
                rule: Color::Black,
                text: Color::White,
                dim: Color::DarkGray,
                muted: Color::Gray,
                accent: Color::Cyan,
                accent_bg: Color::Cyan,
                on_accent: Color::Black,
                select_bg: Color::White,
                warn: Color::Yellow,
                err: Color::Red,
                ok: Color::Green,
                alt: Color::Magenta,
            },
            Theme::Terminal => Palette {
                bg: Color::Reset,
                band: Color::Reset,
                track: Color::Reset,
                // 底色跟着终端，刻线就得留在"看得见的那一档"，黑字在深底终端上会直接消失
                rule: Color::DarkGray,
                text: Color::Reset,
                dim: Color::DarkGray,
                muted: Color::Gray,
                accent: Color::Cyan,
                accent_bg: Color::Cyan,
                on_accent: Color::Black,
                select_bg: Color::Gray,
                warn: Color::Yellow,
                err: Color::Red,
                ok: Color::Green,
                alt: Color::Magenta,
            },
            Theme::HighContrast => Palette {
                bg: Color::Reset,
                band: Color::Reset,
                track: Color::Reset,
                rule: Color::Reset,
                text: Color::Reset,
                dim: Color::Reset,
                muted: Color::Reset,
                accent: Color::LightCyan,
                accent_bg: Color::White,
                on_accent: Color::Black,
                select_bg: Color::White,
                warn: Color::LightYellow,
                err: Color::LightRed,
                ok: Color::LightGreen,
                alt: Color::LightMagenta,
            },
            // 浅底终端：Yellow/Gray 这类浅色字要换成深色档，否则白纸上等于没写字
            Theme::Light => Palette {
                bg: Color::Reset,
                band: Color::Reset,
                track: Color::Reset,
                rule: Color::DarkGray,
                text: Color::Black,
                dim: Color::DarkGray,
                muted: Color::DarkGray,
                accent: Color::Blue,
                accent_bg: Color::LightBlue,
                on_accent: Color::Black,
                // on_accent 恒为黑字，所以选中底色不能落到深灰档
                select_bg: Color::Gray,
                warn: Color::LightRed,
                err: Color::Red,
                ok: Color::Green,
                alt: Color::Magenta,
            },
        }
    }
}

/// 必须是 `static` 而不是 `const`：`const` 的 `AtomicU8` 每次使用都会新建临时对象，
/// `store` 只改到临时件上，主题永远切不过去。
static CURRENT: AtomicU8 = AtomicU8::new(0);

fn index(theme: Theme) -> u8 {
    match theme {
        Theme::Dark => 0,
        Theme::Terminal => 1,
        Theme::HighContrast => 2,
        Theme::Light => 3,
    }
}

/// 生效中的主题。
pub fn current() -> Theme {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Theme::Terminal,
        2 => Theme::HighContrast,
        3 => Theme::Light,
        _ => Theme::Dark,
    }
}

/// 切换主题：只在建应用和设置面板改这一项时调用。
pub fn apply(theme: Theme) {
    CURRENT.store(index(theme), Ordering::Relaxed);
}

fn pal() -> Palette {
    current().palette()
}

/// 深色主题下 `Color::Black` 是真底色，其余主题一律"不画"（Reset）。
pub fn bg() -> Color {
    pal().bg
}

pub fn band() -> Color {
    pal().band
}

/// 进度条空档的底色。只有画死底色的主题才给它一个真颜色。
pub fn track() -> Color {
    pal().track
}

pub fn rule() -> Color {
    pal().rule
}

pub fn text() -> Color {
    pal().text
}

pub fn dim() -> Color {
    pal().dim
}

pub fn muted() -> Color {
    pal().muted
}

pub fn accent() -> Color {
    pal().accent
}

pub fn accent_bg() -> Color {
    pal().accent_bg
}

pub fn on_accent() -> Color {
    pal().on_accent
}

pub fn select_bg() -> Color {
    pal().select_bg
}

pub fn warn() -> Color {
    pal().warn
}

pub fn err() -> Color {
    pal().err
}

pub fn ok() -> Color {
    pal().ok
}

pub fn alt() -> Color {
    pal().alt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_values_round_trip() {
        for theme in Theme::ALL {
            assert_eq!(Theme::parse(theme.ini_value()), Some(theme), "{}", theme.ini_value());
        }
        // 大小写与空格不敏感，写错一律回落默认而不是崩
        assert_eq!(Theme::parse(" TERMINAL "), Some(Theme::Terminal));
        assert_eq!(Theme::parse("contrast"), Some(Theme::HighContrast));
        assert_eq!(Theme::parse("pink"), None);
        assert_eq!(Theme::parse(""), None);
    }

    #[test]
    fn terminal_theme_paints_no_background() {
        // 这一条就是 mac 上"灰底黑块"的解药：跟随终端时不允许任何地方留下画死的底色
        let p = Theme::Terminal.palette();
        assert_eq!(p.bg, Color::Reset);
        assert_eq!(p.band, Color::Reset);
        assert_eq!(p.text, Color::Reset);
        // 但强调底色仍然要有，否则当前标签和别的标签分不出来
        assert_ne!(p.accent_bg, Color::Reset);
    }

    #[test]
    fn the_progress_track_is_visible_on_the_painted_band() {
        // 深色主题把色带刷成浅灰，条的空档要是还靠它透出来，整根条就和背景一个亮度
        let d = Theme::Dark.palette();
        assert_eq!(d.band, Color::DarkGray);
        assert_ne!(d.track, d.band, "轨道和色带同色 = Windows 上那条进度条被背景吃掉");
        // 不画底色的三套交给终端，多画一层反而脏
        for theme in [Theme::Terminal, Theme::HighContrast, Theme::Light] {
            assert_eq!(theme.palette().track, Color::Reset, "{}", theme.ini_value());
        }
    }

    #[test]
    fn every_platform_defaults_to_the_terminal_theme() {
        // 分平台默认值的代价是"同一份代码两台机器长得不一样"：mac 上清楚的条，
        // Windows 上落在浅灰色带里就看不见了。现在一律跟随终端。
        assert_eq!(Theme::platform_default(), Theme::Terminal);
        assert_eq!(Theme::platform_default().palette().band, Color::Reset);
    }

    #[test]
    fn every_theme_keeps_dark_text_for_light_backgrounds() {
        // on_accent/select_bg 永远是一对：亮底配深字，换主题也不能糊
        for theme in Theme::ALL {
            let p = theme.palette();
            assert_eq!(p.on_accent, Color::Black, "{}", theme.ini_value());
            assert_ne!(p.accent_bg, p.on_accent, "{} 的当前标签会看不见", theme.ini_value());
            assert_ne!(p.select_bg, Color::Reset, "{}", theme.ini_value());
        }
    }

    #[test]
    fn rules_and_selection_stay_visible_without_a_painted_band() {
        // 不画底色的主题里，标签之间的刻线绝不能是黑色：黑字压在终端默认深底上就等于没有
        for theme in [Theme::Terminal, Theme::HighContrast, Theme::Light] {
            let p = theme.palette();
            assert_eq!(p.band, Color::Reset, "{}", theme.ini_value());
            assert_ne!(p.rule, Color::Black, "{} 的分隔条会消失", theme.ini_value());
        }
        let d = Theme::Dark.palette();
        assert_eq!(d.band, Color::DarkGray);
        assert_eq!(d.rule, Color::Black, "深色主题维持原样：黑刻线压在灰条上，Windows 观感不变");
    }

    #[test]
    fn shift_walks_the_whole_list_both_ways() {
        // 设置面板只靠 ←→/Enter 换主题，所以循环要覆盖每一档、也不能中途绕回起点
        let mut t = Theme::Dark;
        let mut seen = vec![t];
        for _ in 0..Theme::ALL.len() - 1 {
            t = t.shift(false);
            assert_ne!(t, Theme::Dark, "循环少了一档");
            seen.push(t);
        }
        assert_eq!(seen, Theme::ALL.to_vec());
        assert_eq!(Theme::Dark.shift(true), *Theme::ALL.last().unwrap());
    }

    #[test]
    fn applying_a_theme_is_visible_to_the_role_accessors() {
        apply(Theme::Dark);
        assert_eq!(current(), Theme::Dark);
        assert_eq!(bg(), Color::Black);
        apply(Theme::Terminal);
        assert_eq!(bg(), Color::Reset);
        assert_eq!(band(), Color::Reset);
        apply(Theme::Light);
        assert_eq!(text(), Color::Black);
        apply(Theme::HighContrast);
        assert_eq!(dim(), Color::Reset, "高对比不该再有灰色小字");
        apply(Theme::platform_default());
        assert_eq!(current(), Theme::platform_default());
    }
}

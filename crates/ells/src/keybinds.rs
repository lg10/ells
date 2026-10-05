//! 可自定义的 ells 界面按键，存进 `~/.ells/settings.ini` 的 `key_*=…` 条目。
//!
//! 只收录"不会与远端输入冲突"的两类键：F2–F12 功能键（F1 是帮助键），以及带 Ctrl/Alt
//! 的组合键。裸字符必须留给远端 shell，无修饰的结构性键（Esc/Tab/Enter/Backspace/方向键）
//! 也是界面自身的输入手段，所以都不允许绑定。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 键位本身：功能键或字符键（字符一律以小写存储，比较时忽略大小写）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordKey {
    Function(u8),
    Char(char),
}

/// 一次按键绑定。Shift 不参与比较：终端里 Ctrl+Shift+字母常常和 Ctrl+字母同码，
/// 区分它们只会让绑定看起来"按了没反应"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub key: ChordKey,
    pub ctrl: bool,
    pub alt: bool,
}

impl Chord {
    pub const fn function(n: u8) -> Self {
        Self { key: ChordKey::Function(n), ctrl: false, alt: false }
    }

    pub const fn ctrl(c: char) -> Self {
        Self { key: ChordKey::Char(c), ctrl: true, alt: false }
    }

    fn from_keycode(code: KeyCode, ctrl: bool, alt: bool) -> Option<Self> {
        let key = match code {
            KeyCode::Char(c) => ChordKey::Char(c.to_ascii_lowercase()),
            KeyCode::F(n) => ChordKey::Function(n),
            _ => return None,
        };
        Some(Self { key, ctrl, alt })
    }

    /// 录制态：把一次按键事件转成候选绑定；结构性按键（Esc/Enter/Tab/方向键…）不可绑定。
    pub fn from_event(key: &KeyEvent) -> Option<Self> {
        Self::from_keycode(
            key.code,
            key.modifiers.contains(KeyModifiers::CONTROL),
            key.modifiers.contains(KeyModifiers::ALT),
        )
    }

    /// 从 settings.ini 的取值解析，非法或不可绑定一律 None。
    pub fn parse(text: &str) -> Option<Self> {
        let mut ctrl = false;
        let mut alt = false;
        let mut rest = text.trim();
        loop {
            if rest.len() >= 5 && rest[..5].eq_ignore_ascii_case("ctrl-") {
                ctrl = true;
                rest = &rest[5..];
            } else if rest.len() >= 4 && rest[..4].eq_ignore_ascii_case("alt-") {
                alt = true;
                rest = &rest[4..];
            } else {
                break;
            }
        }
        if rest.is_empty() {
            return None;
        }
        let code = if rest.len() > 1
            && (rest.starts_with('F') || rest.starts_with('f'))
            && rest[1..].chars().all(|c| c.is_ascii_digit())
        {
            let n: u8 = rest[1..].parse().ok()?;
            KeyCode::F(n)
        } else {
            let mut chars = rest.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(c)
        };
        let chord = Self::from_keycode(code, ctrl, alt)?;
        chord.rejection().is_none().then_some(chord)
    }

    /// 界面上显示的写法，如 `F2`、`Ctrl-]`、`Ctrl-S`。
    pub fn display(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("Ctrl-");
        }
        if self.alt {
            out.push_str("Alt-");
        }
        match self.key {
            ChordKey::Function(n) => out.push_str(&format!("F{n}")),
            ChordKey::Char(c) => out.push(c.to_ascii_uppercase()),
        }
        out
    }

    /// 不可绑定时给出中文原因。
    pub fn rejection(&self) -> Option<&'static str> {
        match self.key {
            ChordKey::Function(n) => match n {
                // 会话页的帮助只有 F1（`?` 要留给远端），占用后就再也打不开帮助页
                1 => Some("F1 是帮助键，占用后会无法查看键位说明"),
                2..=12 => None,
                _ => Some("功能键只支持 F2–F12"),
            },
            ChordKey::Char(c) => {
                if !self.ctrl && !self.alt {
                    return Some("必须带 Ctrl 或 Alt 修饰，否则会吞掉普通输入");
                }
                if self.ctrl {
                    // 这些组合在终端里与结构性按键同码，绑定后会随机失灵
                    match c.to_ascii_lowercase() {
                        'c' => return Some("Ctrl-C 要留给远端中断信号"),
                        'h' => return Some("Ctrl-H 与 Backspace 同码"),
                        'i' => return Some("Ctrl-I 与 Tab 同码"),
                        'm' => return Some("Ctrl-M 与 Enter 同码"),
                        '[' => return Some("Ctrl-[ 与 Esc 同码"),
                        _ => {}
                    }
                }
                None
            }
        }
    }

    /// 事件是否命中这条绑定。
    pub fn matches(&self, key: &KeyEvent) -> bool {
        if self.ctrl != key.modifiers.contains(KeyModifiers::CONTROL)
            || self.alt != key.modifiers.contains(KeyModifiers::ALT)
        {
            return false;
        }
        match (self.key, key.code) {
            (ChordKey::Function(n), KeyCode::F(m)) => n == m,
            (ChordKey::Char(a), KeyCode::Char(b)) => {
                if a.is_ascii_alphabetic() && b.is_ascii_alphabetic() {
                    a.to_ascii_lowercase() == b.to_ascii_lowercase()
                } else {
                    a == b
                }
            }
            _ => false,
        }
    }
}

/// 可在设置里改键的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    NewTab,
    NextTab,
    PrevTab,
    CloseTab,
    HostList,
    Browser,
    Passthrough,
    Redraw,
    Search,
}

impl Action {
    pub const ALL: [Action; 9] = [
        Action::NewTab,
        Action::NextTab,
        Action::PrevTab,
        Action::CloseTab,
        Action::HostList,
        Action::Browser,
        Action::Passthrough,
        Action::Redraw,
        Action::Search,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Action::NewTab => "新建标签页",
            Action::NextTab => "下一个标签",
            Action::PrevTab => "上一个标签",
            Action::CloseTab => "关闭标签 / 断开",
            Action::HostList => "返回主机列表",
            Action::Browser => "文件浏览器",
            Action::Passthrough => "内嵌 / 直通切换",
            Action::Redraw => "整屏重绘",
            Action::Search => "搜索历史输出",
        }
    }

    /// settings.ini 里的键名。
    pub fn ini_key(self) -> &'static str {
        match self {
            Action::NewTab => "key_new_tab",
            Action::NextTab => "key_next_tab",
            Action::PrevTab => "key_prev_tab",
            Action::CloseTab => "key_close_tab",
            Action::HostList => "key_host_list",
            Action::Browser => "key_browser",
            Action::Passthrough => "key_passthrough",
            Action::Redraw => "key_redraw",
            Action::Search => "key_search",
        }
    }

    pub fn from_ini_key(key: &str) -> Option<Action> {
        Action::ALL.into_iter().find(|a| a.ini_key() == key)
    }

    fn index(self) -> usize {
        Action::ALL.into_iter().position(|a| a == self).unwrap_or(0)
    }

    pub fn default_chord(self) -> Chord {
        match self {
            Action::NewTab => Chord::function(2),
            Action::NextTab => Chord::function(5),
            Action::PrevTab => Chord::function(6),
            Action::CloseTab => Chord::ctrl(']'),
            Action::HostList => Chord::ctrl('g'),
            Action::Browser => Chord::ctrl('s'),
            Action::Passthrough => Chord::ctrl('q'),
            Action::Redraw => Chord::ctrl('l'),
            Action::Search => Chord::function(3),
        }
    }
}

/// 动作数量固定为 `Action::ALL.len()`，数组长度跟着它走，加动作不用改这里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyBinds {
    items: [Chord; Action::ALL.len()],
}

impl Default for KeyBinds {
    fn default() -> Self {
        Self { items: Action::ALL.map(|a| a.default_chord()) }
    }
}

impl KeyBinds {
    pub fn get(&self, action: Action) -> Chord {
        self.items[action.index()]
    }

    pub fn display(&self, action: Action) -> String {
        self.get(action).display()
    }

    pub fn matches(&self, action: Action, key: &KeyEvent) -> bool {
        self.get(action).matches(key)
    }

    pub fn assign(&mut self, action: Action, chord: Chord) {
        self.items[action.index()] = chord;
    }

    /// 绑定新键；若该键已属于另一个动作，则两个动作互换（永远给出可行结果）。
    /// 返回被换走的那个动作。
    pub fn bind(&mut self, action: Action, chord: Chord) -> Option<Action> {
        let old = self.get(action);
        let swapped = Action::ALL
            .into_iter()
            .find(|a| *a != action && self.get(*a) == chord);
        self.items[action.index()] = chord;
        if let Some(other) = swapped {
            self.items[other.index()] = old;
        }
        swapped
    }

    pub fn reset(&mut self) {
        self.items = Action::ALL.map(|a| a.default_chord());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(code: KeyCode, ctrl: bool, alt: bool) -> KeyEvent {
        let mut m = KeyModifiers::empty();
        if ctrl {
            m |= KeyModifiers::CONTROL;
        }
        if alt {
            m |= KeyModifiers::ALT;
        }
        KeyEvent::new(code, m)
    }

    #[test]
    fn default_binds_are_the_documented_chords() {
        let b = KeyBinds::default();
        assert_eq!(b.display(Action::NewTab), "F2");
        assert_eq!(b.display(Action::NextTab), "F5");
        assert_eq!(b.display(Action::PrevTab), "F6");
        assert_eq!(b.display(Action::CloseTab), "Ctrl-]");
        assert_eq!(b.display(Action::HostList), "Ctrl-G");
        assert_eq!(b.display(Action::Browser), "Ctrl-S");
        assert_eq!(b.display(Action::Passthrough), "Ctrl-Q");
        assert_eq!(b.display(Action::Redraw), "Ctrl-L");
        assert_eq!(b.display(Action::Search), "F3");
    }

    #[test]
    fn defaults_do_not_collide() {
        let b = KeyBinds::default();
        for (idx, a) in Action::ALL.iter().enumerate() {
            for other in Action::ALL.iter().skip(idx + 1) {
                assert_ne!(b.get(*a), b.get(*other), "{} 与 {} 默认撞键", a.ini_key(), other.ini_key());
            }
        }
    }

    #[test]
    fn matches_ignores_case_and_shift() {
        let b = KeyBinds::default();
        assert!(b.matches(Action::Browser, &event(KeyCode::Char('s'), true, false)));
        assert!(b.matches(Action::Browser, &event(KeyCode::Char('S'), true, false)));
        // 少了 Ctrl 就是普通字符，绝不能命中
        assert!(!b.matches(Action::Browser, &event(KeyCode::Char('s'), false, false)));
        assert!(b.matches(Action::NewTab, &event(KeyCode::F(2), false, false)));
    }

    #[test]
    fn parse_round_trips_display() {
        for a in Action::ALL {
            let chord = a.default_chord();
            let text = chord.display();
            assert_eq!(Chord::parse(&text), Some(chord), "{text} 应当能解析回自身");
        }
        assert_eq!(Chord::parse("ctrl-t"), Some(Chord::ctrl('t')));
        assert_eq!(Chord::parse("F12"), Some(Chord::function(12)));
        assert_eq!(Chord::parse("Ctrl-Alt-k"), Chord::parse("Alt-Ctrl-k"));
        // 修饰前缀后的 "2" 是数字键，不是 F2
        assert_eq!(
            Chord::parse("ALT-2"),
            Some(Chord { key: ChordKey::Char('2'), ctrl: false, alt: true })
        );
    }

    #[test]
    fn rejects_unbindable_and_colliding_keys() {
        // 裸字符会吞掉普通输入
        assert_eq!(Chord::function(2).rejection(), None);
        assert!(Chord { key: ChordKey::Char('x'), ctrl: false, alt: false }.rejection().is_some());
        // F1 是会话页唯一的帮助键
        assert!(Chord::function(1).rejection().is_some());
        assert_eq!(Chord::parse("F1"), None);
        // 终端里同码的组合
        for c in ['c', 'h', 'i', 'm', '['] {
            assert!(Chord::ctrl(c).rejection().is_some(), "Ctrl-{c} 必须被拒绝");
        }
        assert_eq!(Chord::parse("x"), None);
        assert_eq!(Chord::parse("Ctrl-c"), None);
        assert_eq!(Chord::parse("F13"), None);
        assert_eq!(Chord::parse("Enter"), None);
        assert_eq!(Chord::parse(""), None);
        assert_eq!(Chord::parse("Ctrl-"), None);
    }

    #[test]
    fn binding_a_taken_key_swaps_the_two_actions() {
        let mut b = KeyBinds::default();
        // 把「新建标签页」改成空闲的 Ctrl-T：不冲突，无互换
        assert_eq!(b.bind(Action::NewTab, Chord::ctrl('t')), None);
        assert_eq!(b.get(Action::NewTab), Chord::ctrl('t'));
        // F2 已被让出，此时把「下一个标签」绑成 F2 同样不冲突
        assert_eq!(b.bind(Action::NextTab, Chord::function(2)), None);
        assert_eq!(b.get(Action::PrevTab), Chord::function(6));
        // 再把「上一个标签」按成 F2：两个动作各自拿到对方原来的键，仍是可行组合
        let swapped = b.bind(Action::PrevTab, Chord::function(2));
        assert_eq!(swapped, Some(Action::NextTab));
        assert_eq!(b.get(Action::PrevTab), Chord::function(2));
        assert_eq!(b.get(Action::NextTab), Chord::function(6));
        // 互换不该牵连其它动作
        assert_eq!(b.get(Action::NewTab), Chord::ctrl('t'));
        assert_eq!(b.get(Action::Search), Chord::function(3));
    }

    #[test]
    fn reset_restores_every_binding() {
        let mut b = KeyBinds::default();
        b.bind(Action::Redraw, Chord::function(9));
        b.reset();
        assert_eq!(b, KeyBinds::default());
    }
}

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// 本地鼠标动作，对应远端程序能收到的四种事件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Drag,
    Move,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

impl MouseButton {
    fn code(self) -> u16 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
            Self::WheelUp => 64,
            Self::WheelDown => 65,
        }
    }
}

/// 远端当前鼠标模式收不收这种事件。X10（`Press`）只报按下，
/// VT200（`PressRelease`）加松开，`ButtonMotion` 再加按住拖动，
/// `AnyMotion` 连没按键的移动也报。
fn mode_accepts(mode: MouseProtocolMode, action: MouseAction) -> bool {
    match mode {
        MouseProtocolMode::None => false,
        _ if action == MouseAction::Press => true,
        MouseProtocolMode::Press => false,
        MouseProtocolMode::PressRelease => action == MouseAction::Release,
        MouseProtocolMode::ButtonMotion => {
            action == MouseAction::Release || action == MouseAction::Drag
        }
        MouseProtocolMode::AnyMotion => true,
    }
}

/// 把本地鼠标事件编码成远端终端协议。`col`/`row` 是远端 PTY 里的
/// 1 起坐标；`Shift` 不在这里处理 —— 它留给本地拖选复制，调用方据此不分流。
pub fn mouse_to_bytes(
    action: MouseAction,
    button: MouseButton,
    col: u16,
    row: u16,
    ctrl: bool,
    alt: bool,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    if !mode_accepts(mode, action) {
        return None;
    }
    let col = col.max(1);
    let row = row.max(1);
    // 低位是按键号；拖动 = 加 32；没按键的移动用 3。
    let mut cb = match action {
        MouseAction::Move => 3,
        _ => button.code(),
    };
    if matches!(action, MouseAction::Drag | MouseAction::Move) {
        cb += 32;
    }
    if alt {
        cb += 8;
    }
    if ctrl {
        cb += 16;
    }
    let mut v = Vec::new();
    match encoding {
        MouseProtocolEncoding::Sgr => {
            v.extend_from_slice(b"\x1b[<");
            v.extend_from_slice(cb.to_string().as_bytes());
            v.push(b';');
            v.extend_from_slice(col.to_string().as_bytes());
            v.push(b';');
            v.extend_from_slice(row.to_string().as_bytes());
            // SGR 用最后一个字节区分按下/松开，松开时 Cb 仍是那个键号
            v.push(if action == MouseAction::Release { b'm' } else { b'M' });
        }
        MouseProtocolEncoding::Default => {
            // X10/VT200：Cb、Cx、Cy 各一个字节，全部 +32；松开固定用 3。
            v.extend_from_slice(b"\x1b[M");
            let cb = if action == MouseAction::Release { 3 } else { cb };
            v.push((cb + 32) as u8);
            v.push((col.min(223) + 32) as u8);
            v.push((row.min(223) + 32) as u8);
        }
        MouseProtocolEncoding::Utf8 => {
            // 1005：坐标不再受 223 限制，按 Unicode 码点编码
            v.extend_from_slice(b"\x1b[M");
            let cb = if action == MouseAction::Release { 3 } else { cb };
            push_utf8_cell(&mut v, cb);
            push_utf8_cell(&mut v, col);
            push_utf8_cell(&mut v, row);
        }
    }
    Some(v)
}

fn push_utf8_cell(v: &mut Vec<u8>, value: u16) {
    let mut buf = [0u8; 4];
    let c = char::from_u32(u32::from(value) + 32).unwrap_or('\u{7f}');
    v.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

/// Translate a local key event into the byte sequence a real terminal would
/// send to a PTY. Returns `None` for release/no-op events.
///
/// `app_cursor` is the remote's DECCKM state. Full-screen programs (vim,
/// htop, less) turn it on with `smkx` (`ESC [ ? 1 h`), after which cursor
/// keys must go out as SS3 (`ESC O A`) because that is what their terminfo
/// entries advertise. Sending CSI there leaves vim unable to match the key
/// at all; shells use readline, which accepts both forms, so the breakage
/// only shows up inside vi.
pub fn key_to_bytes(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let m = key.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);
    let shift = m.contains(KeyModifiers::SHIFT);

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                Some(ctrl_char(c))
            } else if alt {
                let mut v = vec![0x1b];
                v.extend_from_slice(&utf(c));
                Some(v)
            } else {
                Some(utf(c))
            }
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(vec![0x1b, b'[', b'Z']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(cursor_key(b'A', app_cursor, ctrl, alt, shift)),
        KeyCode::Down => Some(cursor_key(b'B', app_cursor, ctrl, alt, shift)),
        KeyCode::Right => Some(cursor_key(b'C', app_cursor, ctrl, alt, shift)),
        KeyCode::Left => Some(cursor_key(b'D', app_cursor, ctrl, alt, shift)),
        KeyCode::Home => Some(home_end(b'H', app_cursor, alt)),
        KeyCode::End => Some(home_end(b'F', app_cursor, alt)),
        KeyCode::PageUp => Some(vec![0x1b, b'[', b'5', b'~']),
        KeyCode::PageDown => Some(vec![0x1b, b'[', b'6', b'~']),
        KeyCode::Insert => Some(vec![0x1b, b'[', b'2', b'~']),
        KeyCode::Delete => Some(vec![0x1b, b'[', b'3', b'~']),
        KeyCode::F(n) => Some(function_key(n, shift, ctrl, alt)),
        _ => None,
    }
}

fn utf(c: char) -> Vec<u8> {
    let mut buf = [0u8; 4];
    c.encode_utf8(&mut buf).as_bytes().to_vec()
}

fn ctrl_char(c: char) -> Vec<u8> {
    let lower = c.to_ascii_lowercase();
    match lower {
        'a'..='z' => vec![lower as u8 - b'a' + 1],
        ' ' | '2' => vec![0],
        '[' | '3' => vec![27],
        '\\' | '4' => vec![28],
        ']' | '5' => vec![29],
        '^' | '6' => vec![30],
        '_' | '7' | '/' => vec![31],
        '8' => vec![127],
        _ => vec![c as u8],
    }
}

/// Unmodified arrows switch to SS3 while DECCKM is on; with any modifier
/// xterm keeps the CSI `1;N` form in both modes.
fn cursor_key(letter: u8, app_cursor: bool, ctrl: bool, alt: bool, shift: bool) -> Vec<u8> {
    if app_cursor && !(ctrl || alt || shift) {
        return vec![0x1b, b'O', letter];
    }
    csi(letter as char, ctrl, alt, shift)
}

/// Home/End follow DECCKM too (terminfo: `khome=\EOH`, `kend=\EOF`).
fn home_end(letter: u8, app_cursor: bool, alt: bool) -> Vec<u8> {
    if alt {
        vec![0x1b, b'[', b'1', b';', b'5', letter]
    } else if app_cursor {
        vec![0x1b, b'O', letter]
    } else {
        vec![0x1b, b'[', letter]
    }
}

fn csi(letter: char, ctrl: bool, alt: bool, shift: bool) -> Vec<u8> {
    let letter = letter as u8;
    let mut modifier = 1;
    if shift {
        modifier += 1;
    }
    if alt {
        modifier += 2;
    }
    if ctrl {
        modifier += 4;
    }
    let mut v: Vec<u8> = Vec::new();
    if alt {
        v.push(0x1b);
    }
    if modifier == 1 {
        v.extend_from_slice(&[0x1b, b'[', letter]);
    } else {
        v.extend_from_slice(&[0x1b, b'[', b'1']);
        v.extend_from_slice(b";");
        v.extend_from_slice(modifier.to_string().as_bytes());
        v.push(letter);
    }
    v
}

fn function_key(n: u8, shift: bool, ctrl: bool, alt: bool) -> Vec<u8> {
    let mut modifier = 1u8;
    if shift {
        modifier += 1;
    }
    if alt {
        modifier += 2;
    }
    if ctrl {
        modifier += 4;
    }
    let mut v: Vec<u8> = Vec::new();
    if alt {
        v.push(0x1b);
    }
    match n {
        1..=4 => {
            let tail = b'P' + (n - 1);
            if modifier == 1 {
                v.extend_from_slice(&[0x1b, b'O', tail]);
            } else {
                v.extend_from_slice(&[0x1b, b'[', b'1', b';']);
                v.extend_from_slice(modifier.to_string().as_bytes());
                v.push(tail);
            }
        }
        other => {
            let code = match other {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return vec![],
            };
            v.extend_from_slice(&[0x1b, b'[']);
            v.extend_from_slice(code.to_string().as_bytes());
            if modifier != 1 {
                v.push(b';');
                v.extend_from_slice(modifier.to_string().as_bytes());
            }
            v.push(b'~');
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// vim 启动后发 smkx 打开 DECCKM，此时 terminfo 里的方向键是 \EOA…
    /// 再发 \E[A 它认不出来，于是 insert 模式下方向键全哑。
    #[test]
    fn unmodified_arrows_switch_to_ss3_in_application_cursor_mode() {
        for (code, letter) in [
            (KeyCode::Up, b'A'),
            (KeyCode::Down, b'B'),
            (KeyCode::Left, b'D'),
            (KeyCode::Right, b'C'),
        ] {
            assert_eq!(
                key_to_bytes(&key(code), true),
                Some(vec![0x1b, b'O', letter]),
                "应用光标键模式下 {code:?} 应发 SS3"
            );
            assert_eq!(
                key_to_bytes(&key(code), false),
                Some(vec![0x1b, b'[', letter]),
                "普通模式下 {code:?} 仍发 CSI"
            );
        }
    }

    #[test]
    fn modified_arrows_stay_csi_in_application_cursor_mode() {
        let shifted = KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(key_to_bytes(&shifted, true), Some(b"\x1b[1;2A".to_vec()));
        assert_eq!(key_to_bytes(&shifted, false), Some(b"\x1b[1;2A".to_vec()));
    }

    #[test]
    fn home_and_end_follow_application_cursor_mode() {
        assert_eq!(key_to_bytes(&key(KeyCode::Home), true), Some(b"\x1bOH".to_vec()));
        assert_eq!(key_to_bytes(&key(KeyCode::Home), false), Some(b"\x1b[H".to_vec()));
        assert_eq!(key_to_bytes(&key(KeyCode::End), true), Some(b"\x1bOF".to_vec()));
        assert_eq!(key_to_bytes(&key(KeyCode::End), false), Some(b"\x1b[F".to_vec()));
    }

    #[test]
    fn paging_keys_ignore_cursor_mode() {
        assert_eq!(key_to_bytes(&key(KeyCode::PageUp), true), Some(b"\x1b[5~".to_vec()));
        assert_eq!(key_to_bytes(&key(KeyCode::PageDown), true), Some(b"\x1b[6~".to_vec()));
    }

    fn mouse(
        action: MouseAction,
        button: MouseButton,
        col: u16,
        row: u16,
        mode: MouseProtocolMode,
        encoding: MouseProtocolEncoding,
    ) -> Option<Vec<u8>> {
        mouse_to_bytes(action, button, col, row, false, false, mode, encoding)
    }

    #[test]
    fn sgr_mouse_click_release_drag_and_wheel() {
        let sgr = MouseProtocolEncoding::Sgr;
        let mode = MouseProtocolMode::ButtonMotion;
        assert_eq!(
            mouse(MouseAction::Press, MouseButton::Left, 10, 5, mode, sgr),
            Some(b"\x1b[<0;10;5M".to_vec())
        );
        assert_eq!(
            mouse(MouseAction::Release, MouseButton::Left, 10, 5, mode, sgr),
            Some(b"\x1b[<0;10;5m".to_vec())
        );
        assert_eq!(
            mouse(MouseAction::Drag, MouseButton::Left, 12, 6, mode, sgr),
            Some(b"\x1b[<32;12;6M".to_vec())
        );
        assert_eq!(
            mouse(MouseAction::Press, MouseButton::WheelUp, 3, 4, mode, sgr),
            Some(b"\x1b[<64;3;4M".to_vec())
        );
        assert_eq!(
            mouse(MouseAction::Press, MouseButton::WheelDown, 3, 4, mode, sgr),
            Some(b"\x1b[<65;3;4M".to_vec())
        );
    }

    #[test]
    fn mouse_modifiers_add_the_xterm_bits() {
        let sgr = MouseProtocolEncoding::Sgr;
        let mode = MouseProtocolMode::PressRelease;
        assert_eq!(
            mouse_to_bytes(MouseAction::Press, MouseButton::Left, 1, 2, true, false, mode, sgr),
            Some(b"\x1b[<16;1;2M".to_vec()),
            "Ctrl 加 16"
        );
        assert_eq!(
            mouse_to_bytes(MouseAction::Press, MouseButton::Left, 1, 2, false, true, mode, sgr),
            Some(b"\x1b[<8;1;2M".to_vec()),
            "Alt 加 8"
        );
    }

    #[test]
    fn x10_encoding_offsets_bytes_and_reports_release_as_three() {
        let utf8 = MouseProtocolEncoding::Default;
        let mode = MouseProtocolMode::PressRelease;
        assert_eq!(
            mouse(MouseAction::Press, MouseButton::Left, 10, 5, mode, utf8),
            Some(vec![0x1b, b'[', b'M', 32, 42, 37])
        );
        assert_eq!(
            mouse(MouseAction::Release, MouseButton::Left, 10, 5, mode, utf8),
            Some(vec![0x1b, b'[', b'M', 35, 42, 37]),
            "X10 松开固定发按键位 3"
        );
        assert_eq!(
            mouse(
                MouseAction::Drag,
                MouseButton::Left,
                10,
                5,
                MouseProtocolMode::ButtonMotion,
                utf8
            ),
            Some(vec![0x1b, b'[', b'M', 64, 42, 37]),
            "拖动位是 32 + 键号"
        );
        // 1005 用 Unicode 码点装坐标，不再被 223 截断
        let wide = mouse(MouseAction::Press, MouseButton::Left, 300, 5, mode, MouseProtocolEncoding::Utf8)
            .unwrap();
        assert_eq!(&wide[..3], b"\x1b[M");
        assert_eq!(&wide[3..], " \u{14c}%".as_bytes(), "300+32=332 编成 UTF-8");
    }

    #[test]
    fn mouse_events_are_filtered_by_the_remote_mode() {
        let sgr = MouseProtocolEncoding::Sgr;
        assert_eq!(mouse(MouseAction::Press, MouseButton::Left, 1, 1, MouseProtocolMode::None, sgr), None);
        assert_eq!(
            mouse(MouseAction::Release, MouseButton::Left, 1, 1, MouseProtocolMode::Press, sgr),
            None,
            "X10 模式不报松开"
        );
        assert_eq!(
            mouse(MouseAction::Drag, MouseButton::Left, 1, 1, MouseProtocolMode::PressRelease, sgr),
            None,
            "1000 不报拖动"
        );
        assert!(mouse(
            MouseAction::Drag,
            MouseButton::Left,
            1,
            1,
            MouseProtocolMode::ButtonMotion,
            sgr
        )
        .is_some());
        assert_eq!(
            mouse(MouseAction::Move, MouseButton::Left, 1, 1, MouseProtocolMode::ButtonMotion, sgr),
            None,
            "只有 1003 才报无键移动"
        );
        assert_eq!(
            mouse(MouseAction::Move, MouseButton::Left, 1, 1, MouseProtocolMode::AnyMotion, sgr),
            Some(b"\x1b[<35;1;1M".to_vec())
        );
    }
}

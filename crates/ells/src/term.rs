//! 进程级终端原语：进入/退出受控状态、异常恢复、OSC 标题与剪贴板。
//!
//! 这些调用必须集中在一处：raw mode / 备用屏 / 鼠标捕获 / 括号粘贴四项只要
//! 有一项没关掉，用户回到 shell 后就会看不见输入或残留 TUI 画面。

use std::io::{stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
use crossterm::execute;

/// 终端是否已被切到 Kitty 键盘协议：没打开就不能发 Pop，否则可能弹掉外层程序的那层。
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

/// 询问终端是否支持 Kitty 键盘协议，支持就把扩展键打开。
///
/// macOS 上这是 Cmd+S / Cmd+Q 能被程序看见的前提：普通终端会把 Cmd 组合
/// 交给自己的菜单，根本不写进 PTY。不支持（如 Terminal.app / conhost）时保持原样，
/// Ctrl 快捷键照常。
///
/// 不用 crossterm 的 `supports_keyboard_enhancement()`：它要等满 2 秒才肯
/// 断定"不支持"，Terminal.app / 大部分 Linux 终端上等于每次启动白停两秒。
/// 这里自己发 `CSI ? u` + DA1，250 毫秒内没动静就放弃。
pub fn enable_keyboard_enhancement() -> bool {
    if !query_keyboard_enhancement() {
        return false;
    }
    let mut out = stdout();
    let pushed = execute!(
        out,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok();
    let _ = out.flush();
    KEYBOARD_ENHANCED.store(pushed, Ordering::Relaxed);
    pushed
}

/// conhost / ConPTY 不认 kitty 扩展键，Win 键组合也到不了程序手里。
#[cfg(windows)]
fn query_keyboard_enhancement() -> bool {
    false
}

/// 在 raw mode 下问一次；此时除了这条应答不该有别的输入，读掉的字节丢掉即可。
#[cfg(unix)]
fn query_keyboard_enhancement() -> bool {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;
    use std::time::{Duration, Instant};

    let Ok(mut tty) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty")
    else {
        return false;
    };
    if tty.write_all(b"\x1b[?u\x1b[c").and_then(|_| tty.flush()).is_err() {
        return false;
    }
    // 先问能力（CSI ? u），再问 DA1（CSI c）。认识 kitty 的终端回 `CSI ? <flags> u`，
    // 不认识的只会回 DA1 的 `CSI ? <params> c`——这就是"没看懂那条问句"的信号。
    let deadline = Instant::now() + Duration::from_millis(250);
    let mut seen = Vec::new();
    let mut buf = [0u8; 96];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let mut pfd = libc::pollfd {
            fd: tty.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if rc <= 0 {
            break;
        }
        match tty.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
        }
        if private_reply(&seen, b'u') {
            return true;
        }
        if private_reply(&seen, b'c') {
            return false;
        }
    }
    false
}

/// 找 `ESC [ ? <数字/分号> <fin>`。`CSI ? u` 即使 flags 是 0 也说明支持协议。
#[cfg(any(unix, test))]
fn private_reply(buf: &[u8], fin: u8) -> bool {
    let mut i = 0;
    while i + 3 < buf.len() {
        if &buf[i..i + 3] == b"\x1b[?" {
            let start = i + 3;
            let mut j = start;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                j += 1;
            }
            if j > start && j < buf.len() && buf[j] == fin {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// 终端是否真的会把 Cmd/Win 组合键送进来（即上面的扩展键已打开）。
pub fn cmd_shortcuts_available() -> bool {
    KEYBOARD_ENHANCED.load(Ordering::Relaxed)
}

/// 无条件把终端交还给 shell。panic 路径上任何一步失败都忽略，尽力而为。
pub fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut out = stdout();
    if KEYBOARD_ENHANCED.swap(false, Ordering::Relaxed) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        out,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        SetCursorStyle::DefaultUserShape
    );
    // 撤销可能残留的颜色/加粗与光标隐藏，否则用户下一条命令是彩色的
    let _ = out.write_all(b"\x1b[0m\x1b[?25h");
    let _ = out.flush();
    // 空标题 = 让终端回落到默认标签名（Windows Terminal/iTerm2 均如此处理）
    set_term_title("");
}

/// 内置编辑器的光标形状：闪烁细竖线（DECSCUSR 5），关闭时恢复终端默认。
/// 形状由终端绘制，所以不会像反显格子那样盖掉光标下的字符；
/// 不认这条指令的终端会保持原样，不会更差。
pub fn set_editor_cursor(on: bool) {
    let mut out = stdout();
    let shape = if on {
        SetCursorStyle::BlinkingBar
    } else {
        SetCursorStyle::DefaultUserShape
    };
    let _ = execute!(out, shape);
    let _ = out.flush();
}

/// 通过 OSC 52 转义序列把文本放进终端模拟器的剪贴板
/// （Windows Terminal / iTerm2 / kitty 等原生支持，无需系统剪贴板依赖）。
pub fn copy_osc52(text: &str) {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = stdout();
    let _ = write!(out, "\x1b]52;c;{b64}\x1b\\");
    let _ = out.flush();
}

/// OSC 0 设置终端标签页/窗口标题（Windows Terminal、iTerm2、kitty 等均支持）。
pub fn set_term_title(title: &str) {
    let mut out = stdout();
    let _ = write!(out, "\x1b]0;{title}\x07");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::private_reply;

    #[test]
    fn kitty_flags_reply_means_supported() {
        assert!(private_reply(b"\x1b[?1u", b'u'));
        assert!(private_reply(b"\x1b[?0u", b'u'));
        assert!(private_reply(b"junk\x1b[?3;5u", b'u'));
        assert!(!private_reply(b"\x1b[?62;4;6c", b'u'));
    }

    #[test]
    fn da1_reply_without_kitty_means_unsupported() {
        assert!(private_reply(b"\x1b[?62;4;6c", b'c'));
        assert!(private_reply(b"\x1b[?1;2c", b'c'));
        assert!(!private_reply(b"\x1b[?1u", b'c'));
        assert!(!private_reply(b"\x1b[?c", b'c'), "没有参数的回答不算数");
        assert!(!private_reply(b"\x1b[?1", b'c'), "半截应答不能算");
    }
}

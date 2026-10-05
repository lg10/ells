//! 进程级终端原语：进入/退出受控状态、异常恢复、OSC 标题与剪贴板。
//!
//! 这些调用必须集中在一处：raw mode / 备用屏 / 鼠标捕获 / 括号粘贴四项只要
//! 有一项没关掉，用户回到 shell 后就会看不见输入或残留 TUI 画面。

use std::io::{stdout, Write};

use crossterm::event::{DisableBracketedPaste, DisableMouseCapture};
use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
use crossterm::execute;

/// 无条件把终端交还给 shell。panic 路径上任何一步失败都忽略，尽力而为。
pub fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut out = stdout();
    let _ = execute!(
        out,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    // 撤销可能残留的颜色/加粗与光标隐藏，否则用户下一条命令是彩色的
    let _ = out.write_all(b"\x1b[0m\x1b[?25h");
    let _ = out.flush();
    // 空标题 = 让终端回落到默认标签名（Windows Terminal/iTerm2 均如此处理）
    set_term_title("");
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

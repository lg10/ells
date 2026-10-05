use std::io::Write;

use crossterm::event::{KeyEvent, KeyEventKind};
use ells_core::ssh::RemoteSession;
use ells_term::{key_to_bytes, Emulator};
use ratatui::layout::Rect;

use crate::keybinds::{Action, KeyBinds};
use crate::zmodem::{Watcher, ZmodemEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermMode {
    Embedded,
    Passthrough,
}

pub struct SessionState {
    pub label: String,
    pub session: RemoteSession,
    pub emu: Emulator,
    pub mode: TermMode,
    header_rows: u16,
    /// 向上回看的行数（0=实时底部），滚轮驱动，历史来自 vt100 scrollback。
    pub scroll: usize,
    /// 鼠标拖选：锚点与当前点（终端绝对坐标 col,row）。
    selection: Option<((u16, u16), (u16, u16))>,
    /// Protocol-level ZMODEM watcher over the remote output stream.
    zmodem: Watcher,
    /// When true we are swallowing remote bytes (a hijacked sz/rz handshake).
    intercepting: bool,
    /// Extra Ctrl-C re-sends already emitted during the current swallow window.
    abort_retries: u8,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SessionAction {
    Keep,
    Detach,
}

impl SessionState {
    pub fn new(label: String, session: RemoteSession, rows: u16, cols: u16) -> Self {
        // 与会话页顶部标题栏的行数保持一致（见 ui::HEADER_ROWS）
        let header_rows = crate::ui::HEADER_ROWS;
        let cols = cols.max(20);
        let term_rows = rows.saturating_sub(header_rows).max(1);
        let emu = Emulator::new(term_rows, cols);
        // PTY was requested with full terminal size at connect time; correct
        // it to the embedded viewport now that chrome is known.
        session.resize(cols, term_rows);
        Self {
            label,
            session,
            emu,
            mode: TermMode::Embedded,
            header_rows,
            scroll: 0,
            selection: None,
            zmodem: Watcher::new(),
            intercepting: false,
            abort_retries: 0,
        }
    }

    pub fn handle_key(&mut self, key: &KeyEvent, binds: &KeyBinds) -> SessionAction {
        if key.kind == KeyEventKind::Release {
            return SessionAction::Keep;
        }
        // 任何按键都回到实时底部并取消选择高亮（与常见终端一致）
        self.scroll = 0;
        self.selection = None;
        if binds.matches(Action::Passthrough, key) {
            self.toggle_mode();
            return SessionAction::Keep;
        }
        if binds.matches(Action::CloseTab, key) {
            return SessionAction::Detach;
        }
        if binds.matches(Action::Redraw, key) {
            self.request_full_redraw();
            return SessionAction::Keep;
        }
        if let Some(bytes) = key_to_bytes(key) {
            self.feed_input(bytes);
        }
        SessionAction::Keep
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.feed_input(text.as_bytes().to_vec());
    }

    fn feed_input(&mut self, bytes: Vec<u8>) {
        self.session.write_input(bytes);
        if self.mode == TermMode::Embedded {
            while let Some(answer) = self.emu.take_answers() {
                self.session.write_input(answer);
            }
        }
    }

    /// Feed remote output. Returns Some(event) exactly once when a ZMODEM
    /// handshake is detected; afterwards all output is swallowed until
    /// `end_intercept`.
    pub fn handle_output(&mut self, bytes: &[u8]) -> Option<ZmodemEvent> {
        if self.intercepting {
            // lrzsz rz/sz re-sends its handshake every ~3s; make sure it dies
            // by re-aborting a few times while we keep swallowing output.
            if !bytes.is_empty() && self.abort_retries < 3 {
                self.abort_retries += 1;
                self.send_abort();
            }
            return None;
        }
        let ev = self.zmodem.feed(bytes);
        if let Some(e) = &ev {
            if !matches!(e, ZmodemEvent::Missing { .. }) {
                self.intercepting = true;
                self.abort_retries = 0;
                self.send_abort();
                return ev;
            }
        }
        match self.mode {
            TermMode::Embedded => self.emu.process(bytes),
            TermMode::Passthrough => {
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(bytes);
                let _ = stdout.flush();
            }
        }
        ev
    }

    pub fn is_intercepting(&self) -> bool {
        self.intercepting
    }

    /// lrzsz aborts a handshake when it sees ≥5 consecutive CAN octets;
    /// send a full CAN×10 burst plus Ctrl-C as insurance.
    fn send_abort(&mut self) {
        self.session.write_input(vec![0x18; 10]);
        self.session.write_input(vec![0x03]);
    }

    pub fn end_intercept(&mut self) {
        let was_intercepting = self.intercepting;
        self.intercepting = false;
        self.abort_retries = 0;
        self.zmodem.reset();
        if was_intercepting {
            // The shell reprinted its prompt while we were swallowing; nudge it
            // so the user lands back on a visible prompt instead of a blank page.
            self.session.write_input(vec![b'\r']);
        }
        // 协议字节在拦截时已整块丢弃，模拟器画面未被污染：只发 SIGWINCH
        // 让全屏应用自绘，绝不 reset_screen（否则每次 rz/sz 后都"清屏"）。
        if self.mode == TermMode::Embedded {
            let (rows, cols) = self.emu.size();
            self.session.resize(cols, rows.saturating_sub(1).max(1));
            self.session.resize(cols, rows);
        }
    }

    /// Drain echoed shell commands observed by the watcher (cd tracking).
    pub fn drain_commands(&mut self) -> Vec<Vec<String>> {
        self.zmodem.take_commands()
    }

    pub fn flush_pending_answers(&mut self) {
        if self.mode == TermMode::Embedded {
            while let Some(answer) = self.emu.take_answers() {
                self.session.write_input(answer);
            }
        }
    }

    pub fn toggle_mode(&mut self) {
        let (cols, rows) = term_size();
        let cols = cols.max(20);
        self.scroll = 0;
        self.selection = None;
        self.mode = match self.mode {
            TermMode::Embedded => {
                self.session.resize(cols, rows.max(1));
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(b"\x1b[2J\x1b[3J\x1b[H\x1b[?25h");
                let _ = stdout.flush();
                TermMode::Passthrough
            }
            TermMode::Passthrough => {
                let term_rows = rows.saturating_sub(self.header_rows).max(1);
                self.emu.resize(term_rows, cols);
                // Two window-changes: apps repaint on SIGWINCH even when the
                // final size matches what the embedded viewport already has.
                self.session.resize(cols, term_rows.saturating_sub(1).max(1));
                self.session.resize(cols, term_rows);
                TermMode::Embedded
            }
        };
    }

    pub fn request_full_redraw(&mut self) {
        if self.mode == TermMode::Embedded {
            let (rows, cols) = self.emu.size();
            self.emu.reset_screen();
            self.session.resize(cols, rows.saturating_sub(1).max(1));
            self.session.resize(cols, rows);
        }
    }

    pub fn handle_resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(20);
        match self.mode {
            TermMode::Embedded => {
                let term_rows = rows.saturating_sub(self.header_rows).max(1);
                self.emu.resize(term_rows, cols);
                self.session.resize(cols, term_rows);
            }
            TermMode::Passthrough => {
                self.session.resize(cols, rows.max(1));
            }
        }
    }

    pub fn close(&mut self) {
        self.session.close();
    }

    /// 滚轮回看：一次 3 行；直接借用 vt100 的 scrollback 视图偏移（保留颜色），
    /// 回读钳制后的实际偏移作为本地状态。
    pub fn handle_wheel(&mut self, delta: i8) {
        if self.mode != TermMode::Embedded {
            return;
        }
        let target = if delta < 0 {
            self.scroll.saturating_add(3)
        } else {
            self.scroll.saturating_sub(3)
        };
        self.emu.set_scrollback(target);
        self.scroll = self.emu.scrollback_offset();
    }

    /// 直通模式下 `screen()` 属于远端程序，ells 无法回看，也就没有历史可搜。
    pub fn history_lines(&mut self) -> (usize, Vec<String>) {
        if self.mode != TermMode::Embedded {
            return (0, Vec::new());
        }
        self.emu.history_lines()
    }

    /// 把视图滚到历史第 `idx` 行（下标来自 `history_lines`），
    /// 返回该行在当前视图中的行号，供高亮使用。
    pub fn jump_history(&mut self, max: usize, idx: usize) -> u16 {
        if self.mode != TermMode::Embedded {
            return 0;
        }
        if idx >= max {
            self.emu.set_scrollback(0);
            self.scroll = 0;
            (idx - max) as u16
        } else {
            self.emu.set_scrollback(max - idx);
            self.scroll = self.emu.scrollback_offset();
            0
        }
    }

    pub fn begin_selection(&mut self, col: u16, row: u16) {
        self.selection = Some(((col, row), (col, row)));
    }

    pub fn update_selection(&mut self, col: u16, row: u16) {
        if let Some((anchor, _)) = self.selection {
            self.selection = Some((anchor, (col, row)));
        }
    }

    /// 选择区域规范化为 (左上, 右下)。
    pub fn selection_rect(&self) -> Option<((u16, u16), (u16, u16))> {
        let ((c0, r0), (c1, r1)) = self.selection?;
        Some((
            (c0.min(c1), r0.min(r1)),
            (c0.max(c1), r0.max(r1)),
        ))
    }

    /// 当前视图第 r 行的纯文本（视图已含 scrollback 偏移，颜色无关）。
    fn view_row_text(&self, r: u16) -> String {
        let (_rows, cols) = self.emu.size();
        let screen = self.emu.screen();
        let mut s = String::new();
        for c in 0..cols {
            match screen.cell(r, c) {
                Some(cell) => {
                    if cell.is_wide_continuation() {
                        continue;
                    }
                    let t = cell.contents();
                    s.push(if t.is_empty() { ' ' } else { t.chars().next().unwrap_or(' ') });
                }
                None => s.push(' '),
            }
        }
        s
    }

    /// 提取选区内的文本（area 为内嵌终端可视区）。
    pub fn selected_text(&self, area: Rect) -> String {
        let Some(((x0, y0), (x1, y1))) = self.selection_rect() else {
            return String::new();
        };
        let y0 = y0.max(area.y);
        let y1 = y1.min(area.y.saturating_add(area.height).saturating_sub(1));
        if y0 > y1 || x1 < area.x {
            return String::new();
        }
        let mut out: Vec<String> = Vec::new();
        for y in y0..=y1 {
            let text = self.view_row_text(y - area.y);
            let cx0 = x0.max(area.x).saturating_sub(area.x) as usize;
            let cx1 = x1.saturating_sub(area.x) as usize + 1;
            let chars: Vec<char> = text.chars().collect();
            let take = cx1.min(chars.len()).saturating_sub(cx0);
            out.push(chars.iter().skip(cx0).take(take).collect::<String>().trim_end().to_string());
        }
        while out.last().is_some_and(|s| s.is_empty()) {
            out.pop();
        }
        out.join("\n")
    }
}

fn term_size() -> (u16, u16) {
    crossterm::terminal::size().map(|(c, r)| (c as u16, r as u16)).unwrap_or((80, 24))
}

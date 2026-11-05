use std::io::Write;

use crossterm::event::{KeyEvent, KeyEventKind};
use ells_core::ssh::RemoteSession;
use ells_term::vt100::{MouseProtocolEncoding, MouseProtocolMode};
use ells_term::{key_to_bytes, mouse_to_bytes, Emulator, MouseAction, MouseButton};
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
    /// 底部指标行占用的行数（0 = 那一行已经让给终端：设置里关了，或这台机器采不到）。
    /// 必须与 `ui::draw_session` 真的画几行保持同步 —— 对不上就正好把远端最后一行裁掉。
    footer_rows: u16,
    /// 向上回看的行数（0=实时底部），滚轮驱动，历史来自 vt100 scrollback。
    pub scroll: usize,
    /// 鼠标拖选：锚点与当前点（终端绝对坐标 col,row）。
    selection: Option<((u16, u16), (u16, u16))>,
    /// 双击选词正在进行：按住期间鼠标抖动不该把选区缩回一个点，松手时按词复制。
    word_grab: bool,
    /// Protocol-level ZMODEM watcher over the remote output stream.
    zmodem: Watcher,
    /// When true we are swallowing remote bytes (a hijacked sz/rz handshake).
    intercepting: bool,
    /// Extra Ctrl-C re-sends already emitted during the current swallow window.
    abort_retries: u8,
    /// 脚本里的 rz（命令行没有回显）：先不中止远端 rz，等本地选完并上传落盘后
    /// 再放行。否则远端 rz 立刻退出，脚本紧接着的 `[ -f ]` 检查必然失败。
    hold_receive: bool,
    /// 远端输出里刚出现过 `docker ps` 的表头：底栏借这个提一句"F4 看容器"。
    docker_hint: crate::containers::DockerHintClock,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SessionAction {
    Keep,
    Detach,
    /// 整屏重绘：内嵌仿真器这边已经复位，界面那边还要作废 ratatui 的差分基线。
    Redraw,
}

impl SessionState {
    pub fn new(
        label: String,
        session: RemoteSession,
        rows: u16,
        cols: u16,
        footer_rows: u16,
    ) -> Self {
        // 与会话页顶部标题栏/底部指标栏的行数保持一致（见 ui::HEADER_ROWS）
        let header_rows = crate::ui::HEADER_ROWS;
        let cols = cols.max(20);
        let term_rows = rows.saturating_sub(header_rows + footer_rows).max(1);
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
            footer_rows,
            scroll: 0,
            selection: None,
            word_grab: false,
            zmodem: Watcher::new(),
            intercepting: false,
            abort_retries: 0,
            hold_receive: false,
            docker_hint: Default::default(),
        }
    }

    /// 底部指标行的行数变了（判定这台采不到、或设置里开关了）：内嵌视口要跟着缩，
    /// 且要发两次 window-change —— 全屏程序（htop/vim）只靠 SIGWINCH 自绘。
    /// 直通模式整块屏幕本来就是远端的，改这个数没有任何效果。
    pub fn set_footer_rows(&mut self, footer_rows: u16) {
        if self.footer_rows == footer_rows {
            return;
        }
        self.footer_rows = footer_rows;
        if self.mode != TermMode::Embedded {
            return;
        }
        let (cols, rows) = term_size();
        self.handle_resize(cols, rows);
    }

    pub fn handle_key(&mut self, key: &KeyEvent, binds: &KeyBinds) -> SessionAction {
        if key.kind == KeyEventKind::Release {
            return SessionAction::Keep;
        }
        // 任何按键都回到实时底部并取消选择高亮（与常见终端一致）
        self.scroll = 0;
        self.clear_selection();
        // 用户已经开始干别的了，底栏那句"F4 看容器"就不必再挂着
        self.docker_hint.clear();
        if binds.matches(Action::Passthrough, key) {
            self.toggle_mode();
            return SessionAction::Keep;
        }
        if binds.matches(Action::CloseTab, key) {
            return SessionAction::Detach;
        }
        if binds.matches(Action::Redraw, key) {
            self.request_full_redraw();
            return SessionAction::Redraw;
        }
        // 方向键/Home/End 的编码取决于远端当前的 DECCKM 状态：vim、htop
        // 一进来就发 smkx 打开它，此后只认 SS3。普通 shell 两种都认，
        // 所以这个分支错了只会表现为"vi 里方向键没反应"。
        let app_cursor = self.emu.application_cursor();
        if let Some(bytes) = key_to_bytes(key, app_cursor) {
            self.feed_input(bytes);
        }
        SessionAction::Keep
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.feed_input(text.as_bytes().to_vec());
    }

    /// 原样写一段字节给远端。vi 拦截时用它发 Ctrl-U，清掉远端 shell 里
    /// 还没提交的那一行（ells 已经吞掉了回车）。
    pub fn send_raw(&mut self, bytes: &[u8]) {
        self.feed_input(bytes.to_vec());
    }

    fn feed_input(&mut self, bytes: Vec<u8>) {
        self.session.write_input(bytes);
        if self.mode == TermMode::Embedded {
            while let Some(answer) = self.emu.take_answers() {
                self.session.write_input(answer);
            }
        }
    }

    /// 把一次鼠标动作编成远端要的报文。`col`/`row` 是从 1 开始的远端屏幕坐标，
    /// 由界面层按内嵌视口或整屏换算好。远端没开鼠标报告时返回 false。
    pub fn forward_mouse(
        &mut self,
        action: MouseAction,
        button: MouseButton,
        col: u16,
        row: u16,
        ctrl: bool,
        alt: bool,
    ) -> bool {
        let (mode, encoding) = self.emu.mouse_protocol();
        if mode == MouseProtocolMode::None {
            return false;
        }
        if action == MouseAction::Press {
            // 点击说的是"实时屏上的那个格子"：先把冻结的历史视图放回底部，
            // 否则远端收到的行列对不上它自己正在画的内容。
            self.scroll = 0;
            self.emu.set_scrollback(0);
            self.clear_selection();
        }
        if let Some(bytes) = mouse_to_bytes(action, button, col, row, ctrl, alt, mode, encoding) {
            self.feed_input(bytes);
        }
        true
    }

    /// Feed remote output. Returns Some(event) exactly once when a ZMODEM
    /// handshake is detected; afterwards all output is swallowed until
    /// `end_intercept`.
    pub fn handle_output(&mut self, bytes: &[u8]) -> Option<ZmodemEvent> {
        if self.intercepting {
            // lrzsz rz/sz re-sends its handshake every ~3s; make sure it dies
            // by re-aborting a few times while we keep swallowing output.
            // 脚本里的 rz 正被有意留着等上传，不能补刀。
            if !self.hold_receive && !bytes.is_empty() && self.abort_retries < 3 {
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
                // 用户手敲的 rz 立刻中止（提示符马上回来）；脚本里的 rz 不回显，
                // 留到上传完成再放行，让脚本后续动作看到已落盘的文件。
                self.hold_receive =
                    matches!(e, ZmodemEvent::Receive) && !self.zmodem.transfer_echoed();
                if !self.hold_receive {
                    self.send_abort();
                }
                return ev;
            }
        }
        match self.mode {
            TermMode::Embedded => {
                self.emu.process(bytes);
                // 新输出把历史往下推时，vt100 会自动调大回看偏移以"钉住"当前视图；
                // 缓存的 scroll 必须同步，否则下一帧按旧值重置会跳回底部。
                if self.scroll > 0 {
                    self.scroll = self.emu.scrollback_offset();
                }
            }
            TermMode::Passthrough => {
                // 直通不画仿真器，但按键编码仍要知道远端是否开着 DECCKM
                //（vim/htop 会开），所以这一路输出也喂一遍。仿真器在直通下
                // 尺寸对不上 PTY，它生成的应答不能当真，直接丢掉即可。
                self.emu.process(bytes);
                while self.emu.take_answers().is_some() {}
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(bytes);
                let _ = stdout.flush();
            }
        }
        self.docker_hint.feed(bytes);
        ev
    }

    /// 最近这一段输出像 `docker ps` 的表（12 秒内有效）。
    pub fn docker_hint(&self) -> bool {
        self.docker_hint.active()
    }

    pub fn is_intercepting(&self) -> bool {
        self.intercepting
    }

    /// 正在等本地把文件传完，之后再中止远端脚本里的 rz。
    pub fn is_holding_receive(&self) -> bool {
        self.hold_receive
    }

    /// 本地这一轮上传已结束（成功、失败或取消）：中止远端 rz，让脚本继续走。
    pub fn release_receive(&mut self) {
        if !self.hold_receive {
            return;
        }
        self.send_abort();
        self.end_intercept();
    }

    /// lrzsz aborts a handshake when it sees ≥5 consecutive CAN octets;
    /// send a full CAN×10 burst plus Ctrl-C as insurance.
    fn send_abort(&mut self) {
        self.session.write_input(vec![0x18; 10]);
        self.session.write_input(vec![0x03]);
    }

    pub fn end_intercept(&mut self) {
        let was_intercepting = self.intercepting;
        let held = self.hold_receive;
        self.intercepting = false;
        self.hold_receive = false;
        self.abort_retries = 0;
        self.zmodem.reset();
        // 脚本还要接着往下跑，它自己会打印后续输出并回到提示符；额外敲回车只会在
        // 脚本结束后的交互提示符上多留一个空行。
        if was_intercepting && !held {
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
        self.clear_selection();
        self.mode = match self.mode {
            TermMode::Embedded => {
                self.session.resize(cols, rows.max(1));
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(b"\x1b[2J\x1b[3J\x1b[H\x1b[?25h");
                let _ = stdout.flush();
                TermMode::Passthrough
            }
            TermMode::Passthrough => {
                let term_rows = rows.saturating_sub(self.header_rows + self.footer_rows).max(1);
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
            // 重建仿真器会连模式位一起忘掉，但远端仍在原来的模式里：
            // 全屏程序（vim/htop）发过 smkx 就不会再发，方向键得继续按 SS3 编码；
            // 鼠标的 1000/1002/1003 与编码同理，丢了就再也转不出去。
            let app_cursor = self.emu.application_cursor();
            let (mouse_mode, mouse_encoding) = self.emu.mouse_protocol();
            self.emu.reset_screen();
            if app_cursor {
                self.emu.process(b"\x1b[?1h");
            }
            match mouse_mode {
                MouseProtocolMode::None => {}
                MouseProtocolMode::Press => self.emu.process(b"\x1b[?9h"),
                MouseProtocolMode::PressRelease => self.emu.process(b"\x1b[?1000h"),
                MouseProtocolMode::ButtonMotion => self.emu.process(b"\x1b[?1002h"),
                MouseProtocolMode::AnyMotion => self.emu.process(b"\x1b[?1003h"),
            }
            match mouse_encoding {
                MouseProtocolEncoding::Default => {}
                MouseProtocolEncoding::Utf8 => self.emu.process(b"\x1b[?1005h"),
                MouseProtocolEncoding::Sgr => self.emu.process(b"\x1b[?1006h"),
            }
            self.session.resize(cols, rows.saturating_sub(1).max(1));
            self.session.resize(cols, rows);
        }
    }

    pub fn handle_resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(20);
        match self.mode {
            TermMode::Embedded => {
                let term_rows = rows.saturating_sub(self.header_rows + self.footer_rows).max(1);
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

    /// 开始拖选：端点记"内容行号"（历史 + 当前屏），滚动时锚点跟着文字走。
    pub fn begin_selection(&mut self, area: Rect, col: u16, row: u16) {
        let line = self.content_line(area, row);
        self.selection = Some(((col, line), (col, line)));
        self.word_grab = false;
    }

    pub fn update_selection(&mut self, area: Rect, col: u16, row: u16) {
        if let Some((anchor, _)) = self.selection {
            let line = self.content_line(area, row);
            self.selection = Some((anchor, (col, line)));
        }
    }

    /// 视口坐标 → 内容行号。拖出可视区时钳到最近的边（真正的滚动由上层定时器做）。
    fn content_line(&mut self, area: Rect, row: u16) -> u16 {
        let r = row
            .saturating_sub(area.y)
            .min(area.height.saturating_sub(1));
        let sb_len = self.emu.scrollback_len();
        view_line(sb_len, self.emu.scrollback_offset(), r) as u16
    }

    /// 双击选词：把选区换成光标下的整个词。点在分隔符或可视区外时返回 false，
    /// 不改动已有选区。
    pub fn select_word(&mut self, area: Rect, col: u16, row: u16) -> bool {
        if row < area.y
            || row >= area.y.saturating_add(area.height)
            || col < area.x
            || col >= area.x.saturating_add(area.width)
        {
            return false;
        }
        let line = self.view_row_text(row - area.y);
        let Some(span) = word_selection(&line, area, col, row) else {
            return false;
        };
        let content = self.content_line(area, row);
        self.selection = Some(((span.0.0, content), (span.1.0, content)));
        self.word_grab = true;
        true
    }

    /// 双击选词是否还按着：按住期间拖拽事件要忽略。
    pub fn word_grab(&self) -> bool {
        self.word_grab
    }

    pub fn end_word_grab(&mut self) {
        self.word_grab = false;
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.word_grab = false;
    }

    /// 松手时这次选择值不值得复制：拖动过、或双击选过词。原地单击为 false ——
    /// 单击只清空选区，不该把光标下的一个字符悄悄写进剪贴板。
    pub fn selection_is_copyable(&self) -> bool {
        self.word_grab || matches!(self.selection, Some((a, b)) if a != b)
    }

    /// 选区按**阅读顺序**归一化的起止点：起点在前、终点在后。
    ///
    /// 不是矩形：跨行拖选时首行从起点列到行尾、中间行整行、末行从行首到终点列，
    /// 和普通终端的选择一致。从下往上拖也按同一顺序归一化。
    pub fn selection_span(&self) -> Option<((u16, u16), (u16, u16))> {
        let (a, b) = self.selection?;
        Some(reading_order(a, b))
    }

    /// 某个终端格子是否落在选区内（绘制高亮用）。`sb_len` 是本帧的历史行数，
    /// 由绘制方一次算好传进来（`selection_contains` 自己是只读的）。
    pub fn selection_contains(&self, col: u16, row: u16, area: Rect, sb_len: usize) -> bool {
        if row < area.y || row >= area.y.saturating_add(area.height) {
            return false;
        }
        let line = view_line(sb_len, self.emu.screen().scrollback(), row - area.y) as u16;
        self.selection_span()
            .is_some_and(|span| selection_covers(span, col, line))
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
    ///
    /// 端点存的是内容行号，所以跨屏选中时历史里的行也能取到：首行从起点列到行尾、
    /// 中间整行、末行从行首到终点列。
    pub fn selected_text(&mut self, area: Rect) -> String {
        let Some(((x0, l0), (x1, l1))) = self.selection_span() else {
            return String::new();
        };
        if area.width == 0 {
            return String::new();
        }
        let start = x0.saturating_sub(area.x) as usize;
        let end = if x1 < area.x {
            0
        } else {
            (x1.saturating_sub(area.x) as usize + 1).min(area.width as usize)
        };
        let (_rows, cols) = self.emu.size();
        let lines = self.emu.content_lines(l0 as usize, l1 as usize, cols);
        let mut out: Vec<String> = Vec::new();
        let last = lines.len().saturating_sub(1);
        for (i, text) in lines.iter().enumerate() {
            let from = if i == 0 { start } else { 0 };
            let to = if i == last { end } else { usize::MAX };
            out.push(slice_columns(text, from, to).trim_end().to_string());
        }
        while out.last().is_some_and(|s| s.is_empty()) {
            out.pop();
        }
        out.join("\n")
    }

    /// 把视图向上（负数）或向下（正数）滚若干行，供拖动越界时按距离滚动用。
    /// 返回视图是否真的动了：到头/到底时不动，上层据此停掉自滚定时器。
    pub fn scroll_by(&mut self, lines: i32) -> bool {
        let before = self.emu.scrollback_offset();
        let target = if lines < 0 {
            self.scroll.saturating_add((-lines) as usize)
        } else {
            self.scroll.saturating_sub(lines as usize)
        };
        self.emu.set_scrollback(target);
        self.scroll = self.emu.scrollback_offset();
        self.scroll != before
    }

    /// 这个方向还滚得动吗（负数向上、正数向下）。拖选定时器据此停表：
    /// 到顶/到底就不要每 50ms 空转，也不能因此丢掉拖选状态。
    pub fn can_scroll(&mut self, lines: i32) -> bool {
        self.emu.can_scroll(lines)
    }
}

/// 双击选词时算作"词内"的 ASCII 标点。路径、URL、`user@host:path`、IP:端口
/// 是一整串；空格、引号、括号、逗号分号、`! | ^ $ *`、制表线条都断开。
const WORD_PUNCT: &str = "_-./~@:%+=&?#";

/// 视口第 `r` 行对应的内容行号（0 = 最早的历史行）。
/// `scroll` 是当前回看偏移，越大越靠上；偏移被钳制后传入旧值时按 0 兜底。
fn view_line(sb_len: usize, scroll: usize, r: u16) -> usize {
    sb_len.saturating_sub(scroll) + r as usize
}

/// 按显示列切一行文本（`end` 为开区间）。宽字符占两列，起点落在宽字中间时
/// 整字算进来 —— 终端选区本来就走整格。
fn slice_columns(text: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut x = 0usize;
    for c in text.chars() {
        if x >= end {
            break;
        }
        let w = if crate::ui::is_wide(c) { 2 } else { 1 };
        if x + w > start {
            out.push(c);
        }
        x += w;
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || WORD_PUNCT.contains(c)
}

/// 一行文本里，显示列 `col` 下的词所占的显示列闭区间 `[起, 止]`。
///
/// `text` 是 `view_row_text` 的形状：一个宽字符只出现一次，但它占两列。
/// 点在空白/分隔符上，或列超出行宽时返回 `None`（双击不该凭空选出一段）。
fn word_columns(text: &str, col: usize) -> Option<(usize, usize)> {
    let mut cells: Vec<(char, usize, usize)> = Vec::new();
    let mut x = 0usize;
    for c in text.chars() {
        let w = if crate::ui::is_wide(c) { 2 } else { 1 };
        cells.push((c, x, w));
        x += w;
    }
    let idx = cells
        .iter()
        .position(|&(_, start, w)| col >= start && col < start + w)?;
    if !is_word_char(cells[idx].0) {
        return None;
    }
    let mut lo = idx;
    while lo > 0 && is_word_char(cells[lo - 1].0) {
        lo -= 1;
    }
    let mut hi = idx;
    while hi + 1 < cells.len() && is_word_char(cells[hi + 1].0) {
        hi += 1;
    }
    Some((cells[lo].1, cells[hi].1 + cells[hi].2 - 1))
}

/// 把 `word_columns` 的显示列换到内嵌视口坐标里，并检查点确实落在 area 内。
fn word_selection(
    text: &str,
    area: Rect,
    col: u16,
    row: u16,
) -> Option<((u16, u16), (u16, u16))> {
    if row < area.y
        || row >= area.y.saturating_add(area.height)
        || col < area.x
        || col >= area.x.saturating_add(area.width)
    {
        return None;
    }
    let (start, end) = word_columns(text, (col - area.x) as usize)?;
    Some(((area.x + start as u16, row), (area.x + end as u16, row)))
}

/// 按阅读顺序排两个端点：先比行、同一行再比列。反向拖拽因此也得到同样的选区。
fn reading_order(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if (a.1, a.0) <= (b.1, b.0) {
        (a, b)
    } else {
        (b, a)
    }
}

/// 阅读顺序的选区覆盖判断：首行从起点列到行尾、中间整行、末行到终点列。
fn selection_covers(((c0, r0), (c1, r1)): ((u16, u16), (u16, u16)), col: u16, row: u16) -> bool {
    if row < r0 || row > r1 {
        return false;
    }
    if r0 == r1 {
        return col >= c0 && col <= c1;
    }
    if row == r0 {
        col >= c0
    } else if row == r1 {
        col <= c1
    } else {
        true
    }
}

fn term_size() -> (u16, u16) {
    crossterm::terminal::size().map(|(c, r)| (c as u16, r as u16)).unwrap_or((80, 24))
}

#[cfg(test)]
mod tests {
    use super::{reading_order, selection_covers, slice_columns, view_line, word_columns, word_selection};
    use ratatui::layout::Rect;

    /// 视口行 → 内容行：回看时顶行会往历史里退，实时底部时顶行就是历史长度的位置。
    #[test]
    fn a_view_row_maps_to_a_content_line_at_any_scroll_offset() {
        assert_eq!(view_line(10, 0, 0), 10);
        assert_eq!(view_line(10, 0, 2), 12);
        assert_eq!(view_line(10, 4, 0), 6);
        assert_eq!(view_line(10, 4, 2), 8);
        // 回看偏移被钳制后调用方可能早一步传入旧值：不能下溢。
        assert_eq!(view_line(3, 9, 0), 0);
        assert_eq!(view_line(3, 9, 1), 1);
    }

    /// 提取文本必须按显示列切：行文本里宽字符只留一个 char，按 char 下标切
    /// 会把后面的字一起带出来（双击中文路径/表头时最容易撞上）。
    #[test]
    fn selection_text_is_sliced_by_display_columns() {
        assert_eq!(slice_columns("磁盘 使用", 0, 4), "磁盘");
        assert_eq!(slice_columns("磁盘 使用", 5, 9), "使用");
        assert_eq!(slice_columns("内存 42%", 5, 8), "42%");
        // 起点落在宽字中间：整字算进来，和终端按整格选择一致。
        assert_eq!(slice_columns("磁盘", 1, 4), "磁盘");
    }

    /// 跨行拖选是"从起点到终点的一段"，不是矩形：首行到行尾、中间整行、末行到终点。
    #[test]
    fn selection_is_reading_order_not_a_rectangle() {
        let span = ((2, 0), (3, 2));
        assert!(!selection_covers(span, 1, 0), "首行起点左边不该选中");
        assert!(selection_covers(span, 2, 0));
        assert!(selection_covers(span, 79, 0), "首行一直到行尾");
        assert!(selection_covers(span, 0, 1), "中间行整行");
        assert!(selection_covers(span, 79, 1), "中间行整行");
        assert!(selection_covers(span, 0, 2), "末行从行首开始");
        assert!(selection_covers(span, 3, 2));
        assert!(!selection_covers(span, 4, 2), "末行终点右边不该选中");
        assert!(!selection_covers(span, 2, 3), "选区之外的行");
    }

    /// 同一行内就是一段连续列。
    #[test]
    fn a_single_row_selection_is_a_contiguous_run() {
        let span = ((3, 4), (7, 4));
        assert!(!selection_covers(span, 2, 4));
        assert!(selection_covers(span, 3, 4));
        assert!(selection_covers(span, 7, 4));
        assert!(!selection_covers(span, 8, 4));
        assert!(!selection_covers(span, 5, 3));
        assert!(!selection_covers(span, 5, 5));
    }

    /// 从下往上拖：端点换成阅读顺序，高亮和复制仍是同一段。
    #[test]
    fn dragging_up_normalizes_to_the_same_span() {
        let down = reading_order((2, 0), (3, 2));
        let up = reading_order((3, 2), (2, 0));
        assert_eq!(down, up);
        assert_eq!(down, ((2, 0), (3, 2)));
        // 同一行内反向拖也一样
        assert_eq!(reading_order((9, 4), (3, 4)), ((3, 4), (9, 4)));
    }

    /// SSH 场景里双击最常复制的东西：路径、URL、`user@host:path`、IP:端口。
    /// 这些都是"一个词"，不能按 `/`、`.`、`:` 拆开。
    #[test]
    fn double_click_takes_a_whole_path_or_url() {
        let line = "open /var/log/nginx/error.log:42 now";
        let start = line.find("/var").expect("路径在行内");
        assert_eq!(
            word_columns(line, start + 6),
            Some((start, start + "/var/log/nginx/error.log:42".len() - 1))
        );
        let url = "see https://example.com/a?b=1&c=2#frag";
        let start = url.find("https").expect("URL 在行内");
        assert_eq!(
            word_columns(url, start + 8),
            Some((start, start + "https://example.com/a?b=1&c=2#frag".len() - 1))
        );
        let target = "scp root@10.0.0.2:/etc/hosts .";
        let start = target.find("root").expect("目标在行内");
        assert_eq!(
            word_columns(target, start + 2),
            Some((start, start + "root@10.0.0.2:/etc/hosts".len() - 1))
        );
    }

    /// 引号、括号、逗号、分号、竖线、感叹号和制表线条都是分隔符：
    /// 双击 `("/etc/passwd")` 只选路径，双击 TUI 表格的某一格不跨到隔壁。
    #[test]
    fn double_click_stops_at_quotes_brackets_and_separators() {
        let line = "(\"/etc/passwd\"), next";
        let start = line.find("/etc").expect("路径在行内");
        assert_eq!(
            word_columns(line, start + 3),
            Some((start, start + "/etc/passwd".len() - 1))
        );
        assert_eq!(word_columns("a,b", 0), Some((0, 0)));
        assert_eq!(word_columns("a;b", 0), Some((0, 0)));
        assert_eq!(word_columns("a|b", 0), Some((0, 0)));
        assert_eq!(word_columns("a!b", 0), Some((0, 0)));
        assert_eq!(word_columns("left│right", 0), Some((0, 3)));
    }

    /// 点在空白、分隔符或行外时没有词可选：双击不该凭空选出一段。
    #[test]
    fn double_click_on_whitespace_or_outside_selects_nothing() {
        assert_eq!(word_columns(" /etc/passwd", 0), None);
        assert_eq!(word_columns("/etc/passwd,", 11), None);
        assert_eq!(word_columns("", 0), None);
        assert_eq!(word_columns("abc", 3), None);
        assert_eq!(word_columns("abc", 99), None);
    }

    /// 汉字占两列：点在一个字的左半或右半都要选中同一个词，返回的也是显示列。
    #[test]
    fn double_click_keeps_display_columns_for_wide_text() {
        let line = "磁盘 使用";
        assert_eq!(word_columns(line, 0), Some((0, 3)));
        assert_eq!(word_columns(line, 1), Some((0, 3)));
        assert_eq!(word_columns(line, 5), Some((5, 8)));
        assert_eq!(word_columns(line, 8), Some((5, 8)));
        assert_eq!(word_columns(line, 4), None);
        // 汉字后面的 ASCII 词也按显示列算
        let line = "内存 42%";
        assert_eq!(word_columns(line, 6), Some((5, 7)));
    }

    /// 选区换到内嵌视口里要加上 area 的偏移；可视区外的点不选。
    #[test]
    fn word_selection_maps_the_view_area_offset() {
        let area = Rect::new(3, 7, 40, 10);
        let line = "sudo reboot";
        assert_eq!(
            word_selection(line, area, area.x + 6, 9),
            Some(((area.x + 5, 9), (area.x + 10, 9)))
        );
        assert_eq!(word_selection(line, area, area.x + 4, 9), None);
        assert_eq!(word_selection(line, area, area.x + 40, 9), None);
        assert_eq!(word_selection(line, area, area.x, 6), None);
    }
}

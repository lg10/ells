//! 内置编辑器面板：远端文件取回到临时文件后，直接在 ells 的终端里编辑。
//!
//! 这里只管内容与绘制；保存后的回传仍走既有的 `EditSaved` / `EditUploaded` 链路，
//! 远端冲突判断也复用那一套。

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent as CtMouseEvent, MouseEventKind};
use edtui::actions::motion::{MoveToFirstRow, MoveToLastRow};
use edtui::actions::{
    CopySelection, MoveToEndOfLine, MoveToStartOfLine, Redo, SwitchMode, Undo,
};
use edtui::clipboard::ClipboardTrait;
use edtui::{
    EditorEventHandler, EditorMode, EditorState, EditorTheme, EditorView, LineNumbers, Lines,
    SyntaxHighlighter,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};
use ratatui::Frame;

use crate::theme;

/// 超过这个体积就关掉语法高亮：syntect 每次重绘都要重新切词，
/// 大文件在按键时会明显卡顿，纯文本编辑反而是瞬时的。
pub const HIGHLIGHT_MAX_BYTES: usize = 512 * 1024;
pub const HIGHLIGHT_MAX_LINES: usize = 10_000;

/// 一格滚轮走几行：edtui 自己一格只走 1 行，桌面编辑器的手感通常是 3 行。
pub const WHEEL_LINES: usize = 3;

/// 滚轮方向：delta > 0 是向下滚（events.rs 里 ScrollDown），正文要往下走。
pub fn wheel_kind(delta: i8) -> MouseEventKind {
    if delta > 0 {
        MouseEventKind::ScrollDown
    } else {
        MouseEventKind::ScrollUp
    }
}

/// 系统剪贴板：写出走 OSC 52，与会话里拖选复制同一条路（SSH 里也能落到本机）；
/// 读回由终端粘贴事件负责，这里保留内部副本供 vim 的 `p` 使用。
struct TerminalClipboard {
    text: String,
    emit: bool,
}

impl Default for TerminalClipboard {
    fn default() -> Self {
        Self { text: String::new(), emit: true }
    }
}

impl ClipboardTrait for TerminalClipboard {
    fn set_text(&mut self, text: String) {
        if self.emit && !text.is_empty() {
            crate::term::copy_osc52(&text);
        }
        self.text = text;
    }

    fn get_text(&mut self) -> String {
        self.text.clone()
    }
}

/// `:` 命令行执行结果：由 app 负责保存 / 关闭，编辑器只管解析。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExAction {
    None,
    Save { close: bool },
    Quit { force: bool },
    Unknown,
}

pub struct EmbeddedEdit {
    pub remote: String,
    pub local: std::path::PathBuf,
    pub name: String,
    pub state: EditorState,
    handler: EditorEventHandler,
    /// 与磁盘对齐时的正文（LF 形式，不含 CR）：判断"有没有未保存改动"只在这一刻比一次。
    saved_text: String,
    extension: Option<String>,
    /// 原文件全是 CRLF：缓冲区按 LF 编辑，写回时再还原，
    /// 否则 `str::lines()` 会把 `\r` 吃掉，保存一次就悄悄把整份文件的换行改了。
    crlf: bool,
    pub highlight: bool,
    /// 有未保存改动时第一次 Ctrl-Q 只提示，再按一次才丢弃。
    pub confirm_close: bool,
    /// `:` 命令行缓冲：Some("") 表示冒号已按下、正在输入命令。
    pub ex: Option<String>,
}

impl EmbeddedEdit {
    pub fn open(remote: &str, local: &Path, name: &str) -> Result<Self, String> {
        let bytes = std::fs::read(local).map_err(|e| format!("读取临时文件失败：{e}"))?;
        // 非 UTF-8 一旦用 lossy 转换，保存时就会把原始字节改坏；宁可拒绝。
        let text = String::from_utf8(bytes)
            .map_err(|_| "这个文件不是 UTF-8 文本，内置编辑器不能安全保存".to_string())?;
        // 只有各行都是 CRLF 才走还原；混合行尾按 LF 处理（无法同时表达两种风格）。
        let crlf = !text.is_empty()
            && text.contains("\r\n")
            && !text.replace("\r\n", "").contains('\n');
        let text = if crlf { text.replace("\r\n", "\n") } else { text };
        let extension = language_extension(name);
        let highlight = extension.is_some()
            && text.len() <= HIGHLIGHT_MAX_BYTES
            && text.lines().count() <= HIGHLIGHT_MAX_LINES;
        // 打开就能打字（INSERT），不是先按 i；复制 / 粘贴接到系统剪贴板。
        let mut state = EditorState::new(Lines::from(text.as_str()));
        state.mode = EditorMode::Insert;
        state.set_clipboard(TerminalClipboard::default());
        Ok(Self {
            remote: remote.to_string(),
            local: local.to_path_buf(),
            name: name.to_string(),
            state,
            handler: EditorEventHandler::default(),
            saved_text: text,
            extension,
            crlf,
            highlight,
            confirm_close: false,
            ex: None,
        })
    }

    /// 缓冲区原文（LF）：落盘前还要按原文件的换行风格还原。
    fn buffer_text(&self) -> String {
        self.state.lines.to_string()
    }

    /// 写回磁盘的内容：CRLF 文件还原成 CRLF。
    pub fn text(&self) -> String {
        let lf = self.buffer_text();
        if self.crlf {
            lf.replace('\n', "\r\n")
        } else {
            lf
        }
    }

    /// 只在要决定"关掉时要不要确认"时调用，不在每帧比较正文。
    pub fn dirty(&self) -> bool {
        self.buffer_text() != self.saved_text
    }

    pub fn mark_saved(&mut self) {
        self.saved_text = self.buffer_text();
        self.confirm_close = false;
    }

    pub fn line_count(&self) -> usize {
        self.state.lines.len()
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        self.handler.on_key_event(key, &mut self.state);
    }

    pub fn on_paste(&mut self, text: &str) {
        self.handler.on_paste_event(text.to_string(), &mut self.state);
    }

    pub fn on_mouse(
        &mut self,
        kind: MouseEventKind,
        column: u16,
        row: u16,
        modifiers: KeyModifiers,
    ) {
        let ev = CtMouseEvent {
            kind,
            column,
            row,
            modifiers,
        };
        self.handler.on_mouse_event(ev, &mut self.state);
    }

    pub fn normal_mode(&self) -> bool {
        self.state.mode == EditorMode::Normal
    }

    /// Ctrl-A：整篇选上（进入 VISUAL，随后 Ctrl-C / y 即可复制）。
    pub fn select_all(&mut self) {
        self.state.execute(MoveToFirstRow());
        self.state.execute(MoveToStartOfLine());
        self.state.execute(SwitchMode(EditorMode::Visual));
        self.state.execute(MoveToLastRow());
        self.state.execute(MoveToEndOfLine());
    }

    /// Ctrl-C：把当前选区写进系统剪贴板；没有选区时返回 false。
    pub fn copy_selection(&mut self) -> bool {
        if self.state.selection.is_none() {
            return false;
        }
        self.state.execute(CopySelection);
        if self.state.mode == EditorMode::Visual {
            self.state.execute(SwitchMode(EditorMode::Normal));
        }
        true
    }

    pub fn undo(&mut self) {
        self.state.execute(Undo);
    }

    pub fn redo(&mut self) {
        self.state.execute(Redo);
    }

    pub fn ex_active(&self) -> bool {
        self.ex.is_some()
    }

    pub fn ex_start(&mut self) {
        self.ex = Some(String::new());
    }

    /// `:` 命令行的按键。Enter 时返回要执行的动作，缓冲区随即关闭。
    pub fn ex_input(&mut self, key: KeyEvent) -> ExAction {
        match key.code {
            KeyCode::Esc => {
                self.ex = None;
            }
            KeyCode::Enter => {
                let cmd = self.ex.take().unwrap_or_default();
                return parse_ex(cmd.trim());
            }
            KeyCode::Backspace => {
                if let Some(buf) = self.ex.as_mut() {
                    buf.pop();
                }
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && !c.is_control() =>
            {
                if let Some(buf) = self.ex.as_mut() {
                    buf.push(c);
                }
            }
            _ => {}
        }
        ExAction::None
    }

    fn mode_label(&self) -> &'static str {
        match self.state.mode {
            EditorMode::Normal => "NORMAL",
            EditorMode::Insert => "INSERT",
            EditorMode::Visual => "VISUAL",
            EditorMode::Search => "SEARCH",
        }
    }
}

fn parse_ex(cmd: &str) -> ExAction {
    match cmd {
        "w" | "write" => ExAction::Save { close: false },
        "wq" | "x" | "exit" => ExAction::Save { close: true },
        "q" | "quit" => ExAction::Quit { force: false },
        "q!" | "quit!" => ExAction::Quit { force: true },
        _ => ExAction::Unknown,
    }
}

fn language_extension(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let ext = base.rsplit_once('.')?.1;
    let ok = !ext.is_empty()
        && ext.len() <= 10
        && ext
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '_'));
    ok.then(|| ext.to_ascii_lowercase())
}

fn syntect_theme() -> &'static str {
    if theme::current() == theme::Theme::Light {
        "base16-ocean-light"
    } else {
        "base16-ocean-dark"
    }
}

fn editor_theme() -> EditorTheme<'static> {
    let base = Style::default().fg(theme::text()).bg(theme::bg());
    EditorTheme::default()
        .base(base)
        // 光标交给终端硬件光标（闪烁细竖线）：光标格必须和正文同色，
        // 否则 edtui 会把整格刷成方块，反而把字盖住。
        .cursor_style(base)
        .selection_style(Style::default().fg(theme::on_accent()).bg(theme::select_bg()))
        .line_numbers_style(Style::default().fg(theme::muted()).bg(theme::bg()))
        .hide_status_line()
}

/// `:` 命令行的硬件光标位置：紧跟在 `:{buf}` 之后，宽字符按显示列算。
fn ex_cursor(area: Rect, buf: &str) -> ratatui::layout::Position {
    use unicode_width::UnicodeWidthStr;
    let x = area.x.saturating_add(1 + UnicodeWidthStr::width(buf) as u16);
    ratatui::layout::Position::new(x.min(area.right().saturating_sub(1)), area.y)
}

/// 渲染正文，并把 edtui 算出的光标位置（绝对屏幕坐标）交回去。
fn render_body(
    f: &mut Frame,
    area: Rect,
    ed: &mut EmbeddedEdit,
    theme: EditorTheme<'static>,
    highlighter: Option<SyntaxHighlighter>,
) -> Option<ratatui::layout::Position> {
    let view = EditorView::new(&mut ed.state)
        .theme(theme)
        .line_numbers(LineNumbers::Absolute)
        .wrap(false)
        .tab_width(4)
        .syntax_highlighter(highlighter);
    view.render(area, f.buffer_mut());
    let pos = ed.state.cursor_screen_position()?;
    area.contains(pos).then_some(pos)
}

pub fn draw_panel(f: &mut Frame, area: Rect, app: &mut crate::app::App) {
    let idx = app.active;
    let status = app.slots[idx].status.clone();
    let Some(ed) = app.slots[idx].editor.as_mut() else {
        return;
    };

    let title = format!(
        " 编辑 {} · {} · Ctrl-S 保存 · Ctrl-Q 退出 ",
        ed.name,
        ed.mode_label()
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(theme::accent()))
        .style(Style::default().bg(theme::bg()));
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

    let highlighter = if ed.highlight {
        ed.extension
            .as_deref()
            .and_then(|ext| SyntaxHighlighter::new(syntect_theme(), ext).ok())
    } else {
        None
    };
    let cursor = render_body(f, chunks[0], ed, editor_theme(), highlighter);
    // `:` 命令行打开时，硬件光标跟着它走；否则留在正文里。
    let cursor = match &ed.ex {
        Some(buf) => Some(ex_cursor(chunks[1], buf)),
        None => cursor,
    };
    if let Some(pos) = cursor {
        f.set_cursor_position(pos);
    }

    // 命令行打开时整条底栏让给它，和 vim 的 `:` 行一致。
    let line = if let Some(buf) = &ed.ex {
        Line::from(vec![
            Span::styled(
                format!(":{buf}"),
                Style::default()
                    .fg(theme::on_accent())
                    .bg(theme::accent_bg())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "Enter 执行 · Esc 取消（:w · :wq · :q · :q!）",
                Style::default().fg(theme::dim()).bg(theme::bg()),
            ),
        ])
    } else {
        let hint = status.unwrap_or_else(|| {
            let tail = "Ctrl-S 保存 · Ctrl-Q 退出 · 拖选后 Ctrl-C 复制 · Ctrl-A 全选";
            if ed.highlight {
                tail.to_string()
            } else {
                format!("大文件已关高亮 · {tail}")
            }
        });
        let info = format!(" {} · {} 行", ed.remote, ed.line_count());
        Line::from(vec![
            Span::styled(info, Style::default().fg(theme::dim()).bg(theme::bg())),
            Span::raw("  "),
            Span::styled(hint, Style::default().fg(theme::warn()).bg(theme::bg())),
        ])
    };
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme::bg())),
        chunks[1],
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
    use edtui::EditorMode;

    use super::{
        language_extension, wheel_kind, ExAction, TerminalClipboard, EmbeddedEdit,
        HIGHLIGHT_MAX_LINES, WHEEL_LINES,
    };
    use edtui::clipboard::ClipboardTrait;

    use crate::theme;

    fn temp_file(contents: &[u8], suffix: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ells-editor-test-{}-{n}{suffix}",
            std::process::id()
        ));
        std::fs::write(&path, contents).expect("write temp file");
        path
    }

    #[test]
    fn fresh_buffer_is_clean_until_edited_then_marked_saved() {
        let path = temp_file(b"fn main() {}\n", ".rs");
        let mut ed = EmbeddedEdit::open("/tmp/main.rs", &path, "main.rs").expect("open");
        assert!(!ed.dirty());
        assert!(ed.highlight);
        assert_eq!(ed.line_count(), 2);

        ed.on_paste("x");
        assert!(ed.dirty());
        ed.mark_saved();
        assert!(!ed.dirty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn crlf_files_round_trip_unchanged() {
        let path = temp_file(b"one\r\ntwo\r\n", ".txt");
        let mut ed = EmbeddedEdit::open("/tmp/a.txt", &path, "a.txt").expect("open");
        assert!(!ed.dirty(), "opening a CRLF file is not an edit");
        assert_eq!(ed.text(), "one\r\ntwo\r\n");
        ed.on_paste("three");
        ed.mark_saved();
        assert!(ed.text().contains("\r\n"), "line endings stay CRLF after save");
        assert!(
            !ed.text().replace("\r\n", "").contains('\n'),
            "no bare LF may leak into a CRLF file"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn non_utf8_files_are_rejected_instead_of_lossy_decoded() {
        let path = temp_file(b"abc\xff\xfe", ".txt");
        let err = EmbeddedEdit::open("/tmp/x", &path, "x.txt").err();
        assert!(err.is_some(), "invalid UTF-8 must not open");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn huge_files_open_without_highlighting() {
        let long = "a\n".repeat(HIGHLIGHT_MAX_LINES + 1);
        let path = temp_file(long.as_bytes(), ".rs");
        let ed = EmbeddedEdit::open("/tmp/big.rs", &path, "big.rs").expect("open");
        assert!(!ed.highlight, "over the line cap the panel drops highlighting");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn files_without_a_known_extension_open_plain() {
        let path = temp_file(b"hello\n", "");
        let ed = EmbeddedEdit::open("/tmp/Makefile", &path, "Makefile").expect("open");
        assert!(!ed.highlight);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn common_extensions_are_lowercased() {
        assert_eq!(language_extension("main.RS").as_deref(), Some("rs"));
        assert_eq!(language_extension("/etc/nginx.conf").as_deref(), Some("conf"));
        assert_eq!(language_extension("Cargo.toml").as_deref(), Some("toml"));
    }

    #[test]
    fn names_without_a_usable_extension_get_none() {
        assert_eq!(language_extension("Makefile"), None);
        assert_eq!(language_extension("trailing."), None);
        assert_eq!(language_extension("a.reallylongextension"), None);
    }

    fn press(ed: &mut EmbeddedEdit, code: KeyCode) {
        ed.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn run_ex(ed: &mut EmbeddedEdit, text: &str) -> ExAction {
        ed.ex_start();
        for c in text.chars() {
            ed.ex_input(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        ed.ex_input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    }

    #[test]
    fn opens_in_insert_mode_so_typing_works_immediately() {
        let path = temp_file(b"hello\n", ".txt");
        let mut ed = EmbeddedEdit::open("/tmp/a.txt", &path, "a.txt").expect("open");
        assert_eq!(ed.state.mode, EditorMode::Insert);
        press(&mut ed, KeyCode::Char('X'));
        assert!(ed.text().starts_with('X'), "打开即可打字，不用先按 i");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn wheel_down_scrolls_the_view_down_three_lines() {
        assert_eq!(wheel_kind(1), MouseEventKind::ScrollDown);
        assert_eq!(wheel_kind(-1), MouseEventKind::ScrollUp);
        assert_eq!(WHEEL_LINES, 3);
    }

    #[test]
    fn select_all_then_copy_covers_the_buffer_and_returns_to_normal() {
        let path = temp_file(b"one\ntwo\nthree", ".txt");
        let mut ed = EmbeddedEdit::open("/tmp/a.txt", &path, "a.txt").expect("open");
        ed.select_all();
        assert!(ed.state.selection.is_some());
        assert_eq!(ed.state.cursor.row, 2, "选区一直拉到末行");
        assert!(ed.copy_selection());
        assert!(ed.state.selection.is_none(), "复制后选区清空");
        assert_eq!(ed.state.mode, EditorMode::Normal);
        assert!(!ed.copy_selection(), "没有选区时复制不做事");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ex_commands_map_to_save_and_quit() {
        let path = temp_file(b"hello\n", ".txt");
        let mut ed = EmbeddedEdit::open("/tmp/a.txt", &path, "a.txt").expect("open");
        assert_eq!(run_ex(&mut ed, "w"), ExAction::Save { close: false });
        assert_eq!(run_ex(&mut ed, "wq"), ExAction::Save { close: true });
        assert_eq!(run_ex(&mut ed, "x"), ExAction::Save { close: true });
        assert_eq!(run_ex(&mut ed, "q"), ExAction::Quit { force: false });
        assert_eq!(run_ex(&mut ed, "quit!"), ExAction::Quit { force: true });
        assert_eq!(run_ex(&mut ed, "zz"), ExAction::Unknown);
        assert!(!ed.ex_active(), "执行后命令行关闭");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn esc_cancels_the_ex_command_line() {
        let path = temp_file(b"hello\n", ".txt");
        let mut ed = EmbeddedEdit::open("/tmp/a.txt", &path, "a.txt").expect("open");
        ed.ex_start();
        press(&mut ed, KeyCode::Char('w'));
        assert_eq!(
            ed.ex_input(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ExAction::None
        );
        assert!(!ed.ex_active());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn terminal_clipboard_keeps_the_last_text() {
        let mut cb = TerminalClipboard { text: String::new(), emit: false };
        cb.set_text("hi".to_string());
        assert_eq!(cb.get_text(), "hi");
    }

    /// 光标必须交给终端硬件光标画：edtui 只报位置，光标格保持正文样式。
    /// 否则会退回到"整格反显方块"，把光标下的字符盖掉。
    #[test]
    fn the_cursor_is_handed_to_the_terminal_instead_of_painted_as_a_block() {
        use ratatui::backend::TestBackend;
        use ratatui::style::Modifier;
        use ratatui::Terminal;

        theme::apply(theme::Theme::Dark);
        let path = temp_file(b"hello\nworld\n", ".txt");
        let mut ed = EmbeddedEdit::open("/tmp/a.txt", &path, "a.txt").expect("open");
        ed.state.cursor.col = 2;

        let mut terminal = Terminal::new(TestBackend::new(30, 6)).unwrap();
        let mut pos = None;
        let mut base_bg = None;
        terminal
            .draw(|f| {
                let theme = super::editor_theme();
                base_bg = theme.base.bg;
                pos = super::render_body(f, f.area(), &mut ed, theme, None);
                if let Some(p) = pos {
                    f.set_cursor_position(p);
                }
            })
            .unwrap();

        let pos = pos.expect("光标位置要交回框架");
        assert_eq!(pos.y, 0);
        assert!(terminal.backend().cursor_visible(), "硬件光标要显示出来");
        assert_eq!(terminal.get_cursor_position().unwrap(), pos);

        let cell = &terminal.backend().buffer()[(pos.x, pos.y)];
        assert_eq!(cell.symbol(), "l", "光标下的字符不能被盖掉");
        assert_eq!(cell.style().bg, base_bg, "光标格不再是方块底色");
        assert!(!cell.style().add_modifier.contains(Modifier::REVERSED));
        let _ = std::fs::remove_file(path);
    }

    /// `:` 行的光标跟在输入后面；中文按显示列算，超宽时钳在行尾。
    #[test]
    fn the_ex_prompt_cursor_sits_right_after_the_typed_text() {
        use ratatui::layout::{Position, Rect};

        let area = Rect::new(2, 20, 40, 1);
        assert_eq!(super::ex_cursor(area, "w"), Position::new(4, 20));
        assert_eq!(super::ex_cursor(area, "w 文件"), Position::new(9, 20));
        assert_eq!(
            super::ex_cursor(Rect::new(0, 0, 5, 1), "wwwwww"),
            Position::new(4, 0)
        );
    }
}

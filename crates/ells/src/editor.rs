//! 内置编辑器面板：远端文件取回到临时文件后，直接在 ells 的终端里编辑。
//!
//! 这里只管内容与绘制；保存后的回传仍走既有的 `EditSaved` / `EditUploaded` 链路，
//! 远端冲突判断也复用那一套。

use std::path::Path;

use crossterm::event::{KeyEvent, KeyModifiers, MouseEvent as CtMouseEvent, MouseEventKind};
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
        Ok(Self {
            remote: remote.to_string(),
            local: local.to_path_buf(),
            name: name.to_string(),
            state: EditorState::new(Lines::from(text.as_str())),
            handler: EditorEventHandler::default(),
            saved_text: text,
            extension,
            crlf,
            highlight,
            confirm_close: false,
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

    fn mode_label(&self) -> &'static str {
        match self.state.mode {
            EditorMode::Normal => "NORMAL",
            EditorMode::Insert => "INSERT",
            EditorMode::Visual => "VISUAL",
            EditorMode::Search => "SEARCH",
        }
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

    let editor_theme = EditorTheme::default()
        .base(Style::default().fg(theme::text()).bg(theme::bg()))
        .cursor_style(
            Style::default()
                .fg(theme::on_accent())
                .bg(theme::accent_bg())
                .add_modifier(Modifier::BOLD),
        )
        .selection_style(Style::default().fg(theme::on_accent()).bg(theme::select_bg()))
        .line_numbers_style(Style::default().fg(theme::muted()).bg(theme::bg()))
        .hide_status_line();
    let highlighter = if ed.highlight {
        ed.extension
            .as_deref()
            .and_then(|ext| SyntaxHighlighter::new(syntect_theme(), ext).ok())
    } else {
        None
    };
    let view = EditorView::new(&mut ed.state)
        .theme(editor_theme)
        .line_numbers(LineNumbers::Absolute)
        .wrap(false)
        .tab_width(4)
        .syntax_highlighter(highlighter);
    view.render(chunks[0], f.buffer_mut());

    let hint = status.unwrap_or_else(|| {
        if ed.highlight {
            "Ctrl-S 保存并回传 · Esc 退回 NORMAL".to_string()
        } else {
            "大文件已关高亮 · Ctrl-S 保存并回传".to_string()
        }
    });
    let info = format!(" {} · {} 行", ed.remote, ed.line_count());
    let line = Line::from(vec![
        Span::styled(info, Style::default().fg(theme::dim()).bg(theme::bg())),
        Span::raw("  "),
        Span::styled(hint, Style::default().fg(theme::warn()).bg(theme::bg())),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme::bg())),
        chunks[1],
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::{language_extension, EmbeddedEdit, HIGHLIGHT_MAX_LINES};

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
}

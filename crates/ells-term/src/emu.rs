use vt100::{Callbacks, Parser, Screen};

#[derive(Default)]
pub struct TerminalEvents {
    answers: Vec<Vec<u8>>,
    title: Option<String>,
    bell: u32,
    resize_request: Option<(u16, u16)>,
}

impl Callbacks for TerminalEvents {
    fn audible_bell(&mut self, _screen: &mut Screen) {
        self.bell += 1;
    }

    fn resize(&mut self, _screen: &mut Screen, request: (u16, u16)) {
        self.resize_request = Some(request);
    }

    fn set_window_title(&mut self, _screen: &mut Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut Screen,
        _intermediates_one: Option<u8>,
        _intermediates_two: Option<u8>,
        params: &[&[u16]],
        final_byte: char,
    ) {
        // DSR: cursor position report ("\e[6n") and device status.
        if final_byte == 'n'
            && params.iter().any(|p| p.first().copied() == Some(6))
        {
            let (row, col) = screen.cursor_position();
            self.answers
                .push(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
            return;
        }
        // Primary device attributes ("\e[c" / "\e[0c"): claim VT100 with AVO.
        if final_byte == 'c'
            && params.is_empty()
                || params.iter().all(|p| p.first().copied().unwrap_or(0) == 0)
        {
            self.answers.push(b"\x1b[?1;2c".to_vec());
        }
    }
}

/// Wraps a vt100 parser as the embedded terminal state machine.
pub struct Emulator {
    parser: Parser<TerminalEvents>,
    rows: u16,
    cols: u16,
}

impl Emulator {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: Parser::new_with_callbacks(rows, cols, 2000, TerminalEvents::default()),
            rows,
            cols,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.parser.screen_mut().set_size(rows, cols);
    }

    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    pub fn screen(&self) -> &Screen {
        self.parser.screen()
    }

    /// 滚动可视区到 scrollback 中的偏移（0=实时底部），vt100 会自动钳制到
    /// 实际可回看的行数；此后 `screen()` 的取格方法都反映滚动后的视图。
    pub fn set_scrollback(&mut self, rows: usize) {
        self.parser.screen_mut().set_scrollback(rows);
    }

    /// 当前实际生效的回看偏移（经钳制）。
    #[must_use]
    pub fn scrollback_offset(&self) -> usize {
        self.parser.screen().scrollback()
    }

    pub fn title(&self) -> Option<&str> {
        self.parser.callbacks().title.as_deref()
    }

    /// Escape-sequence answers the emulator owes the remote (cursor position
    /// reports, device attributes, ...). Call after `process`.
    pub fn take_answers(&mut self) -> Option<Vec<u8>> {
        self.parser.callbacks_mut().answers.pop()
    }

    /// Remote-requested resize (via `\e[8;rows;colst`), consumed once.
    pub fn take_resize_request(&mut self) -> Option<(u16, u16)> {
        self.parser.callbacks_mut().resize_request.take()
    }

    /// Wipe the grid, keeping the current size. Used when re-entering
    /// embedded mode so the next draw starts from a clean slate.
    pub fn reset_screen(&mut self) {
        let (rows, cols) = (self.rows, self.cols);
        self.parser = Parser::new_with_callbacks(rows, cols, 2000, TerminalEvents::default());
    }
}

//! 终端里手敲 `vi` 命令时的输入行镜像。
//!
//! 远端 shell 的行缓冲在服务器上，本地只能根据"自己发出去的按键"复刻一份。
//! 复刻一旦遇到补全、历史、光标移动这类本地看不见结果的操作就不可信了；这时
//! 宁可放行回车（让远端照常开 vi），也不拿一份可能过时的行去猜文件名。

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// 可直接被 ells 接管的编辑器命令名。带路径的 `/usr/bin/vim` 按最后一段比对。
const EDITORS: [&str; 4] = ["vi", "vim", "nvim", "view"];

/// 交给 shell 后结果不再可知的字符：引号、展开、管道、重定向、通配符都算。
/// 遇到它们不是错误，只是不拦——远端 vi 仍然照常启动。
const UNSAFE: [char; 19] = [
    '\'', '"', '`', '$', '\\', ';', '&', '|', '>', '<', '*', '?', '[', ']', '{', '}', '(', ')', '#',
];

/// 当前输入行 + 这份镜像还可不可信。
#[derive(Debug, Clone)]
pub struct LineMirror {
    text: String,
    trusted: bool,
    /// 回车提交时这行已经不可信、但看着像 vi 命令：留给调用方提示"为什么没接管"。
    skipped: Option<String>,
}

impl Default for LineMirror {
    fn default() -> Self {
        Self::new()
    }
}

impl LineMirror {
    pub fn new() -> Self {
        Self { text: String::new(), trusted: true, skipped: None }
    }

    /// 清空并恢复可信：回车已经提交、或用户按 Ctrl-C 重新开了一行。
    pub fn clear(&mut self) {
        self.text.clear();
        self.trusted = true;
        self.skipped = None;
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// 本地补全把远端回显也补上来的字符：镜像跟着走，这行仍然可信。
    pub fn push_str(&mut self, s: &str) {
        self.text.push_str(s);
    }

    /// 只有可信的行才敢在本地补全：动过光标之后镜像可能已经不是远端那行了。
    pub fn is_trusted(&self) -> bool {
        self.trusted
    }

    /// 补全失败时把这一行标成不可信：后续回车交给远端。
    pub fn mark_untrusted(&mut self) {
        self.trusted = false;
    }

    /// 取走"回车时被放行的 vi 命令目标"，只在刚回车后调用一次。
    pub fn take_skipped(&mut self) -> Option<String> {
        self.skipped.take()
    }

    /// 记录一个按键。回车时如果整行是一条可直接接管的 vi 命令，
    /// 返回目标路径（调用方应吞掉这次回车）。
    pub fn observe(&mut self, key: &KeyEvent) -> Option<String> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter => {
                let target = vi_target(&self.text);
                let accepted = if self.trusted { target.clone() } else { None };
                // 不可信的行仍然认出目标，只为了告诉用户"这行本来能接管，但因为动过光标/补全放弃了"。
                let skipped = if accepted.is_none() && !self.trusted { target } else { None };
                self.clear();
                self.skipped = skipped;
                return accepted;
            }
            // Ctrl-C 在 shell 里意味着"这一行作废、新起一行"。
            KeyCode::Char('c') if ctrl => self.clear(),
            // Ctrl-U 只删到行首；如果之前动过光标（已不可信），后面可能还留着尾巴，
            // 所以清空内容但保留可信度。
            KeyCode::Char('u') if ctrl => self.text.clear(),
            // Ctrl-L 只是重画，行内容和光标都不变。
            KeyCode::Char('l') if ctrl => {}
            KeyCode::Char(c) if !ctrl && !alt && !c.is_control() => self.text.push(c),
            KeyCode::Backspace => {
                self.text.pop();
            }
            // 补全、历史、光标移动、剪切粘贴之外的编辑键，都会让本地复刻失真。
            _ => self.trusted = false,
        }
        None
    }

    /// 粘贴算一手字：带换行或控制字符的粘贴可能一次提交多条命令，放弃这条行。
    pub fn observe_paste(&mut self, text: &str) {
        if text.chars().any(char::is_control) {
            self.trusted = false;
            return;
        }
        self.text.push_str(text);
    }
}

/// 从一整行里认出"可接管的 vi 命令"，给出那个文件名。
/// 多个文件、无文件名、命令前缀（sudo/env/…）一律返回 None。
fn vi_target(line: &str) -> Option<String> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let first = *tokens.first()?;
    // ./vi 这类当前目录里的脚本不是系统编辑器，别替它做文件接管。
    if first.contains('/') && !first.starts_with('/') {
        return None;
    }
    let base = first.rsplit('/').next().unwrap_or(first);
    if !EDITORS.contains(&base) {
        return None;
    }
    let mut file = None;
    for tok in &tokens[1..] {
        // -R / --clean 之类选项、+123 跳行参数：不影响文件名。
        if tok.starts_with('-') || tok.starts_with('+') {
            continue;
        }
        if file.is_some() {
            return None;
        }
        file = Some(*tok);
    }
    let file = file?;
    plain_path(file).then(|| file.to_string())
}

fn plain_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    // 开头的 ~ / ~/ 交给 SFTP 按家目录解析；~user 不认。
    let rest = match path.strip_prefix('~') {
        Some(r) if r.is_empty() || r.starts_with('/') => r,
        Some(_) => return false,
        None => path,
    };
    !rest.chars().any(|c| c.is_control() || UNSAFE.contains(&c))
}

/// 这一行现在要补全的「目录 + 前缀」：`vi /etc/ng` → (`/etc/`, `ng`)。
///
/// 只认正在写的最后一个文件名：行尾是空格（参数已写完）、已经出现过别的文件名
/// （vi 多文件本就不接管）、或者不是一条可接管的 vi 命令时返回 None —— 调用方
/// 把 Tab 原样交给远端。
pub fn completion_request(line: &str) -> Option<(String, String)> {
    if line.is_empty() || line.ends_with(char::is_whitespace) {
        return None;
    }
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let first = *tokens.first()?;
    if first.contains('/') && !first.starts_with('/') {
        return None;
    }
    let base = first.rsplit('/').next().unwrap_or(first);
    if !EDITORS.contains(&base) {
        return None;
    }
    let partial = *tokens.last()?;
    if partial == first {
        return None;
    }
    // 前面已经写过一个文件名：vi 多文件不接管，补全也不该假装接管。
    for tok in &tokens[1..tokens.len() - 1] {
        if !tok.starts_with('-') && !tok.starts_with('+') {
            return None;
        }
    }
    if !plain_path(partial) {
        return None;
    }
    if partial == "~" {
        return Some(("~".to_string(), String::new()));
    }
    Some(match partial.rfind('/') {
        Some(i) => (partial[..=i].to_string(), partial[i + 1..].to_string()),
        None => (String::new(), partial.to_string()),
    })
}

/// 名字里带空格 / 通配符 / 引号时，bash 会自己转义；本地补出去的回显就对不上了。
pub fn completion_name_safe(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || UNSAFE.contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_line(m: &mut LineMirror, text: &str) {
        for c in text.chars() {
            m.observe(&key(KeyCode::Char(c)));
        }
    }

    fn enter(line: &str) -> Option<String> {
        let mut m = LineMirror::new();
        type_line(&mut m, line);
        m.observe(&key(KeyCode::Enter))
    }

    #[test]
    fn plain_vi_commands_are_intercepted() {
        assert_eq!(enter("vi /etc/hosts").as_deref(), Some("/etc/hosts"));
        assert_eq!(enter("vim notes.md").as_deref(), Some("notes.md"));
        assert_eq!(enter("nvim ~/.bashrc").as_deref(), Some("~/.bashrc"));
        assert_eq!(enter("/usr/bin/view logs/app.log").as_deref(), Some("logs/app.log"));
    }

    #[test]
    fn options_and_line_numbers_are_skipped() {
        assert_eq!(enter("vim -R +120 /var/log/syslog").as_deref(), Some("/var/log/syslog"));
        assert_eq!(enter("nvim --clean a.txt").as_deref(), Some("a.txt"));
    }

    #[test]
    fn other_commands_are_left_to_the_remote() {
        assert_eq!(enter("sudo vi /etc/nginx/nginx.conf"), None);
        assert_eq!(enter("vi"), None);
        assert_eq!(enter("vi one.txt two.txt"), None);
        assert_eq!(enter("cat /etc/hosts"), None);
        assert_eq!(enter("./vi file"), None, "相对路径的脚本不是编辑器命令");
    }

    #[test]
    fn shell_metacharacters_are_left_to_the_remote() {
        for line in [
            "vi \"my file.txt\"",
            "vi $HOME/x",
            "vi *.log",
            "vi a.txt > out",
            "vi a.txt | less",
            "vi a.txt; rm b",
            "vi $(pwd)/x",
            "vi a\\ b",
        ] {
            assert_eq!(enter(line), None, "{line} 不该被接管");
        }
    }

    #[test]
    fn tilde_user_is_not_treated_as_home() {
        assert_eq!(enter("vi ~/a.txt").as_deref(), Some("~/a.txt"));
        assert_eq!(enter("vi ~root/a.txt"), None);
    }

    #[test]
    fn completion_and_history_make_the_line_untrusted() {
        let mut m = LineMirror::new();
        type_line(&mut m, "vi /etc/hos");
        m.observe(&key(KeyCode::Tab));
        m.observe(&key(KeyCode::Enter));
        assert_eq!(m.text(), "", "回车后镜像要清空");
        // 重建一份，确认不可信状态让回车返回 None
        let mut m = LineMirror::new();
        type_line(&mut m, "vi a.txt");
        m.observe(&key(KeyCode::Up));
        assert_eq!(m.observe(&key(KeyCode::Enter)), None);
    }

    #[test]
    fn cursor_keys_make_the_line_untrusted() {
        for code in [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Delete,
            KeyCode::Esc,
        ] {
            let mut m = LineMirror::new();
            type_line(&mut m, "vi a.txt");
            m.observe(&key(code));
            assert_eq!(m.observe(&key(KeyCode::Enter)), None, "{code:?} 之后不该再拦");
        }
        let mut m = LineMirror::new();
        type_line(&mut m, "vi a.txt");
        m.observe(&ctrl('a'));
        assert_eq!(m.observe(&key(KeyCode::Enter)), None, "Ctrl-A 之后不该再拦");
    }

    #[test]
    fn backspace_edits_the_mirror() {
        let mut m = LineMirror::new();
        type_line(&mut m, "vi a.txtx");
        m.observe(&key(KeyCode::Backspace));
        assert_eq!(m.observe(&key(KeyCode::Enter)).as_deref(), Some("a.txt"));
    }

    #[test]
    fn ctrl_u_clears_and_ctrl_c_resets_trust() {
        let mut m = LineMirror::new();
        type_line(&mut m, "vi a.txt");
        m.observe(&ctrl('u'));
        assert_eq!(m.text(), "");
        type_line(&mut m, "vi b.txt");
        assert_eq!(m.observe(&key(KeyCode::Enter)).as_deref(), Some("b.txt"));

        let mut m = LineMirror::new();
        type_line(&mut m, "vi a.txt");
        m.observe(&key(KeyCode::Tab)); // 不可信
        m.observe(&ctrl('c')); // 新起一行，恢复可信
        type_line(&mut m, "vi c.txt");
        assert_eq!(m.observe(&key(KeyCode::Enter)).as_deref(), Some("c.txt"));
    }

    #[test]
    fn ctrl_l_keeps_the_line() {
        let mut m = LineMirror::new();
        type_line(&mut m, "vi a.txt");
        m.observe(&ctrl('l'));
        assert_eq!(m.observe(&key(KeyCode::Enter)).as_deref(), Some("a.txt"));
    }

    #[test]
    fn multiline_paste_makes_the_line_untrusted() {
        let mut m = LineMirror::new();
        m.observe_paste("vi a.txt");
        assert_eq!(m.observe(&key(KeyCode::Enter)).as_deref(), Some("a.txt"));

        let mut m = LineMirror::new();
        m.observe_paste("vi a.txt\rrm -rf b");
        assert_eq!(m.observe(&key(KeyCode::Enter)), None);
    }

    #[test]
    fn edited_lines_still_parse_after_fixes() {
        let mut m = LineMirror::new();
        type_line(&mut m, "vi /etc/hostss");
        m.observe(&key(KeyCode::Backspace));
        assert_eq!(m.observe(&key(KeyCode::Enter)).as_deref(), Some("/etc/hosts"));
    }

    #[test]
    fn completion_splits_the_word_being_written() {
        assert_eq!(
            completion_request("vi /etc/ng"),
            Some(("/etc/".to_string(), "ng".to_string()))
        );
        assert_eq!(
            completion_request("vim notes.t"),
            Some((String::new(), "notes.t".to_string()))
        );
        assert_eq!(
            completion_request("nvim ~/do"),
            Some(("~/".to_string(), "do".to_string()))
        );
        assert_eq!(
            completion_request("vi ~"),
            Some(("~".to_string(), String::new()))
        );
        assert_eq!(
            completion_request("vim -R +120 /var/lo"),
            Some(("/var/".to_string(), "lo".to_string()))
        );
    }

    #[test]
    fn completion_stops_at_lines_it_would_not_take_over() {
        for line in [
            "vi",
            "vi ",
            "vi a.txt ",
            "sudo vi /etc/ng",
            "vi one two.txt",
            "vi \"a b",
            "./vi x",
        ] {
            assert_eq!(completion_request(line), None, "{line} 不该补全");
        }
    }

    #[test]
    fn names_bash_would_escape_are_not_completed_locally() {
        assert!(completion_name_safe("notes.txt"));
        assert!(completion_name_safe("中文.md"));
        assert!(!completion_name_safe("my notes.txt"));
        assert!(!completion_name_safe("$x"));
        assert!(!completion_name_safe("a*b"));
        assert!(!completion_name_safe(""));
    }

    #[test]
    fn an_untrusted_line_reports_the_target_it_skipped() {
        let mut m = LineMirror::new();
        type_line(&mut m, "vi a.txt");
        m.observe(&key(KeyCode::Left));
        assert_eq!(m.observe(&key(KeyCode::Enter)), None);
        assert_eq!(m.take_skipped().as_deref(), Some("a.txt"));
        assert_eq!(m.take_skipped(), None, "提示只取一次");
    }

    #[test]
    fn a_completed_suffix_keeps_the_line_interceptable() {
        let mut m = LineMirror::new();
        type_line(&mut m, "vi no");
        m.push_str("tes.txt ");
        assert_eq!(m.observe(&key(KeyCode::Enter)).as_deref(), Some("notes.txt"));
    }
}

//! 终端输出行级高亮：docker ps 表头/状态词、常见日志级别、行首时间戳。
//! 只给"默认前景色"的单元格着色，绝不覆盖程序自己发出的 ANSI 颜色。

use ratatui::style::Color;

/// 关键词 → 颜色（大小写敏感，需词边界）。规则来自常见运维输出的社区惯例
/// （docker/kubectl/systemd/git/日志级别），只增高频词，避免误染普通英文句子。
const WORD_RULES: &[(&str, Color)] = &[
    // —— 错误/失败 ——
    ("ERROR", Color::Red),
    ("FATAL", Color::Red),
    ("CRITICAL", Color::Red),
    ("PANIC", Color::Red),
    ("ALERT", Color::Red),
    ("SEVERE", Color::Red),
    ("Error", Color::Red),
    ("ERR", Color::Red),
    ("EXITED", Color::Red),
    ("Exited", Color::Red),
    ("FAILED", Color::Red),
    ("Failed", Color::Red),
    ("Failure", Color::Red),
    ("failed", Color::Red),
    ("refused", Color::Red),
    ("denied", Color::Red),
    ("Timeout", Color::Red),
    ("CrashLoopBackOff", Color::Red),
    ("ImagePullBackOff", Color::Red),
    ("ErrImagePull", Color::Red),
    ("OOMKilled", Color::Red),
    ("Evicted", Color::Red),
    ("masked", Color::Red),
    ("dead", Color::Red),
    ("Traceback", Color::Red),
    ("Exception", Color::Red),
    ("Unauthorized", Color::Red),
    ("Forbidden", Color::Red),
    ("NotFound", Color::Red),
    ("SYN_SENT", Color::Red),
    // —— 警告/中间态 ——
    ("WARNING", Color::Yellow),
    ("WARN", Color::Yellow),
    ("Warning", Color::Yellow),
    ("Paused", Color::Yellow),
    ("Created", Color::Yellow),
    ("Removing", Color::Yellow),
    ("Terminating", Color::Yellow),
    ("Pending", Color::Yellow),
    ("ContainerCreating", Color::Yellow),
    ("NotReady", Color::Yellow),
    ("stopped", Color::Yellow),
    ("CLOSE_WAIT", Color::Yellow),
    ("deprecated", Color::Yellow),
    ("Deprecated", Color::Yellow),
    // —— 正常/成功 ——
    ("OK", Color::Green),
    ("SUCCESS", Color::Green),
    ("Succeeded", Color::Green),
    ("succeeded", Color::Green),
    ("PASSED", Color::Green),
    ("Running", Color::Green),
    ("Completed", Color::Green),
    ("active", Color::Green),
    ("Up", Color::Green),
    ("healthy", Color::Green),
    ("Connected", Color::Green),
    // —— 信息/动作 ——
    ("INFO", Color::Cyan),
    ("NOTICE", Color::Blue),
    ("Downloading", Color::Cyan),
    ("Pulling", Color::Cyan),
    // HTTP 方法（访问日志高频词，来自 tailspin 的分类惯例）
    ("GET", Color::Cyan),
    ("POST", Color::Cyan),
    ("PUT", Color::Cyan),
    ("PATCH", Color::Cyan),
    ("DELETE", Color::Cyan),
    ("HEAD", Color::Cyan),
    ("OPTIONS", Color::Cyan),
    ("LISTEN", Color::Cyan),
    // 布尔/空值（JSON/kv 输出）
    ("true", Color::Magenta),
    ("false", Color::Magenta),
    ("null", Color::Magenta),
    ("nil", Color::Magenta),
    // —— 弱化的次要状态 ——
    ("DEBUG", Color::DarkGray),
    ("TRACE", Color::DarkGray),
    ("VERBOSE", Color::DarkGray),
    ("inactive", Color::DarkGray),
    ("disabled", Color::DarkGray),
    ("Unknown", Color::DarkGray),
    // —— 需要盯住的状态 ——
    ("Restarting", Color::Magenta),
    ("restarting", Color::Magenta),
    ("unhealthy", Color::Magenta),
];

/// 输入按单元格对齐的一行文本（宽字符续格用 '\0' 占位），
/// 输出与输入等长的逐格着色表；None 表示保持原色。
pub fn line_colors(text: &str) -> Vec<Option<Color>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Option<Color>> = vec![None; chars.len()];
    let set = |out: &mut Vec<Option<Color>>, start: usize, end: usize, color: Color| {
        for slot in out.iter_mut().take(end).skip(start) {
            if slot.is_none() {
                *slot = Some(color);
            }
        }
    };

    // docker ps 表头行：整行青色加粗（由调用方读 bold 标记，这里只给色）
    let trimmed = text.trim_start_matches([' ', '\0']);
    if trimmed.starts_with("CONTAINER ID") {
        set(&mut out, 0, chars.len(), Color::Cyan);
        return out;
    }

    // 行首时间戳（ISO 或 docker logs 带毫秒）弱化为灰色
    if let Some(len) = timestamp_len(&chars) {
        set(&mut out, 0, len, Color::DarkGray);
    }

    for (word, color) in WORD_RULES {
        for (start, _) in find_words(&chars, word) {
            set(&mut out, start, start + word.chars().count(), *color);
        }
    }

    // IPv4 地址浅蓝（版本号 x.y.z / x.y.z.w.v 不匹配，避免误染）
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() && (i == 0 || !is_word_char(chars[i - 1])) {
            if let Some(len) = ip_len_at(&chars, i) {
                set(&mut out, i, i + len, Color::LightBlue);
                i += len;
                continue;
            }
        }
        i += 1;
    }

    // 百分比（如 CPU/内存占用）黄色
    for (i, c) in chars.iter().enumerate() {
        if *c != '%' {
            continue;
        }
        let mut j = i;
        while j > 0 && chars[j - 1].is_ascii_digit() {
            j -= 1;
        }
        if j < i && i - j <= 3 && (j == 0 || !chars[j - 1].is_ascii_alphabetic()) {
            set(&mut out, j, i + 1, Color::Yellow);
        }
    }
    out
}

/// 从 `start` 起是否是一个规范 IPv4（4 段、各 0-255、后面不接 `.`/数字）。
fn ip_len_at(chars: &[char], start: usize) -> Option<usize> {
    let mut pos = start;
    for octet in 0..4 {
        let mut digits = 0;
        let mut value: u16 = 0;
        while pos < chars.len() && chars[pos].is_ascii_digit() && digits < 3 {
            value = value * 10 + (chars[pos] as u16 - '0' as u16);
            digits += 1;
            pos += 1;
        }
        if digits == 0 || value > 255 {
            return None;
        }
        if octet < 3 {
            if chars.get(pos) != Some(&'.') {
                return None;
            }
            pos += 1;
        }
    }
    // 尾部不能继续是 IP/版本号的一部分
    match chars.get(pos) {
        Some(c) if c.is_ascii_digit() || *c == '.' => None,
        _ => Some(pos - start),
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// 在单元格序列中查找带词边界的关键词，返回起始下标。
fn find_words(chars: &[char], word: &str) -> Vec<(usize, ())> {
    let w: Vec<char> = word.chars().collect();
    if w.is_empty() || chars.len() < w.len() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    'outer: for i in 0..=chars.len() - w.len() {
        if chars[i] != w[0] {
            continue;
        }
        if i > 0 && is_word_char(chars[i - 1]) {
            continue;
        }
        let end = i + w.len();
        if end < chars.len() && is_word_char(chars[end]) {
            continue;
        }
        for (k, cw) in w.iter().enumerate() {
            if chars[i + k] != *cw {
                continue 'outer;
            }
        }
        hits.push((i, ()));
    }
    hits
}

/// 匹配 `2026-10-04`、`2026-10-04 12:34:56`、`2026-10-04T12:34:56.789Z` 前缀。
fn timestamp_len(chars: &[char]) -> Option<usize> {
    let digit = |i: usize| chars.get(i).copied().is_some_and(|c| c.is_ascii_digit());
    for i in 0..4 {
        if !digit(i) {
            return None;
        }
    }
    if chars.get(4) != Some(&'-') || chars.get(7) != Some(&'-') {
        return None;
    }
    for i in [5, 6, 8, 9] {
        if !digit(i) {
            return None;
        }
    }
    let mut len = 10;
    if matches!(chars.get(10), Some('T') | Some(' '))
        && digit(11)
        && digit(12)
        && chars.get(13) == Some(&':')
    {
        len = 16;
        if digit(14) && digit(15) && chars.get(16) == Some(&':') && digit(17) && digit(18) {
            len = 19;
            if matches!(chars.get(19), Some('.')) {
                let mut i = 20;
                while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                }
                len = i;
            }
            if matches!(chars.get(len), Some('Z')) {
                len += 1;
            }
        }
    }
    Some(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colored(text: &str) -> Vec<Option<Color>> {
        line_colors(text)
    }

    #[test]
    fn docker_ps_header_line_is_cyan() {
        let c = colored("CONTAINER ID   IMAGE     COMMAND   CREATED   STATUS    PORTS     NAMES");
        assert!(c.iter().all(|x| *x == Some(Color::Cyan)));
    }

    #[test]
    fn docker_status_words() {
        let text = "abc123def456   nginx:latest   \"docker-entry\"   Up 3 hours   0.0.0.0:80->80/tcp   web";
        let c = colored(text);
        let up_at = text.find("Up").unwrap();
        assert_eq!(c[up_at], Some(Color::Green));
        // "Up" 出现在单词内部时不着色
        let text2 = "group 3 hours";
        let c2 = colored(text2);
        assert!(c2.iter().all(|x| x.is_none()));
    }

    #[test]
    fn exited_red_and_restarting_magenta() {
        let text = "Exited (0) 2 hours ago";
        let c = colored(text);
        assert_eq!(c[0], Some(Color::Red));
        let text = "Restarting (1) 5 seconds ago";
        let c = colored(text);
        assert_eq!(c[0], Some(Color::Magenta));
    }

    #[test]
    fn log_levels() {
        let text = "2026-10-04 12:34:56 [ERROR] failed to connect; INFO retried; warning lowercase stays";
        let c = colored(text);
        // 时间戳灰
        assert_eq!(c[0], Some(Color::DarkGray));
        assert_eq!(c[3], Some(Color::DarkGray));
        let err = text.find("ERROR").unwrap();
        assert_eq!(c[err], Some(Color::Red));
        let info = text.find("INFO").unwrap();
        assert_eq!(c[info], Some(Color::Cyan));
        // 小写 warning 不着色
        let warn = text.find("warning").unwrap();
        assert_eq!(c[warn], None);
    }

    #[test]
    fn timestamp_with_millis_and_z() {
        let text = "2026-10-04T12:34:56.789Z hello";
        let c = colored(text);
        assert_eq!(c[23], Some(Color::DarkGray));
        assert_eq!(c[25], None);
    }

    #[test]
    fn plain_text_untouched() {
        let c = colored("total 12\ndrwxr-xr-x 2 root root 4096 Oct  4 12:00 src");
        assert!(c.iter().all(|x| x.is_none()));
    }

    #[test]
    fn kubectl_statuses() {
        let text = "web-7d9f   1/1   Running   0     3d";
        let c = colored(text);
        let run = text.find("Running").unwrap();
        assert_eq!(c[run], Some(Color::Green));
        let text = "db-1   0/1   CrashLoopBackOff   5   10s";
        let c = colored(text);
        assert_eq!(c[text.find("CrashLoopBackOff").unwrap()], Some(Color::Red));
        let text = "job-a   0/1   Terminating   0   1m";
        let c = colored(text);
        assert_eq!(c[text.find("Terminating").unwrap()], Some(Color::Yellow));
    }

    #[test]
    fn systemd_and_results() {
        let text = "nginx.service - active (running)";
        let c = colored(text);
        assert_eq!(c[text.find("active").unwrap()], Some(Color::Green));
        let text = "check OK; build FAILED; access denied";
        let c = colored(text);
        assert_eq!(c[text.find("OK").unwrap()], Some(Color::Green));
        assert_eq!(c[text.find("FAILED").unwrap()], Some(Color::Red));
        assert_eq!(c[text.find("denied").unwrap()], Some(Color::Red));
    }

    #[test]
    fn ipv4_colored_but_versions_not() {
        let text = "connect 10.2.3.4:80 refused";
        let c = colored(text);
        let ip = text.find("10.2.3.4").unwrap();
        assert_eq!(c[ip], Some(Color::LightBlue));
        assert_eq!(c[ip + 7], Some(Color::LightBlue));
        assert_eq!(c[text.find("refused").unwrap()], Some(Color::Red));
        // 软件版本号（5 段/超大段）不当 IP 染
        let text = "node v18.19.0.5 compiled";
        let c = colored(text);
        assert!(c.iter().all(|x| x.is_none()));
        // 256 越界不算 IP
        let text = "bad 256.1.1.1 addr";
        let c = colored(text);
        let at = text.find("256").unwrap();
        assert_eq!(c[at], None);
    }

    #[test]
    fn http_methods_booleans_and_conn_states() {
        let text = "GET /api/health 200 OK";
        let c = colored(text);
        assert_eq!(c[0], Some(Color::Cyan));
        assert_eq!(c[text.find("OK").unwrap()], Some(Color::Green));
        let text = "{\"enabled\": true, \"gone\": null}";
        let c = colored(text);
        assert_eq!(c[text.find("true").unwrap()], Some(Color::Magenta));
        assert_eq!(c[text.find("null").unwrap()], Some(Color::Magenta));
        let text = "tcp 0 0 0.0.0.0:22 0.0.0.0:* LISTEN";
        let c = colored(text);
        assert_eq!(c[text.find("LISTEN").unwrap()], Some(Color::Cyan));
        // 小写 get 不着色
        let text = "get the data";
        let c = colored(text);
        assert!(c.iter().all(|x| x.is_none()));
    }

    #[test]
    fn percent_yellow() {
        let text = "CPU: 45% MEM: 100%";
        let c = colored(text);
        assert_eq!(c[text.find("45%").unwrap()], Some(Color::Yellow));
        assert_eq!(c[text.find("45%").unwrap() + 2], Some(Color::Yellow));
        assert_eq!(c[text.find("100%").unwrap()], Some(Color::Yellow));
        // 字母紧跟的数字不算百分比主体（保守）
        let text = "abc45% done";
        let c = colored(text);
        assert_eq!(c[text.find('4').unwrap()], None);
    }
}

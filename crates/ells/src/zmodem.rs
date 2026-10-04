//! 协议级 ZMODEM 检测：监听远端 PTY 输出流中 lrzsz(sz/rz) 的启动特征，
//! 命中后由上层吞掉协议字节、中止远端进程并改走 SFTP。不干预键盘输入。

use std::collections::VecDeque;

const CAN: u8 = 0x18;
/// lrzsz 在横幅/头部前先发送 8 个 CAN 填充（zspuck），二进制头另有 2 个 CAN
/// 框定；正常终端输出几乎不会出现连续 8 个 CAN。
const MIN_CAN_RUN: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZmodemEvent {
    /// 远端 sz 想发文件 → 用 SFTP 下载这些文件
    Send { files: Vec<String> },
    /// 远端 rz 等待接收 → 本地选文件后 SFTP 上传
    Receive,
    /// 远端提示 sz/rz 不存在
    Missing { sz: bool },
    /// 检测到 ZMODEM 握手但无法判定方向
    Unknown,
}

#[derive(Default)]
pub struct Watcher {
    line: Vec<u8>,
    /// 最近的完整行（用于命令行回显与文本标记）
    lines: VecDeque<String>,
    /// 最近识别出的 sz/rz 命令行
    cmds: VecDeque<Vec<String>>,
    /// 供上层消费的回显命令队列（cd 跟踪等）
    out_cmds: Vec<Vec<String>>,
    can_run: usize,
    /// ANSI 转义序列吸收状态：0=不在序列中，1=ESC 后，2=CSI 参数，3=OSC
    esc: u8,
    /// 回车结尾的裸 `sz`（无任何文件参数）回显：lrzsz 必然报错退出且不发
    /// 协议帧，直接触发文件浏览器选文件。
    bare_sz: bool,
    /// 会话内收到的第一个可解码头类型（ZRQINIT/ZFILE=sz 下载，ZRINIT=rz 上传）。
    /// 必须按"首帧"定方向：sz 启动后几毫秒内会紧跟一个 ZRINIT 回帧，
    /// 若按"最近一帧"判定会把 sz 误判成 rz。
    first_header: Option<u8>,
    /// 已解码的最近头类型（供首帧不可解码时兜底）
    header_type: Option<u8>,
    /// CAN 串后紧跟 `**`（lrzsz 十六进制头文本的框定标志）
    hex_head: bool,
    missing: Option<bool>,
    fired: bool,
}

impl Watcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段远端输出；返回 Some 表示检测到 ZMODEM 会话开始。
    pub fn feed(&mut self, bytes: &[u8]) -> Option<ZmodemEvent> {
        if bytes.contains(&CAN) {
            zlog(&format_args!(
                "feed {}B: {}",
                bytes.len(),
                hex_preview(bytes, 96)
            ));
        }
        let mut protocol_hit = false;
        for &b in bytes {
            // 整段吸收 ANSI 转义序列（彩色提示符、括号粘贴 ESC[200~/201~ 等），
            // 否则回显行会被 ESC 截断，命令行参数（文件名）就丢了。
            if self.esc != 0 {
                self.esc = match self.esc {
                    1 => match b {
                        b'[' => 2,
                        b']' => 3,
                        0x40..=0x5f => 0,
                        _ => 1,
                    },
                    2 => {
                        if (0x20..=0x3f).contains(&b) {
                            2
                        } else {
                            0
                        }
                    }
                    3 => {
                        if b == 0x07 {
                            0
                        } else if b == 0x1b {
                            1
                        } else {
                            3
                        }
                    }
                    _ => 0,
                };
                continue;
            }
            if b == 0x1b {
                self.esc = 1;
                continue;
            }
            if b == CAN {
                self.can_run += 1;
            } else {
                if self.can_run >= MIN_CAN_RUN {
                    if let Some(t) = header_type_of(b) {
                        self.header_type = Some(t);
                        if self.first_header.is_none() {
                            self.first_header = Some(t);
                        }
                        protocol_hit = true;
                    } else if b == b'*' {
                        // CAN 填充后紧跟 "**"：lrzsz 十六进制文本头的框定，绝无仅有
                        self.hex_head = true;
                        protocol_hit = true;
                    } else if !self.cmds.is_empty() {
                        // 真实 lrzsz 在 CAN 填充后先发横幅文本（如
                        // "++rz waiting to receive.**B0100000023be50"）再发头，
                        // 只要刚回显过 sz/rz 命令，CAN 串本身就是启动信号。
                        protocol_hit = true;
                    }
                }
                self.can_run = 0;
            }
            match b {
                b'\r' | b'\n' => self.end_line(true),
                // readline 退格编辑输出 "\b \b"：逐字节回删即可正确还原行内容
                0x08 | 0x7f => {
                    self.line.pop();
                }
                b'\t' => self.line.push(b),
                0x00..=0x1f => {
                    if !self.line.is_empty() {
                        self.end_line(false);
                    }
                }
                _ => self.line.push(b),
            }
            if let Some(sz) = self.missing.take() {
                return Some(ZmodemEvent::Missing { sz });
            }
        }
        if self.fired {
            return None;
        }
        if self.bare_sz {
            self.fired = true;
            let ev = self.classify();
            zlog(&format_args!("bare-sz fire -> {ev:?}"));
            return Some(ev);
        }
        if protocol_hit || self.marker_seen() {
            self.fired = true;
            let ev = self.classify();
            zlog(&format_args!(
                "fire hit={protocol_hit} first={:?} last={:?} hex_head={} cmds={:?} -> {ev:?}",
                self.first_header, self.header_type, self.hex_head, self.cmds
            ));
            return Some(ev);
        }
        None
    }

    /// 中止/完成后复位全部状态。
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// 取出（并清空）观察到的回显命令行，供上层做 cd 跟踪。
    pub fn take_commands(&mut self) -> Vec<Vec<String>> {
        std::mem::take(&mut self.out_cmds)
    }

    fn end_line(&mut self, final_line: bool) {
        if self.line.is_empty() {
            return;
        }
        let raw = std::mem::take(&mut self.line);
        let text = String::from_utf8_lossy(&raw).into_owned();
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        if self.lines.len() >= 64 {
            self.lines.pop_front();
        }
        // 只有带提示符前缀的行才是用户命令回显。真实 lrzsz 的 sz 启动时会先
        // 打印兼容横幅 "rz\r"（提示对端运行 rz），裸 "rz" 行绝不能当回显命令，
        // 否则会把 sz 误判成 rz。
        let has_prompt =
            trimmed.contains("# ") || trimmed.contains("$ ") || trimmed.contains("> ");
        if let Some(cmd) = parse_command(&trimmed) {
            if !cmd.is_empty() {
                if has_prompt && cmd.first().is_some_and(|w| is_transfer_cmd(w)) {
                    if self.cmds.len() >= 16 {
                        self.cmds.pop_front();
                    }
                    zlog(&format_args!("echo {:?}", cmd));
                    if final_line && is_bare_sz(&cmd) {
                        self.bare_sz = true;
                    }
                    self.cmds.push_back(cmd.clone());
                }
                self.out_cmds.push(cmd);
            }
        }
        if self.missing.is_none() {
            self.missing = extract_missing(&trimmed);
        }
        self.lines.push_back(trimmed);
    }

    fn marker_seen(&self) -> bool {
        // 按时间顺序取最近 3 个完整行 + 未完成的当前行
        let mut chrono: Vec<String> = self.lines.iter().rev().take(3).cloned().collect();
        chrono.reverse();
        if !self.line.is_empty() {
            chrono.push(String::from_utf8_lossy(&self.line).into_owned());
        }
        if chrono.iter().any(|l| is_zmodem_marker(l)) {
            return true;
        }
        // 重试的 CAN 填充会把十六进制头截成两段（"**B0" | CAN | "100000023be50"），
        // 拼接后再查一次
        is_zmodem_marker(&chrono.concat())
    }

    fn classify(&self) -> ZmodemEvent {
        // 优先看最近一条 sz/rz 命令回显：方向由用户执行的命令决定，最可靠。
        // sz 启动后会立刻回发一个 ZRINIT 帧，若只按头帧判定会把 sz 误判成 rz。
        for cmd in self.cmds.iter().rev() {
            match cmd.first().map(|w| w.rsplit('/').next().unwrap_or(w)).as_deref() {
                Some("sz") => {
                    return ZmodemEvent::Send {
                        files: cmd[1..]
                            .iter()
                            .filter(|t| !t.starts_with('-'))
                            .cloned()
                            .collect(),
                    };
                }
                Some("rz") => return ZmodemEvent::Receive,
                _ => {}
            }
        }
        // 无命令回显（如外部程序直接发起 ZMODEM）时以首帧头类型判定：
        // ZRQINIT/ZFILE=sz 发起下载，ZRINIT=rz 等待上传。
        match self.first_header.or(self.header_type) {
            Some(0) | Some(2) => return ZmodemEvent::Send { files: self.sz_files() },
            Some(1) => return ZmodemEvent::Receive,
            _ => {}
        }
        // 再退一步：lrzsz 横幅文本（含尚未结束的当前行）
        let current = String::from_utf8_lossy(&self.line).into_owned();
        let recent: Vec<&str> = self.lines.iter().rev().take(6).map(String::as_str).collect();
        for l in std::iter::once(current.as_str()).chain(recent) {
            if l.contains("rz waiting") {
                return ZmodemEvent::Receive;
            }
            if l.contains("**ZSEND") || l.contains("**zsend") || l.contains("**Zmodem") {
                return ZmodemEvent::Send { files: Vec::new() };
            }
        }
        ZmodemEvent::Unknown
    }

    /// 最近一条 sz 命令行里的文件名（过滤 - 选项）。
    fn sz_files(&self) -> Vec<String> {
        self.cmds
            .iter()
            .rev()
            .find(|cmd| cmd.first().map(|w| w.rsplit('/').next().unwrap_or(w)) == Some("sz"))
            .map(|cmd| {
                cmd[1..]
                    .iter()
                    .filter(|t| !t.starts_with('-'))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// CAN 串后紧跟的头类型字节（含 ZDLE 转义与扩展头形式）→ 0=ZRQINIT 1=ZRINIT 2=ZFILE。
fn header_type_of(b: u8) -> Option<u8> {
    match b {
        0x78 | 0x58 => Some(0), // ZRQINIT：转义形式 18 0x78；扩展头 0x18|0x40|0 = 0x58
        0x19 | 0x59 => Some(1),
        0x1a | 0x5a => Some(2),
        _ => None,
    }
}

/// lrzsz 对哑终端会把 ZMODEM 头渲染成十六进制文本：`**` + 至少 12 位十六进制
/// （如真实 rz 的 `**B0100000023be50`）。
fn looks_like_hex_header(line: &str) -> bool {
    line.match_indices("**").any(|(i, _)| {
        line[i + 2..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .count()
            >= 12
    })
}

fn is_zmodem_marker(line: &str) -> bool {
    line.contains("**Zmodem")
        || line.contains("**ZSEND")
        || line.contains("**zsend")
        || line.contains("rz waiting")
        || looks_like_hex_header(line)
}

fn is_transfer_cmd(word: &str) -> bool {
    let base = word.rsplit('/').next().unwrap_or(word);
    base == "sz" || base == "rz"
}

/// `sz` 且没有任何文件参数（lrzsz 会直接报 "need at least one file"）。
fn is_bare_sz(cmd: &[String]) -> bool {
    cmd.first().map(|w| w.rsplit('/').next().unwrap_or(w)) == Some("sz")
        && cmd[1..].iter().all(|t| t.starts_with('-'))
}

fn hex_preview(bytes: &[u8], max: usize) -> String {
    let mut out = bytes
        .iter()
        .take(max)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    if bytes.len() > max {
        out.push_str(" ..");
    }
    out
}

/// 真机排障日志：追加写入 %TEMP%\ells-zmodem.log；失败静默忽略，不影响功能。
fn zlog(args: &std::fmt::Arguments) {
    use std::fmt::Write as _;
    use std::io::Write as _;
    let mut msg = String::new();
    let _ = write!(msg, "{args}");
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = std::env::temp_dir().join("ells-zmodem.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{ms} {msg}");
    }
}

/// 把一行回显文本解析为命令 token 列表：剥掉常见提示符前缀。
pub fn parse_command(line: &str) -> Option<Vec<String>> {
    let cut = line
        .rfind("# ")
        .map(|i| i + 2)
        .or_else(|| line.rfind("$ ").map(|i| i + 2))
        .or_else(|| line.rfind("> ").map(|i| i + 2))
        .unwrap_or(0);
    let cmd_part = line[cut..].trim();
    if cmd_part.is_empty() {
        return None;
    }
    let toks: Vec<String> = cmd_part.split_whitespace().map(|s| s.to_string()).collect();
    let first = toks.first()?;
    let base = first.rsplit('/').next().unwrap_or(first);
    if base.is_empty()
        || !base
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
    {
        return None;
    }
    Some(toks)
}

fn extract_missing(line: &str) -> Option<bool> {
    if !line.contains("not found") {
        return None;
    }
    if line.contains("sz:") || line.contains("'sz'") || line.ends_with(": sz") {
        return Some(true);
    }
    if line.contains("rz:") || line.contains("'rz'") || line.ends_with(": rz") {
        return Some(false);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lrzsz_init(typ: u8) -> Vec<u8> {
        // 8 CAN 填充 + 2 CAN 框定 + 类型字节（ZRQINIT 走 ZDLE 转义）
        let mut v = vec![CAN; 10];
        match typ {
            0 => v.extend_from_slice(&[0x78]),
            1 => v.extend_from_slice(&[0x19, 0x00, 0x8a, 0xff, 0x0f]),
            2 => v.extend_from_slice(&[0x1a, 0x00, 0x8a]),
            _ => {}
        }
        v
    }

    #[test]
    fn detects_sz_with_filenames() {
        let mut w = Watcher::new();
        w.feed(b"root@host:~# sz hello.txt notes\r\n");
        let ev = w.feed(&lrzsz_init(0));
        assert_eq!(
            ev,
            Some(ZmodemEvent::Send {
                files: vec!["hello.txt".into(), "notes".into()]
            })
        );
    }

    #[test]
    fn detects_rz_as_receive() {
        let mut w = Watcher::new();
        w.feed(b"root@host:~# rz -be\r\n");
        let ev = w.feed(&lrzsz_init(1));
        assert_eq!(ev, Some(ZmodemEvent::Receive));
    }

    #[test]
    fn direction_from_header_without_echo() {
        let mut w = Watcher::new();
        assert_eq!(w.feed(&lrzsz_init(1)), Some(ZmodemEvent::Receive));
        let mut w2 = Watcher::new();
        assert_eq!(
            w2.feed(&lrzsz_init(2)),
            Some(ZmodemEvent::Send { files: vec![] })
        );
    }

    #[test]
    fn can_run_split_across_chunks() {
        let mut w = Watcher::new();
        assert_eq!(w.feed(&vec![CAN; 6]), None);
        assert_eq!(w.feed(&vec![CAN; 4]), None);
        // CAN 串跨块累积后，头字节 \x19 → ZRINIT → Receive
        assert_eq!(w.feed(b"\x19\x00"), Some(ZmodemEvent::Receive));
    }

    /// 重试的 ZRINIT 被 CAN 填充截成两段时，第一段以 "**" 开头即可命中
    #[test]
    fn hex_header_split_by_can_padding_still_fires() {
        let mut w = Watcher::new();
        w.feed(b"[root@x ~]# rz\r\n");
        let mut first = vec![CAN; 10];
        first.extend_from_slice(b"**B0");
        assert_eq!(w.feed(&first), Some(ZmodemEvent::Receive));
    }

    /// 没有 CAN 前缀、纯按行切断的两段十六进制头，靠拼接后的行标记命中
    #[test]
    fn hex_header_split_across_lines_concatenates() {
        let mut w = Watcher::new();
        assert_eq!(w.feed(b"**B0\r\n"), None);
        assert_eq!(w.feed(b"100000023be50\r\n"), Some(ZmodemEvent::Unknown));
    }

    /// CAN 串后紧跟 "**" 本身就是十六进制头的框定信号
    #[test]
    fn can_run_followed_by_double_star_fires() {
        let mut w = Watcher::new();
        let mut stream = vec![CAN; 10];
        stream.extend_from_slice(b"**B0100000023be50");
        assert_eq!(w.feed(&stream), Some(ZmodemEvent::Unknown));
    }

    /// 真实 sz 启动流：ZRQINIT 之后几毫秒内紧跟一个 ZRINIT 回帧，
    /// 方向必须按首帧判定（曾因此把 sz 误判成 rz 弹了上传框）
    #[test]
    fn sz_followed_by_zrinit_frame_stays_send() {
        let mut w = Watcher::new();
        w.feed(b"[root@x ~]# sz 20260323.xlsx\r\n");
        let mut stream = vec![CAN; 8];
        stream.extend_from_slice(&[0x18, 0x18, 0x78, 0x00, 0x00, 0x36, 0x7e]); // ZRQINIT
        stream.extend_from_slice(&[CAN; 8]);
        stream.extend_from_slice(&[0x18, 0x18, 0x19, 0x00, 0x8a, 0xff, 0x0f]); // ZRINIT 回帧
        assert_eq!(
            w.feed(&stream),
            Some(ZmodemEvent::Send {
                files: vec!["20260323.xlsx".into()]
            })
        );
    }

    /// 真实 lrzsz sz 发的是扩展头（type|0x40）：ZRQINIT = 0x58，
    /// 其后紧跟自己的 ZRINIT 回帧（0x19/0x59）。首帧必须认得 0x58，
    /// 否则 ZRINIT 会被当首帧误判成 rz。
    #[test]
    fn extended_zrqinit_header_is_send() {
        let mut w = Watcher::new();
        w.feed(b"[root@x ~]# sz 20260323.xlsx\r\n");
        let mut stream = vec![CAN; 8];
        stream.extend_from_slice(&[0x18, 0x18, 0x58, 0x00, 0x00, 0x36, 0x7e]); // 扩展 ZRQINIT
        stream.extend_from_slice(&[CAN; 8]);
        stream.extend_from_slice(&[0x18, 0x18, 0x59, 0x00, 0x8a, 0xff, 0x0f]); // ZRINIT 回帧
        assert_eq!(
            w.feed(&stream),
            Some(ZmodemEvent::Send {
                files: vec!["20260323.xlsx".into()]
            })
        );
    }

    /// 头帧完全不可解时，最近的 sz 命令回显也要定方向为下载
    #[test]
    fn sz_echo_outranks_zrinit_header() {
        let mut w = Watcher::new();
        w.feed(b"[root@x ~]# sz a.txt\r\n");
        let mut stream = vec![CAN; 8];
        stream.extend_from_slice(&[0x99, 0x00]); // 不可解码的首帧字节
        stream.extend_from_slice(&[CAN; 8]);
        stream.extend_from_slice(&[0x18, 0x18, 0x19, 0x00]); // ZRINIT
        assert_eq!(
            w.feed(&stream),
            Some(ZmodemEvent::Send {
                files: vec!["a.txt".into()]
            })
        );
    }

    /// 真机 x1 捕获：lrzsz 的 sz 启动时先打印兼容横幅 "rz\r"，再发十六进制
    /// ZRQINIT 头（** + CAN + B0…）。裸 "rz" 横幅行不得被当成命令回显，
    /// 否则方向会被误判成 Receive（曾导致 sz 弹上传框）。
    #[test]
    fn sz_rz_compat_banner_does_not_flip_direction() {
        let mut w = Watcher::new();
        w.feed("[root@demo-server ~]# sz 20260323部门.xlsx\r\n".as_bytes());
        let ev = w.feed(b"rz\r**\x18B0000000000000\r\x8a\x11");
        assert_eq!(
            ev,
            Some(ZmodemEvent::Send {
                files: vec!["20260323部门.xlsx".to_string()]
            })
        );
    }

    /// 裸 sz（回车时无文件参数）不发协议帧，回显本身就要触发文件浏览器
    #[test]
    fn bare_sz_echo_fires_send_with_empty_files() {
        let mut w = Watcher::new();
        assert_eq!(
            w.feed(b"[root@x ~]# sz\r\n"),
            Some(ZmodemEvent::Send { files: vec![] })
        );
    }

    /// 括号粘贴/TAB 补全产生的 ANSI 转义序列不得截断回显行，文件名要保住
    #[test]
    fn bracketed_paste_filename_survives_echo() {
        let mut w = Watcher::new();
        assert_eq!(
            w.feed("[root@x ~]# sz \x1b[200~20260323部门.xlsx\x1b[201~\r\n".as_bytes()),
            None
        );
        let mut stream = vec![CAN; 8];
        stream.extend_from_slice(&[0x18, 0x18, 0x58, 0x00]); // 扩展 ZRQINIT
        assert_eq!(
            w.feed(&stream),
            Some(ZmodemEvent::Send {
                files: vec!["20260323部门.xlsx".to_string()]
            })
        );
    }

    /// readline 退格编辑（"\b \b"）后回显行仍是完整命令
    #[test]
    fn backspace_editing_keeps_filename() {
        let mut w = Watcher::new();
        w.feed(b"[root@x ~]# sz fooo\x08 \x08\r\n");
        // 此刻回显是 "sz foo"（未带 \r 的第二段被 Enter 结束）
        let mut stream = vec![CAN; 8];
        stream.extend_from_slice(&[0x18, 0x18, 0x78, 0x00]); // ZRQINIT
        assert_eq!(
            w.feed(&stream),
            Some(ZmodemEvent::Send {
                files: vec!["foo".to_string()]
            })
        );
    }

    /// 真实服务器（x1, lrzsz）上输入 rz 后的原始流：
    /// CAN 填充 + "++rz waiting to receive." + 十六进制 ZRINIT 头
    #[test]
    fn detects_real_lrzsz_rz_banner() {
        let mut w = Watcher::new();
        w.feed(b"[root@demo-server ~]# rz\r\n");
        assert_eq!(w.feed(&vec![CAN; 10]), None);
        let ev = w.feed(b"++rz waiting to receive.**B0100000023be50");
        assert_eq!(ev, Some(ZmodemEvent::Receive));
    }

    #[test]
    fn detects_real_lrzsz_sz_banner() {
        let mut w = Watcher::new();
        w.feed(b"[root@x ~]# sz hello.txt\r\n");
        let mut stream = vec![CAN; 8];
        stream.extend_from_slice(b"++Sending hello.txt");
        assert_eq!(
            w.feed(&stream),
            Some(ZmodemEvent::Send {
                files: vec!["hello.txt".into()]
            })
        );
    }

    /// 没有 CAN 填充（或填充被分块吞掉）时，横幅文本本身也要能触发
    #[test]
    fn banner_text_alone_fires_even_without_newline() {
        let mut w = Watcher::new();
        let ev = w.feed(b"rz waiting to receive.**B0100000023be50");
        assert_eq!(ev, Some(ZmodemEvent::Receive));
        let mut w2 = Watcher::new();
        assert_eq!(
            w2.feed(b"**ZSEND 04 / 010600\r\n"),
            Some(ZmodemEvent::Send { files: vec![] })
        );
    }

    #[test]
    fn ordinary_binary_noise_does_not_trigger() {
        let mut w = Watcher::new();
        // 短 CAN 串或非头字节不触发
        assert_eq!(w.feed(&vec![CAN; 5]), None);
        assert_eq!(w.feed(b"random\xff\xfe"), None);
        // 10 个 CAN 但后随非头字节 → 不触发
        let mut noise = vec![CAN; 10];
        noise.extend_from_slice(b"zzz");
        assert_eq!(w.feed(&noise), None);
    }

    #[test]
    fn missing_command_reported() {
        let mut w = Watcher::new();
        let ev = w.feed(b"bash: sz: command not found\r\n");
        assert_eq!(ev, Some(ZmodemEvent::Missing { sz: true }));
        // 再次执行仍会提示（用户可能重试）
        let ev = w.feed(b"bash: sz: command not found\r\n");
        assert_eq!(ev, Some(ZmodemEvent::Missing { sz: true }));
    }

    #[test]
    fn take_commands_surfaces_cd() {
        let mut w = Watcher::new();
        w.feed(b"root@host:/tmp# cd /var/log\r\n");
        let cmds = w.take_commands();
        assert_eq!(cmds, vec![vec!["cd".to_string(), "/var/log".to_string()]]);
        assert!(w.take_commands().is_empty());
    }

    #[test]
    fn fires_only_once_until_reset() {
        let mut w = Watcher::new();
        assert!(w.feed(&lrzsz_init(1)).is_some());
        assert!(w.feed(&lrzsz_init(1)).is_none());
        w.reset();
        assert!(w.feed(&lrzsz_init(1)).is_some());
    }

    /// 真实 fake_sshd 捕获流回放（crates/ells/tests/capture_zmodem.py 生成）
    #[test]
    fn replays_fake_sshd_sz_capture() {
        let bytes = include_bytes!("../tests/zmodem_sz_capture.bin");
        let mut w = Watcher::new();
        assert_eq!(
            w.feed(bytes),
            Some(ZmodemEvent::Send {
                files: vec!["hello.txt".into()]
            })
        );
    }

    #[test]
    fn replays_fake_sshd_rz_capture() {
        let bytes = include_bytes!("../tests/zmodem_rz_capture.bin");
        let mut w = Watcher::new();
        assert_eq!(w.feed(bytes), Some(ZmodemEvent::Receive));
    }
}

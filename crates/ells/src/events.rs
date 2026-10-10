use crossterm::event::{Event as CrosstermEvent, KeyEvent, MouseEventKind};
use ells_core::ssh::{RemoteEvent, RemoteSession};
use ells_core::vault::VaultKey;
use ells_core::{Host, HostKeyPrompt, Probe, Vault, tunnel::TunnelEvent};
use ells_transfer::{FileEntry, Progress};
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::app::ConflictPrompt;

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Paste(String),
    Resize(u16, u16),
    /// Left-button press at a terminal cell (for header buttons / progress bar).
    MousePress { column: u16, row: u16 },
    /// Left-button drag (text selection in the embedded terminal).
    MouseDrag { column: u16, row: u16 },
    /// Left-button release (finish selection → copy).
    MouseRelease { column: u16, row: u16 },
    /// Mouse wheel: -1 = up, +1 = down (file browser / scrollback).
    MouseScroll { delta: i8 },
    /// 以下带 `slot` 的事件属于某个标签页（多会话）：标签可能已被关掉，
    /// 收到找不到对应 id 的事件就直接丢弃，绝不能落到"当前标签"上。
    RemoteData { slot: u32, bytes: Vec<u8> },
    /// `graceful`：这一场收得干净（远端退出、对端正常关通道、本地主动断），
    /// 区别于链路凭空没掉的异常掉线
    RemoteClosed { slot: u32, graceful: bool },
    /// 主机密钥待确认：由 UI 弹窗回答，连接任务在等待这个回答。
    HostKey(HostKeyPrompt),
    /// 后台连接（含认证与主机密钥确认）结束。
    Connected {
        slot: u32,
        res: std::result::Result<RemoteSession, String>,
    },
    /// 用户在"连接已断开"弹窗里点了重连。
    Reconnect { slot: u32, host: Host },
    /// 自动重连定时器到点。`seq` 是这一路的代号：期间用户 Ctrl-]、关标签或
    /// 自己重连过，代号就对不上，这条定时就作废（同 `ZmodemClear`）。
    AutoReconnect {
        slot: u32,
        host: Host,
        attempt: u32,
        seq: u64,
    },
    /// 会话页底部那排指标：5 秒定时器到点（磁盘那一路每 12 轮才真问一次）。同样带 `seq` —— 断开、关标签、
    /// 或设置里关掉指标都会顶掉代号，这条定时因此作废（不会出现"你以为停了、
    /// 它还在每条连接上开采集通道"）。
    MetricsTick { slot: u32, seq: u64 },
    /// 采集通道回来了。`Err` 是通道级失败（开不了通道、执行失败、超时），
    /// `Ok` 里哪项是 `None` 表示这台机器没给这项数据。
    MetricsProbe {
        slot: u32,
        seq: u64,
        res: std::result::Result<Probe, String>,
    },
    /// 传输目标已存在，等用户选择覆盖 / 改名 / 取消。
    Conflict { slot: u32, prompt: ConflictPrompt },
    /// 用户在导入确认框里点了"导入"（空列表 = 取消）。
    ImportHosts(Vec<Host>),
    /// 用户在导出确认框里点了"导出"（列表页 `x`）。同样不带 slot：
    /// 写的是本机 `~/.ells` 下的一个文件，与哪个标签无关。
    ExportSshConfig,
    /// 隧道状态变化（连接中 / 就绪 / 重连退避 / 失败 / 已停止）。不属于任何标签：
    /// 一台主机一条隧道，跨标签、跨会话存活，所以不带 slot。
    Tunnel(TunnelEvent),
    /// Result of the native file dialog: path selected for a form field
    /// (None when the dialog was cancelled).
    PickedFile { field: usize, path: Option<String> },
    /// Background argon2 unlock finished.
    VaultUnlocked(std::result::Result<(Vault, VaultKey), String>),
    /// Background vault creation finished.
    VaultCreated(std::result::Result<VaultKey, String>),
    /// 主密码修改完成：新密钥 + 新密码明文（供自动解锁凭据同步）。
    MasterRotated(std::result::Result<(VaultKey, String), String>),
    /// Remote working directory resolved (for sz/rz SFTP rerouting).
    SftpCwd {
        slot: u32,
        res: std::result::Result<String, String>,
    },
    /// End the ZMODEM output-swallow window. Carries a generation counter so
    /// stale timers from an earlier window are ignored (the window slides
    /// forward whenever more protocol bytes arrive).
    ZmodemClear { slot: u32, seq: u64 },
    /// Native dialog result for an SFTP upload (browser screen).
    PickedUpload { slot: u32, path: Option<String> },
    /// Native "Save As" dialog result for a download (sz interception flow).
    PickedSave {
        slot: u32,
        entry: FileEntry,
        path: Option<String>,
    },
    /// 目录上传：系统目录选择框的结果（递归上传整个目录）。
    PickedUploadDir { slot: u32, path: Option<String> },
    /// 目录下载：本地落点目录（sz 传目录 / 浏览器下载目录）。
    PickedSaveDir {
        slot: u32,
        entry: FileEntry,
        path: Option<String>,
    },
    /// 传输通过覆盖确认、真正开跑：UI 这时才登记进度条目。
    SftpStarted {
        slot: u32,
        label: String,
        direction: &'static str,
    },
    /// 远端改名/新建/删除完成（Ok 给状态行文案）。
    SftpOp { slot: u32, res: std::result::Result<String, String> },
    /// 历史搜索框的回答：None = 取消，Some("") = 清空即退出搜索。
    Search { slot: u32, value: Option<String> },
    /// Resolved remote home directory for the browser.
    SftpHome {
        slot: u32,
        res: std::result::Result<String, String>,
    },
    /// Directory listing for the browser.
    SftpListed {
        slot: u32,
        res: std::result::Result<Vec<FileEntry>, String>,
    },
    /// Progress tick of an in-flight transfer.
    SftpProgress { slot: u32, pr: Progress },
    /// A transfer finished (Err carries the failure message).
    SftpDone {
        slot: u32,
        res: std::result::Result<String, (String, String)>,
    },
    /// 自更新：后台检查完成。Ok(Some(tag)) = 有更新，Ok(None) = 已是最新。
    UpdateChecked(std::result::Result<Option<String>, String>),
    /// 用户在确认弹窗里点了【更 新】（回调够不着 App，只能再发一个事件回来）。
    UpdateAccepted,
    /// 自更新：下载进度（total 为 None 时服务端没给 Content-Length）。
    UpdateProgress { transferred: u64, total: Option<u64> },
    /// 自更新：下载 + 校验 + 替换的整体结果。
    UpdateDone(std::result::Result<(), String>),
}

pub fn spawn_input_stream(tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let mut stream = crossterm::event::EventStream::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(CrosstermEvent::Key(key)) => {
                    if tx.send(AppEvent::Key(key)).is_err() {
                        return;
                    }
                }
                Ok(CrosstermEvent::Paste(text)) => {
                    if tx.send(AppEvent::Paste(text)).is_err() {
                        return;
                    }
                }
                Ok(CrosstermEvent::Resize(cols, rows)) => {
                    if tx.send(AppEvent::Resize(cols, rows)).is_err() {
                        return;
                    }
                }
                Ok(CrosstermEvent::Mouse(m)) => {
                    let ev = match m.kind {
                        MouseEventKind::Down(_) => Some(AppEvent::MousePress {
                            column: m.column,
                            row: m.row,
                        }),
                        MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                            Some(AppEvent::MouseDrag {
                                column: m.column,
                                row: m.row,
                            })
                        }
                        MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                            Some(AppEvent::MouseRelease {
                                column: m.column,
                                row: m.row,
                            })
                        }
                        MouseEventKind::ScrollUp => Some(AppEvent::MouseScroll { delta: -1 }),
                        MouseEventKind::ScrollDown => Some(AppEvent::MouseScroll { delta: 1 }),
                        _ => None,
                    };
                    if let Some(ev) = ev {
                        if tx.send(ev).is_err() {
                            return;
                        }
                    }
                }
                Ok(CrosstermEvent::FocusGained) | Ok(CrosstermEvent::FocusLost) => {}
                Err(_) => {}
            }
        }
    });
}

/// 把 SSH 通道的输出灌进唯一的 UI 事件队列，主循环只 select 一个接收端。
/// `slot` 给每条事件盖上所属标签的 id：后台标签的输出绝不能画进用户正在看的标签。
pub fn spawn_remote_pump(
    mut rx: mpsc::UnboundedReceiver<RemoteEvent>,
    tx: mpsc::UnboundedSender<AppEvent>,
    slot: u32,
) {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let app_ev = match ev {
                RemoteEvent::Data(bytes) => AppEvent::RemoteData { slot, bytes },
                RemoteEvent::Closed { graceful } => AppEvent::RemoteClosed { slot, graceful },
            };
            if tx.send(app_ev).is_err() {
                return;
            }
        }
    });
}

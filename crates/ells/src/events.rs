use crossterm::event::{Event as CrosstermEvent, KeyEvent, MouseEventKind};
use ells_core::ssh::{RemoteEvent, RemoteSession};
use ells_core::vault::VaultKey;
use ells_core::{Host, HostKeyPrompt, Vault};
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
    RemoteData(Vec<u8>),
    RemoteClosed,
    /// 主机密钥待确认：由 UI 弹窗回答，连接任务在等待这个回答。
    HostKey(HostKeyPrompt),
    /// 后台连接（含认证与主机密钥确认）结束。
    Connected(std::result::Result<RemoteSession, String>),
    /// 传输目标已存在，等用户选择覆盖 / 改名 / 取消。
    Conflict(ConflictPrompt),
    /// 用户在导入确认框里点了"导入"（空列表 = 取消）。
    ImportHosts(Vec<Host>),
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
    SftpCwd(std::result::Result<String, String>),
    /// End the ZMODEM output-swallow window. Carries a generation counter so
    /// stale timers from an earlier window are ignored (the window slides
    /// forward whenever more protocol bytes arrive).
    ZmodemClear(u64),
    /// Native dialog result for an SFTP upload (browser screen).
    PickedUpload(Option<String>),
    /// Native "Save As" dialog result for a download (sz interception flow).
    PickedSave { entry: FileEntry, path: Option<String> },
    /// 目录上传：系统目录选择框的结果（递归上传整个目录）。
    PickedUploadDir(Option<String>),
    /// 目录下载：本地落点目录（sz 传目录 / 浏览器下载目录）。
    PickedSaveDir { entry: FileEntry, path: Option<String> },
    /// 传输通过覆盖确认、真正开跑：UI 这时才登记进度条目。
    SftpStarted { label: String, direction: &'static str },
    /// 远端改名/新建/删除完成（Ok 给状态行文案）。
    SftpOp(std::result::Result<String, String>),
    /// Resolved remote home directory for the browser.
    SftpHome(std::result::Result<String, String>),
    /// Directory listing for the browser.
    SftpListed(std::result::Result<Vec<FileEntry>, String>),
    /// Progress tick of an in-flight transfer.
    SftpProgress(Progress),
    /// A transfer finished (Err carries the failure message).
    SftpDone(std::result::Result<String, (String, String)>),
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

/// Pump SSH channel output into the single UI event queue so the main loop
/// only ever selects on one receiver.
pub fn spawn_remote_pump(mut rx: mpsc::UnboundedReceiver<RemoteEvent>, tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let app_ev = match ev {
                RemoteEvent::Data(bytes) => AppEvent::RemoteData(bytes),
                RemoteEvent::Closed(res) => {
                    if let Err(err) = res {
                        tracing::warn!(%err, "remote session error");
                    }
                    AppEvent::RemoteClosed
                }
            };
            if tx.send(app_ev).is_err() {
                return;
            }
        }
    });
}

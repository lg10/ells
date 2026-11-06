use std::path::PathBuf;

use crossterm::event::{
    Event as CrosstermEvent, KeyEvent, KeyModifiers, MouseButton as CrosstermMouseButton,
    MouseEventKind,
};
use ells_core::ssh::{RemoteEvent, RemoteSession};
use ells_core::vault::VaultKey;
use ells_core::{Host, HostKeyPrompt, Probe, Vault, tunnel::TunnelEvent};
use ells_term::MouseButton;
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
    /// 远端应用开了鼠标报告时，这个事件也会被转成它要的报文（含右键/中键）。
    MousePress {
        column: u16,
        row: u16,
        button: MouseButton,
        modifiers: KeyModifiers,
    },
    /// Button drag (text selection in the embedded terminal, or remote motion).
    MouseDrag {
        column: u16,
        row: u16,
        button: MouseButton,
        modifiers: KeyModifiers,
    },
    /// Button release (finish selection → copy, or remote release).
    MouseRelease {
        column: u16,
        row: u16,
        button: MouseButton,
        modifiers: KeyModifiers,
    },
    /// Mouse wheel: -1 = up, +1 = down (file browser / scrollback / remote app).
    MouseScroll {
        column: u16,
        row: u16,
        delta: i8,
        modifiers: KeyModifiers,
    },
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
    /// 解锁忙帧：只为了重画那根进度条。`seq` 是这一轮忙态的代号，忙态一结束就对不上，
    /// 在途的那一帧因此作废（不会有人停在列表页上还接着收定时）。
    UnlockTick { seq: u64 },
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
    /// 远端文件已取回到本地临时文件，等待编辑（内置面板或外部编辑器）。
    EditReady {
        slot: u32,
        remote: String,
        local: PathBuf,
        /// 下载那一刻远端的 mtime（Unix 秒），回传前用它判断远端有没有被改过。
        remote_mtime: Option<u32>,
        /// true = 用 ells 内置面板，不需要拉起外部程序，也不启动文件监视。
        embedded: bool,
    },
    /// 取回 / 打开编辑器这一步失败了（下载、二进制检查、拉起编辑器）。
    EditFailed { slot: u32, err: String },
    /// 本地临时文件被编辑器保存了（已做去抖，不会把半截写入当成一次保存）。
    EditSaved {
        slot: u32,
        remote: String,
        local: PathBuf,
    },
    /// 远端文件在编辑期间被改过：要不要覆盖。
    EditConflict {
        slot: u32,
        remote: String,
        local: PathBuf,
        remote_mtime: Option<u32>,
        current_mtime: Option<u32>,
    },
    /// 回传结束。`Ok(Some(mtime))` 是写回后远端的新 mtime（下一轮冲突检测的基线）。
    EditUploaded {
        slot: u32,
        remote: String,
        local: PathBuf,
        res: std::result::Result<Option<u32>, String>,
    },
    /// 编辑会话长时间没有保存，监视任务收工（临时文件仍留在磁盘上）。
    EditExpired { slot: u32, remote: String },
    /// 终端里手敲的 vi 命令解析成功（文件存在、可读、大小合适）：
    /// UI 这时才清掉远端输入行并开始取回。
    ViTargetResolved {
        slot: u32,
        seq: u64,
        remote: String,
        name: String,
    },
    /// 解析失败（不存在 / 是目录 / 太大 / SFTP 出错 / 超时）：不接管，
    /// 把回车补发给远端，照旧用远端 vi 打开。`reason` 写进状态行，别再静默。
    ViTargetFailed { slot: u32, seq: u64, reason: String },
    /// 输入行里的文件名用 SFTP 补全完成。`append` 是要补进远端和镜像的字符；
    /// `None` 表示补不了（没有匹配 / 多个候选 / 出错），`reason` 说明原因。
    ViCompleted {
        slot: u32,
        seq: u64,
        append: Option<String>,
        reason: String,
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
    /// `rz`：这一笔就是被挂起的脚本 `rz` 在等的那次上传（无论成败都要放行远端）。
    SftpDone {
        slot: u32,
        rz: bool,
        res: std::result::Result<String, (String, String)>,
    },
    /// 容器视图的一轮查询（Socket 直连或 docker ps 兜底）回来了。
    /// `seq` 同其它定时回包：关掉视图、切 all、再刷新都会顶掉旧的一包。
    DockerContainers {
        slot: u32,
        seq: u64,
        res: std::result::Result<Vec<ells_core::docker::DockerContainer>, String>,
    },
    /// 启动/停止/重启/删除等动作在远端执行完了。`name` 只用于状态行提示。
    DockerAction {
        slot: u32,
        action: ells_core::docker::ContainerAction,
        name: String,
        res: std::result::Result<(), String>,
    },
    /// 容器详情（inspect + 一次性资源占用）回来了。`seq` 顶掉关掉/换容器前的旧回包。
    DockerDetail {
        slot: u32,
        seq: u64,
        res: std::result::Result<ells_core::docker::ContainerDetail, String>,
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

fn mouse_button(button: CrosstermMouseButton) -> MouseButton {
    match button {
        CrosstermMouseButton::Left => MouseButton::Left,
        CrosstermMouseButton::Right => MouseButton::Right,
        CrosstermMouseButton::Middle => MouseButton::Middle,
    }
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
                        MouseEventKind::Down(b) => Some(AppEvent::MousePress {
                            column: m.column,
                            row: m.row,
                            button: mouse_button(b),
                            modifiers: m.modifiers,
                        }),
                        MouseEventKind::Drag(b) => Some(AppEvent::MouseDrag {
                            column: m.column,
                            row: m.row,
                            button: mouse_button(b),
                            modifiers: m.modifiers,
                        }),
                        MouseEventKind::Up(b) => Some(AppEvent::MouseRelease {
                            column: m.column,
                            row: m.row,
                            button: mouse_button(b),
                            modifiers: m.modifiers,
                        }),
                        MouseEventKind::ScrollUp => Some(AppEvent::MouseScroll {
                            column: m.column,
                            row: m.row,
                            delta: -1,
                            modifiers: m.modifiers,
                        }),
                        MouseEventKind::ScrollDown => Some(AppEvent::MouseScroll {
                            column: m.column,
                            row: m.row,
                            delta: 1,
                            modifiers: m.modifiers,
                        }),
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

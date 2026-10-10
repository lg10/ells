use anyhow::{anyhow, bail, Context, Result};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::host::{Auth, Host};
use crate::hostkey::HostKeyPolicy;

/// 通道是怎么停下来的。只有后两种才算掉线。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Stop {
    /// 还在跑（不该出现在收尾里，留着让 default 安全）
    #[default]
    Running,
    /// 对端发来 CHANNEL_EOF / CHANNEL_CLOSE：它在道别
    PeerClosed,
    /// 本地主动断开：Ctrl-]、关标签、退出程序
    Local,
    /// 通道凭空消失：TCP 死亡、keepalive 超时、服务端进程被杀
    Vanished,
    /// 往已断的通道里写按键
    Broken,
}

/// 一条会话通道的收尾依据。
#[derive(Debug, Clone, Copy, Default)]
struct SessionEnd {
    /// 收到过远端的 exit-status：shell 自己退出的硬证据
    exit_status: bool,
    stop: Stop,
}

impl SessionEnd {
    /// 算不算"正常结束"（不弹重连、不自动重连）。
    ///
    /// 为什么光看 exit-status 不够：跳板机/网关这类服务端在 `logout` 之后直接关通道，
    /// 一个 exit-status 都不发，于是用户敲了 exit 也被判成掉线、被追问要不要重连。
    /// 反过来也不能把 `None` 算正常：russh 收到对端的 CHANNEL_CLOSE 时会先把消息转给
    /// 等待中的通道再摘除它，所以"对端道别"和"链路凭空断了"分得开。
    fn is_graceful(self) -> bool {
        self.exit_status || matches!(self.stop, Stop::PeerClosed | Stop::Local)
    }
}

/// Events streamed back from the remote SSH channel to the UI.
#[derive(Debug)]
pub enum RemoteEvent {
    Data(Vec<u8>),
    /// 通道结束。`graceful = true` 表示这一场是**谁主动收尾**能解释得通的：远端 shell 退了、
    /// 对端正常关了通道、或本地按 Ctrl-] 断的；只有通道凭空消失/写不进去才算掉线，
    /// 那种情况才该走重连。
    Closed { graceful: bool },
}

/// 空闲保活间隔（秒），全局可调（ells 设置弹窗），默认 30s。
static KEEPALIVE_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(30);

pub fn set_keepalive_interval(secs: u64) {
    KEEPALIVE_SECS.store(secs.max(5), std::sync::atomic::Ordering::Relaxed);
}

pub fn keepalive_interval() -> u64 {
    KEEPALIVE_SECS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Input submitted to the remote side (keystrokes, paste, emulator answers).
pub enum SessionInput {
    Bytes(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

pub struct RemoteSession {
    input_tx: mpsc::UnboundedSender<SessionInput>,
    output_rx: Option<mpsc::UnboundedReceiver<RemoteEvent>>,
    sftp: Option<Arc<russh_sftp::client::SftpSession>>,
    /// 留着这条连接的句柄：会话页底部那排指标要在**同一条**连接上另开一条 exec 通道，
    /// 绝不能为了一排进度条再走一遍 TCP + 握手 + 认证（那才是真会把服务器打穿的做法）。
    /// 连接关掉时置空，免得在一条已死的连接上继续要通道。
    /// 用 `Arc` 包着是因为 russh 的 `Handle` 自己不是 `Clone`，而采集任务要 'static：
    /// 共享的是同一条控制通道，`clone` 一次引用计数，没有任何网络往返。
    handle: Option<Arc<russh::client::Handle<Handler>>>,
    /// 磁盘那一路的缓存（跟单哪块盘 + 这台问不问得动 statvfs），住在连接上而不是
    /// 采集任务里：任务每轮摘一份新句柄，缓存必须跨轮活着。见 `metrics::DiskState`。
    disk: Arc<std::sync::Mutex<crate::metrics::DiskState>>,
    alive: bool,
}

// AppEvent 派生了 Debug，因此这里必须可实现；通道本身没有意义且不可打印。
impl std::fmt::Debug for RemoteSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSession")
            .field("sftp", &self.sftp.is_some())
            .field("alive", &self.alive)
            .finish_non_exhaustive()
    }
}

/// russh 回调：唯一的职责是把主机密钥交给 `HostKeyPolicy` 判定。
/// 旧的实现无条件 `Ok(true)`，任何中间人都能透明接管连接。
pub struct Handler {
    host: String,
    port: u16,
    policy: HostKeyPolicy,
}

impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handler")
            .field("host", &self.host)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl Handler {
    fn new(host: &str, port: u16, policy: &HostKeyPolicy) -> Self {
        Self {
            host: host.to_string(),
            port,
            policy: policy.clone(),
        }
    }
}

impl russh::client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let key = server_public_key.public_key();
        // 拒绝 = russh 中断握手；具体原因（首次待确认 / 密钥变更）已由 UI 弹窗告知用户
        Ok(self.policy.verify(&self.host, self.port, &key).await)
    }
}

impl RemoteSession {
    pub async fn connect(
        host: &Host,
        vault: &crate::Vault,
        cols: u16,
        rows: u16,
        policy: &HostKeyPolicy,
    ) -> Result<Self> {
        let mut visited = Vec::new();
        let handle = connect_handle(host, vault, &mut visited, policy).await?;

        // Open the SFTP subsystem first; a server without it must not block
        // the shell session.
        let sftp = async {
            let ch = handle.channel_open_session().await.ok()?;
            ch.request_subsystem(true, "sftp").await.ok()?;
            russh_sftp::client::SftpSession::new(ch.into_stream())
                .await
                .map(Arc::new)
                .ok()
        }
        .await;
        if sftp.is_none() {
            tracing::warn!("服务器不支持 SFTP 子系统，文件传输不可用");
        }

        let channel = handle
            .channel_open_session()
            .await
            .context("打开会话通道失败")?;

        channel
            .request_pty(true, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
            .await
            .context("申请伪终端(PTY)失败")?;
        channel
            .request_shell(true)
            .await
            .context("申请 shell 失败")?;

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<SessionInput>();
        let (output_tx, output_rx) = mpsc::unbounded_channel::<RemoteEvent>();

        let writer = channel.make_writer();
        let mut writer = Box::pin(writer);
        let mut channel = channel;

        tokio::spawn(async move {
            let mut end = SessionEnd::default();
            'channel: loop {
                tokio::select! {
                    msg = channel.wait() => {
                        match msg {
                            Some(russh::ChannelMsg::Data { data }) => {
                                if output_tx.send(RemoteEvent::Data(data.to_vec())).is_err() {
                                    // UI 侧已经不听这路了，按本地收尾处理
                                    end.stop = Stop::Local;
                                    break 'channel;
                                }
                            }
                            Some(russh::ChannelMsg::ExtendedData { data, .. }) => {
                                if output_tx.send(RemoteEvent::Data(data.to_vec())).is_err() {
                                    end.stop = Stop::Local;
                                    break 'channel;
                                }
                            }
                            Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                                // 记下来但**不**立刻收场：`logout` 之类的收尾输出可能还在后面
                                end.exit_status = true;
                                tracing::debug!("remote shell exited with status {exit_status}");
                            }
                            Some(russh::ChannelMsg::Eof)
                            | Some(russh::ChannelMsg::Close) => {
                                end.stop = Stop::PeerClosed;
                                break 'channel;
                            }
                            None => {
                                end.stop = Stop::Vanished;
                                break 'channel;
                            }
                            Some(_) => {}
                        }
                    }
                    Some(input) = input_rx.recv() => {
                        match input {
                            SessionInput::Bytes(bytes) => {
                                if writer.write_all(&bytes).await.is_err() {
                                    end.stop = Stop::Broken;
                                    break 'channel;
                                }
                            }
                            SessionInput::Resize { cols, rows } => {
                                let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
                            }
                            SessionInput::Close => {
                                end.stop = Stop::Local;
                                break 'channel;
                            }
                        }
                    }
                }
            }
            let _ = channel.close().await;
            let graceful = end.is_graceful();
            tracing::debug!("会话通道收尾：{end:?}，graceful={graceful}");
            let _ = output_tx.send(RemoteEvent::Closed { graceful });
        });

        Ok(Self {
            input_tx,
            output_rx: Some(output_rx),
            sftp,
            handle: Some(Arc::new(handle)),
            // 每条连接自己一份：换了一条会话，上一台的挂载点缓存就该跟着没了，
            // 否则第一波跟单会把上一台机器最满那块盘当成这台的。
            disk: Arc::default(),
            alive: true,
        })
    }

    /// Shared SFTP session handle, or None if the server lacks the subsystem.
    pub fn sftp(&self) -> Option<Arc<russh_sftp::client::SftpSession>> {
        self.sftp.clone()
    }

    /// 摘一份采集句柄（同一条连接、可跨任务移动）：句柄 + 已有的 SFTP 会话 + 磁盘缓存。
    /// 连接已关给 `None`：这时候该停止轮询，而不是在一条死连接上要通道。
    pub fn probe_target(&self) -> Option<ProbeTarget> {
        let handle = self.handle.clone()?;
        Some(ProbeTarget {
            handle,
            sftp: self.sftp.clone(),
            disk: self.disk.clone(),
        })
    }

    pub fn is_alive(&self) -> bool {
        self.alive
    }

    pub fn take_output(&mut self) -> Option<mpsc::UnboundedReceiver<RemoteEvent>> {
        self.output_rx.take()
    }

    pub fn write_input(&self, bytes: Vec<u8>) {
        let _ = self.input_tx.send(SessionInput::Bytes(bytes));
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.input_tx.send(SessionInput::Resize { cols, rows });
    }

    pub fn close(&mut self) {
        self.alive = false;
        // 句柄一起放手：连接都要关了，不该再往上开采集通道。
        // 读循环是 russh 自己 spawn 的，这里掉最后一个句柄不会提前打断连接。
        self.handle = None;
        let _ = self.input_tx.send(SessionInput::Close);
    }
}

/// 采集用的连接凭据：那条已认证连接的共享句柄，加上同一条连接上已经开好的 SFTP 会话。
///
/// 为什么要单独一个类型而不是把 `&RemoteSession` 交给任务：会话结构体住在标签页里、
/// 主循环每帧都在借它画图，任务借不到（要 'static）。而 russh 的 `Handle` 自己不是
/// `Clone`，所以这里共享的是同一个 `Arc`：`clone` 只加一次引用计数，没有任何网络往返，
/// 更不会为了一排进度条重做 TCP + 握手 + 认证。
#[derive(Clone)]
pub struct ProbeTarget {
    handle: Arc<russh::client::Handle<Handler>>,
    /// 可能为 `None`（服务器不给 sftp subsystem），也可能正和文件传输面板共用着。
    /// 采集在它上面只是几个小包，不抢传输的顺序、也不另开一条会话。
    sftp: Option<Arc<russh_sftp::client::SftpSession>>,
    /// 磁盘那一路的缓存，和这条连接同生共死：`clone` 同样只加一次引用计数。
    disk: Arc<std::sync::Mutex<crate::metrics::DiskState>>,
}

/// 这一轮的磁盘腿对缓存说了什么。
struct DiskUpdate {
    /// 下一轮跟单哪个挂载点；`None` 表示"下一轮先普查"。
    follow: Option<String>,
    /// 只在真问到东西时才带态度回来：`None` 意思是"这轮没改变看法"。
    statvfs_supported: Option<bool>,
}

impl ProbeTarget {
    /// 一次采集：SFTP 优先，没凑齐的那几格再用一条一次性 exec 补。
    ///
    /// 磁盘有两个开关，分开才有意义：
    /// - `want_disks`：这一轮要不要拿到磁盘数。跟单轮（只对缓存里最满那块问一次
    ///   `statvfs`）也要 true，否则界面上那一格 5 秒一档就没了。
    /// - `want_survey`：这一轮要不要**普查**（重读 `/proc/mounts` + 逐块 `statvfs`）。
    ///   界面上 60 秒一次，由调用方数轮次。
    ///
    /// 兜底的 `df` **只在普查轮**允许回落：一台没有 `statvfs@openssh.com` 的服务器，
    /// 跟单轮如果也能回落，就等于每 5 秒 fork 一个 shell 跑 `df` —— 那正是这套 SFTP
    /// 主路径要避免的东西。跟单轮问不到就沿用上一轮的数（那一格本来就变得慢）。
    ///
    /// 三条红线都守在这里：不另开连接（用这条已有连接上的 SFTP 会话，或另开一条
    /// exec 通道）、不碰交互 PTY、远端不留常驻进程（每轮都是一次性请求，读完就完）。
    pub async fn gather(
        &self,
        timeout: std::time::Duration,
        max_bytes: usize,
        want_disks: bool,
        want_survey: bool,
    ) -> Result<crate::metrics::Probe> {
        use crate::metrics;
        let deadline = tokio::time::Instant::now() + timeout;
        // 锁只护住"这一轮该问谁"这个决定，守卫绝不跨过 await：同时开着的标签页各自
        // 有各自的缓存，但同一台机器上重复摘句柄时，抱着守卫等一条慢 SFTP 会把别人钉住。
        let plan = if want_disks {
            let state = self.disk.lock().unwrap_or_else(|e| e.into_inner());
            metrics::plan_disks(want_survey, state.follow.as_deref(), state.statvfs_supported)
        } else {
            metrics::DiskPlan::Skip
        };
        let mut probe = match &self.sftp {
            Some(sftp) => {
                let (probe, update) = gather_over_sftp(sftp, deadline, max_bytes, plan).await;
                if let Some(update) = update {
                    let mut state = self.disk.lock().unwrap_or_else(|e| e.into_inner());
                    state.follow = update.follow;
                    if update.statvfs_supported.is_some() {
                        state.statvfs_supported = update.statvfs_supported;
                    }
                }
                probe
            }
            // 没有 SFTP 会话的服务器只能走 exec，和以前一样
            None => metrics::Probe::default(),
        };
        // 只有"这一轮本来就在普查、却没普查出任何磁盘数"才值得补一轮一次性 exec：
        // 读不到 /proc/mounts、或者服务器对 statvfs 爱答不理，都归这一条。
        let disk_missing = want_disks && want_survey && probe.disks.is_empty();
        if probe.has_data() && !disk_missing {
            return Ok(probe);
        }
        let Some(rest) = budget(deadline) else {
            return into_result(probe, anyhow!("采集超时（SFTP 那一路没在预算里读完）"));
        };
        match self.probe(crate::metrics::PROBE_COMMAND, rest, max_bytes).await {
            Ok(text) => probe.fill_gaps(&crate::metrics::Probe::parse(&text)),
            // exec 也不通，但 SFTP 已经读到过东西：那一格照旧画，缺的显示 —
            Err(_) if probe.has_data() => {}
            Err(e) => return Err(e),
        }
        into_result(probe, anyhow!("这台主机既读不到 /proc，也拿不到磁盘"))
    }

    /// 在这条已认证的连接上跑一条一次性命令，收完标准输出返回。
    ///
    /// 不开 PTY：要的是能解析的纯文本，不是 ANSI；也不会碰交互 shell 那一格，
    /// 用户在敲的东西一个字都不受影响。
    ///
    /// `timeout` 是必需的而非好看：远端 `df` 卡在失效的 NFS 挂载上是真实存在的场景，
    /// 卡住的这一轮要被丢掉、通道要被关掉，不能把这一格钉死、更不能把后面的采集堵住。
    /// `max_bytes` 防的是把登录横幅写成几 MB 的那类机器。
    pub async fn probe(
        &self,
        command: &str,
        timeout: std::time::Duration,
        max_bytes: usize,
    ) -> Result<String> {
        handle_probe(&self.handle, command, timeout, max_bytes).await
    }
}

/// 凑到数据才算成功：一个数都没有就报错，让界面那句状态话说明"这台采不到"。
fn into_result(probe: crate::metrics::Probe, why: anyhow::Error) -> Result<crate::metrics::Probe> {
    if probe.has_data() {
        Ok(probe)
    } else {
        Err(why)
    }
}

/// 这一轮还剩多少预算；已经过点了给 `None`，让调用方丢掉这一轮而不是再来一次往返。
fn budget(deadline: tokio::time::Instant) -> Option<std::time::Duration> {
    let now = tokio::time::Instant::now();
    (deadline > now).then(|| deadline - now)
}

/// 一个挂载点的 `statvfs` 结果。
enum FsInfo {
    Stats(crate::metrics::FsStats),
    /// 这一块问失败（权限、已经卸载、挂载点没了）：接着问下一块。
    Failed,
    /// 服务器没有 `statvfs@openssh.com` 扩展：再问几块也只是浪费往返。
    Unsupported,
}

/// 主路径：`/proc` 用 SFTP 读文件，磁盘按 `plan` 走（普查 / 跟单一块 / 什么都不问）。
///
/// 为什么值得为它多写一层：exec 每一轮都要远端 fork 一个 shell、再 fork 一条 `df`
/// （每个开着的标签页都是 5 秒一轮），而这里只是几个请求包；更要紧的是有些机器
/// 根本不让 exec（forced-command、shell 是 nologin），那种机器上 SFTP 通着就能看见
/// 内存和 CPU。
///
/// 反过来磁盘只问 `mount_candidates` 筛过的**本地**文件系统：远端 sftp-server 是
/// 单线程的，一次卡在失效 NFS 挂载点上的 `statvfs` 会把整条 SFTP 会话一起拖住，
/// 而文件传输面板用的就是这一条。每一次调用都在这轮剩余的预算里做超时。
///
/// 返回的 `DiskUpdate` 是给缓存写回去用的；`None` 表示这一轮压根没走磁盘腿（没什么可说的）。
async fn gather_over_sftp(
    sftp: &russh_sftp::client::SftpSession,
    deadline: tokio::time::Instant,
    max_bytes: usize,
    plan: crate::metrics::DiskPlan,
) -> (crate::metrics::Probe, Option<DiskUpdate>) {
    use crate::metrics::{self, DiskPlan};
    let mut probe = metrics::Probe::default();
    // /proc 这几份又小又便宜，每轮都读；磁盘那一路按 plan 走，跟单轮只有 1 个往返。
    probe.cpu = read_proc(sftp, "/proc/stat", deadline, max_bytes)
        .await
        .and_then(|text| metrics::cpu_from_stat(&text));
    probe.mem = read_proc(sftp, "/proc/meminfo", deadline, max_bytes)
        .await
        .and_then(|text| metrics::mem_from_meminfo(&text));
    probe.load = read_proc(sftp, "/proc/loadavg", deadline, max_bytes)
        .await
        .and_then(|text| metrics::load_from_loadavg(&text));

    let mut disks = Vec::new();
    let mut update = None;
    match plan {
        // 普查：重读挂载点表、逐块问一遍，重新决定"哪块最满"，下一轮就跟单它。
        DiskPlan::Survey => {
            let mut unsupported = false;
            if let Some(mounts) = read_proc(sftp, "/proc/mounts", deadline, max_bytes).await {
                for candidate in metrics::mount_candidates(&mounts) {
                    match fs_info_of(sftp, &candidate.mount, deadline).await {
                        FsInfo::Stats(stats) => {
                            if let Some(disk) = metrics::disk_from_fs_stats(
                                candidate.device,
                                candidate.mount,
                                &stats,
                            ) {
                                disks.push(disk);
                            }
                        }
                        // 这一块问失败（已经卸载、权限不够）：接着问下一块。
                        FsInfo::Failed => continue,
                        // 没有这个扩展，再问几块也只是浪费往返。
                        FsInfo::Unsupported => {
                            unsupported = true;
                            break;
                        }
                    }
                }
            }
            let ranked = metrics::rank_disks(disks);
            // 跟单目标取实测最满那块；一块都没问到时留 None，下一轮重新普查（自愈）。
            update = Some(DiskUpdate {
                follow: ranked.first().map(|d| d.mount.clone()),
                // 明确不支持，或者这一轮问不到任何东西，都算"跟单没意义"：留着 None
                // 的话下一轮又会去撞第一次 fs_info，等于每 5 秒普查一次。
                statvfs_supported: if unsupported || ranked.is_empty() {
                    Some(false)
                } else {
                    Some(true)
                },
            });
            probe.disks = ranked;
        }
        // 跟单：只对缓存里那块问一次。问到了就把同一个挂载点续上；问不到就交回普查，
        // 但**这一轮绝不回落成 df**（回落只归普查轮，见 `gather`）。
        DiskPlan::Follow(mount) => {
            let (follow, attitude) = match fs_info_of(sftp, &mount, deadline).await {
                FsInfo::Stats(stats) => {
                    // 跟单的这一轮只有一块盘，`device` 就留空：界面上那一格写的是
                    // 挂载点 + 用量，设备名只有普查那轮才带着走。
                    match metrics::disk_from_fs_stats(String::new(), mount.clone(), &stats) {
                        Some(disk) => {
                            disks.push(disk);
                            (Some(mount), Some(true))
                        }
                        // 数算不出来（块数为 0 之类的怪东西）：这块不值得继续跟，交回普查。
                        None => (None, Some(true)),
                    }
                }
                // 瞬时问不到（卸载了、权限、超时）：下一轮重做普查，让它自愈。
                FsInfo::Failed => (None, None),
                FsInfo::Unsupported => (None, Some(false)),
            };
            update = Some(DiskUpdate { follow, statvfs_supported: attitude });
            probe.disks = metrics::rank_disks(disks);
        }
        // 背景轮 / 这台问不动：一个磁盘请求都不发，那一格沿用上次的数。
        DiskPlan::Skip => {}
    }
    probe.alive = probe.has_data();
    (probe, update)
}

/// 读一个远端小文件（`/proc` 那些）。读不到一律 `None`：这不叫失败 —— FreeBSD
/// 和奇怪的容器里根本没有 `/proc`，ells 对"这台机器不给这个数"的态度是画 `—`。
async fn read_proc(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
    deadline: tokio::time::Instant,
    max_bytes: usize,
) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::time::timeout(budget(deadline)?, sftp.open(path))
        .await
        .ok()?
        .ok()?;
    let mut buf: Vec<u8> = Vec::new();
    // 上限是真要守的，但也不必宽：要的东西都在文件头几行（cpu 汇总行、
    // MemTotal/MemAvailable、loadavg），16 KiB 连几百核机器的 /proc/stat 头部都够读。
    // 读到离谱长度只可能是这台机器把那个路径映射成了别的东西，那种数据宁可不要。
    let cap = max_bytes.min(16 * 1024) as u64;
    let mut capped = (&mut file).take(cap);
    if tokio::time::timeout(budget(deadline)?, capped.read_to_end(&mut buf))
        .await
        .ok()?
        .is_err()
    {
        return None;
    }
    // 句柄要关掉：sftp-server 的句柄表有上限，每 5 秒泄四个迟早把传输面板挤掉。
    if let Some(rest) = budget(deadline) {
        let _ = tokio::time::timeout(rest, file.close()).await;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// 问一个挂载点的块数。路径直接交给服务端解释，所以只传绝对挂载点（挑候选时验过）。
async fn fs_info_of(
    sftp: &russh_sftp::client::SftpSession,
    mount: &str,
    deadline: tokio::time::Instant,
) -> FsInfo {
    let Some(rest) = budget(deadline) else {
        return FsInfo::Failed;
    };
    let Ok(Ok(info)) = tokio::time::timeout(rest, sftp.fs_info(mount)).await else {
        return FsInfo::Failed;
    };
    match info {
        Some(v) => FsInfo::Stats(crate::metrics::FsStats {
            block_size: v.block_size,
            fragment_size: v.fragment_size,
            blocks: v.blocks,
            blocks_free: v.blocks_free,
            blocks_avail: v.blocks_avail,
        }),
        None => FsInfo::Unsupported,
    }
}

async fn handle_probe(
    handle: &russh::client::Handle<Handler>,
    command: &str,
    timeout: std::time::Duration,
    max_bytes: usize,
) -> Result<String> {
    let channel = handle
        .channel_open_session()
        .await
        .context("打开采集通道失败")?;
    channel.exec(true, command).await.context("采集命令执行失败")?;
    let mut channel = channel;
    let text = tokio::time::timeout(timeout, async move {
        let mut buf: Vec<u8> = Vec::new();
        while buf.len() < max_bytes {
            match channel.wait().await {
                Some(russh::ChannelMsg::Data { data }) => buf.extend_from_slice(&data),
                Some(russh::ChannelMsg::Eof) | Some(russh::ChannelMsg::Close) | None => break,
                // stderr 和退出码都不参与：真采集不到东西，解析那边会认成"没有心跳"
                Some(_) => {}
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
    .await
    .context("采集超时（远端可能卡在失效的挂载点上）")?;
    Ok(text)
}

/// Establish an authenticated handle to `host`, tunneling through the
/// configured jump host (recursively, ProxyJump style) when present.
pub(crate) fn connect_handle<'a>(
    host: &'a Host,
    vault: &'a crate::Vault,
    visited: &'a mut Vec<String>,
    policy: &'a HostKeyPolicy,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<russh::client::Handle<Handler>>> + Send + 'a>,
> {
    Box::pin(async move {
        if visited.iter().any(|a| a == &host.alias) {
            let chain = visited.join(" → ");
            return Err(anyhow!("跳板链存在循环: {chain} → {}", host.alias));
        }
        visited.push(host.alias.clone());
        // 空闲保活：云上 NAT/防火墙会静默掐断长时间无流量的 SSH TCP，
        // 表现为界面卡死。每 30s 发一次 keepalive@openssh.com；连续 3 次
        // 无响应则主动断开（触发 RemoteClosed，会话界面给出结束提示）。
        let config = std::sync::Arc::new(russh::client::Config {
            keepalive_interval: Some(std::time::Duration::from_secs(keepalive_interval())),
            keepalive_max: 3,
            ..Default::default()
        });
        let handler = Handler::new(&host.hostname, host.port, policy);
        let mut handle = match host.jump.as_deref() {
            Some(alias) => {
                let jump_host = vault
                    .find(alias)
                    .with_context(|| format!("找不到跳板机别名 `{alias}`"))?
                    .clone();
                let jump_handle = connect_handle(&jump_host, vault, visited, policy).await?;
                let channel = jump_handle
                    .channel_open_direct_tcpip(
                        host.hostname.as_str(),
                        host.port as u32,
                        "127.0.0.1",
                        0,
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "跳板机 `{alias}` 到 {}:{} 的转发通道打开失败",
                            host.hostname, host.port
                        )
                    })?;
                russh::client::connect_stream(config, channel.into_stream(), handler)
                    .await
                    .with_context(|| {
                        format!("无法经跳板机与 {}:{} 建立 SSH", host.hostname, host.port)
                    })?
            }
            None => russh::client::connect(config, (host.hostname.as_str(), host.port), handler)
                .await
                .with_context(|| format!("无法连接 {}:{}", host.hostname, host.port))?,
        };
        authenticate(&mut handle, host).await?;
        Ok(handle)
    })
}

/// 对外的一步式连接：拿到一条已认证的 `Handle`，跳板链自行展开。
/// 无头 CLI（`ells exec`）与端口转发都从这里进，避免每个调用方自己构造 `visited`。
pub async fn connect_handle_to(
    host: &Host,
    vault: &crate::Vault,
    policy: &HostKeyPolicy,
) -> Result<russh::client::Handle<Handler>> {
    connect_handle(host, vault, &mut Vec::new(), policy).await
}

/// 只要一条 SFTP 会话，不申请 shell。
///
/// 无头传文件走这条：很多网关账号禁 shell 但允许子系统，用
/// `RemoteSession::connect` 反而会在"申请 shell 失败"上挂掉。
pub async fn connect_sftp(
    host: &Host,
    vault: &crate::Vault,
    policy: &HostKeyPolicy,
) -> Result<Arc<russh_sftp::client::SftpSession>> {
    let handle = connect_handle_to(host, vault, policy).await?;
    let channel = handle
        .channel_open_session()
        .await
        .context("打开会话通道失败")?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("服务器不支持 SFTP 子系统")?;
    let session = russh_sftp::client::SftpSession::new(channel.into_stream())
        .await
        .context("建立 SFTP 会话失败")?;
    Ok(Arc::new(session))
}

/// RSA 密钥要用哪种哈希：跟随服务器 `server-sig-algs` 协商结果。
/// 固定 `None`（即 ssh-rsa/SHA1）在 OpenSSH 8.8+ 上会被直接拒登。
async fn rsa_hash(
    handle: &russh::client::Handle<Handler>,
    algorithm: russh::keys::Algorithm,
) -> Option<russh::keys::HashAlg> {
    if !matches!(algorithm, russh::keys::Algorithm::Rsa { .. }) {
        return None;
    }
    match handle.best_supported_rsa_hash().await {
        Ok(Some(hash)) => hash,
        _ => None,
    }
}

async fn authenticate(handle: &mut russh::client::Handle<Handler>, host: &Host) -> Result<()> {
    match &host.auth {
        Auth::Password => {
            let password = host
                .password
                .as_deref()
                .context("该主机没有保存密码")?;
            let result = handle
                .authenticate_password(&host.user, password)
                .await
                .map_err(|e| anyhow!("密码认证失败: {e}"))?;
            describe(result, "密码")
        }
        Auth::PrimaryKey { path, passphrase } => {
            let expanded = expand_tilde(path);
            if expanded.ends_with(".pub") {
                bail!("私钥路径指向的是公钥(.pub)，请选择私钥文件");
            }
            let raw = std::fs::read_to_string(&expanded)
                .with_context(|| format!("无法读取私钥文件 {}", expanded))?;
            let key = if raw.contains("DEK-Info: DES-EDE3-CBC") {
                let pass = passphrase.as_deref().ok_or_else(|| {
                    anyhow!("该私钥为旧式 DES-EDE3-CBC 加密格式，请在「私钥口令」中填写其密码")
                })?;
                let der = decrypt_des3_pem(&raw, pass)
                    .with_context(|| format!("解密旧式 3DES 私钥失败（口令可能不正确）: {}", expanded))?;
                russh::keys::decode_secret_key(&pkcs1_pem(&der), None)
                    .map_err(|e| anyhow!("解析已解密的 PKCS#1 私钥失败: {e}"))?
            } else {
                match russh::keys::load_secret_key(&expanded, passphrase.as_deref()) {
                    Ok(k) => k,
                    Err(e) if raw.contains("DEK-Info:") => return Err(e).with_context(|| {
                        format!("无法加载加密私钥 {expanded}（旧式加密格式暂不支持，可用 ssh-keygen -p -m PEM 转换）")
                    }),
                    Err(e) => return Err(e)
                        .with_context(|| format!("无法加载私钥 {}", expanded)),
                }
            };
            let hash = rsa_hash(handle, key.algorithm()).await;
            let key = russh::keys::PrivateKeyWithHashAlg::new(std::sync::Arc::new(key), hash);
            let result = handle
                .authenticate_publickey(&host.user, key)
                .await
                .map_err(|e| anyhow!("密钥认证失败: {e}"))?;
            describe(result, "密钥")
        }
        Auth::Agent => authenticate_agent(handle, host).await,
    }
}

/// ssh-agent / Pageant 认证：把签名外包给 agent，ells 进程不接触私钥。
async fn authenticate_agent(handle: &mut russh::client::Handle<Handler>, host: &Host) -> Result<()> {
    use russh::keys::agent::client::AgentClient;

    let mut client: AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>> =
        connect_agent().await?;
    let identities = client
        .request_identities()
        .await
        .map_err(|e| anyhow!("向 ssh-agent 查询密钥失败: {e}"))?;
    if identities.is_empty() {
        bail!("ssh-agent 里没有任何密钥（先用 ssh-add 加入密钥再试）");
    }
    let mut last = None;
    for identity in identities {
        let public = identity.public_key().as_ref().clone();
        let hash = rsa_hash(handle, public.algorithm()).await;
        let result = handle
            .authenticate_publickey_with(&host.user, public, hash, &mut client)
            .await
            .map_err(|e| anyhow!("ssh-agent 认证失败: {e}"))?;
        match result {
            russh::client::AuthResult::Success => return Ok(()),
            failure @ russh::client::AuthResult::Failure { .. } => last = Some(failure),
        }
    }
    describe(last.expect("agent 至少提供了一把密钥"), "ssh-agent")
}

/// 按平台连接可用的 agent：Windows 先试 OpenSSH 命名管道再试 Pageant，
/// Unix/macOS 走 SSH_AUTH_SOCK。
#[cfg(unix)]
async fn connect_agent(
) -> Result<russh::keys::agent::client::AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>>>
{
    use russh::keys::agent::client::AgentClient;
    let client = AgentClient::connect_env()
        .await
        .map_err(|e| anyhow!("连不上 ssh-agent（SSH_AUTH_SOCK 未设置或已失效）: {e}"))?;
    Ok(client.dynamic())
}

#[cfg(windows)]
async fn connect_agent(
) -> Result<russh::keys::agent::client::AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>>>
{
    use russh::keys::agent::client::AgentClient;
    if let Ok(client) = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
        return Ok(client.dynamic());
    }
    let client = AgentClient::connect_pageant()
        .await
        .map_err(|e| anyhow!("连不上 ssh-agent / Pageant（请确认 ssh-agent 服务或 Pageant 正在运行）: {e}"))?;
    Ok(client.dynamic())
}

#[cfg(not(any(unix, windows)))]
async fn connect_agent(
) -> Result<russh::keys::agent::client::AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>>>
{
    bail!("此平台不支持 ssh-agent 认证")
}

fn describe(result: russh::client::AuthResult, method: &str) -> Result<()> {
    match result {
        russh::client::AuthResult::Success => Ok(()),
        russh::client::AuthResult::Failure {
            remaining_methods,
            partial_success,
        } => Err(anyhow!(
            "服务器{method}认证{}；仍可用的认证方式: {remaining_methods:?}",
            if partial_success {
                "已通过，但需要后续步骤"
            } else {
                "被拒绝"
            }
        )),
    }
}

/// 展开 `~/`：ssh 配置里的 IdentityFile 常带波浪号，落库前必须换成绝对路径，
/// 否则备份私钥、加载私钥都会去找一个字面叫 `~` 的目录。
pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

/// Decrypt a legacy PEM private key encrypted with `DEK-Info: DES-EDE3-CBC`
/// (OpenSSL `-des3` output) and return the inner PKCS#1 DER.
fn decrypt_des3_pem(pem: &str, passphrase: &str) -> Result<Vec<u8>> {
    use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
    use md5::Digest;
    use zeroize::{Zeroize, Zeroizing};

    let iv_hex = pem
        .lines()
        .find_map(|l| l.split_once("DES-EDE3-CBC,"))
        .map(|(_, rest)| rest.trim().to_string())
        .ok_or_else(|| anyhow!("缺少 DEK-Info 头"))?;
    let iv = hex_decode(&iv_hex).ok_or_else(|| anyhow!("DEK-Info IV 非法"))?;
    anyhow::ensure!(iv.len() == 8, "DEK-Info IV 长度异常");

    let b64: String = pem
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty()
                && !t.starts_with("-----")
                && !t.starts_with("Proc-Type:")
                && !t.starts_with("DEK-Info:")
        })
        .map(|l| l.trim())
        .collect();
    use base64::Engine;
    let ct = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| anyhow!("私钥 base64 解码失败: {e}"))?;

    // OpenSSL EVP_BytesToKey (MD5, no iteration, salt = IV first 8 bytes).
    let mut dk = Vec::with_capacity(24);
    let mut prev = [0u8; 16];
    let mut first = true;
    while dk.len() < 24 {
        let mut h = md5::Md5::new();
        if !first {
            h.update(prev);
        }
        h.update(passphrase.as_bytes());
        h.update(&iv[..8]);
        let cur: [u8; 16] = h.finalize().into();
        dk.extend_from_slice(&cur);
        prev = cur;
        first = false;
    }
    dk.truncate(24);

    let cipher = cbc::Decryptor::<des::TdesEde3>::new_from_slices(&dk, &iv)
        .map_err(|e| anyhow!("3DES 初始化失败: {e}"))?;
    dk.zeroize();
    let n = ct.len();
    anyhow::ensure!(n > 0 && n % 8 == 0, "私钥密文长度异常");
    // Zeroizing：明文私钥在 drop 时清零，不留在校堆内存里等 GC/交换
    let mut buf = Zeroizing::new(ct);
    let len = cipher
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| anyhow!("3DES 解密失败（口令可能不正确）"))?
        .len();
    Ok(buf[..len].to_vec())
}

/// Wrap raw PKCS#1 DER in a plaintext PEM envelope russh can parse.
fn pkcs1_pem(der: &[u8]) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    format!("-----BEGIN RSA PRIVATE KEY-----\n{b64}\n-----END RSA PRIVATE KEY-----\n")
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let nib = |c: u8| c.is_ascii_digit().then(|| c - b'0').or_else(|| {
        c.is_ascii_hexdigit()
            .then(|| c.to_ascii_lowercase() - b'a' + 10)
    });
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks(2) {
        out.push((nib(pair[0])? << 4) | nib(pair[1])?);
    }
    Some(out)
}

#[cfg(test)]
mod legacy_key_tests {
    use super::*;

    #[test]
    fn decrypts_openssl_des3_pem() {
        let pem = include_str!("../tests/legacy_des3.pem");
        let der = decrypt_des3_pem(pem, "test123").expect("decrypt");
        let key = russh::keys::decode_secret_key(&pkcs1_pem(&der), None).expect("parse pkcs1");
        assert!(key.algorithm().is_rsa());
        // Wrong passphrase must fail cleanly, not panic.
        assert!(decrypt_des3_pem(pem, "wrong").is_err());
    }
}

#[cfg(test)]
mod session_end_tests {
    use super::*;

    fn end(exit_status: bool, stop: Stop) -> SessionEnd {
        SessionEnd { exit_status, stop }
    }

    /// exit / logout 最硬的一条证据是 exit-status，收到就该安静收尾
    #[test]
    fn an_exit_status_is_never_a_drop() {
        for stop in [Stop::PeerClosed, Stop::Vanished, Stop::Broken, Stop::Running] {
            assert!(end(true, stop).is_graceful(), "{stop:?}");
        }
    }

    /// 跳板机/网关在 logout 之后直接关通道，一个 exit-status 都不发：
    /// 这仍然是远端主动收尾，追问"要不要重连"就是把正常退出当成了事故。
    #[test]
    fn a_peer_closed_channel_is_a_goodbye_not_a_drop() {
        assert!(end(false, Stop::PeerClosed).is_graceful());
        // Ctrl-] 与关标签也是主动行为
        assert!(end(false, Stop::Local).is_graceful());
    }

    /// 只有链路自己没了才该走重连：TCP 死亡、keepalive 超时、服务端进程被杀
    #[test]
    fn only_a_dead_link_reconnects() {
        assert!(!end(false, Stop::Vanished).is_graceful());
        assert!(!end(false, Stop::Broken).is_graceful());
        // 一条收尾消息都没收到
        assert!(!end(false, Stop::Running).is_graceful());
    }
}

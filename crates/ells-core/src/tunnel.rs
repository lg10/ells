//! 端口转发隧道：一台主机一条 SSH 连接，承载它的全部转发规则。
//!
//! 与 `ssh -L` / `-D` 的模型一致：本地监听端口在**拨号之前**绑定，并一直持有到
//! 隧道被显式停止。这样掉线重连期间端口不会被别的进程抢走，新来的连接排在
//! listen backlog 里等着，而不是"连不上就悄悄换个端口"。
//!
//! 远程转发（`-R`）的规则可以存进保险库、也能从 `~/.ssh/config` 往返导入，但这里
//! 不为它建立监听：`forwarded-tcpip` 的目的端到底由哪一侧解析需要对着真实服务器
//! 验证，猜错的后果是"流量安静地走错地方"，比明确拒绝更糟。所以这里返回显式的
//! 不支持状态，让界面和 `ells tunnels` 都能看见原因。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};

use crate::host::{Forward, Host};
use crate::hostkey::HostKeyPolicy;
use crate::ssh::{Handler, connect_handle_to};

/// 连续失败时的重连间隔；用完后保持最后一档，不再无限指数增长。
const BACKOFF_SECS: [u64; 6] = [1, 2, 5, 15, 30, 60];

/// 停止信号的检查粒度：重连间隔以秒计，100ms 足够灵敏又不空转。
const STOP_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelState {
    Connecting,
    Up,
    Retrying { attempt: u32, wait_secs: u64 },
    /// 不会重试的终局：端口被占、规则为空、规则类型不支持。
    Failed(String),
    Stopped,
}

/// 一条规则实际占住的本地监听口。
///
/// 用户没写本地口（`auto`）时，这里的 `port` 就是系统分配的结果——界面上必须把它
/// 显示出来，否则"自动分配"等于"没人知道该连哪个口"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortMap {
    /// 规则的写法（`-L 8080:db:5432` / `-D 自动`），与界面显示一致。
    pub spec: String,
    /// 实际监听的地址（`*` 会归一成 `0.0.0.0`）。
    pub addr: IpAddr,
    pub port: u16,
    /// 本地口是不是系统分配的。
    pub auto: bool,
}

impl PortMap {
    /// `127.0.0.1:52133（自动）`
    pub fn endpoint(&self) -> String {
        if self.auto {
            format!("{}:{}（自动）", self.addr, self.port)
        } else {
            format!("{}:{}", self.addr, self.port)
        }
    }
}

#[derive(Debug, Clone)]
pub struct TunnelEvent {
    pub alias: String,
    pub state: TunnelState,
    /// 这条隧道当前实际持有的监听口；还没绑上口（规则为空、类型不支持、绑失败）时是空的。
    pub ports: Vec<PortMap>,
}

impl TunnelEvent {
    pub fn label(&self) -> String {
        let base = match &self.state {
            TunnelState::Connecting => format!("{} · 连接中", self.alias),
            TunnelState::Up => format!("{} · 已就绪", self.alias),
            TunnelState::Retrying { attempt, wait_secs } => {
                format!("{} · 已断开，第 {attempt} 次重连（{wait_secs}s 后）", self.alias)
            }
            TunnelState::Failed(reason) => format!("{} · 失败：{reason}", self.alias),
            TunnelState::Stopped => format!("{} · 已停止", self.alias),
        };
        // 自动口的实际结果必须跟着状态走，不然只有打开映射弹窗才看得见
        if self.ports.is_empty() {
            base
        } else {
            let ports = self.ports.iter().map(|p| p.endpoint()).collect::<Vec<_>>().join(" ");
            format!("{base} [{ports}]")
        }
    }
}

/// 会话看门狗收到的指令：一条本地 TCP 连接连同它的目的端。
///
/// `Handle` 不是 `Clone`，而每条本地连接都要开一个通道，所以让一个任务独占
/// `Handle`。russh 的通道流类型定义在私有模块里、无法命名，因此本地流也一并
/// 交给它——通道打开后由 actor 就地 spawn 搬运任务，类型全程只做推断。
enum Command {
    Open {
        dest_host: String,
        dest_port: u16,
        originator: SocketAddr,
        stream: TcpStream,
        /// 来自 `-D` 动态口：成功要回 SOCKS 应答，失败要按规范回拒绝
        dynamic: bool,
    },
}

async fn session_actor(mut handle: russh::client::Handle<Handler>, mut rx: mpsc::Receiver<Command>) {
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(Command::Open { dest_host, dest_port, originator, stream, dynamic }) => {
                    match handle
                        .channel_open_direct_tcpip(
                            dest_host,
                            u32::from(dest_port),
                            originator.ip().to_string(),
                            u32::from(originator.port()),
                        )
                        .await
                    {
                        Ok(channel) => {
                            let mut remote = channel.into_stream();
                            tokio::spawn(async move {
                                relay(stream, dynamic, &mut remote).await;
                            });
                        }
                        Err(err) => {
                            tracing::debug!(%err, "转发通道打开失败");
                            tokio::spawn(reject(stream, dynamic));
                        }
                    }
                }
                // 所有发送者都已 drop：会话可以收了
                None => break,
            },
            // Handle 自身实现了 Future，会话结束（对端断开、保活超时）它就 Ready
            result = &mut handle => {
                if result.is_err() {
                    tracing::debug!("隧道会话异常结束");
                }
                break;
            }
        }
    }
    let _ = handle
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await;
}

/// 双向搬运；`-D` 的客户端要先等到通道开成的应答。
async fn relay<R>(mut stream: TcpStream, dynamic: bool, remote: &mut R)
where
    R: AsyncRead + AsyncWrite + Unpin,
{
    if dynamic {
        let _ = socks5_ok(&mut stream).await;
    }
    if let Err(err) = tokio::io::copy_bidirectional(&mut stream, remote).await {
        tracing::debug!(%err, "转发中断");
    }
}

/// 通道没开成：`-D` 按规范回拒绝，普通本地流直接收尾。
async fn reject(mut stream: TcpStream, dynamic: bool) {
    if dynamic {
        let _ = socks5_fail(&mut stream).await;
    }
}

/// 每台主机一个任务的隧道管理器。
pub struct TunnelManager {
    tasks: HashMap<String, Running>,
    events: mpsc::UnboundedSender<TunnelEvent>,
    policy: HostKeyPolicy,
}

struct Running {
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

impl TunnelManager {
    pub fn new(events: mpsc::UnboundedSender<TunnelEvent>, policy: HostKeyPolicy) -> Self {
        Self { tasks: HashMap::new(), events, policy }
    }

    pub fn is_running(&self, alias: &str) -> bool {
        self.tasks.contains_key(alias)
    }

    pub fn running(&self) -> Vec<String> {
        let mut aliases: Vec<String> = self.tasks.keys().cloned().collect();
        aliases.sort();
        aliases
    }

    /// 起一条隧道。`vault` 是启动时刻的快照：跳板链与凭据就此固化，之后在界面里
    /// 编辑主机不会影响已在跑的隧道（要改就先停再起，行为可预期）。
    pub fn start(&mut self, host: &Host, vault: Arc<crate::Vault>) {
        self.stop_one(&host.alias);
        let stop = Arc::new(AtomicBool::new(false));
        let join = tokio::spawn(run_tunnel(
            host.clone(),
            Arc::clone(&vault),
            self.policy.clone(),
            self.events.clone(),
            Arc::clone(&stop),
        ));
        self.tasks.insert(host.alias.clone(), Running { stop, join });
    }

    pub fn stop_one(&mut self, alias: &str) {
        if let Some(task) = self.tasks.remove(alias) {
            task.stop.store(true, Ordering::Relaxed);
            task.join.abort();
        }
    }

    pub fn stop_all(&mut self) {
        for alias in self.running() {
            self.stop_one(&alias);
        }
    }
}

impl Drop for TunnelManager {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// 监听口 + 它对应的规则。`Arc` 让同一份监听器能跨越多次重连存活。
struct Bound {
    listener: Arc<TcpListener>,
    forward: Forward,
    /// 实际监听端点：本地口写 `0` 时这是系统分配的结果，重连期间也不会变。
    endpoint: SocketAddr,
}

impl Bound {
    fn mapping(&self) -> PortMap {
        PortMap {
            spec: self.forward.display(),
            addr: self.endpoint.ip(),
            port: self.endpoint.port(),
            auto: self.forward.auto_port(),
        }
    }
}

/// 端口绑不上时的报因。`os error 10048` 对用户没有信息量：先说是哪台主机的哪条规则
/// 占着同一个口，再给"本地口留空"这条出路；没有同口的 ells 规则才归给外部程序。
fn bind_failure(hosts: &[Host], alias: &str, addr: IpAddr, port: u16, err: &std::io::Error) -> String {
    let others: Vec<String> = hosts
        .iter()
        .filter(|h| h.alias != alias)
        .flat_map(|h| h.forwards.iter().map(move |f| (&h.alias, f)))
        .filter(|(_, f)| f.fixed_local_port() == Some(port))
        .map(|(other, f)| format!("{other} 的 {}", f.display()))
        .collect();
    if others.is_empty() {
        format!("本地端口 {addr}:{port} 绑定失败：{err}（没有其它 ells 规则用它，应该是外部程序占着）")
    } else {
        format!(
            "本地端口 {addr}:{port} 绑定失败：{} 用的是同一个口 — 先停掉其中一个，或把本地口留空交给系统分配",
            others.join("、")
        )
    }
}

async fn run_tunnel(
    host: Host,
    vault: Arc<crate::Vault>,
    policy: HostKeyPolicy,
    events: mpsc::UnboundedSender<TunnelEvent>,
    stop: Arc<AtomicBool>,
) {
    let alias = host.alias.clone();
    // 每次状态变化都带上端口映射，界面不必自己再算"实际落在哪个口"
    let report = |state: TunnelState, ports: &[PortMap]| {
        let _ = events.send(TunnelEvent { alias: alias.clone(), state, ports: ports.to_vec() });
    };

    if host.forwards.is_empty() {
        report(TunnelState::Failed("该主机没有配置转发规则".into()), &[]);
        return;
    }
    if let Some(unsupported) = host
        .forwards
        .iter()
        .find(|f| matches!(f, Forward::Remote { .. }))
    {
        report(
            // display() 而不是 label()：自动口不该在报错里写成看着像填错的 `-R 0:backup:22`
            TunnelState::Failed(format!("{} 这类远程转发暂不支持", unsupported.display())),
            &[],
        );
        return;
    }

    // 先绑端口再拨号：端口被占这件事不该等一轮网络往返才知道
    let mut bound = Vec::new();
    for rule in host.forwards.clone() {
        let Some(port) = rule.local_listen_port() else { continue };
        let addr = rule.bind_address();
        match TcpListener::bind((addr, port)).await {
            Ok(listener) => {
                let endpoint = listener.local_addr().unwrap_or(SocketAddr::new(addr, port));
                bound.push(Bound { listener: Arc::new(listener), forward: rule, endpoint });
            }
            Err(err) => {
                report(TunnelState::Failed(bind_failure(&vault.hosts, &alias, addr, port, &err)), &[]);
                return;
            }
        }
    }
    if bound.is_empty() {
        report(TunnelState::Failed("没有可监听的本地端口".into()), &[]);
        return;
    }
    let ports: Vec<PortMap> = bound.iter().map(Bound::mapping).collect();

    let mut attempt = 0u32;
    loop {
        if stopped(&stop) {
            report(TunnelState::Stopped, &ports);
            return;
        }
        report(TunnelState::Connecting, &ports);
        match connect_handle_to(&host, &vault, &policy).await {
            Ok(handle) => {
                attempt = 0;
                report(TunnelState::Up, &ports);
                let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(32);
                let actor = tokio::spawn(session_actor(handle, cmd_rx));
                serve(&bound, cmd_tx.clone(), &stop).await;
                // 先关掉发送端：actor 收到 None 就会自行 disconnect，不必强杀
                drop(cmd_tx);
                tokio::time::sleep(Duration::from_millis(50)).await;
                actor.abort();
                if stopped(&stop) {
                    report(TunnelState::Stopped, &ports);
                    return;
                }
            }
            Err(err) => {
                if stopped(&stop) {
                    report(TunnelState::Stopped, &ports);
                    return;
                }
                tracing::warn!(%err, alias, "隧道拨号失败");
            }
        }
        attempt += 1;
        let wait = BACKOFF_SECS[(attempt as usize - 1).min(BACKOFF_SECS.len() - 1)];
        report(TunnelState::Retrying { attempt, wait_secs: wait }, &ports);
        if sleep_until_stop(Duration::from_secs(wait), &stop).await {
            report(TunnelState::Stopped, &ports);
            return;
        }
    }
}

/// 连接活着期间接受本地连接；每条本地连接交给独立的 relay 任务，互不阻塞。
async fn serve(bound: &[Bound], cmd_tx: mpsc::Sender<Command>, stop: &AtomicBool) {
    let (conn_tx, mut conn_rx) = mpsc::unbounded_channel::<(TcpStream, Forward, SocketAddr)>();
    let mut acceptors = JoinSet::new();
    for b in bound {
        let listener = Arc::clone(&b.listener);
        let forward = b.forward.clone();
        let conn_tx = conn_tx.clone();
        acceptors.spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, originator)) => {
                        if conn_tx.send((stream, forward.clone(), originator)).is_err() {
                            return;
                        }
                    }
                    Err(err) => {
                        tracing::debug!(%err, "监听口出错，停止接受连接");
                        return;
                    }
                }
            }
        });
    }
    drop(conn_tx);

    loop {
        if stopped(stop) {
            break;
        }
        tokio::select! {
            accepted = conn_rx.recv() => match accepted {
                Some((stream, forward, originator)) => {
                    let cmd_tx = cmd_tx.clone();
                    tokio::spawn(relay_connection(stream, forward, originator, cmd_tx));
                }
                // 所有 accept 任务都已退出：这条连接没有活的可服务了
                None => break,
            },
            _ = wait_stop(stop) => break,
        }
    }
    acceptors.abort_all();
}

/// 一条本地连接：先确定目的端（`-D` 要读 SOCKS 请求），再把整条连接交给会话 actor。
async fn relay_connection(
    mut stream: TcpStream,
    forward: Forward,
    originator: SocketAddr,
    cmd_tx: mpsc::Sender<Command>,
) {
    let (dest_host, dest_port, dynamic) = match &forward {
        Forward::Local { dest_host, dest_port, .. } => {
            (dest_host.clone(), *dest_port, false)
        }
        Forward::Dynamic { .. } => match socks5_target(&mut stream).await {
            Ok(Some(target)) => (target.0, target.1, true),
            // 握手不成或客户端要的是 BIND/UDP：已经按 SOCKS 规范回过拒绝了
            Ok(None) => return,
            Err(err) => {
                tracing::debug!(%err, "SOCKS 握手失败");
                return;
            }
        },
        Forward::Remote { .. } => return,
    };

    let request = Command::Open { dest_host, dest_port, originator, stream, dynamic };
    if cmd_tx.send(request).await.is_err() {
        // actor 已收摊：随 `request` 一起交出的本地连接就此作废，不让它干等超时
        tracing::debug!("转发会话已结束，本地连接作废");
    }
}

fn stopped(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Relaxed)
}

async fn wait_stop(stop: &AtomicBool) {
    while !stopped(stop) {
        tokio::time::sleep(STOP_POLL).await;
    }
}

/// 睡眠到点或被停止信号打断；返回 `true` 表示是被打断的。
async fn sleep_until_stop(wait: Duration, stop: &AtomicBool) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if stopped(stop) {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep(STOP_POLL.min(deadline - now)).await;
    }
}

// ---------------------------------------------------------------------------
// SOCKS5（无认证）：`-D` 的本地协议面
// ---------------------------------------------------------------------------

/// 读出客户端要连的目的端。`Ok(None)` 表示已按规范拒绝、直接收尾即可。
async fn socks5_target(stream: &mut TcpStream) -> Result<Option<(String, u16)>> {
    let mut greeting = [0u8; 2];
    stream
        .read_exact(&mut greeting)
        .await
        .context("读取 SOCKS 问候失败")?;
    if greeting[0] != 5 {
        return Ok(None);
    }
    let mut methods = vec![0u8; usize::from(greeting[1])];
    stream.read_exact(&mut methods).await.context("读取方法列表失败")?;
    // 只支持"无认证"：客户端没提供它就不硬来，回 0xFF 让它自己决定
    if !methods.contains(&0) {
        stream.write_all(&[5, 0xFF]).await.ok();
        return Ok(None);
    }
    stream.write_all(&[5, 0]).await.context("回 SOCKS 方法失败")?;

    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await.context("读取请求头失败")?;
    if header[0] != 5 {
        return Ok(None);
    }
    if header[1] != 1 {
        // 1 = CONNECT；BIND 与 UDP  assoc 在直连转发里没有对应物
        stream.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await.ok();
        return Ok(None);
    }
    let target = match header[3] {
        1 => {
            let mut octets = [0u8; 4];
            stream
                .read_exact(&mut octets)
                .await
                .context("读取 IPv4 目的端失败")?;
            IpAddr::V4(octets.into()).to_string()
        }
        4 => {
            let mut octets = [0u8; 16];
            stream
                .read_exact(&mut octets)
                .await
                .context("读取 IPv6 目的端失败")?;
            IpAddr::V6(octets.into()).to_string()
        }
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.context("读取域名长度失败")?;
            let mut name = vec![0u8; usize::from(len[0])];
            stream.read_exact(&mut name).await.context("读取域名失败")?;
            String::from_utf8(name).map_err(|_| anyhow!("SOCKS 域名不是 UTF-8"))?
        }
        other => {
            // 8 = 地址类型不支持
            stream.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0]).await.ok();
            return Err(anyhow!("不支持的地址类型 {other}"));
        }
    };
    let mut port = [0u8; 2];
    stream.read_exact(&mut port).await.context("读取端口失败")?;
    if target.is_empty() || forward_is_unroutable(&target) {
        return Ok(None);
    }
    Ok(Some((target, u16::from_be_bytes(port))))
}

/// 空目的端和 0.0.0.0 都不是可转发的目标；把它们交给 SSH 只会得到一个语义不明的
/// 通道拒绝。
fn forward_is_unroutable(target: &str) -> bool {
    target == "0.0.0.0" || target.parse::<IpAddr>().is_ok_and(|ip| ip.is_unspecified())
}

/// 转发通道开成之后的成功应答。绑定地址回零：客户端不关心它。
async fn socks5_ok(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await
}

/// 通道开不成时按规范回拒绝，而不是默默断开让客户端干等。
async fn socks5_fail(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(&[5, 1, 0, 1, 0, 0, 0, 0, 0, 0]).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Auth, Forward};
    use tokio::io::AsyncWriteExt;

    fn host_with(forwards: Vec<Forward>) -> Host {
        Host {
            alias: "tun".into(),
            hostname: "127.0.0.1".into(),
            port: 1,
            user: "nobody".into(),
            auth: Auth::Password,
            forwards,
            ..Default::default()
        }
    }

    /// 端口绑定失败必须在拨号之前就被报告出来：先占住一个端口，再让隧道去绑同一个，
    /// 断的是"失败原因要说端口占用，而不是等一轮网络往返才说连接失败"。
    #[tokio::test]
    async fn unbindable_port_reports_before_dialing() {
        let occupied = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = occupied.local_addr().unwrap().port();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let policy = HostKeyPolicy::trust_all();
        let stop = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run_tunnel(
            host_with(vec![Forward::Local {
                bind: None,
                listen_port: port,
                dest_host: "localhost".into(),
                dest_port: 80,
            }]),
            Arc::new(crate::Vault::default()),
            policy,
            tx,
            Arc::clone(&stop),
        ));
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("应报告状态")
            .expect("通道不应提前关闭");
        match event.state {
            TunnelState::Failed(reason) => assert!(
                reason.contains("绑定失败"),
                "失败原因应指向端口绑定，实际是: {reason}"
            ),
            other => panic!("期望 Failed，实际 {other:?}"),
        }
        task.abort();
    }

    /// 本地口留空（`0`）时，界面要在第一条状态里就拿到系统分配到的端口。
    /// 拨号对象是不存在的 `127.0.0.1:1`，事件顺序因此是确定的：先绑口、再 Connecting。
    #[tokio::test]
    async fn auto_port_reports_the_assigned_endpoint() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run_tunnel(
            host_with(vec![Forward::Local {
                bind: None,
                listen_port: 0,
                dest_host: "localhost".into(),
                dest_port: 80,
            }]),
            Arc::new(crate::Vault::default()),
            HostKeyPolicy::trust_all(),
            tx,
            Arc::clone(&stop),
        ));
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("应有状态事件")
            .expect("通道不应提前关闭");
        assert_eq!(event.ports.len(), 1, "一条规则应报告一个监听口");
        let map = &event.ports[0];
        assert!(map.auto, "本地口写 0 就是自动分配");
        assert_ne!(map.port, 0, "报告的必须是实际端口，不是写进去的 0");
        assert_eq!(map.addr, IpAddr::from([127, 0, 0, 1]));
        assert_eq!(map.spec, "-L 自动 → localhost:80");
        assert!(event.label().contains("自动"), "状态文本要看得见实际口: {}", event.label());
        task.abort();
    }

    /// 撞口时分得清"是另一台 ells 主机的规则"还是"外部程序"，并且给出出路。
    #[test]
    fn bind_failure_names_the_rival_rule_or_points_outside() {
        let rule = |spec: &str| Forward::parse_specs(spec).0.remove(0);
        let hosts = vec![
            Host {
                alias: "db-prod".into(),
                forwards: vec![rule("-L 8080:127.0.0.1:5432")],
                ..Default::default()
            },
            Host {
                alias: "web-prod".into(),
                forwards: vec![rule("-L 8080:127.0.0.1:80")],
                ..Default::default()
            },
        ];
        let err = std::io::Error::other("占用");
        let addr = IpAddr::from([127, 0, 0, 1]);
        let msg = bind_failure(&hosts, "web-prod", addr, 8080, &err);
        assert!(msg.contains("db-prod 的 -L 8080:127.0.0.1:5432"), "应点名对方: {msg}");
        assert!(msg.contains("留空"), "应给出出路: {msg}");
        assert!(!msg.contains("web-prod 的"), "不能把自己算进去");

        let free = bind_failure(&hosts, "web-prod", addr, 9999, &err);
        assert!(free.contains("外部程序"), "没有同口规则时归给外部: {free}");

        // 自动口的规则占的是实际口，不该被算成"和谁同口"
        let auto = vec![Host {
            alias: "x".into(),
            forwards: vec![rule("-L redis:6379")],
            ..Default::default()
        }];
        assert!(bind_failure(&auto, "y", addr, 8080, &err).contains("外部程序"));
    }

    /// 空规则与 `-R` 都要给明确原因，不能静默不动。
    /// 两个用例各自一条事件通道：共用通道时"先到者"取决于两个任务的调度顺序。
    #[tokio::test]
    async fn empty_rules_fail_loudly() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        run_tunnel(
            host_with(Vec::new()),
            Arc::new(crate::Vault::default()),
            HostKeyPolicy::trust_all(),
            tx,
            Arc::clone(&stop),
        )
        .await;
        let event = rx.recv().await.expect("空规则应有状态");
        assert!(matches!(event.state, TunnelState::Failed(ref r) if r.contains("没有配置")));
    }

    #[tokio::test]
    async fn remote_forward_is_reported_as_unsupported() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        run_tunnel(
            host_with(vec![Forward::Remote {
                bind: None,
                listen_port: 9000,
                dest_host: "backup".into(),
                dest_port: 22,
            }]),
            Arc::new(crate::Vault::default()),
            HostKeyPolicy::trust_all(),
            tx,
            Arc::clone(&stop),
        )
        .await;
        let event = rx.recv().await.expect("-R 应有状态");
        match event.state {
            TunnelState::Failed(reason) => {
                assert!(reason.contains("远程转发"), "实际: {reason}");
                assert!(reason.contains("-R"), "应给出 ssh 形式，实际: {reason}");
            }
            other => panic!("期望 Failed，实际 {other:?}"),
        }
    }

    /// 起一个本地客户端与之握手，返回服务端一侧的 TcpStream。
    /// 目的端只支持 IPv4 / 域名两种写法，够覆盖 `-D` 的实际用法。
    async fn socks_pair(
        request: Vec<u8>,
    ) -> (
        TcpStream,
        tokio::task::JoinHandle<Vec<u8>>,
        TcpListener,
    ) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reader = tokio::spawn(async move {
            let mut c = TcpStream::connect(addr).await.unwrap();
            // 先完成方法协商（无认证），再送请求
            c.write_all(&[5, 1, 0]).await.unwrap();
            let mut ack = [0u8; 2];
            c.read_exact(&mut ack).await.unwrap();
            c.write_all(&request).await.unwrap();
            // 有拒绝应答就能读满 10 字节；没有应答（CONNECT 成功路径此刻不该回）
            // 就超时返回空，避免测试互相等死
            let reply = tokio::time::timeout(Duration::from_millis(300), async {
                let mut buf = [0u8; 10];
                match c.read_exact(&mut buf).await {
                    Ok(_) => buf.to_vec(),
                    Err(_) => Vec::new(),
                }
            })
            .await
            .unwrap_or_default();
            reply
        });
        let (stream, _) = listener.accept().await.unwrap();
        (stream, reader, listener)
    }

    #[tokio::test]
    async fn socks5_connect_handshake_yields_the_target() {
        // CONNECT 到 127.0.0.1:8080（端口是大端 0x1F90）
        let (mut stream, _reader, listener) =
            socks_pair(vec![5, 1, 0, 1, 127, 0, 0, 1, 31, 144]).await;
        assert_eq!(
            socks5_target(&mut stream).await.unwrap(),
            Some(("127.0.0.1".to_string(), 8080))
        );
        drop(listener);
    }

    /// 域名型目的端：长度前缀之后的字节是主机名，不能按 IP 解析。
    #[tokio::test]
    async fn socks5_domain_target_is_read_as_utf8() {
        let mut request = vec![5, 1, 0, 3, 11];
        request.extend_from_slice(b"example.com");
        request.extend_from_slice(&[0, 22]);
        let (mut stream, _reader, listener) = socks_pair(request).await;
        assert_eq!(
            socks5_target(&mut stream).await.unwrap(),
            Some(("example.com".to_string(), 22))
        );
        drop(listener);
    }

    /// 客户端只提出"需要认证"时必须被拒绝，而不是当成无认证放行。
    #[tokio::test]
    async fn socks5_refuses_when_no_auth_offered() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reader = tokio::spawn(async move {
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(&[5, 1, 2]).await.unwrap();
            let mut reply = [0u8; 2];
            c.read_exact(&mut reply).await.unwrap();
            reply
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(socks5_target(&mut stream).await.unwrap(), None);
        assert_eq!(reader.await.unwrap(), [5, 0xFF]);
    }

    /// BIND / UDP ASSOCIATE 在直连转发里没有对应物，要按规范回 7（不支持的命令）。
    #[tokio::test]
    async fn socks5_rejects_non_connect_commands() {
        let (mut stream, reader, listener) =
            socks_pair(vec![5, 3, 0, 1, 127, 0, 0, 1, 0, 80]).await;
        assert_eq!(socks5_target(&mut stream).await.unwrap(), None);
        let reply = reader.await.unwrap();
        assert_eq!(reply.first(), Some(&5), "应答版本应是 5，实际 {reply:?}");
        assert_eq!(reply.get(1), Some(&7), "命令不支持应回 REP=7，实际 {reply:?}");
        drop(listener);
    }

    #[test]
    fn bind_address_defaults_to_loopback() {
        let local = Forward::Local {
            bind: None,
            listen_port: 8080,
            dest_host: "localhost".into(),
            dest_port: 80,
        };
        assert_eq!(
            local.bind_address(),
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
        // 解析不了的主机名绝不能退化成"所有网卡"
        let named = Forward::Dynamic { bind: Some("proxy.internal".into()), listen_port: 1080 };
        assert_eq!(
            named.bind_address(),
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
        assert_eq!(local.local_listen_port(), Some(8080));
        assert_eq!(
            Forward::Remote {
                bind: None,
                listen_port: 9000,
                dest_host: "h".into(),
                dest_port: 22
            }
            .local_listen_port(),
            None
        );
    }

    #[test]
    fn backoff_caps_at_the_last_step() {
        let pick = |attempt: u32| BACKOFF_SECS[(attempt as usize - 1).min(BACKOFF_SECS.len() - 1)];
        assert_eq!(pick(1), 1);
        assert_eq!(pick(6), 60);
        assert_eq!(pick(99), 60, "退避必须封顶，否则一次长断线后永远醒不过来");
    }
}

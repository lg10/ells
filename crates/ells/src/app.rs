use anyhow::Result;
use crossterm::event::{
    EnableBracketedPaste, EnableMouseCapture, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::enable_raw_mode;
use crossterm::execute;
use crossterm::terminal::EnterAlternateScreen;
use ells_core::host::{Auth, Forward, Host};
use ells_core::ssh::RemoteSession;
use ells_core::vault::{self, VaultKey};
use ells_core::{HostKeyPolicy, HostKeyPrompt, KeyTrust, PortMap, TunnelEvent, TunnelManager, TunnelState, Vault, audit, filter, hostkey, sessionlog};
use ells_core::audit::AuditKind;
use ells_transfer::{self, Cancel, FileEntry, Progress};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use russh_sftp::client::SftpSession;
use std::collections::HashMap;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use crate::events::{self, AppEvent};
use crate::keybinds::{Action, Chord};
use crate::session::{SessionAction, SessionState, TermMode};
use crate::settings::Settings;
use crate::term;
use crate::theme;
use crate::ui;
use crate::update;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ScreenKind {
    Unlock,
    #[default]
    List,
    Form,
    Session,
    Browser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockStage {
    Open,
    CreateFirst,
    CreateConfirm,
}

/// 设置弹窗里"修改主密码"的输入阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpStage {
    Idle,
    First,
    Confirm,
}

pub struct UnlockState {
    pub input: String,
    pub error: Option<String>,
    pub stage: UnlockStage,
    pub pending_master: Option<String>,
    pub busy: bool,
}

pub struct ListState {
    pub selected: usize,
    /// 折叠起来的分组名（会话内状态，不落盘：折叠是"这会儿不想看"，不是配置）。
    pub folded: Vec<String>,
}

/// 「折叠全部」折的是哪些段：保险库里出现过的分组名，去重后按分组视图的先后排。
pub(crate) fn foldable_groups(hosts: &[Host]) -> Vec<String> {
    let order = order_hosts(hosts, "", ListSort::Section);
    let mut out: Vec<String> = Vec::new();
    for &i in &order {
        let g = group_of(&hosts[i]);
        if !out.contains(&g) {
            out.push(g);
        }
    }
    out
}

/// 列表排序方式：列表页 `o` 循环，选中的那个写进 `settings.ini` 的 `list_sort=`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListSort {
    /// 分组 → 组内收藏 → 组内最近使用 → 别名
    Grouped,
    /// 最近使用优先，不看分组
    Recent,
    /// 纯按别名排
    Alias,
    /// 分组 → 别名（组织架构视角，忽略收藏与最近使用）
    Section,
}

impl ListSort {
    const ALL: [ListSort; 4] = [ListSort::Grouped, ListSort::Recent, ListSort::Alias, ListSort::Section];

    pub fn label(self) -> &'static str {
        match self {
            Self::Grouped => "默认",
            Self::Recent => "最近使用",
            Self::Alias => "别名",
            Self::Section => "分组",
        }
    }

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|&m| m == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    /// 只有真的按分组排的两种模式才画段头，折叠也随着段头一起生效。
    pub fn groups_ordered(self) -> bool {
        matches!(self, Self::Grouped | Self::Section)
    }

    pub fn ini_value(self) -> &'static str {
        match self {
            Self::Grouped => "grouped",
            Self::Recent => "recent",
            Self::Alias => "alias",
            Self::Section => "section",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.ini_value() == text.trim())
    }
}

/// 列表页的一行。段头只是装饰，光标永远落在 `Host` 行上，所以 `list.selected`
/// 数的是主机而不是行——绘制时用 `selected_row()` 换算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListRow {
    Header { group: String, count: usize, folded: bool },
    Host(usize),
}

/// 主机所属分组的段头名：`None`、空串、纯空格都归到「未分组」，与排序键一致。
pub(crate) fn group_of(host: &Host) -> String {
    match host.group.as_deref().map(str::trim) {
        Some(g) if !g.is_empty() => g.to_string(),
        _ => filter::group_label(None).to_string(),
    }
}

/// 排序 + 过滤后的 `hosts` 下标序列。**包含**被折叠分组里的主机：段头要写出
/// 「组名 (n)」的 n，而且折叠掉的段还得留在列表上。
pub(crate) fn order_hosts(hosts: &[Host], query: &str, sort: ListSort) -> Vec<usize> {
    let query = query.trim();
    let mut idx: Vec<usize> = if query.is_empty() {
        (0..hosts.len()).collect()
    } else {
        (0..hosts.len())
            .filter(|&i| filter::fuzzy_score(query, &filter::haystack(&hosts[i])).is_some())
            .collect()
    };
    idx.sort_by(|&a, &b| {
        let (ha, hb) = (&hosts[a], &hosts[b]);
        if !query.is_empty() {
            let sa = filter::fuzzy_score(query, &filter::haystack(ha)).unwrap_or(0);
            let sb = filter::fuzzy_score(query, &filter::haystack(hb)).unwrap_or(0);
            return sb.cmp(&sa).then_with(|| ha.alias.cmp(&hb.alias));
        }
        match sort {
            ListSort::Grouped => group_rank(ha.group.as_deref())
                .cmp(&group_rank(hb.group.as_deref()))
                .then_with(|| hb.favorite.cmp(&ha.favorite))
                .then_with(|| hb.last_connected.cmp(&ha.last_connected))
                .then_with(|| ha.alias.cmp(&hb.alias)),
            ListSort::Recent => ha
                .last_connected
                .cmp(&hb.last_connected)
                .reverse()
                .then_with(|| ha.alias.cmp(&hb.alias)),
            ListSort::Alias => ha.alias.cmp(&hb.alias),
            ListSort::Section => group_rank(ha.group.as_deref())
                .cmp(&group_rank(hb.group.as_deref()))
                .then_with(|| ha.alias.cmp(&hb.alias)),
        }
    });
    idx
}

/// 把排好序的主机下标铺成要绘制的行，顺带算出"没被折叠"的那份主机序列。
/// 两个返回值必须同源：段头按分组连号插入，可见主机又要跳过折叠段，分开算
/// 迟早会对不上，光标就会指到隔壁组去。
pub(crate) fn build_list_rows(
    hosts: &[Host],
    order: &[usize],
    headers: bool,
    folded: &[String],
) -> (Vec<ListRow>, Vec<usize>) {
    if !headers {
        let visible: Vec<usize> = order.to_vec();
        return (visible.iter().copied().map(ListRow::Host).collect(), visible);
    }
    let mut rows: Vec<ListRow> = Vec::with_capacity(order.len() + 4);
    let mut visible: Vec<usize> = Vec::with_capacity(order.len());
    let mut current: Option<String> = None;
    let mut header_at = 0usize;
    for &i in order {
        let g = group_of(&hosts[i]);
        let is_folded = folded.iter().any(|f| f == &g);
        if current.as_deref() != Some(g.as_str()) {
            current = Some(g.clone());
            header_at = rows.len();
            rows.push(ListRow::Header { group: g, count: 0, folded: is_folded });
        }
        // 计数连折叠起来的那些一起算：段头写「生产 (3)」时被藏的 3 台也得在里面，
        // 否则展开前后数字会变，用户以为丢了两台。
        if let ListRow::Header { count, .. } = &mut rows[header_at] {
            *count += 1;
        }
        if is_folded {
            continue;
        }
        rows.push(ListRow::Host(i));
        visible.push(i);
    }
    (rows, visible)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Number,
    Secret,
    AuthChoice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldRole {
    Alias,
    Hostname,
    Port,
    User,
    Auth,
    Password,
    KeyPath,
    KeyPass,
    Jump,
    Group,
    Tags,
    Forwards,
}

pub struct Field {
    pub role: FieldRole,
    pub label: &'static str,
    pub value: String,
    pub kind: FieldKind,
}

impl FieldRole {
    /// 这三项决定"连不连得上"，缺一个都存不下去——表单里用 ＊ 标出来，
    /// 提交失败时也只点这几项的名，不再报一句笼统的"必填项"。
    pub(crate) fn required(self) -> bool {
        matches!(self, Self::Alias | Self::Hostname | Self::User)
    }
}

/// 已有分组及各组主机数，按分组视图里的先后排。表单敲分组名时当提示用：
/// "生产"、"生产 "、"prod" 混着写，列表页就会裂成三个段头。
pub(crate) fn group_counts(hosts: &[Host]) -> Vec<(String, usize)> {
    foldable_groups(hosts)
        .into_iter()
        .map(|g| {
            let n = hosts.iter().filter(|h| group_of(h) == g).count();
            (g, n)
        })
        .collect()
}

/// 规则表格的一行（`t` 面板按 Enter 进来编辑的就是它）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleRow {
    /// `L` / `D` / `R`，大写存着显示用；输入不分大小写。
    pub kind: String,
    pub bind: String,
    /// 留空（或写 `0`）= 本地口交给系统分配，这样两台主机想要同一个口就不会互挤。
    pub listen: String,
    pub dest_host: String,
    pub dest_port: String,
}

/// 表格的列名，`RuleRow` 的下标语义以这里的顺序为准。
pub const RULE_COLS: [&str; 5] = ["类型", "绑定", "本地端口", "目标主机", "目标端口"];

impl RuleRow {
    pub fn empty() -> Self {
        Self { kind: "L".into(), bind: String::new(), listen: String::new(), dest_host: String::new(), dest_port: String::new() }
    }

    pub fn cell(&self, col: usize) -> &str {
        match col {
            0 => &self.kind,
            1 => &self.bind,
            2 => &self.listen,
            3 => &self.dest_host,
            _ => &self.dest_port,
        }
    }

    fn set_cell(&mut self, col: usize, v: String) {
        match col {
            0 => self.kind = v,
            1 => self.bind = v,
            2 => self.listen = v,
            3 => self.dest_host = v,
            _ => self.dest_port = v,
        }
    }

    /// 这行的本地口是否自动分配。
    fn auto(&self) -> bool {
        let t = self.listen.trim();
        t.is_empty() || t == "0"
    }

    /// 它占住的固定本地口；自动口返回 `None`，永远不参与撞口判定。
    fn port(&self) -> Option<u16> {
        if self.auto() { None } else { self.listen.trim().parse().ok() }
    }

    /// 这一行是不是还没开始填。`kind` 默认就是 `L`，只按了 `Ctrl-N` 的行不算填过东西——
    /// 把它当"填了一半"拦下来，用户会看见一条自己根本没写过的规则在报错。
    fn blank(&self) -> bool {
        let kind = self.kind.trim();
        (kind.is_empty() || kind == "L")
            && [self.bind.as_str(), self.listen.as_str(), self.dest_host.as_str(), self.dest_port.as_str()]
                .iter()
                .all(|s| s.trim().is_empty())
    }
}

/// 主机的规则 → 表格行。`-R` 也照原样列出：它存得进来（`~/.ssh/config` 会带），
/// 只是起不来，编辑器不该顺手把它抹掉。
pub(crate) fn rules_from_forwards(forwards: &[Forward]) -> Vec<RuleRow> {
    forwards
        .iter()
        .map(|f| RuleRow {
            kind: match f {
                Forward::Local { .. } => "L".into(),
                Forward::Remote { .. } => "R".into(),
                Forward::Dynamic { .. } => "D".into(),
            },
            bind: f.bind().unwrap_or_default().to_string(),
            listen: listen_text(f),
            dest_host: match f {
                Forward::Local { dest_host, .. } | Forward::Remote { dest_host, .. } => dest_host.clone(),
                Forward::Dynamic { .. } => String::new(),
            },
            dest_port: match f {
                Forward::Local { dest_port, .. } | Forward::Remote { dest_port, .. } => dest_port.to_string(),
                Forward::Dynamic { .. } => String::new(),
            },
        })
        .collect()
}

/// 端口格显示文本：自动口显示成空格，界面上再补一句"自动"。
/// `Remote` 没有本地口，但它的服务器侧端口同样要能显示和编辑。
fn listen_text(f: &Forward) -> String {
    if f.auto_port() {
        return String::new();
    }
    match f {
        Forward::Local { listen_port, .. }
        | Forward::Remote { listen_port, .. }
        | Forward::Dynamic { listen_port, .. } => listen_port.to_string(),
    }
}

/// 表格行 → 规则。逐行给原因，且只给有问题的行号：整表拦成一句"格式不对"没人改得动。
///
/// 全空的行直接丢掉（多半是刚按了 `n` 还没填）；填了一半的必须报错，静默丢规则比不保存更糟。
pub(crate) fn rules_to_forwards(rows: &[RuleRow]) -> (Vec<Forward>, Vec<String>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        if r.blank() {
            continue;
        }
        let n = i + 1;
        let kind = r.kind.trim().to_ascii_uppercase();
        if !matches!(kind.as_str(), "L" | "D" | "R") {
            bad.push(format!("第 {n} 行：类型只能是 L、D 或 R"));
            continue;
        }
        let listen = match r.listen.trim() {
            "" | "0" => 0,
            other => match other.parse::<u16>() {
                Ok(p) => p,
                Err(_) => {
                    bad.push(format!("第 {n} 行：本地端口要么留空（自动分配），要么写成数字"));
                    continue;
                }
            },
        };
        let bind = (!r.bind.trim().is_empty()).then(|| r.bind.trim().to_string());
        let dest_host = r.dest_host.trim().to_string();
        let dest_port = match r.dest_port.trim() {
            "" => None,
            other => match other.parse::<u16>() {
                Ok(p) => Some(p),
                Err(_) => {
                    bad.push(format!("第 {n} 行：目标端口要写成数字"));
                    continue;
                }
            },
        };
        if kind == "D" {
            ok.push(Forward::Dynamic { bind, listen_port: listen });
            continue;
        }
        let mut missing: Vec<&str> = Vec::new();
        if dest_host.is_empty() {
            missing.push("目标主机");
        }
        if dest_port.is_none() {
            missing.push("目标端口");
        }
        if !missing.is_empty() {
            bad.push(format!("第 {n} 行：{}不能空着", missing.join("、")));
            continue;
        }
        let (dest_host, dest_port) = (dest_host, dest_port.unwrap());
        ok.push(match kind.as_str() {
            "L" => Forward::Local { bind, listen_port: listen, dest_host, dest_port },
            _ => Forward::Remote { bind, listen_port: listen, dest_host, dest_port },
        });
    }
    (ok, bad)
}

/// 这一行的本地口和谁撞：本表其他行 + 其他主机的规则。自动口什么都不撞。
///
/// 只比端口、不比 bind（与 `ells_core::host::ports_clash` 同一口径）：`*:8080` 和
/// `127.0.0.1:8080` 实际上也只能起来一个，提示多了不比漏了安全。
pub(crate) fn row_clash_text(rows: &[RuleRow], hosts: &[Host], alias: &str, idx: usize) -> String {
    let Some(port) = rows.get(idx).and_then(RuleRow::port) else { return String::new() };
    let mut names: Vec<String> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        if i != idx && r.port() == Some(port) {
            names.push(format!("本表第 {} 行", i + 1));
        }
    }
    for h in hosts {
        if h.alias != alias && h.forwards.iter().any(|f| f.fixed_local_port() == Some(port)) {
            names.push(h.alias.clone());
        }
    }
    if names.is_empty() {
        String::new()
    } else {
        format!("⚠ 与 {} 同用 {port} — 留空本地口即可自动分配", names.join("、"))
    }
}

pub struct JumpPicker {
    /// (alias, display label); an empty alias means "no jump host".
    pub items: Vec<(String, String)>,
    pub selected: usize,
}

pub struct FormState {
    pub fields: Vec<Field>,
    pub focus: usize,
    /// 底部按钮焦点：None=仍在输入框，Some(0)=保存，Some(1)=取消。
    pub footer: Option<usize>,
    pub editing_alias: Option<String>,
    pub error: Option<String>,
    pub jump_picker: Option<JumpPicker>,
    /// Ctrl-R 暂时显形：密码写错了看不见是最常见的一类"存完连不上"。
    pub reveal_secret: bool,
}

#[derive(Debug)]
pub struct TransferItem {
    pub label: String,
    pub direction: &'static str,
    pub progress: Option<Progress>,
    pub done: bool,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct BrowserState {
    pub path: String,
    pub entries: Vec<FileEntry>,
    pub selected: usize,
    /// 列表首个可见项（鼠标滚动/点击映射用）
    pub scroll: usize,
    pub loading: bool,
    pub error: Option<String>,
    pub transfers: Vec<TransferItem>,
}

impl BrowserState {
    fn reset(&mut self) {
        self.path = String::new();
        self.entries.clear();
        self.selected = 0;
        self.scroll = 0;
        self.loading = false;
        self.error = None;
    }
}

/// 一个标签页 = 一路 SSH 会话，外加它自己的 SFTP 浏览器、zmodem 挂起态与搜索状态。
/// 切标签只换 `App.active`：后台标签的远端输出继续进它自己的 vt100 缓冲，
/// 在跑的传输也继续，进度/完成事件靠 `id` 找回自己所属的标签。
#[derive(Default)]
pub struct Slot {
    pub id: u32,
    pub session: Option<SessionState>,
    pub sftp: Option<Arc<SftpSession>>,
    pub browser: BrowserState,
    /// 本标签要连（或已连上）的主机：标签名与断线重连都用它
    pub host: Option<Host>,
    /// 后台连接进行中
    pub connecting: bool,
    /// Tracked remote shell working directory (for hijacked sz/rz transfers).
    pub remote_cwd: String,
    /// sz files waiting for remote_cwd to resolve.
    pub sz_pending: Vec<String>,
    /// rz waiting for remote_cwd before opening the native picker.
    pub rz_pending: bool,
    /// Destination directory for the next upload (browser path or remote cwd).
    pub upload_dest: Option<String>,
    /// Browser was opened by an argument-less `sz` to pick a download target.
    pub sz_pick_mode: bool,
    /// Single-file `sz` waiting for remote_cwd before opening Save As.
    pub sz_saveas_pending: bool,
    /// ZMODEM event that arrived while a dialog was open; replayed after close.
    pub pending_zmodem: Option<crate::zmodem::ZmodemEvent>,
    /// Generation counter for the ZMODEM swallow-window timer (sliding).
    pub zclear_seq: u64,
    /// 历史输出搜索（内嵌终端专用，直通模式没有可回看的缓冲）
    pub search: Option<SearchState>,
    /// 本标签自己的页面（Session / Browser）：切回标签时恢复它当时看的东西。
    /// 列表 / 表单是全局页面，不记在这里。
    pub view: ScreenKind,
    /// 本标签的状态行（会话/浏览器页第二行）；列表页用 `App.status`
    pub status: Option<String>,
    /// 本标签所有传输共享的取消位（Ctrl-C 一次停本标签）
    pub cancel: Cancel,
    /// 会话输出落盘（`~/.ells/logs/<别名>-<时间>.log`）；没连上或关了记录就是 None。
    /// 跟着标签走：重连会换新的一条，旧文件留在磁盘上可回看。
    pub log: Option<ells_core::sessionlog::Recorder>,
    /// 自动重连：本轮已用到的第几次尝试（0 = 没有在等重连）
    pub reconnect_attempt: u32,
    /// 自动重连定时器代号：每次布防/主动断开都 +1，到点的旧定时器因此作废
    pub reconnect_seq: u64,
    /// 等重连的那台主机（`host` 在连接失败时会被清掉，这份是它自己的副本）
    pub reconnect_host: Option<Host>,
    /// 这次会话连上的时刻，用来判断断开算不算"站稳之后偶发的一次"
    pub connected_at: Option<std::time::Instant>,
    /// 会话页底部那排 CPU / 内存 / 磁盘条的数据与采集状态（每标签一份）
    pub metrics: Metrics,
}

/// 采集周期。5 秒是"CPU 这根条看着像活的"和"把服务器打穿"之间的位置：CPU 那一格
/// 本来就是两次 `/proc/stat` 做差，5 秒的窗口足够分辨"有人在跑东西"和"这根条坏了"，
/// 而内存和负载本来也是秒级的事。真正贵的是磁盘那一路，它单独按 60 秒走。
pub const METRICS_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// 磁盘**普查**每这么多轮才做一次：12 × 5 秒 = 60 秒。
///
/// 注意这不再是"磁盘 60 秒才问一次"：磁盘那一格跟着 5 秒的节奏换数，中间那些轮只对
/// 缓存里最满那块问一次 `statvfs`（挂载点表住在连接上，见 `ells_core::metrics::DiskState`）。
/// 分频分的是**普查** —— 读 `/proc/mounts` 再逐块问，是最贵的一条腿，而挂载点集合
/// 一分钟里几乎不动；真要说动，动的是"哪块最满"，那个答案本来就该等到下一个普查点。
/// 一台没有 `statvfs@openssh.com` 的服务器上，跟单轮什么都不问，`df` 只在这个普查点兜底。
pub const METRICS_DISK_EVERY_ROUNDS: u32 = 12;

/// 连上之后第一轮的等待：**不给延迟**。
///
/// 内存、负载、磁盘在连接建立的那一瞬间就已经能拿了：SFTP 子系统是在 `connect()` 里
/// 先于 PTY 打开的（服务器不支持时只会让文件传输降级，不会挡住会话），所以第一轮过去
/// 就有线上的数。让人对着空行等两秒，看着就像这一行坏了。
pub const METRICS_FIRST_DELAY: std::time::Duration = std::time::Duration::ZERO;

/// CPU 从基线到第二个点之间等多久：两秒。
///
/// CPU 那一格本来就是两次 `/proc/stat` 做差，第一个点只是基线、差不出数。刚连上时
/// 用户最想看到的偏偏就是它，所以拿到基线的那一轮之后单独用两秒接力；从第二轮起回到
/// `METRICS_EVERY`。这台机器没有 `/proc/stat` 时 `cpu_pending` 永远是 false，不会出现
/// "两秒一次猛采还差不出数"的空转。
pub const METRICS_CPU_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// 单轮采集的超时：比 5 秒的周期短，才不会两轮叠在一起。远端 `df` 卡在失效的 NFS
/// 挂载点上是真实场景，卡住的那一轮要被丢掉。
pub const METRICS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// 一轮回包的字节上限：几百个挂载点的 `df` 也就几十 KB，留足量的同时挡住
/// 那种把登录横幅写成几 MB 的机器。
pub const METRICS_MAX_BYTES: usize = 256 * 1024;

/// 连续这么多轮回包是空的（这台机器什么都不给），就认定它采不到：停止轮询，把那一行
/// 还给终端。
const METRICS_GIVE_UP: u32 = 3;

/// 背景标签的节奏：用户没在看的那几格降到一分钟一轮。
///
/// 一排标签各开一条采集通道时，服务器看到的是"每五秒一轮 × 标签数"，而其中绝大部分
/// 结果没人看 —— 换回去的时候再快采就行。一分钟这个数取的是磁盘那一档：一个后台标签
/// 每分钟一次 SFTP 读，和它本来就有的 keepalive 一个量级。
pub const METRICS_IDLE_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// 通道级失败（开不了通道、执行失败、超时）之后的间隔阶梯：5 → 10 → 30 → 60 封顶。
///
/// 这类失败说的是"这条连接这会儿不方便"，不是"这台机器没有可采的东西"：网络抖一下、
/// 服务器瞬间过载、被跳板机掐了一下，都不该让底部那一行永久消失。所以只退避、不判死 ——
/// 最慢一分钟一次，多个标签一起开着也压不出负载，而恢复是自动的：任意一轮拿到数就回到 5 秒。
pub const METRICS_BACKOFF: [std::time::Duration; 4] = [
    METRICS_EVERY,
    std::time::Duration::from_secs(10),
    std::time::Duration::from_secs(30),
    std::time::Duration::from_secs(60),
];

/// 连着失败这么多轮就退到阶梯的顶（60 秒）：这一刻值得说一句，因为数字从此每分钟才动一次。
const METRICS_CAPPED_AT: u32 = (METRICS_BACKOFF.len() as u32) - 1;

/// 分频与退避合成出下一次该等的时长：唯一的额外输入是"这一格是不是用户正看着的那一格"。
///
/// 正看着的按 `fast`（5 秒，连着失败就沿 `METRICS_BACKOFF` 退）；背景那几格不早于
/// `METRICS_IDLE_EVERY`。写成自由函数是为了让这条规则本身能被单测钉住 —— 它需要 `App`
/// 的标签表，而 `App` 的构造会去动真实的 `~/.ells`。
pub fn metrics_every(active: bool, fast: std::time::Duration) -> std::time::Duration {
    if active {
        fast
    } else {
        fast.max(METRICS_IDLE_EVERY)
    }
}

/// 一次回包之后下一轮该等多久。在 `metrics_every` 之上只多一条抢跑的规则：
///
/// 这一轮只拿到 CPU 基线、还差一个数才能做差，而用户正看着这一格 —— 那就两秒后接力，
/// 让他进会话时不必为了一格 CPU 多等三轮。背景那几格不抢（它本来就是分钟级），一台没有
/// `/proc/stat` 的机器 `pending_cpu` 永远是 false（见 `Metrics::adopt`），也不会两秒一次
/// 空转。写成自由函数同样是为了让这条规则本身能被单测钉住。
pub fn metrics_next(
    pending_cpu: bool,
    active: bool,
    fast: std::time::Duration,
) -> std::time::Duration {
    if pending_cpu && active {
        METRICS_CPU_WINDOW
    } else {
        metrics_every(active, fast)
    }
}

/// 磁盘那一格：使用率最高的那块**真实**挂载点（挑法见 `ells_core::metrics::Probe::worst_disk`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskGauge {
    pub mount: String,
    pub percent: u8,
    pub used_kb: u64,
    pub total_kb: u64,
}

/// 一个标签的底部指标行：既有面向绘制的"最近一次结果"，也有面向调度的"代号 + 连续失败"。
///
/// 节奏沿用自动重连那套（**没有全局 tick**）：一次性定时器睡到点发一个带 `seq` 的
/// 事件，结果回来后才布防下一轮；期间任何打断（断开、关标签、设置里关掉）都 +1 代号，
/// 在途的旧定时器和旧回包因此全部作废。
#[derive(Debug, Default)]
pub struct Metrics {
    /// CPU 使用率（%）；首轮没有基线，所以是 `None`
    pub cpu: Option<u8>,
    /// 内存使用率（%）
    pub mem: Option<u8>,
    /// 1/5/15 分钟平均负载；没有 `/proc/loadavg` 的机器是 `None`
    pub load: Option<ells_core::LoadSample>,
    /// 磁盘容量使用率（每一轮都跟着换数，但中间那些轮只跟单最满那块；见 `disk_rounds`）
    pub disk: Option<DiskGauge>,
    /// 上一轮 `/proc/stat` 快照：CPU 利用率是两次快照做差出来的
    prev_cpu: Option<ells_core::CpuSample>,
    /// 只拿到基线、还没差出 CPU 数：为 true 时下一轮用 `METRICS_CPU_WINDOW` 接力。
    pub cpu_pending: bool,
    /// 本轮采集的代号
    seq: u64,
    /// 有没有一轮在途（或已排期）。关掉指标、断开、换会话都要把它落回 false。
    polling: bool,
    /// 连续"回包是空的"的轮数（这台机器什么都不给）—— 只有这个计数能把采集判死
    misses: u32,
    /// 连续"通道级失败"的轮数：只用来退避，永远不参与判死
    fails: u32,
    /// 距下一轮**问磁盘**还剩几轮：0 就是这一轮要问。默认 0，所以连上的第一轮就有磁盘。
    disk_rounds: u32,
    /// 已判定这台机器采不到：不再布防，那一行也让给终端
    pub unsupported: bool,
}

impl Metrics {
    /// 布防下一轮：返回要写进定时器事件的代号。
    fn arm(&mut self) -> u64 {
        self.seq += 1;
        self.polling = true;
        self.seq
    }

    /// 作废在等的定时器与在途回包（断开、关标签、设置里关掉）。
    pub fn cancel(&mut self) {
        self.seq += 1;
        self.polling = false;
    }

    /// 换了一条会话（或用户重新打开了开关）：把上一台机器的数清掉重新计。
    /// 代号要留着继续往上走 —— 上一轮的在途回包不能对上新会话的代号。
    fn reset(&mut self) {
        let seq = self.seq;
        *self = Metrics::default();
        self.seq = seq;
    }

    /// 这一轮还该不该继续采。
    pub fn is_polling(&self) -> bool {
        self.polling
    }

    /// 这一轮要不要**普查**磁盘（重读 `/proc/mounts` + 逐块 `statvfs`，或兜底跑一次 `df`）。
    ///
    /// 每一轮都想要一个磁盘数，但普查不是：普查一轮有 `1 + 候选数` 个往返，而挂载点集合
    /// 一分钟里几乎不动。所以中间那 11 轮只做一次跟单（`ProbeTarget::gather` 里由
    /// `metrics::plan_disks` 决定跟哪块），到点这一轮才重排一次"谁最满"。
    pub fn disk_survey_due(&self) -> bool {
        self.disk_rounds == 0
    }

    /// 四项里有没有任何一项可用（决定那一行有没有内容可画）。
    pub fn has_data(&self) -> bool {
        self.cpu.is_some() || self.mem.is_some() || self.load.is_some() || self.disk.is_some()
    }

    /// 采纳一次解析结果，返回是否继续轮询。
    pub fn adopt(&mut self, probe: &ells_core::Probe) -> bool {
        match (self.prev_cpu, probe.cpu) {
            (Some(prev), Some(now)) => {
                self.cpu = ells_core::metrics::round_percent(ells_core::metrics::cpu_percent(
                    &prev, &now,
                ));
                self.prev_cpu = Some(now);
                self.cpu_pending = false;
            }
            // 首轮只有基线、没有可差的上一轮：这一格明说"还没数"，不填 0%
            (None, Some(now)) => {
                self.prev_cpu = Some(now);
                self.cpu = None;
                self.cpu_pending = true;
            }
            // 这台机器没有 /proc/stat（FreeBSD 之类）
            _ => {
                self.prev_cpu = None;
                self.cpu = None;
                self.cpu_pending = false;
            }
        }
        self.mem = ells_core::metrics::round_percent(
            probe.mem.as_ref().and_then(ells_core::metrics::mem_percent),
        );
        self.load = probe.load;
        // 磁盘那一格：这一轮问到数就换上（跟单轮每 5 秒都问得到同一块盘的新数），
        // 没问到就沿用上次的数 —— 那一格本来就该以"变化时才动"为主。
        if let Some(d) = probe.worst_disk() {
            self.disk = Some(DiskGauge {
                mount: d.mount.clone(),
                percent: d.used_percent,
                used_kb: d.used_kb,
                total_kb: d.total_kb,
            });
        }
        // 普查点：到点的这一轮**不管问到没问到**都往后数 11 轮。没问到也要数，是因为
        // "没问到"里就有这台没有 statvfs、df 又给不出盘的一类机器 —— 不数的话下一轮
        // 又是一次普查加一条 df，60 秒的节奏当场退化成 5 秒。
        if self.disk_rounds == 0 {
            self.disk_rounds = METRICS_DISK_EVERY_ROUNDS - 1;
        } else {
            self.disk_rounds -= 1;
        }
        // "这一轮有没有拿到东西"看这一轮本身，不能被缓存的磁盘数糊过去：不然一台
        // 早就什么都采不到的机器会靠着 60 秒前那块盘一直撑着轮询。
        self.note(probe.has_data())
    }

    /// 通道级失败（开不了通道、执行失败、超时）：慢下来，但这一行留着。
    ///
    /// 这一类**不**计进判死：它说的是"这会儿采不到"，不是"这台机器没有可采的东西"。
    /// 返回值仍是"继续轮询"，只是下一轮的间隔由 `interval()` 说了算。
    pub fn missed(&mut self) -> bool {
        self.fails += 1;
        !self.unsupported
    }

    /// 下一次采集该等多久：连着失败就沿 `METRICS_BACKOFF` 往上退。
    pub fn interval(&self) -> std::time::Duration {
        METRICS_BACKOFF[(self.fails as usize).min(METRICS_BACKOFF.len() - 1)]
    }

    /// 正好退到阶梯顶端的那一刻（之前还没到过），值得为它说一句状态行。
    pub fn just_capped(&self) -> bool {
        self.fails == METRICS_CAPPED_AT
    }

    fn note(&mut self, got_anything: bool) -> bool {
        if got_anything {
            self.misses = 0;
            // 拿到数就说明通道也是通的，退避同时结束
            self.fails = 0;
        } else {
            self.misses += 1;
            if self.misses >= METRICS_GIVE_UP {
                // 判定采不到：这一轮之后不再占通道，那一行也还给终端
                self.unsupported = true;
                self.polling = false;
            }
        }
        !self.unsupported
    }
}

impl Slot {
    fn new(id: u32) -> Self {
        Self {
            id,
            ..Default::default()
        }
    }

    /// 空闲标签 = 既没连上也没在连（启动时那一个，以及连接失败后剩下的）
    pub(crate) fn is_idle(&self) -> bool {
        self.session.is_none() && !self.connecting
    }

    /// 标签条上的名字：优先用主机别名，退回会话标签。
    pub(crate) fn title(&self) -> String {
        if let Some(host) = &self.host {
            return host.alias.clone();
        }
        self.session
            .as_ref()
            .map(|s| s.label.clone())
            .unwrap_or_else(|| "空标签".to_string())
    }
}

/// 通用确认弹窗：若干说明行 + 2~3 个按钮，答案用回调送回发起任务。
/// 主机密钥确认与传输覆盖确认共用它，避免每加一种确认就多一套状态与绘制代码。
pub struct Choice {
    pub title: String,
    pub lines: Vec<String>,
    pub options: Vec<String>,
    pub selected: usize,
    /// 快捷字母（如 y/n）到选项下标的映射
    pub shortcuts: &'static [(char, usize)],
    /// 危险态：红色标题（密钥变更这类安全告警）
    pub danger: bool,
    /// None = 用户按 Esc 关闭
    on_pick: Box<dyn FnOnce(Option<usize>) + Send>,
}

/// 传输目标已存在时的用户决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    Cancel,
    Rename,
    Overwrite,
}

/// 后台传输任务发给 UI 的覆盖确认请求（UI 回答后任务继续）。
pub struct ConflictPrompt {
    /// "远端" / "本地"
    pub location: &'static str,
    pub name: String,
    pub target: String,
    /// 目标已存在内容的大小（未知传 None）
    pub size: Option<u64>,
    pub responder: oneshot::Sender<Conflict>,
}

impl std::fmt::Debug for ConflictPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConflictPrompt")
            .field("location", &self.location)
            .field("name", &self.name)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// 历史输出搜索的常驻状态（F3，或回看历史时按 `/` 打开；Esc / 任意其它按键退出）。
#[derive(Debug)]
pub struct SearchState {
    pub query: String,
    /// 命中行在历史快照里的下标（由旧到新）
    pub hits: Vec<usize>,
    /// 当前停在 `hits` 的第几个
    pub cursor: usize,
    /// 快照对应的最大回看偏移（把下标换算成滚动量要用）
    pub max: usize,
    /// 当前命中行在终端视图里的行号（高亮带）
    pub view_row: u16,
}

/// 通用文本输入弹窗：远端新建目录 / 重命名 / 会话内搜索共用一套。
/// 回调在 UI 事件环里执行，只能靠自己捕获的克隆句柄干活（不能碰 App）。
pub struct Prompt {
    pub title: String,
    pub label: &'static str,
    pub buffer: String,
    pub error: Option<String>,
    /// 用于把输入拼成提示（如搜索的 "n 下一个"）；None = 无附加操作
    pub hint: Option<&'static str>,
    /// 允许空值提交（搜索框清空 = 取消过滤）
    pub allow_empty: bool,
    /// None = 用户取消
    on_done: Box<dyn FnOnce(Option<String>) + Send>,
}

/// 自更新的界面状态。徽标只在 `latest` 有值时画；`downloading`/`applied` 期间弹窗是模态的。
#[derive(Default)]
pub struct UpdateState {
    /// 确认比当前新的版本号（`v0.1.6`）；None = 无更新可提示
    pub latest: Option<String>,
    /// 后台检查在跑（启动即真，界面不为它单独画东西）
    pub checking: bool,
    /// 后台下载在跑
    pub downloading: bool,
    pub transferred: u64,
    pub total: Option<u64>,
    /// 已经替换到位，只差重启
    pub applied: bool,
    /// 换上去的目标版本（`latest` 在成功后要清空，弹窗文案得留着它）
    pub applied_tag: Option<String>,
    /// 失败文案：只在用户主动检查/更新时出现，启动那次静默（没网不该拦人）
    pub error: Option<String>,
    /// 上次检查成功的时刻（unix 秒），设置页说成"多久之前"
    pub checked_at: Option<u64>,
    /// 取消位：Clone 出的 Arc 交给后台任务，界面按【取消下载】置位
    cancel: Arc<AtomicBool>,
}

impl UpdateState {
    /// 弹窗是否占屏：下载中，或"已替换待重启"。
    pub fn modal(&self) -> bool {
        self.downloading || self.applied
    }

    /// 顶部徽标文案。绘制与命中测试共用这一份，宽度才不会两边算得不一致。
    pub fn badge(&self) -> Option<String> {
        self.latest.as_ref().map(|tag| format!("【{tag} 可更新】"))
    }

    /// 重新开始一轮下载：取消位必须是新的，否则下一次一点就"已取消"。
    fn reset_cancel(&mut self) {
        self.cancel = Arc::new(AtomicBool::new(false));
    }

    /// 请后台停下（真正中止要等它下一次读到这个位）。
    fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub struct App {
    pub screen: ScreenKind,
    pub unlock: UnlockState,
    pub list: ListState,
    pub form: FormState,
    /// 标签页。`active` 恒在 `0..slots.len()` 内：启动即有一个空标签，
    /// 关掉最后一个标签时也是替换而不是清空，所以下标取用不会越界。
    pub slots: Vec<Slot>,
    pub active: usize,
    /// 当前正在被处理的标签：事件自带 id 时是它（可能是后台标签），
    /// 键盘/鼠标来自界面时等于 `active`。所有会话级助手都读写 `slots[work]`，
    /// 需要动全局界面（切屏/弹系统对话框）时先判 `work == active`。
    work: usize,
    next_slot_id: u32,
    /// A native file dialog is currently on screen (only one at a time).
    dialog_open: bool,
    pub settings: Settings,
    /// 全局设置弹窗是否打开（会话界面顶部「设置」按钮触发）。
    pub settings_open: bool,
    /// 设置弹窗中当前聚焦的选项行（0=高亮 1=保活 2=主密码开关 3=修改主密码 4=快捷键
    /// 5=主题 6=检查更新 7=保存 8=取消）。
    pub settings_focus: usize,
    /// 快捷键设置子面板（从设置弹窗进入）。
    pub keybinds_open: bool,
    /// 快捷键面板聚焦项（0–7=动作，8=恢复默认，9=返回设置）。
    pub keybinds_focus: usize,
    /// 正在等待用户按下新按键的动作；非 None 时本面板吃掉全部按键。
    pub keybinds_recording: Option<Action>,
    /// 快捷键面板底部的一次性提示（绑定成功 / 冲突互换 / 拒绝原因）。
    pub keybinds_msg: Option<String>,
    /// 本次运行解锁用过的明文主密码（改密/关闭保护时写入自动解锁凭据需要它）。
    /// Zeroizing：这份密码要在整个运行期留着，drop 时必须抹掉，不能留在堆里。
    pub master_secret: Option<Zeroizing<String>>,
    /// 自动解锁时暂存的凭据，解锁成功后转入 master_secret。
    auto_master: Option<Zeroizing<String>>,
    pub mp_stage: MpStage,
    pub mp_buf: String,
    pub mp_busy: bool,
    mp_first: String,
    /// 主机列表删除的二级确认（存待删别名）。
    pub delete_confirm: Option<String>,
    /// 删除确认弹窗聚焦按钮（0=保留 1=删除）。
    pub confirm_index: usize,
    /// 当前确认弹窗（主机密钥 / 覆盖冲突），同一时刻最多一个。
    pub choice: Option<Choice>,
    /// 当前文本输入弹窗（新建目录 / 重命名 / 搜索），同一时刻最多一个。
    pub prompt: Option<Prompt>,
    /// 全键位帮助页（? / F1 打开，任意退出键关闭）。
    pub help_open: bool,
    /// 帮助页滚动行数：章节变多后一屏装不下，不滚动就看不到下面的内容。
    pub help_scroll: usize,
    /// 主机密钥策略：连接任务用它发问，UI 用它的通道回答。
    hostkey: HostKeyPolicy,
    /// 端口转发管理器：一台主机一条 SSH 连接承载它的全部规则。放在 App 而不是
    /// Slot 里——会话关闭不该把隧道一起带走（隧道有自己的重连退避）。
    tunnels: TunnelManager,
    /// 每台主机隧道的最新状态（列表标记 + 隧道面板）。只留最新一条：
    /// 历史在 `~/.ells/audit.log` 里，面板要的是"现在到底通不通"。
    pub tunnel_state: HashMap<String, TunnelState>,
    /// 隧道面板（列表页按 t 打开）。
    pub tunnels_open: bool,
    pub tunnels_selected: usize,
    /// 规则表格编辑器：隧道面板 Enter / 列表页 p 打开，改的是 `rules_alias` 那台的 forwards。
    pub rules_open: bool,
    pub rules_alias: String,
    pub rules_rows: Vec<RuleRow>,
    pub rules_row: usize,
    pub rules_col: usize,
    pub rules_error: Option<String>,
    /// 端口映射总表（m 打开）。数据只能来自隧道事件：自动分配的口只有它自己知道。
    pub ports_open: bool,
    pub tunnel_ports: HashMap<String, Vec<PortMap>>,
    /// known_hosts 管理页（列表页按 h 打开）。
    pub known_open: bool,
    pub known_entries: Vec<hostkey::KnownEntry>,
    pub known_selected: usize,
    /// 审计日志页（列表页按 l 打开）。
    pub audit_open: bool,
    pub audit_lines: Vec<String>,
    /// 审计面板的滚动行（记录是"末尾若干行"，所以从 0 往上翻是往更早看）。
    pub audit_scroll: usize,
    /// 会话记录页（列表页按 v 打开）：先列文件，Enter 看内容。
    pub sessions_open: bool,
    pub sessions_rows: Vec<sessionlog::Entry>,
    pub sessions_selected: usize,
    /// 正在查看的那份日志（已去掉控制序列的正文）。
    pub sessions_text: Option<String>,
    pub sessions_scroll: usize,
    /// 主机列表过滤词（`/` 输入，空 = 全显示）。
    pub filter: String,
    /// 过滤词正在被编辑（`/` 进入，Enter/Esc 退出）：此时按键进过滤框而不是列表键位。
    pub filtering: bool,
    /// known_hosts 页的一次性提示（删不掉 ~/.ssh 记录时要说清原因）。
    pub known_msg: Option<String>,
    /// 传输详情弹窗是否打开（顶部聚合进度条触发）。
    pub transfer_popup: bool,
    /// 自更新状态（启动后台查一次，徽标点开才下载）。
    pub update: UpdateState,
    /// 更新已替换到位，退出 `loop_run` 后由 `run()` 负责拉起新版本。
    pub restart_after_exit: bool,
    /// 最近一次绘制的终端区域，用于把鼠标坐标映射到顶部按钮。
    pub last_area: Rect,
    pub vault: Vault,
    pub vault_key: Option<VaultKey>,
    /// 待连接的主机 + 落到哪个标签下标（选标签的规则在 `start_connect` 里定）。
    pub pending_connect: Option<(Host, usize)>,
    pub direct_alias: Option<String>,
    pub pending_unlock_action: bool,
    pub status: Option<String>,
    pub done: bool,
    /// 退出二次确认：多标签后台还在传输时，第一次 q 只提示。
    pub quit_confirm: bool,
    /// 关闭标签二次确认（存待关标签的 id）：该标签还在传输时，第一次只提示。
    pub close_tab_confirm: Option<u32>,
    /// 下一帧画之前要不要先整屏重绘。`Ctrl-L` 直接置位；屏幕换主（直通交回来、
    /// 原生文件框关掉、最后一层弹窗关闭）由主循环自己判。
    repaint: bool,
    /// 上一帧有没有盖模态层、是不是直通、原生文件框在不在——这三样决定
    /// "上一帧那块屏幕是谁画的"，从有到无的那一刻必须全量重画一次。
    overlay_last: bool,
    passthrough_last: bool,
    dialog_last: bool,
    /// 上一次写入终端标签名的文本（变化才重发 OSC 0）。
    term_title: Option<String>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    event_rx: mpsc::UnboundedReceiver<AppEvent>,
}

/// 返回 `Ok(true)` 表示更新已替换到位、调用方（主线程）负责拉起新版本：
/// unix 上 restart 走 exec，必须发生在主线程，工作线程里只能做标记。
pub async fn run(alias: Option<String>, dev: bool, yes: bool) -> Result<bool> {
    enable_raw_mode()?;
    let mut out = stdout();
    // 括号粘贴：粘贴整段命令时终端会包上 ESC[200~/201~，ells 据此把它当一次
    // 粘贴处理，而不是逐字符输入（也避免换行被当成回车立刻执行）。
    execute!(out, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let (tx, rx) = mpsc::unbounded_channel::<AppEvent>();
    events::spawn_input_stream(tx.clone());

    // 主机密钥发问走 UI 事件环：连接必须在后台任务里跑完，
    // 否则等待弹窗回答时会把事件环卡死（弹窗没人按键 = 死锁）。
    let (hkey_tx, mut hkey_rx) = mpsc::unbounded_channel::<HostKeyPrompt>();
    let forward = tx.clone();
    tokio::spawn(async move {
        while let Some(prompt) = hkey_rx.recv().await {
            if forward.send(AppEvent::HostKey(prompt)).is_err() {
                return;
            }
        }
    });

    // 隧道状态变化同样并进 UI 事件环：管理器在后台任务里跑，界面只收最新状态。
    let (tun_tx, mut tun_rx) = mpsc::unbounded_channel::<TunnelEvent>();
    let forward = tx.clone();
    tokio::spawn(async move {
        while let Some(event) = tun_rx.recv().await {
            if forward.send(AppEvent::Tunnel(event)).is_err() {
                return;
            }
        }
    });
    let policy = HostKeyPolicy::new(hkey_tx, yes);
    let tunnels = TunnelManager::new(tun_tx, policy.clone());

    let mut app = App::startup(tx, rx, alias, dev, policy, tunnels);
    let result = app.loop_run(&mut terminal).await;
    let restart = std::mem::take(&mut app.restart_after_exit);

    term::restore_terminal();
    terminal.show_cursor()?;
    result?;
    if restart {
        return Ok(true);
    }
    Ok(false)
}

/// [0,1) 的伪随机数：重连抖动只要把"同一秒掉线的几路"错开，不需要好熵，
/// 所以不为此引一个随机数依赖。
fn pseudo_spread() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mixed = nanos ^ (std::process::id() as u64) << 17;
    ((mixed % 1_000_000) as f64) / 1_000_000.0
}

/// 这次失败是不是卡在主机密钥上（我们自己打的文案 + russh 的文案）。
fn key_trouble(err: &str) -> bool {
    err.contains("主机密钥") || err.contains("server key") || err.contains("known_hosts")
}

/// 分组排序权重：有名字的组合并同类、按名字排，没分组的恒排最后。
/// 返回元组而不是字符串，是为了让"未分组"不必真的占一个组名。
fn group_rank(group: Option<&str>) -> (u8, String) {
    match group {
        Some(g) if !g.trim().is_empty() => (0, g.trim().to_lowercase()),
        _ => (1, String::new()),
    }
}

impl App {
    fn startup(
        tx: mpsc::UnboundedSender<AppEvent>,
        rx: mpsc::UnboundedReceiver<AppEvent>,
        alias: Option<String>,
        dev: bool,
        hostkey: HostKeyPolicy,
        tunnels: TunnelManager,
    ) -> Self {
        let settings = Settings::load();
        // 保险库、known_hosts、自动解锁凭据都在 ~/.ells：先把它收紧到仅当前用户可读
        ells_core::harden_config_dir();
        ells_core::ssh::set_keepalive_interval(settings.keepalive_secs);
        // 上一次更新留下的备份件与陈旧临时件：Windows 上运行中的 exe 删不掉，只能等这次
        update::cleanup_leftovers();
        // 会话记录按天轮转：不主动清，一个月就是几百个文件，列表页翻不到重点
        sessionlog::prune();
        let mut base = Self {
            screen: ScreenKind::List,
            unlock: UnlockState {
                input: String::new(),
                error: None,
                stage: UnlockStage::Open,
                pending_master: None,
                busy: false,
            },
            list: ListState {
                selected: 0,
                folded: Vec::new(),
            },
            form: FormState::blank(),
            slots: vec![Slot::new(0)],
            active: 0,
            work: 0,
            next_slot_id: 1,
            dialog_open: false,
            settings,
            settings_open: false,
            settings_focus: 0,
            keybinds_open: false,
            keybinds_focus: 0,
            keybinds_recording: None,
            keybinds_msg: None,
            master_secret: None,
            auto_master: None,
            mp_stage: MpStage::Idle,
            mp_buf: String::new(),
            mp_first: String::new(),
            mp_busy: false,
            delete_confirm: None,
            confirm_index: 0,
            choice: None,
            prompt: None,
            help_open: false,
            help_scroll: 0,
            hostkey,
            tunnels,
            tunnel_state: HashMap::new(),
            tunnels_open: false,
            tunnels_selected: 0,
            rules_open: false,
            rules_alias: String::new(),
            rules_rows: Vec::new(),
            rules_row: 0,
            rules_col: 0,
            rules_error: None,
            ports_open: false,
            tunnel_ports: HashMap::new(),
            known_open: false,
            known_entries: Vec::new(),
            known_selected: 0,
            audit_open: false,
            audit_lines: Vec::new(),
            audit_scroll: 0,
            sessions_open: false,
            sessions_rows: Vec::new(),
            sessions_selected: 0,
            sessions_text: None,
            sessions_scroll: 0,
            filter: String::new(),
            filtering: false,
            known_msg: None,
            transfer_popup: false,
            update: UpdateState::default(),
            restart_after_exit: false,
            last_area: Rect::ZERO,
            vault: Vault::default(),
            vault_key: None,
            pending_connect: None,
            direct_alias: alias,
            pending_unlock_action: false,
            status: None,
            done: false,
            quit_confirm: false,
            close_tab_confirm: None,
            repaint: false,
            overlay_last: false,
            passthrough_last: false,
            dialog_last: false,
            term_title: None,
            event_tx: tx,
            event_rx: rx,
        };
        // 每次启动都问一次（用户选的），缓存只用来把"上次检查"说成"多久之前"
        base.update.checked_at = update::read_cache().map(|c| c.checked_at);
        if base.settings.auto_update {
            base.spawn_update_check();
        }
        if dev {
            let mut app = base;
            app.vault = vault::load_dev_vault().unwrap_or_default();
            app.status = Some("开发模式：读取 ~/.ells/hosts.dev.toml".to_string());
            app.pending_unlock_action = true;
            return app;
        }
        let stage = if vault::vault_exists() {
            UnlockStage::Open
        } else {
            UnlockStage::CreateFirst
        };
        let mut app = base;
        app.screen = ScreenKind::Unlock;
        app.unlock.stage = stage;
        // 主密码保护被关闭过且存在凭据文件：直接后台自动解锁；
        // 失败（凭据过期/被手删）则回落到手动输入界面。
        if !app.settings.master_password_enabled && stage == UnlockStage::Open {
            if let Some(master) = crate::settings::read_master_backup() {
                let tx = app.event_tx.clone();
                app.auto_master = Some(Zeroizing::new(master.clone()));
                app.unlock.busy = true;
                let master = Zeroizing::new(master);
                tokio::spawn(async move {
                    let res = tokio::task::spawn_blocking(move || vault::unlock_vault(&master))
                        .await
                        .map_err(|e| format!("后台任务异常: {e}"))
                        .and_then(|r| r.map_err(|e| e.to_string()));
                    let _ = tx.send(AppEvent::VaultUnlocked(res));
                });
            }
        }
        app
    }

    /// 这一帧是否盖着 ells 自己的模态层（`ui::draw` 里那些先用 `Clear` 铺底、
    /// 再画边框面板的层）。必须与 `ui::draw` 的分层条件一致：漏一层就少一次
    /// 收尾重绘，那一层的残影就留下来了。
    fn overlays_open(&self) -> bool {
        self.choice.is_some()
            || self.prompt.is_some()
            || self.help_open
            || self.delete_confirm.is_some()
            || self.tunnels_open
            || self.rules_open
            || self.ports_open
            || self.known_open
            || self.audit_open
            || self.sessions_open
            || self.settings_open
            || self.keybinds_open
            || self.transfer_popup
            || self.update.modal()
            || self.form.jump_picker.is_some()
    }

    async fn loop_run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        while !self.done {
            // 界面（键盘/鼠标/绘制）永远作用于当前标签：先把手工作区切回来
            self.work = self.active;
            self.sync_term_title();
            // `ells <别名>` / `s <别名>`：保险库就绪后一次性直连，不停在主机列表
            if std::mem::take(&mut self.pending_unlock_action) {
                self.try_direct_connect();
            }
            let passthrough = self.screen == ScreenKind::Session
                && self.slots[self.active].session.as_ref().map(|s| s.mode) == Some(TermMode::Passthrough);
            if passthrough {
                // 直通期间整块屏幕归远端，ells 一个字节都不画：重绘请求攒着，等屏幕交回来再用
                self.passthrough_last = true;
            } else {
                let overlays = self.overlays_open();
                let dialog = self.dialog_open;
                // 屏幕换主的这一帧必须整屏重画。直通期间是远端在往 stdout 直接写字、原生文件框
                // 和弹窗各自盖住一片格子，而 ratatui 只补它以为变了的那几个格子；Windows 10 的
                // 控制台在这些位置会把上一画面的残片（尤其被切断的全角字半个格子）一直留在那儿，
                // 差分永远等不到有人重写它 —— 这就是"弹窗关掉后还有残留"。
                // terminal.clear() 会清屏并作废差分基线，于是这一帧是真正的全量第一帧。
                if self.repaint || self.passthrough_last
                    || self.dialog_last && !dialog
                    || self.overlay_last && !overlays
                {
                    terminal.clear()?;
                    self.repaint = false;
                }
                terminal.draw(|f| ui::draw(f, self))?;
                self.passthrough_last = false;
                self.overlay_last = overlays;
                self.dialog_last = dialog;
            }
            // Connect only AFTER the "正在连接…" frame is on screen.
            if let Some((host, idx)) = self.pending_connect.take() {
                self.spawn_connect(host, idx);
            }
            let Some(ev) = self.event_rx.recv().await else {
                break;
            };
            // 后台标签的事件绝不落到当前标签上：找不到对应标签就直接丢弃
            let Some(idx) = self.event_slot(&ev) else {
                continue;
            };
            self.work = idx;
            match ev {
                AppEvent::Key(key) => self.handle_key(key).await,
                AppEvent::Paste(text) => self.handle_paste(&text),
                AppEvent::Resize(cols, rows) => {
                    // 每个标签的终端都要跟着改尺寸，否则后台标签的换行会错位
                    for slot in &mut self.slots {
                        if let Some(s) = &mut slot.session {
                            s.handle_resize(cols, rows);
                        }
                    }
                }
                AppEvent::MousePress { column, row } => self.handle_mouse(column, row),
                AppEvent::MouseDrag { column, row } => self.handle_mouse_drag(column, row),
                AppEvent::MouseRelease { column, row } => self.handle_mouse_release(column, row),
                AppEvent::MouseScroll { delta } => self.handle_scroll(delta),
                AppEvent::RemoteData { bytes, .. } => {
                    if let Some(lg) = &mut self.slots[self.work].log {
                        lg.record(&bytes);
                    }
                    let mut zev = None;
                    let mut cmds = Vec::new();
                    if let Some(s) = &mut self.slots[self.work].session {
                        zev = s.handle_output(&bytes);
                        s.flush_pending_answers();
                        cmds = s.drain_commands();
                    }
                    for cmd in &cmds {
                        self.track_cwd_cmd(cmd);
                    }
                    if let Some(ev) = zev {
                        self.on_zmodem_event(ev);
                    }
                    // Sliding swallow window: rz/sz retries ZRINIT every ~3s;
                    // keep hiding output (and re-aborting) while bytes keep coming.
                    if self.slots[self.work].session.as_ref().is_some_and(|s| s.is_intercepting()) {
                        self.clear_zmodem_soon();
                    }
                }
                AppEvent::RemoteClosed { graceful, .. } => {
                    let closed_alias = self.slots[self.work]
                        .host
                        .as_ref()
                        .map(|h| h.alias.clone());
                    if let Some(alias) = &closed_alias {
                        let _ = audit::record(
                            AuditKind::Disconnect,
                            alias,
                            if graceful { "远端退出" } else { "连接断开" },
                        );
                    }
                    if let Some(mut lg) = self.slots[self.work].log.take() {
                        lg.note(if graceful {
                            "会话结束 · 远端退出"
                        } else {
                            "会话结束 · 连接断开"
                        });
                    }
                    let label = if let Some(mut s) = self.slots[self.work].session.take() {
                        s.close();
                        Some(s.label.clone())
                    } else {
                        None
                    };
                    // Ctrl-] 主动断开会先把 host 清掉，那种情况不弹重连确认
                    let reconnect = label.as_ref().map(|_| self.slots[self.work].host.clone()).flatten();
                    let slot = &mut self.slots[self.work];
                    slot.connecting = false;
                    slot.sftp = None;
                    slot.view = ScreenKind::List;
                    slot.connected_at = None;
                    // 在途的采集回包一并作废：会话都没了，不该再把数写进已经消失的那一行
                    slot.metrics.cancel();
                    slot.remote_cwd.clear();
                    slot.sz_pending.clear();
                    slot.rz_pending = false;
                    slot.sz_pick_mode = false;
                    slot.sz_saveas_pending = false;
                    slot.pending_zmodem = None;
                    slot.search = None;
                    if let Some(label) = label {
                        slot.status = Some(format!("[{label}] 会话已结束"));
                    }
                    if self.work == self.active {
                        // 用户正在看这一路：回到列表并（非主动断开时）提供重连
                        self.screen = ScreenKind::List;
                        self.settings_open = false;
                        self.transfer_popup = false;
                        self.dialog_open = false;
                    }
                    if let (true, Some(host)) = (self.work == self.active, reconnect) {
                        // 远端 shell 正常退出（exit/logout）是主动行为，不弹重连；
                        // 只有没收到 exit-status 的异常关闭才提供一键重连。
                        // 已有弹窗时不再叠加：一次只弹一个，且会孤儿掉前一个的应答通道
                        if graceful || self.choice.is_some() {
                            self.slots[self.work].reconnect_attempt = 0;
                        } else {
                            self.on_session_lost(self.work, host);
                        }
                    }
                }
                AppEvent::HostKey(prompt) => self.ask_host_key(prompt),
                AppEvent::UpdateChecked(res) => self.on_update_checked(res),
                AppEvent::UpdateAccepted => {
                    if let Some(tag) = self.update.latest.clone() {
                        self.spawn_update_apply(tag);
                    }
                }
                AppEvent::UpdateProgress { transferred, total } => {
                    self.update.transferred = transferred;
                    self.update.total = total;
                }
                AppEvent::UpdateDone(res) => self.on_update_done(res),
                AppEvent::Connected { res, .. } => self.on_connected(res),
                AppEvent::Reconnect { host, .. } => self.start_connect(host, Some(self.work)),
                AppEvent::AutoReconnect { host, attempt, seq, .. } => {
                    self.on_reconnect_timer(host, attempt, seq);
                }
                AppEvent::MetricsTick { seq, .. } => self.on_metrics_tick(self.work, seq),
                AppEvent::MetricsProbe { seq, res, .. } => {
                    self.on_metrics_probe(self.work, seq, res)
                }
                AppEvent::Conflict { prompt, .. } => self.ask_conflict(prompt),
                AppEvent::ImportHosts(hosts) => self.import_hosts(hosts),
                AppEvent::ExportSshConfig => self.export_ssh_config(),
                AppEvent::Tunnel(event) => self.on_tunnel_event(event),
                AppEvent::PickedFile { field, path } => {
                    if self.screen == ScreenKind::Form {
                        if let Some(p) = path {
                            if let Some(f) = self.form.fields.get_mut(field) {
                                f.value = p;
                            }
                            self.form.error = None;
                        }
                    }
                }
                AppEvent::VaultUnlocked(res) => {
                    self.unlock.busy = false;
                    match res {
                        Ok((v, k)) => {
                            self.vault = v;
                            self.vault_key = Some(k);
                            self.master_secret = match self.auto_master.take() {
                                Some(m) => Some(m),
                                // take 而不是 clone：输入框里那份明文跟着一起清掉
                                None => Some(Zeroizing::new(std::mem::take(&mut self.unlock.input))),
                            };
                            self.screen = ScreenKind::List;
                            self.pending_unlock_action = true;
                        }
                        Err(msg) => {
                            // 自动解锁失败：凭据已不可用，清掉以免下次又失败
                            if self.auto_master.take().is_some() {
                                crate::settings::clear_master_backup();
                                self.settings.master_password_enabled = true;
                                self.settings.save();
                                self.unlock.input.clear();
                                self.unlock.error =
                                    Some("自动解锁失败，已恢复主密码保护，请输入主密码".to_string());
                            } else {
                                self.unlock.error = Some(msg);
                                self.unlock.input.clear();
                            }
                        }
                    }
                }
                AppEvent::VaultCreated(res) => {
                    self.unlock.busy = false;
                    match res {
                        Ok(k) => {
                            self.vault = Vault::default();
                            self.vault_key = Some(k);
                            self.master_secret =
                                Some(Zeroizing::new(std::mem::take(&mut self.unlock.input)));
                            self.screen = ScreenKind::List;
                            self.pending_unlock_action = true;
                        }
                        Err(msg) => {
                            self.unlock.error = Some(msg);
                            self.unlock.stage = UnlockStage::CreateFirst;
                            self.unlock.pending_master = None;
                            self.unlock.input.clear();
                        }
                    }
                }
                AppEvent::MasterRotated(res) => {
                    self.mp_busy = false;
                    match res {
                        Ok((vk, new_master)) => {
                            let new_master = Zeroizing::new(new_master);
                            self.vault_key = Some(vk);
                            self.master_secret = Some(Zeroizing::new((*new_master).clone()));
                            self.mp_stage = MpStage::Idle;
                            self.mp_buf.clear();
                            self.mp_first.clear();
                            if !self.settings.master_password_enabled {
                                let _ = crate::settings::write_master_backup(new_master.as_str());
                            }
                            self.status = Some("主密码已修改，下次启动用新密码".to_string());
                            let _ = audit::record(AuditKind::MasterPassword, "vault", "主密码已修改");
                        }
                        Err(msg) => {
                            self.status = Some(format!("修改主密码失败：{msg}"));
                        }
                    }
                }
                AppEvent::PickedUpload { path, .. } => {
                    self.dialog_open = false;
                    match path {
                        Some(p) => self.start_upload(PathBuf::from(p)),
                        None => {
                            self.slots[self.work].status =
                                Some("已取消上传（未选择文件）".to_string());
                            self.slots[self.work].upload_dest = None;
                        }
                    }
                    self.replay_pending_zmodem();
                }
                AppEvent::PickedSave { entry, path, .. } => {
                    self.dialog_open = false;
                    match path {
                        Some(p) => {
                            if self.slots[self.work].sz_pick_mode {
                                self.slots[self.work].sz_pick_mode = false;
                                self.leave_picker_to_session();
                            }
                            self.start_download_to(entry, PathBuf::from(p));
                        }
                        None => {
                            self.slots[self.work].status =
                                Some("已取消下载（未选择保存位置）".to_string());
                        }
                    }
                    self.replay_pending_zmodem();
                }
                AppEvent::PickedUploadDir { path, .. } => {
                    self.dialog_open = false;
                    match path {
                        Some(p) => self.start_upload(PathBuf::from(p)),
                        None => {
                            self.slots[self.work].status =
                                Some("已取消上传（未选择目录）".to_string());
                            self.slots[self.work].upload_dest = None;
                        }
                    }
                    self.replay_pending_zmodem();
                }
                AppEvent::PickedSaveDir { entry, path, .. } => {
                    self.dialog_open = false;
                    match path {
                        Some(p) => {
                            if self.slots[self.work].sz_pick_mode {
                                self.slots[self.work].sz_pick_mode = false;
                                self.leave_picker_to_session();
                            }
                            self.start_download_dir(entry, PathBuf::from(p));
                        }
                        None => {
                            self.slots[self.work].status =
                                Some("已取消下载（未选择保存目录）".to_string());
                        }
                    }
                    self.replay_pending_zmodem();
                }
                AppEvent::SftpStarted {
                    label, direction, ..
                } => {
                    self.register_transfer(label, direction);
                }
                AppEvent::SftpOp { res, .. } => match res {
                    Ok(msg) => {
                        self.slots[self.work].status = Some(msg);
                        // 目录内容变了：立刻重扫，否则用户看到的还是旧列表
                        if self.work == self.active
                            && self.screen == ScreenKind::Browser
                            && !self.slots[self.work].browser.loading
                        {
                            let path = self.slots[self.work].browser.path.clone();
                            self.start_listing(path);
                        }
                    }
                    Err(err) => self.slots[self.work].status = Some(err),
                },
                AppEvent::Search { value, .. } => match value {
                    Some(q) if !q.trim().is_empty() => self.run_search(q.trim()),
                    // 取消或空关键词：搜索态结束，视图回到实时底部
                    _ => self.clear_search(),
                },
                AppEvent::SftpCwd { res, .. } => match res {
                    Ok(dir) => {
                        self.slots[self.work].remote_cwd = dir.clone();
                        if self.slots[self.work].rz_pending {
                            self.slots[self.work].rz_pending = false;
                            self.open_upload_picker(dir.clone());
                        }
                        if !self.slots[self.work].sz_pending.is_empty() {
                            let files = std::mem::take(&mut self.slots[self.work].sz_pending);
                            if files.len() == 1
                                && std::mem::take(&mut self.slots[self.work].sz_saveas_pending)
                            {
                                self.open_sz_save_as(&files[0], dir);
                            } else {
                                self.slots[self.work].sz_saveas_pending = false;
                                self.start_sz(files, dir);
                            }
                        }
                    }
                    Err(err) => {
                        self.slots[self.work].status =
                            Some(format!("无法解析远端目录: {err}"));
                        // 目录拿不到就别让 pending 状态过夜，否则会串到下一次 sz/rz
                        self.slots[self.work].rz_pending = false;
                        self.slots[self.work].sz_pending.clear();
                        self.slots[self.work].sz_saveas_pending = false;
                    }
                },
                AppEvent::ZmodemClear { seq, .. } => {
                    if seq == self.slots[self.work].zclear_seq {
                        // 无条件复位：裸 sz 只触发检测不进入吞流，但 watcher 的
                        // fired 状态同样需要清掉，否则下次 sz/rz 不会被检测。
                        if let Some(s) = &mut self.slots[self.work].session {
                            s.end_intercept();
                        }
                    }
                }
                AppEvent::SftpHome { res, .. } => match res {
                    Ok(dir) => self.start_listing(dir),
                    Err(err) => {
                        self.slots[self.work].browser.loading = false;
                        self.slots[self.work].browser.error = Some(err);
                    }
                },
                AppEvent::SftpListed { res, .. } => {
                    let slot = &mut self.slots[self.work];
                    slot.browser.loading = false;
                    match res {
                        Ok(entries) => {
                            slot.browser.entries = entries;
                            slot.browser.selected = slot
                                .browser
                                .selected
                                .min(slot.browser.entries.len().saturating_sub(1));
                            slot.browser.scroll = slot.browser.scroll.min(slot.browser.selected);
                            slot.browser.error = None;
                        }
                        Err(err) => slot.browser.error = Some(err),
                    }
                }
                AppEvent::SftpProgress { pr, .. } => {
                    let slot = &mut self.slots[self.work];
                    if let Some(item) = slot
                        .browser
                        .transfers
                        .iter_mut()
                        .find(|t| t.label == pr.label && !t.done)
                    {
                        item.progress = Some(pr);
                    } else {
                        let label = pr.label.clone();
                        slot.browser.transfers.push(TransferItem {
                            label,
                            direction: "?",
                            progress: Some(pr),
                            done: false,
                            error: None,
                        });
                    }
                }
                AppEvent::SftpDone { res, .. } => {
                    let slot = &mut self.slots[self.work];
                    match res {
                        Ok(label) => {
                            let dir = if let Some(item) = slot
                                .browser
                                .transfers
                                .iter_mut()
                                .find(|t| t.label == label && !t.done)
                            {
                                item.done = true;
                                item.direction.to_string()
                            } else {
                                "传输".to_string()
                            };
                            slot.status = Some(format!("{dir}完成：{label}"));
                            let _ = audit::record(AuditKind::Transfer, &label, &dir);
                        }
                        Err((label, msg)) => {
                            if let Some(item) = slot
                                .browser
                                .transfers
                                .iter_mut()
                                .find(|t| t.label == label && !t.done)
                            {
                                item.done = true;
                                item.error = Some(msg.clone());
                            }
                            let _ = audit::record(AuditKind::Transfer, &label, &format!("失败：{msg}"));
                            slot.status = Some(msg);
                        }
                    }
                    // 只重扫当前标签：后台标签回到它的浏览器时自然会重新列
                    if self.work == self.active
                        && self.screen == ScreenKind::Browser
                        && !self.slots[self.work].browser.loading
                    {
                        let path = self.slots[self.work].browser.path.clone();
                        self.start_listing(path);
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        // Some terminals deliver Ctrl+<letter> as a bare ASCII control char
        // (\x03 for Ctrl-C) without the CONTROL modifier; a bare control
        // char must never reach text inputs. 0x1C–0x1F 同理，它们是 Ctrl-
        // \ ] ^ _ 的字节值（mac/Linux 终端经 crossterm 会变成 Ctrl-4…7）。
        let key = match key.code {
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL) && c.is_control() =>
            {
                let code = match c as u32 {
                    0x01..=0x1a => KeyCode::Char((c as u32 + 0x60) as u8 as char),
                    0x1c..=0x1f => KeyCode::Char((c as u32 + 0x40) as u8 as char),
                    _ => return,
                };
                KeyEvent::new(code, KeyModifiers::CONTROL)
            }
            _ => key,
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // 确认弹窗（主机密钥 / 覆盖冲突）优先吃掉所有按键：此刻后台任务在等回答
        if self.choice.is_some() {
            self.handle_choice_key(&key);
            return;
        }
        if self.prompt.is_some() {
            self.handle_prompt_key(&key, ctrl);
            return;
        }
        if self.help_open {
            // 帮助页是全屏信息层：除了退出与滚动，其余一律不落到下层页面
            match key.code {
                KeyCode::Esc
                | KeyCode::Char('q')
                | KeyCode::Char('?')
                | KeyCode::F(1) => self.help_open = false,
                KeyCode::Up | KeyCode::Char('k') => self.help_scroll = self.help_scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => self.help_scroll += 1,
                KeyCode::PageUp | KeyCode::Char('u') => self.help_scroll = self.help_scroll.saturating_sub(8),
                KeyCode::PageDown | KeyCode::Char('d') | KeyCode::Enter => self.help_scroll += 8,
                KeyCode::Home | KeyCode::Char('g') => self.help_scroll = 0,
                _ => {}
            }
            return;
        }
        if self.settings_open {
            if self.keybinds_open {
                self.handle_keybinds_key(&key);
            } else {
                self.handle_settings_key(&key);
            }
            return;
        }
        if self.update.modal() {
            self.handle_update_key(&key);
            return;
        }
        if self.transfer_popup {
            // 详情弹窗是只读信息层：任意按键即关闭。
            self.transfer_popup = false;
            return;
        }
        if self.delete_confirm.is_some() {
            self.handle_delete_confirm_key(&key);
            return;
        }
        // 只读面板：隧道、主机密钥记录、审计日志、会话记录。放在删除确认之后，
        // 保证"正在等二级确认"时不会被面板抢走按键。
        // 叠着开的按层从最上面那个开始吃键：端口映射弹窗是从隧道面板里按 m 开出来的，
        // 两个标记同时为真时先判弹窗，否则 Esc 关掉的是背后那个面板、弹窗留在屏幕上。
        if self.ports_open {
            self.handle_ports_key(&key);
            return;
        }
        if self.rules_open {
            self.handle_rules_key(&key, ctrl);
            return;
        }
        if self.tunnels_open {
            self.handle_tunnels_key(&key);
            return;
        }
        if self.known_open {
            self.handle_known_key(&key);
            return;
        }
        if self.audit_open {
            self.handle_audit_key(&key);
            return;
        }
        if self.sessions_open {
            self.handle_sessions_key(&key);
            return;
        }
        // 标签页控制在列表/会话/浏览器页都可用；弹窗、帮助页和表单里不抢键。
        // 按键全部来自设置里的绑定表（F2/F5/F6/Ctrl-] 只是默认值）。
        let binds = self.settings.keybinds;
        if binds.matches(Action::NewTab, &key) {
            self.new_tab();
            return;
        }
        if binds.matches(Action::NextTab, &key) {
            self.cycle_tab(true);
            return;
        }
        if binds.matches(Action::PrevTab, &key) {
            self.cycle_tab(false);
            return;
        }
        if binds.matches(Action::HostList, &key) {
            self.toggle_host_list();
            return;
        }
        if binds.matches(Action::CloseTab, &key) {
            if self.screen == ScreenKind::Browser {
                self.slots[self.work].sz_pick_mode = false;
                self.detach_or_close_tab();
                return;
            }
            // 从会话页 Ctrl-G 来到列表后，这一格只有在这里能断开；
            // 还在拨号的标签不碰，那会儿 take() 掉不了正在跑的连接任务
            if self.screen == ScreenKind::List && self.slots[self.active].session.is_some() {
                self.detach_or_close_tab();
            }
        }
        match self.screen {
            ScreenKind::Session => self.handle_session_key(&key).await,
            ScreenKind::Unlock => self.handle_unlock_key(&key, ctrl),
            ScreenKind::List => self.handle_list_key(&key, ctrl),
            ScreenKind::Form => {
                self.handle_form_key(&key, ctrl);
            }
            ScreenKind::Browser => self.handle_browser_key(&key, ctrl),
        }
    }

    async fn handle_session_key(&mut self, key: &KeyEvent) {
        let binds = self.settings.keybinds;
        if binds.matches(Action::Browser, key) {
            self.open_browser();
            return;
        }
        // 会话页不能占用 `?`（那是远端的字符），帮助只用 F1；
        // 直通模式不绘制 ells 界面，此时开帮助只会把按键吞进空气里。
        if matches!(key.code, KeyCode::F(1))
            && self.slots[self.work].session.as_ref().map(|s| s.mode) != Some(TermMode::Passthrough)
        {
            self.help_open = true;
            return;
        }
        // 搜索态只占用 n/N/Esc：其它按键先退出搜索，再原样交给远端
        if self.slots[self.work].search.is_some() {
            let scrolled = self.slots[self.work].session.as_ref().is_some_and(|s| s.scroll > 0);
            match key.code {
                KeyCode::Esc => self.clear_search(),
                KeyCode::Char('n') if scrolled => self.step_search(true),
                KeyCode::Char('N') if scrolled => self.step_search(false),
                _ => {
                    self.clear_search();
                    self.forward_to_remote(key);
                }
            }
            return;
        }
        // 搜索键（默认 F3）；`/` 仅在已回看历史时可用（平时它是远端的路径字符）。
        let embedded = self.slots[self.work].session.as_ref().map(|s| s.mode) == Some(TermMode::Embedded);
        let scrolled = self.slots[self.work].session.as_ref().is_some_and(|s| s.scroll > 0);
        if embedded
            && (binds.matches(Action::Search, key)
                || (scrolled && matches!(key.code, KeyCode::Char('/'))))
        {
            self.open_search();
            return;
        }
        self.forward_to_remote(key);
    }

    fn forward_to_remote(&mut self, key: &KeyEvent) {
        let binds = self.settings.keybinds;
        let action = match &mut self.slots[self.work].session {
            Some(s) => s.handle_key(key, &binds),
            None => SessionAction::Keep,
        };
        if action == SessionAction::Detach {
            self.detach_or_close_tab();
        } else if action == SessionAction::Redraw {
            self.repaint = true;
        }
    }

    /// F3：搜索已回看的终端输出（内嵌模式专有）。
    fn open_search(&mut self) {
        let slot_id = self.slots[self.work].id;
        let buffer = self.slots[self.work].search.as_ref().map(|s| s.query.clone()).unwrap_or_default();
        let tx = self.event_tx.clone();
        self.prompt = Some(Prompt {
            title: "搜索历史输出".to_string(),
            label: "关键词",
            buffer,
            error: None,
            hint: Some("回车跳到首个命中 · n/N 下一条/上一条 · Esc 退出"),
            allow_empty: true,
            on_done: Box::new(move |value| {
                let _ = tx.send(AppEvent::Search { slot: slot_id, value });
            }),
        });
    }

    fn run_search(&mut self, query: &str) {
        let slot = &mut self.slots[self.work];
        let Some(s) = slot.session.as_mut() else {
            slot.search = None;
            return;
        };
        let (max, lines) = s.history_lines();
        let needle = query.to_lowercase();
        let hits: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if hits.is_empty() {
            slot.search = None;
            slot.status = Some(format!("历史输出里没有「{query}」（共 {} 行）", lines.len()));
            return;
        }
        // 从当前视图往下找第一个命中，到底了再回头（与 less 的 / 一致）
        let top = max.saturating_sub(s.scroll);
        let cursor = hits.iter().position(|i| *i > top).unwrap_or(0);
        let view_row = s.jump_history(max, hits[cursor]);
        slot.status = None;
        slot.search = Some(SearchState {
            query: query.to_string(),
            hits,
            cursor,
            max,
            view_row,
        });
    }

    fn step_search(&mut self, next: bool) {
        let slot = &mut self.slots[self.work];
        let (Some(search), Some(s)) = (slot.search.as_mut(), slot.session.as_mut()) else {
            return;
        };
        let count = search.hits.len();
        if count == 0 {
            return;
        }
        search.cursor = if next {
            (search.cursor + 1) % count
        } else {
            (search.cursor + count - 1) % count
        };
        let idx = search.hits[search.cursor];
        search.view_row = s.jump_history(search.max, idx);
    }

    fn clear_search(&mut self) {
        if self.slots[self.work].search.take().is_none() {
            return;
        }
        if let Some(s) = self.slots[self.work].session.as_mut() {
            s.emu.set_scrollback(0);
            s.scroll = 0;
        }
    }

    /// 事件属于哪个标签。会话级事件自带 id；那个标签已经被关掉就返回 None，
    /// 事件直接丢弃——后台标签的回复绝不能落到用户正在看的标签上。
    fn event_slot(&self, ev: &AppEvent) -> Option<usize> {
        let id = match ev {
            AppEvent::RemoteData { slot, .. }
            | AppEvent::RemoteClosed { slot, .. }
            | AppEvent::Connected { slot, .. }
            | AppEvent::Reconnect { slot, .. }
            | AppEvent::AutoReconnect { slot, .. }
            | AppEvent::MetricsTick { slot, .. }
            | AppEvent::MetricsProbe { slot, .. }
            | AppEvent::Conflict { slot, .. }
            | AppEvent::SftpCwd { slot, .. }
            | AppEvent::ZmodemClear { slot, .. }
            | AppEvent::PickedUpload { slot, .. }
            | AppEvent::PickedSave { slot, .. }
            | AppEvent::PickedUploadDir { slot, .. }
            | AppEvent::PickedSaveDir { slot, .. }
            | AppEvent::SftpStarted { slot, .. }
            | AppEvent::SftpOp { slot, .. }
            | AppEvent::Search { slot, .. }
            | AppEvent::SftpHome { slot, .. }
            | AppEvent::SftpListed { slot, .. }
            | AppEvent::SftpProgress { slot, .. }
            | AppEvent::SftpDone { slot, .. } => *slot,
            _ => return Some(self.active),
        };
        self.slots.iter().position(|s| s.id == id)
    }

    /// 当前处理中的标签 id：派后台任务时带上，回调才找得到自己那一路。
    fn slot_id(&self) -> u32 {
        self.slots[self.work].id
    }

    /// 切换当前标签的页面。会话/浏览器页记在标签上，切回标签时恢复原样。
    fn set_view(&mut self, view: ScreenKind) {
        self.screen = view;
        self.slots[self.work].view = view;
    }

    /// 标签当时看的页面：会话没了就只能回列表。
    fn slot_view(&self, idx: usize) -> ScreenKind {
        let slot = &self.slots[idx];
        match slot.view {
            ScreenKind::Session | ScreenKind::Browser if slot.session.is_some() => slot.view,
            _ => ScreenKind::List,
        }
    }

    /// 弹窗、系统对话框、表单开着时不允许换标签：挂在那里的回调会落到错误的标签上。
    fn tabs_locked(&self) -> bool {
        self.screen == ScreenKind::Form
            || self.screen == ScreenKind::Unlock
            || self.choice.is_some()
            || self.prompt.is_some()
            || self.dialog_open
            || self.help_open
    }

    /// 追加一个空标签，返回它的下标。
    fn push_tab(&mut self) -> usize {
        let id = self.next_slot_id;
        self.next_slot_id = self.next_slot_id.wrapping_add(1);
        self.slots.push(Slot::new(id));
        self.slots.len() - 1
    }

    /// 切到某个标签：页面跟着它自己走，全局弹窗与状态行一律清掉。
    fn focus_tab(&mut self, idx: usize) {
        if idx >= self.slots.len() || self.tabs_locked() {
            return;
        }
        // 点"自己那一格"= 从主机列表回到它当时的页面，所以只有真的换页才算一次切换
        let view = self.slot_view(idx);
        if idx == self.active && view == self.screen {
            return;
        }
        self.active = idx;
        self.work = idx;
        self.screen = view;
        self.settings_open = false;
        self.transfer_popup = false;
        self.status = None;
        // 切回来就要看得见数：这一格在背景时是一分钟一轮，那 58 秒的旧数不该继续挂着
        self.revive_metrics(idx);
        // 后台标签的目录可能已经被它自己的传输改过：切回来先重扫一次
        let slot = &self.slots[idx];
        if self.screen == ScreenKind::Browser
            && slot.sftp.is_some()
            && !slot.browser.loading
            && !slot.browser.path.is_empty()
        {
            let path = slot.browser.path.clone();
            self.start_listing(path);
        }
    }

    /// 保持连接回到主机列表（默认 Ctrl-G，可改键）。再按一次回到那一格标签当时的页面：
    /// 只换 `App.screen`，`slots[active].view` 不动，所以会话画面/浏览器目录都还在原地。
    fn toggle_host_list(&mut self) {
        if self.tabs_locked() {
            return;
        }
        if self.screen == ScreenKind::List {
            // 已经在列表页：这一格有活着的会话就切回去，还在拨号就报一声，别让按键像失灵
            if self.slots[self.active].session.is_some() {
                self.focus_tab(self.active);
                return;
            }
            if self.slots[self.active].connecting {
                let alias = self.slots[self.active]
                    .host
                    .as_ref()
                    .map(|h| h.alias.clone())
                    .unwrap_or_default();
                self.status = Some(format!("「{alias}」正在连接…"));
            }
            return;
        }
        if self.slots[self.active]
            .session
            .as_ref()
            .is_some_and(|s| s.mode == TermMode::Passthrough)
        {
            // 直通模式由远端独占屏幕，此刻 ells 自己的界面画了也没人看得见
            self.status = Some(format!(
                "直通模式下远端独占屏幕 · 先按 {} 回内嵌再看列表",
                self.settings.keybinds.display(Action::Passthrough)
            ));
            return;
        }
        self.screen = ScreenKind::List;
        self.settings_open = false;
        self.transfer_popup = false;
        let live = self.slots.iter().filter(|s| s.session.is_some()).count();
        self.status = if live > 0 {
            Some(format!(
                "{live} 路会话仍在后台运行 · 点击标签或选中该机按 Enter 切回去 · {} 新建标签",
                self.settings.keybinds.display(Action::NewTab)
            ))
        } else {
            None
        };
    }

    /// Enter 选中主机：这台已经连着（或正在连）就切回它那一格标签，绝不重复拨号。
    fn open_host(&mut self, host: Host) {
        let alive = self.slots.iter().position(|s| {
            s.host.as_ref().is_some_and(|h| h.alias == host.alias)
                && (s.session.is_some() || s.connecting)
        });
        match alive {
            Some(idx) => {
                // 还在拨号的那一格切过去也看不到会话画面（此刻它仍停在列表），
                // 说成"正在连接"比"已切到"更贴合用户看到的东西
                let dialing = self.slots[idx].session.is_none();
                self.focus_tab(idx);
                self.status = Some(if dialing {
                    format!("标签 {}「{}」正在连接…", idx + 1, host.alias)
                } else {
                    format!("已切到标签 {}「{}」", idx + 1, host.alias)
                });
            }
            None => self.start_connect(host, None),
        }
    }

    /// F2：新建标签。已经有空标签就直接复用它，一路狂加空页没有意义。
    fn new_tab(&mut self) {
        if self.tabs_locked() {
            return;
        }
        let idx = match self.slots.iter().position(Slot::is_idle) {
            Some(idx) => idx,
            None => self.push_tab(),
        };
        self.active = idx;
        self.work = idx;
        self.screen = ScreenKind::List;
        self.settings_open = false;
        self.transfer_popup = false;
        self.status = None;
    }

    /// F5 / F6：在标签间循环。
    fn cycle_tab(&mut self, next: bool) {
        if self.tabs_locked() || self.slots.len() < 2 {
            return;
        }
        let len = self.slots.len();
        let idx = if next {
            (self.active + 1) % len
        } else {
            (self.active + len - 1) % len
        };
        self.focus_tab(idx);
    }

    /// 关掉当前标签（断开它的会话）。关掉最后一个时换一个空标签，
    /// 保证界面上永远有可停留的页面，`active` 也不会越界。
    fn close_tab(&mut self) {
        if self.tabs_locked() {
            return;
        }
        if let Some(mut s) = self.slots[self.active].session.take() {
            s.close();
        }
        if let Some(mut lg) = self.slots[self.active].log.take() {
            lg.note("会话结束 · 关闭标签");
        }
        self.slots.remove(self.active);
        if self.slots.is_empty() {
            self.push_tab();
        }
        self.active = self.active.min(self.slots.len() - 1);
        self.work = self.active;
        self.screen = self.slot_view(self.active);
        self.settings_open = false;
        self.transfer_popup = false;
        self.status = None;
        // 落到的那一格现在是用户在看的那一格，别让它继续按背景的分钟节奏供数
        self.revive_metrics(self.active);
    }

    /// Ctrl-]：结束这一路会话。标签不止一个时关掉这个，只剩一个才退回列表。
    fn detach_or_close_tab(&mut self) {
        // 断开会话等于掐掉它手上的传输，第一次只提示；用标签 id 做键，
        // 切到别的标签或隔了一次别的操作就不会误当成"第二次确认"。
        let busy = self.slots[self.active]
            .browser
            .transfers
            .iter()
            .any(|t| !t.done);
        let id = self.slots[self.active].id;
        if busy && self.close_tab_confirm != Some(id) {
            self.close_tab_confirm = Some(id);
            self.status = Some(format!(
                "该标签还在传输，再按一次 {} 确认断开",
                self.settings.keybinds.display(Action::CloseTab)
            ));
            return;
        }
        self.close_tab_confirm = None;
        if self.slots.len() > 1 {
            self.close_tab();
        } else {
            self.detach_session("已返回列表");
        }
    }

    /// 系统弹窗关掉后，把期间挂起的 zmodem 检测继续走完。
    fn replay_pending_zmodem(&mut self) {
        let Some(ev) = self.slots[self.work].pending_zmodem.take() else {
            return;
        };
        self.on_zmodem_event(ev);
    }

    /// 无参数 `sz` 把浏览器借去当"挑一个要下载的文件"，选完就回会话页。
    fn leave_picker_to_session(&mut self) {
        if self.work == self.active {
            self.set_view(ScreenKind::Session);
        } else {
            self.slots[self.work].view = ScreenKind::Session;
        }
    }

    /// 标签条命中测试：列表页/会话页/浏览器页共用第 1 行的同一份几何。
    /// 返回 true 表示这一下已被标签条吃掉，页面自己的命中测试不用再跑。
    fn hit_tab_bar(&mut self, column: u16, row: u16) -> bool {
        // 标签宽度随标题长度走，所以命中测试必须拿同一份标题来算，不能用个数
        let titles: Vec<String> = self.slots.iter().map(Slot::title).collect();
        if hit(ui::tab_new_rect(self.last_area, &titles), column, row) {
            self.new_tab();
            return true;
        }
        if let Some((idx, _)) = ui::tab_rects(self.last_area, &titles)
            .into_iter()
            .find(|(_, r)| hit(*r, column, row))
        {
            self.focus_tab(idx);
            return true;
        }
        false
    }

    /// 顶部按钮行/弹窗的鼠标命中测试。
    fn handle_mouse(&mut self, column: u16, row: u16) {
        if self.choice.is_some() {
            let (lines, options) = match &self.choice {
                Some(c) => (c.lines.len(), c.options.len()),
                None => (0, 0),
            };
            let (_, buttons) = ui::choice_rects(self.last_area, lines, options);
            if let Some(idx) = buttons.iter().position(|r| hit(*r, column, row)) {
                self.answer_choice(Some(idx));
            } else {
                // 点在面板外：Esc 同义（拒绝），绝不把点击透传到下层页面
                self.answer_choice(None);
            }
            return;
        }
        if self.prompt.is_some() {
            // 输入弹窗只认键盘；点面板外 = 取消，绝不把点击透传给下层页面
            let panel = ui::prompt_rect(self.last_area);
            if !hit(panel, column, row) {
                self.answer_prompt(None);
            }
            return;
        }
        if self.help_open {
            // 帮助页几乎占满屏幕，无法区分内外：任意按下即关闭，不透传给下层页面
            self.help_open = false;
            return;
        }
        if self.settings_open {
            if self.mp_stage != MpStage::Idle {
                // 改密输入中：鼠标不参与，只认键盘
                return;
            }
            if self.keybinds_recording.is_some() {
                // 录制中：等键盘输入，鼠标不参与（否则会误取消/改错行）
                return;
            }
            if self.keybinds_open {
                let rects = ui::keybinds_hit_rects(self.last_area);
                for (idx, rect) in rects.iter().enumerate() {
                    if !hit(*rect, column, row) {
                        continue;
                    }
                    self.keybinds_focus = idx;
                    if idx < Action::ALL.len() {
                        self.keybinds_recording = Some(Action::ALL[idx]);
                    } else if idx == Action::ALL.len() {
                        self.reset_keybinds();
                    } else {
                        self.close_keybinds();
                    }
                    return;
                }
                return;
            }
            let [
                hl_r,
                ka_r,
                mp_r,
                change_r,
                kb_r,
                theme_r,
                update_r,
                slog_r,
                mt_r,
                rc_r,
                save_r,
                cancel_r,
            ] = ui::settings_hit_rects(self.last_area);
            if hit(hl_r, column, row) {
                self.settings_focus = 0;
                self.settings.highlight = !self.settings.highlight;
            } else if hit(ka_r, column, row) {
                self.settings_focus = 1;
                self.settings.keepalive_secs = cycle_keepalive(self.settings.keepalive_secs);
            } else if hit(mp_r, column, row) {
                self.toggle_master_setting();
            } else if hit(change_r, column, row) {
                self.begin_master_change();
            } else if hit(kb_r, column, row) {
                self.open_keybinds();
            } else if hit(theme_r, column, row) {
                self.settings_focus = 5;
                self.step_theme(false);
            } else if hit(update_r, column, row) {
                self.settings_focus = 6;
                self.run_update_row();
            } else if hit(slog_r, column, row) {
                self.settings_focus = 7;
                self.settings.session_log = !self.settings.session_log;
            } else if hit(mt_r, column, row) {
                self.settings_focus = 8;
                self.set_metrics_setting(!self.settings.metrics);
            } else if hit(rc_r, column, row) {
                self.settings_focus = 9;
                self.settings.reconnect.max_attempts =
                    cycle_reconnect_attempts(self.settings.reconnect.max_attempts);
            } else if hit(save_r, column, row) {
                self.save_settings();
            } else if hit(cancel_r, column, row) {
                self.cancel_settings();
            }
            return;
        }
        if self.update.modal() {
            // 更新弹窗是模态的：面板外的点击也不许穿到下层页面
            let panel = ui::update_popup_rect(self.last_area);
            let [main_r, alt_r] = ui::update_button_rects(self.last_area);
            if self.update.downloading {
                if hit(main_r, column, row) {
                    self.update.request_cancel();
                    self.status = Some("正在取消下载…".to_string());
                }
            } else if hit(main_r, column, row) {
                self.quit_for_restart();
            } else if hit(alt_r, column, row) || !hit(panel, column, row) {
                self.dismiss_update();
            }
            return;
        }
        if self.transfer_popup {
            self.transfer_popup = false;
            return;
        }
        if self.delete_confirm.is_some() {
            let [keep_r, del_r] = ui::delete_confirm_rects(self.last_area);
            if hit(keep_r, column, row) {
                self.delete_confirm = None;
            } else if hit(del_r, column, row) {
                self.confirm_delete();
            }
            return;
        }
        // 端口映射弹窗：点外面就关，表本身只读不必选。
        if self.ports_open {
            if !hit(ui::ports_rect(self.last_area), column, row) {
                self.ports_open = false;
            }
            return;
        }
        // 规则表格：只有点中某个格子才移动光标；点面板外不关——编辑到一半误点一下
        // 就把改动丢了，比"关不掉"糟得多。要退出请用 Esc（放弃）或 Enter（保存）。
        // 格子矩形来自 rules_cell_rects，里面已经算好滚动窗口，所以点到的行号就是表内行号。
        if self.rules_open {
            let (_, window) = ui::rules_cell_rects(self.last_area, self.rules_rows.len(), self.rules_row);
            for (i, cols) in window {
                for (j, r) in cols.iter().enumerate() {
                    if hit(*r, column, row) {
                        self.rules_row = i;
                        self.rules_col = j;
                        self.rules_error = None;
                        return;
                    }
                }
            }
            return;
        }
        if self.tunnels_open {
            let (panel, _chip, list) = ui::tunnel_rects(self.last_area);
            if hit(ui::tunnel_ports_chip_rect(self.last_area), column, row) {
                self.open_ports();
                return;
            }
            if hit(list, column, row) {
                let rows = self.tunnel_rows();
                let idx = (row - list.y) as usize;
                if idx < rows.len() {
                    self.tunnels_selected = idx;
                }
                return;
            }
            if !hit(panel, column, row) {
                self.tunnels_open = false;
            }
            return;
        }
        match self.screen {
            ScreenKind::Session => {
                if hit(ui::homepage_rect(self.last_area), column, row) {
                    self.open_homepage();
                    return;
                }
                // 标签条：点标签切过去，点末尾的 + 新建一个
                if self.hit_tab_bar(column, row) {
                    return;
                }
                let [settings_r, upload_r, download_r, list_r, progress_r] =
                    ui::header_button_rects(self.last_area);
                if hit(settings_r, column, row) {
                    self.open_settings();
                } else if hit(upload_r, column, row) {
                    self.trigger_upload();
                } else if hit(download_r, column, row) {
                    self.trigger_download();
                } else if hit(list_r, column, row) {
                    self.toggle_host_list();
                } else if hit(progress_r, column, row) && !self.slots[self.work].browser.transfers.is_empty() {
                    self.transfer_popup = true;
                } else if hit(
                    ui::session_emu_rect(self.last_area, self.footer_rows(self.work)),
                    column,
                    row,
                ) {
                    // 终端区按下 = 开始拖选（鼠标捕获后原生选择失效，由 ells 自绘）
                    if let Some(s) = &mut self.slots[self.work].session {
                        s.begin_selection(column, row);
                    }
                }
            }
            ScreenKind::Browser => {
                if self.hit_tab_bar(column, row) {
                    return;
                }
                let list = ui::browser_layout(self.last_area)[2];
                if row >= list.y && row < list.y.saturating_add(list.height) {
                    let idx = (row - list.y) as usize + self.slots[self.work].browser.scroll;
                    if idx < self.slots[self.work].browser.entries.len() {
                        self.slots[self.work].browser.selected = idx;
                    }
                }
            }
            ScreenKind::Form => {
                if self.form.jump_picker.is_some() {
                    return;
                }
                let vis = self.form.visible();
                let inner = ui::form_inner(self.last_area, vis.len());
                if row >= inner.y && row < inner.y + vis.len() as u16 {
                    // 点在输入行上：聚焦该输入框（footer 焦点态下打字会自然回落）
                    self.form.footer = None;
                    self.form.focus = vis[(row - inner.y) as usize];
                    self.form.error = None;
                    return;
                }
                let [save_r, cancel_r] = ui::form_button_rects(inner, vis.len());
                if hit(save_r, column, row) {
                    self.form_submit();
                } else if hit(cancel_r, column, row) {
                    self.form.error = None;
                    self.form.footer = None;
                    self.screen = ScreenKind::List;
                }
            }
            ScreenKind::List => {
                if self.hit_tab_bar(column, row) {
                    return;
                }
                // 徽标只画在第 0 行，命中范围跟着文案宽度走（同源，见 UpdateState::badge）
                if let Some(label) = self.update.badge() {
                    if hit(ui::update_badge_rect(self.last_area, &label), column, row) {
                        self.offer_update();
                        return;
                    }
                }
                if hit(ui::homepage_rect(self.last_area), column, row) {
                    self.open_homepage();
                } else if hit(ui::list_settings_rect(self.last_area), column, row) {
                    self.open_settings();
                }
            }
            _ => {}
        }
    }

    fn handle_mouse_drag(&mut self, column: u16, row: u16) {
        if self.screen != ScreenKind::Session {
            return;
        }
        if let Some(s) = &mut self.slots[self.work].session {
            s.update_selection(column, row);
        }
    }

    fn handle_mouse_release(&mut self, column: u16, row: u16) {
        if self.screen != ScreenKind::Session {
            return;
        }
        let area = ui::session_emu_rect(self.last_area, self.footer_rows(self.work));
        let text = match &mut self.slots[self.work].session {
            Some(s) => {
                s.update_selection(column, row);
                s.selected_text(area)
            }
            None => return,
        };
        if !text.is_empty() {
            term::copy_osc52(&text);
            let n = text.chars().count();
            self.slots[self.work].status = Some(format!("已复制 {n} 个字符（OSC 52 剪贴板）"));
        }
    }

    /// 滚轮：会话=回看历史；浏览器=移动选择（列表窗口跟随滚动）。
    fn handle_scroll(&mut self, delta: i8) {
        match self.screen {
            ScreenKind::Session => {
                if let Some(s) = &mut self.slots[self.work].session {
                    s.handle_wheel(delta);
                }
            }
            ScreenKind::Browser => {
                if self.slots[self.work].browser.entries.is_empty() {
                    return;
                }
                let height = ui::browser_layout(self.last_area)[2].height.max(1) as usize;
                let len = self.slots[self.work].browser.entries.len();
                if delta > 0 {
                    self.slots[self.work].browser.selected = (self.slots[self.work].browser.selected + 1).min(len - 1);
                } else {
                    self.slots[self.work].browser.selected = self.slots[self.work].browser.selected.saturating_sub(1);
                }
                let b = &mut self.slots[self.work].browser;
                if b.selected >= b.scroll + height {
                    b.scroll = b.selected - height + 1;
                }
                if b.selected < b.scroll {
                    b.scroll = b.selected;
                }
            }
            _ => {}
        }
    }

    /// 顶部「上传」按钮 = rz 功能：解析远端当前目录后弹系统文件选择框。
    fn trigger_upload(&mut self) {
        if self.slots[self.work].sftp.is_none() {
            self.slots[self.work].status = Some("该服务器不支持 SFTP 文件传输".to_string());
            return;
        }
        if self.slots[self.work].remote_cwd.is_empty() {
            self.slots[self.work].rz_pending = true;
            self.refresh_remote_cwd();
        } else {
            let dir = self.slots[self.work].remote_cwd.clone();
            self.open_upload_picker(dir);
        }
    }

    /// 顶部「下载」按钮 = sz 功能：打开远端文件浏览器选择下载目标。
    fn trigger_download(&mut self) {
        if self.slots[self.work].sftp.is_none() {
            self.slots[self.work].status = Some("该服务器不支持 SFTP 文件传输".to_string());
            return;
        }
        self.slots[self.work].sz_pick_mode = true;
        self.open_browser();
    }

    fn handle_settings_key(&mut self, key: &KeyEvent) {
        if self.mp_stage != MpStage::Idle {
            self.handle_master_input(key);
            return;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.settings_focus = self.settings_focus.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.settings_focus = (self.settings_focus + 1).min(11);
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.apply_settings_focus(),
            KeyCode::Left => match self.settings_focus {
                0 => self.settings.highlight = !self.settings.highlight,
                1 => {
                    self.settings.keepalive_secs =
                        step_keepalive(self.settings.keepalive_secs, false);
                }
                2 => self.toggle_master_setting(),
                5 => self.step_theme(true),
                6 => self.settings.auto_update = false,
                7 => self.settings.session_log = false,
                8 => {
                    self.set_metrics_setting(false);
                }
                9 => {
                    self.settings.reconnect.max_attempts =
                        step_reconnect_attempts(self.settings.reconnect.max_attempts, false);
                }
                _ => {}
            },
            KeyCode::Right => match self.settings_focus {
                0 => self.settings.highlight = !self.settings.highlight,
                1 => {
                    self.settings.keepalive_secs =
                        step_keepalive(self.settings.keepalive_secs, true);
                }
                2 => self.toggle_master_setting(),
                5 => self.step_theme(false),
                6 => self.settings.auto_update = true,
                7 => self.settings.session_log = true,
                8 => {
                    self.set_metrics_setting(true);
                }
                9 => {
                    self.settings.reconnect.max_attempts =
                        step_reconnect_attempts(self.settings.reconnect.max_attempts, true);
                }
                _ => {}
            },
            KeyCode::Char('h') => self.settings.highlight = !self.settings.highlight,
            KeyCode::Esc => self.cancel_settings(),
            _ => {}
        }
    }

    /// 修改主密码的掩码输入：First 收集新密码，Confirm 复核。
    fn handle_master_input(&mut self, key: &KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.mp_stage = MpStage::Idle;
                self.mp_buf.clear();
                self.mp_first.clear();
            }
            KeyCode::Backspace => {
                self.mp_buf.pop();
            }
            KeyCode::Enter => {
                if self.mp_busy {
                    return;
                }
                match self.mp_stage {
                    MpStage::First => {
                        if self.mp_buf.is_empty() {
                            return;
                        }
                        self.mp_first = std::mem::take(&mut self.mp_buf);
                        self.mp_stage = MpStage::Confirm;
                    }
                    MpStage::Confirm => {
                        if self.mp_buf == self.mp_first {
                            let new_master = std::mem::take(&mut self.mp_buf);
                            self.rotate_master(new_master);
                        } else {
                            self.mp_buf.clear();
                            self.mp_first.clear();
                            self.mp_stage = MpStage::First;
                        }
                    }
                    MpStage::Idle => {}
                }
            }
            KeyCode::Char(c) if !ctrl && !c.is_control() => {
                if !self.mp_busy {
                    self.mp_buf.push(c);
                }
            }
            _ => {}
        }
    }

    /// 后台重加密保险库（argon2 KDF ~1s，必须 spawn_blocking）。
    fn rotate_master(&mut self, new_master: String) {
        let vault = self.vault.clone();
        let tx = self.event_tx.clone();
        self.mp_busy = true;
        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(move || {
                let vk = vault::create_vault_key(&new_master)?;
                vault::store_vault_key(&vault, &vault::vault_path()?, &vk)?;
                Ok::<_, anyhow::Error>((vk, new_master))
            })
            .await
            .map_err(|e| format!("后台任务异常: {e}"))
            .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = tx.send(AppEvent::MasterRotated(res));
        });
    }

    fn toggle_master_setting(&mut self) {
        if self.master_secret.is_none() {
            self.status = Some("开发模式（--dev）没有主密码，无法开关保护".to_string());
            return;
        }
        self.settings.master_password_enabled = !self.settings.master_password_enabled;
        self.settings_focus = 2;
    }

    fn begin_master_change(&mut self) {
        if self.master_secret.is_none() {
            self.status = Some("开发模式（--dev）没有主密码".to_string());
            return;
        }
        self.mp_stage = MpStage::First;
        self.mp_buf.clear();
        self.mp_first.clear();
        self.settings_focus = 3;
    }

    /// Enter/空格/点击 对当前聚焦项生效：0 切换高亮、1 循环保活档位、
    /// 2 主密码保护开关、3 进入修改主密码输入、4 打开快捷键面板、5 下一套主题、
    /// 6 检查/开始更新、7 保存、其余取消
    fn apply_settings_focus(&mut self) {
        match self.settings_focus {
            0 => self.settings.highlight = !self.settings.highlight,
            1 => {
                self.settings.keepalive_secs = cycle_keepalive(self.settings.keepalive_secs);
            }
            2 => self.toggle_master_setting(),
            3 => self.begin_master_change(),
            4 => self.open_keybinds(),
            5 => self.step_theme(false),
            6 => self.run_update_row(),
            7 => self.settings.session_log = !self.settings.session_log,
            8 => {
                self.set_metrics_setting(!self.settings.metrics);
            }
            9 => {
                self.settings.reconnect.max_attempts =
                    cycle_reconnect_attempts(self.settings.reconnect.max_attempts);
            }
            10 => self.save_settings(),
            _ => self.cancel_settings(),
        }
    }

    /// 换主题：只改内存里的草稿并立刻重绘，所以能当场看到效果；
    /// 按【保存】才写盘，按【取消】会走 Settings::load() 把主题一起回滚。
    fn step_theme(&mut self, backwards: bool) {
        self.settings.theme = self.settings.theme.shift(backwards);
        theme::apply(self.settings.theme);
    }

    /// 后台查一次新版本。启动时问一次、设置页按行时再问一次，都不占事件环。
    fn spawn_update_check(&mut self) {
        self.update.checking = true;
        self.update.error = None;
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(update::check)
                .await
                .map_err(|e| format!("后台任务异常: {e}"))
                .and_then(|r| r);
            let _ = tx.send(AppEvent::UpdateChecked(res));
        });
    }

    /// 检查回来的结果：有更新才立徽标，失败只记一句话（启动那次没网不该拦人）。
    fn on_update_checked(&mut self, res: std::result::Result<Option<String>, String>) {
        self.update.checking = false;
        self.update.checked_at = update::read_cache().map(|c| c.checked_at);
        match res {
            Ok(Some(tag)) => {
                let fresh = self.update.latest.as_deref() != Some(tag.as_str());
                self.update.latest = Some(tag.clone());
                if fresh {
                    self.status = Some(format!("发现新版本 {tag}，点顶部徽标即可更新"));
                }
            }
            Ok(None) => self.update.latest = None,
            Err(msg) => self.update.error = Some(msg),
        }
    }

    /// 设置页第 6 行的 Enter：已知有更新就直接进确认，否则先查一次。
    fn run_update_row(&mut self) {
        if self.update.latest.is_some() {
            self.offer_update();
        } else {
            self.spawn_update_check();
        }
    }

    /// 更新确认弹窗：说清下什么、会不会打断现有会话，默认停在【更 新】。
    fn offer_update(&mut self) {
        let Some(tag) = self.update.latest.clone() else {
            return;
        };
        if self.choice.is_some() {
            // 一次只弹一个模态框：已有弹窗时把这次意愿留在徽标上，不叠加
            self.status = Some("请先处理当前弹窗，再点顶部徽标更新".to_string());
            return;
        }
        let asset = update::asset_name().unwrap_or("对应平台的安装包");
        let mut lines = vec![
            format!("当前 v{} · 最新 {tag}", update::current_version()),
            format!("将下载 {asset} 并替换 ells 自身（SHA256 校验不过不会替换）。"),
        ];
        if self.slots.iter().any(|s| s.session.is_some() || s.connecting) {
            lines.push("注意：有会话正在连接或已连着，重启后才能用新版本。".to_string());
        }
        let tx = self.event_tx.clone();
        self.choice = Some(Choice {
            title: format!("更新到 {tag}"),
            lines,
            options: vec!["稍 后".to_string(), "更 新".to_string()],
            selected: 1,
            shortcuts: &[('n', 0), ('y', 1)],
            danger: false,
            on_pick: Box::new(move |idx| {
                if matches!(idx, Some(1)) {
                    let _ = tx.send(AppEvent::UpdateAccepted);
                }
            }),
        });
    }

    /// 下载并替换自身：进度走事件，取消位交给界面按。
    fn spawn_update_apply(&mut self, tag: String) {
        self.update.reset_cancel();
        self.update.downloading = true;
        self.update.applied = false;
        self.update.applied_tag = Some(tag.clone());
        self.update.transferred = 0;
        self.update.total = None;
        self.update.error = None;
        // 弹窗已经吃掉下层界面，设置页留着只会让用户以为还能点
        self.settings_open = false;
        let cancel = self.update.cancel.clone();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let report = tx.clone();
            let res = tokio::task::spawn_blocking(move || {
                update::apply(&tag, &cancel, &mut |done, total| {
                    let _ = report.send(AppEvent::UpdateProgress {
                        transferred: done,
                        total,
                    });
                })
            })
            .await
            .map_err(|e| format!("后台任务异常: {e}"))
            .and_then(|r| r);
            let _ = tx.send(AppEvent::UpdateDone(res));
        });
    }

    fn on_update_done(&mut self, res: std::result::Result<(), String>) {
        self.update.downloading = false;
        match res {
            Ok(()) => {
                self.update.applied = true;
                self.update.latest = None;
                self.update.error = None;
            }
            Err(msg) => {
                // 取消和失败分得开：用户自己按的取消不该写成"更新失败"
                if self.update.cancel.load(Ordering::Relaxed) {
                    self.status = Some(format!(
                        "已取消下载，仍是 v{}",
                        update::current_version()
                    ));
                } else {
                    self.update.error = Some(msg.clone());
                    self.status = Some(format!("更新失败：{msg}"));
                }
            }
        }
    }

    /// 更新弹窗按键：下载中只认取消，替换完只认重启/稍后。
    fn handle_update_key(&mut self, key: &KeyEvent) {
        if self.update.downloading {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('c')) {
                self.update.request_cancel();
                self.status = Some("正在取消下载…".to_string());
            }
            return;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') => self.quit_for_restart(),
            _ => self.dismiss_update(),
        }
    }

    /// 关掉"已替换待重启"弹窗：新版本已经在盘上，只是这个进程还是旧的。
    fn dismiss_update(&mut self) {
        self.update.applied = false;
        self.status = Some(format!(
            "已更新到新版本，重启 ells 后生效（当前仍是 v{}）",
            update::current_version()
        ));
    }

    /// 退出并把终端交还给新版本（`run()` 收尾时负责真正拉起）。
    fn quit_for_restart(&mut self) {
        self.restart_after_exit = true;
        self.done = true;
    }

    fn open_settings(&mut self) {
        self.settings_open = true;
        self.settings_focus = 0;
        // 子面板状态不跨开关保留：否则上次停在「快捷键」里，重开就直接落在子面板上
        self.keybinds_open = false;
        self.keybinds_recording = None;
        self.keybinds_msg = None;
    }

    fn open_keybinds(&mut self) {
        self.keybinds_open = true;
        self.keybinds_focus = 0;
        self.keybinds_recording = None;
        self.keybinds_msg = None;
    }

    /// 关闭快捷键面板回到设置页（改动已在绑定时逐项落盘，这里不再需要保存）
    fn close_keybinds(&mut self) {
        self.keybinds_open = false;
        self.keybinds_recording = None;
        self.keybinds_msg = None;
        self.settings_focus = 4;
    }

    fn handle_keybinds_key(&mut self, key: &KeyEvent) {
        if let Some(action) = self.keybinds_recording {
            // 录制态吞掉所有按键：Esc/Backspace 取消，其余尝试绑定
            if matches!(key.code, KeyCode::Esc | KeyCode::Backspace) {
                self.keybinds_recording = None;
                self.keybinds_msg = Some(format!("已取消修改「{}」", action.label()));
                return;
            }
            self.apply_binding(action, key);
            return;
        }
        let last = Action::ALL.len() + 1;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.keybinds_focus = self.keybinds_focus.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.keybinds_focus = (self.keybinds_focus + 1).min(last);
            }
            KeyCode::Enter | KeyCode::Char(' ') => match self.keybinds_focus {
                idx if idx < Action::ALL.len() => {
                    let action = Action::ALL[idx];
                    self.keybinds_recording = Some(action);
                    self.keybinds_msg =
                        Some(format!("「{}」= {}", action.label(), self.settings.keybinds.display(action)));
                }
                i if i == Action::ALL.len() => self.reset_keybinds(),
                _ => self.close_keybinds(),
            },
            KeyCode::Esc => self.close_keybinds(),
            _ => {}
        }
    }

    /// 把一次按键写成绑定：非法键给出原因，撞键则两个动作互换，成功即刻保存。
    fn apply_binding(&mut self, action: Action, key: &KeyEvent) {
        self.keybinds_recording = None;
        let Some(chord) = Chord::from_event(key) else {
            self.keybinds_msg = Some("请按功能键（F2–F9）或 Ctrl/Alt 组合键".to_string());
            return;
        };
        if let Some(reason) = chord.rejection() {
            self.keybinds_msg = Some(reason.to_string());
            return;
        }
        let swapped = self.settings.keybinds.bind(action, chord);
        self.settings.save();
        self.keybinds_msg = Some(match swapped {
            Some(other) => format!(
                "「{}」= {}，与「{}」自动互换为 {}",
                action.label(),
                chord.display(),
                other.label(),
                self.settings.keybinds.display(other)
            ),
            None => format!("「{}」= {}，已保存", action.label(), chord.display()),
        });
    }

    fn reset_keybinds(&mut self) {
        self.settings.keybinds.reset();
        self.settings.save();
        self.keybinds_msg = Some("已恢复默认快捷键并保存".to_string());
    }

    fn save_settings(&mut self) {
        self.settings.save();
        let _ = audit::record(
            AuditKind::Settings,
            "settings",
            &format!(
                "主题={} 保活={}s 高亮={} 自动更新={} 会话记录={} 主机指标={} 自动重连={}次",
                self.settings.theme.ini_value(),
                self.settings.keepalive_secs,
                self.settings.highlight,
                self.settings.auto_update,
                self.settings.session_log,
                self.settings.metrics,
                self.settings.reconnect.attempts_label()
            ),
        );
        ells_core::ssh::set_keepalive_interval(self.settings.keepalive_secs);
        // 指标开关立刻生效：关掉就收回那一行、停掉所有采集通道；
        // 打开就给已经连上的标签补上，不必重连
        self.sync_metrics();
        // 主密码开关联动本地自动解锁凭据：关闭=写入，开启=删除
        if self.settings.master_password_enabled {
            crate::settings::clear_master_backup();
        } else if let Some(master) = &self.master_secret {
            if let Err(err) = crate::settings::write_master_backup(master.as_str()) {
                self.status = Some(format!("设置已保存，但免密凭据写入失败: {err}"));
                self.settings_open = false;
                self.keybinds_open = false;
                self.reset_master_edit();
                return;
            }
        }
        self.settings_open = false;
        self.keybinds_open = false;
        self.keybinds_recording = None;
        self.keybinds_msg = None;
        self.reset_master_edit();
        self.status = Some("设置已保存（保活间隔对下次连接生效）".to_string());
    }

    fn cancel_settings(&mut self) {
        // 丢弃未保存的改动，回到磁盘上的当前值
        self.settings = Settings::load();
        ells_core::ssh::set_keepalive_interval(self.settings.keepalive_secs);
        // 指标开关立刻生效：关掉就收回那一行、停掉所有采集通道；
        // 打开就给已经连上的标签补上，不必重连
        self.sync_metrics();
        self.settings_open = false;
        self.keybinds_open = false;
        self.keybinds_recording = None;
        self.keybinds_msg = None;
        self.reset_master_edit();
    }

    fn reset_master_edit(&mut self) {
        self.mp_stage = MpStage::Idle;
        self.mp_buf.clear();
        self.mp_first.clear();
    }

    fn detach_session(&mut self, status: &str) {
        // 主动断开：不给"连接已断开 / 重连"弹窗，这条会话到此为止
        // 等在表里的自动重连一起作废（用户说了不要，就不能还在后台偷偷连）
        self.stop_reconnect(self.work);
        let label = if let Some(mut s) = self.slots[self.work].session.take() {
            s.close();
            Some(s.label.clone())
        } else {
            None
        };
        let slot = &mut self.slots[self.work];
        if let Some(mut lg) = slot.log.take() {
            lg.note(&format!("会话结束 · {status}"));
        }
        // 主动断开：采集的那条链一起停掉，别让在途回包写进已经没有了的那一行
        slot.metrics.cancel();
        slot.host = None;
        slot.connecting = false;
        slot.sftp = None;
        slot.browser.reset();
        slot.browser.transfers.clear();
        slot.remote_cwd.clear();
        slot.sz_pending.clear();
        slot.rz_pending = false;
        slot.sz_pick_mode = false;
        slot.sz_saveas_pending = false;
        slot.pending_zmodem = None;
        slot.search = None;
        slot.view = ScreenKind::List;
        if let Some(label) = label {
            slot.status = Some(format!("[{label}] {status}"));
        }
        self.settings_open = false;
        self.transfer_popup = false;
        self.screen = ScreenKind::List;
        self.list.selected = 0;
    }

    fn open_browser(&mut self) {
        let Some(sftp) = self.slots[self.work].sftp.clone() else {
            self.slots[self.work].status = Some("该服务器不支持 SFTP 文件传输".to_string());
            return;
        };
        self.slots[self.work].browser.reset();
        self.slots[self.work].browser.loading = true;
        self.set_view(ScreenKind::Browser);
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        tokio::spawn(async move {
            let res = sftp
                .canonicalize(".")
                .await
                .map_err(|e| format!("无法解析远端目录: {e}"));
            let _ = tx.send(AppEvent::SftpHome { slot, res });
        });
    }

    fn start_listing(&mut self, dir: String) {
        let Some(sftp) = self.slots[self.work].sftp.clone() else {
            return;
        };
        self.slots[self.work].browser.path = dir.clone();
        self.slots[self.work].browser.loading = true;
        self.slots[self.work].browser.error = None;
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        tokio::spawn(async move {
            let res = ells_transfer::list(&sftp, &dir)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::SftpListed { slot, res });
        });
    }

    fn open_upload_picker(&mut self, dest: String) {
        if self.slots[self.work].sftp.is_none() {
            return;
        }
        self.dialog_open = true;
        self.slots[self.work].upload_dest = Some(dest);
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        tokio::spawn(async move {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            let path = tokio::task::spawn_blocking(move || {
                crate::dialog::pick_file("选择要上传的文件", home, Vec::new())
            })
            .await
            .ok()
            .flatten();
            let _ = tx.send(AppEvent::PickedUpload { slot, path });
        });
    }

    /// 递归上传整个目录（浏览器 `U`）。
    fn open_upload_dir_picker(&mut self, dest: String) {
        if self.slots[self.work].sftp.is_none() {
            return;
        }
        self.dialog_open = true;
        self.slots[self.work].upload_dest = Some(dest);
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        tokio::spawn(async move {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            let path = tokio::task::spawn_blocking(move || {
                crate::dialog::pick_directory("选择要上传的目录", home)
            })
            .await
            .ok()
            .flatten();
            let _ = tx.send(AppEvent::PickedUploadDir { slot, path });
        });
    }

    /// 上传本地文件或目录（目录自动走递归上传）。目标目录取 upload_dest，其次当前浏览目录。
    fn start_upload(&mut self, local: PathBuf) {
        let Some(sftp) = self.slots[self.work].sftp.clone() else {
            self.slots[self.work].status = Some("上传失败：该连接没有 SFTP 通道".to_string());
            return;
        };
        let Some(name) = local.file_name().map(|s| s.to_string_lossy().into_owned()) else {
            self.slots[self.work].status = Some("无法解析所选文件名".to_string());
            return;
        };
        let dest = self.slots[self.work].upload_dest.take().unwrap_or_default();
        if dest.is_empty() {
            self.slots[self.work].status = Some("上传失败：还不知道要传到哪个远端目录".to_string());
            return;
        }
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        let cancel = self.fresh_cancel();
        tokio::spawn(async move {
            let (ptx, mut prx) = mpsc::unbounded_channel::<Progress>();
            let pump_tx = tx.clone();
            let pump = tokio::spawn(async move {
                while let Some(pr) = prx.recv().await {
                    let _ = pump_tx.send(AppEvent::SftpProgress { slot, pr });
                }
            });
            let res = run_upload(sftp, local, dest, name, ptx, &tx, slot, &cancel).await;
            // 先收干进度转发、再发 Done：顺序反了的话，尾随的进度事件会因为
            // 条目已 done 匹配不上，被登记成一条永不完结的"?"传输（100% 卡住、1/2）
            let _ = pump.await;
            let _ = tx.send(AppEvent::SftpDone { slot, res });
        });
    }

    fn start_download(&mut self, entry: FileEntry) {
        let dest_dir = download_home();
        self.download_to(entry, dest_dir, None, true);
    }

    /// 下载到系统「另存为」给出的确切路径：原生对话框自己会问覆盖，不再多问一次。
    fn start_download_to(&mut self, entry: FileEntry, dest: PathBuf) {
        let dest_dir = dest
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let name = dest
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.name.clone());
        self.download_to(entry, dest_dir, Some(name), false);
    }

    /// 下载目录：落到用户选定的目录里（同名子目录存在时问覆盖 / 改名 / 取消）。
    fn start_download_dir(&mut self, entry: FileEntry, dir: PathBuf) {
        self.download_to(entry, dir, None, true);
    }

    fn download_to(
        &mut self,
        entry: FileEntry,
        dest_dir: PathBuf,
        rename: Option<String>,
        confirm_overwrite: bool,
    ) {
        let Some(sftp) = self.slots[self.work].sftp.clone() else {
            self.slots[self.work].status = Some("该服务器不支持 SFTP 文件传输".to_string());
            return;
        };
        let label = entry.name.clone();
        let name = rename.unwrap_or(label.clone());
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        let cancel = self.fresh_cancel();
        tokio::spawn(async move {
            let (ptx, mut prx) = mpsc::unbounded_channel::<Progress>();
            let pump_tx = tx.clone();
            let pump = tokio::spawn(async move {
                while let Some(pr) = prx.recv().await {
                    let _ = pump_tx.send(AppEvent::SftpProgress { slot, pr });
                }
            });
            let res = run_download(sftp, entry, dest_dir, name, confirm_overwrite, ptx, &tx, slot, &cancel)
                .await;
            // 同 start_upload：尾随进度必须先于 Done 入队，否则会在传输列表里留下永不完结的残项
            let _ = pump.await;
            let _ = tx.send(AppEvent::SftpDone { slot, res });
        });
    }

    /// `sz <one-file>`: 远端可能是目录，先问清类型再决定弹「另存为」还是「选择目录」。
    fn open_sz_save_as(&mut self, file: &str, cwd: String) {
        let path = if file.starts_with('/') {
            file.to_string()
        } else {
            ells_transfer::remote_join(&cwd, file)
        };
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        self.open_pick_target(FileEntry {
            name,
            path,
            is_dir: false,
            size: 0,
        });
    }

    /// 为一次下载选落点：目录→系统目录选择框，文件→系统另存为。
    /// 类型未知时（sz 只给了路径）先在后台查一次远端元数据。
    fn open_pick_target(&mut self, entry: FileEntry) {
        let Some(sftp) = self.slots[self.work].sftp.clone() else {
            self.slots[self.work].status = Some("该服务器不支持 SFTP 文件传输".to_string());
            return;
        };
        self.dialog_open = true;
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        tokio::spawn(async move {
            let mut entry = entry;
            if entry.size == 0 {
                if let Ok(Some(meta)) = ells_transfer::remote_meta(&sftp, &entry.path).await {
                    entry.is_dir = meta.is_dir;
                    entry.size = meta.size;
                }
            }
            let dir = download_home();
            let is_dir = entry.is_dir;
            let name = entry.name.clone();
            let path = tokio::task::spawn_blocking(move || {
                if is_dir {
                    crate::dialog::pick_directory("选择保存目录", dir)
                } else {
                    crate::dialog::save_file("保存下载文件", dir, &name)
                }
            })
            .await
            .ok()
            .flatten();
            let _ = tx.send(if is_dir {
                AppEvent::PickedSaveDir { slot, entry, path }
            } else {
                AppEvent::PickedSave { slot, entry, path }
            });
        });
    }

    fn open_save_as(&mut self, entry: FileEntry) {
        self.open_pick_target(entry);
    }

    /// 每次新传输领一个干净的取消位（上一次 Ctrl-C 之后必须还能继续传）。
    fn fresh_cancel(&mut self) -> Cancel {
        if self.slots[self.work].cancel.is_cancelled() {
            self.slots[self.work].cancel = Cancel::default();
        }
        self.slots[self.work].cancel.clone()
    }

    /// Ctrl-C：取消本标签全部进行中的传输。已写入的部分不会被自动删除。
    fn cancel_transfers(&mut self) {
        let active = self.slots[self.work]
            .browser
            .transfers
            .iter()
            .filter(|t| !t.done)
            .count();
        if active == 0 {
            self.slots[self.work].status =
                Some("没有进行中的传输（Ctrl-C 用于取消传输）".to_string());
            return;
        }
        // 覆盖确认弹窗挂在那里时先替用户答"取消"，否则任务会一直等
        if self.choice.is_some() {
            self.answer_choice(None);
        }
        self.slots[self.work].cancel.cancel();
        self.slots[self.work].status = Some(format!(
            "已请求取消 {active} 个传输（已下载的部分文件不会自动删除）"
        ));
    }

    /// m：在当前远端目录下新建子目录。
    fn prompt_mkdir(&mut self) {
        let Some(sftp) = self.slots[self.work].sftp.clone() else {
            self.slots[self.work].status = Some("该服务器不支持 SFTP 文件传输".to_string());
            return;
        };
        let dir = self.slots[self.work].browser.path.clone();
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        self.prompt = Some(Prompt {
            title: "新建目录".to_string(),
            label: "目录名",
            buffer: String::new(),
            error: None,
            hint: None,
            allow_empty: false,
            on_done: Box::new(move |name| {
                let Some(name) = name else { return };
                let path = ells_transfer::remote_join(&dir, &name);
                tokio::spawn(async move {
                    let res = ells_transfer::mkdir(&sftp, &path)
                        .await
                        .map(|_| format!("已创建目录 {path}"))
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(AppEvent::SftpOp { slot, res });
                });
            }),
        });
    }

    /// n：重命名选中项（仍在原目录内，改名 = 移到同目录的新名字）。
    fn prompt_rename(&mut self) {
        let Some(sftp) = self.slots[self.work].sftp.clone() else {
            self.slots[self.work].status = Some("该服务器不支持 SFTP 文件传输".to_string());
            return;
        };
        let Some(entry) = self.slots[self.work].browser.entries.get(self.slots[self.work].browser.selected).cloned() else {
            self.slots[self.work].status = Some("请先选中要重命名的项".to_string());
            return;
        };
        let parent = ells_transfer::remote_parent(&entry.path);
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        self.prompt = Some(Prompt {
            title: format!("重命名 {}", entry.name),
            label: "新名字",
            buffer: entry.name.clone(),
            error: None,
            hint: None,
            allow_empty: false,
            on_done: Box::new(move |name| {
                let Some(name) = name else { return };
                let to = ells_transfer::remote_join(&parent, &name);
                if to == entry.path {
                    let _ = tx.send(AppEvent::SftpOp {
                        slot,
                        res: Ok("名字没变，未做改动".to_string()),
                    });
                    return;
                }
                let from = entry.path.clone();
                tokio::spawn(async move {
                    let res = ells_transfer::rename(&sftp, &from, &to)
                        .await
                        .map(|_| format!("已重命名 {from} → {to}"))
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(AppEvent::SftpOp { slot, res });
                });
            }),
        });
    }

    /// c：改选中项的权限（八进制，如 600 / 0644）。只送权限位，不动属主和时间。
    fn prompt_chmod(&mut self) {
        let Some(sftp) = self.slots[self.work].sftp.clone() else {
            self.slots[self.work].status = Some("该服务器不支持 SFTP 文件传输".to_string());
            return;
        };
        let Some(entry) = self
            .slots[self.work]
            .browser
            .entries
            .get(self.slots[self.work].browser.selected)
            .cloned()
        else {
            self.slots[self.work].status = Some("请先选中要改权限的项".to_string());
            return;
        };
        let alias = self
            .slots[self.work]
            .host
            .as_ref()
            .map(|h| h.alias.clone())
            .unwrap_or_else(|| "?".to_string());
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        self.prompt = Some(Prompt {
            title: format!("改权限 {}", entry.name),
            label: "八进制(如 600)",
            buffer: String::new(),
            error: None,
            hint: None,
            allow_empty: false,
            on_done: Box::new(move |mode| {
                let Some(mode) = mode else { return };
                let Some(bits) = ells_transfer::parse_mode(&mode) else {
                    let _ = tx.send(AppEvent::SftpOp {
                        slot,
                        res: Err(format!("权限要八进制，如 600 / 0644（写的是 {mode}）")),
                    });
                    return;
                };
                let path = entry.path.clone();
                let shown = format!("{bits:o}");
                tokio::spawn(async move {
                    let res = ells_transfer::chmod(&sftp, &path, bits)
                        .await
                        .map(|_| format!("已把 {path} 改成 {shown}"))
                        .map_err(|e| format!("{e:#}"));
                    if res.is_ok() {
                        let _ = audit::record(
                            AuditKind::Transfer,
                            &alias,
                            &format!("chmod {shown} {path}"),
                        );
                    }
                    let _ = tx.send(AppEvent::SftpOp { slot, res });
                });
            }),
        });
    }

    /// D：删除选中项。目录会递归删除且不可恢复，因此永远先问一次。
    fn ask_delete_entry(&mut self) {
        let Some(sftp) = self.slots[self.work].sftp.clone() else {
            self.slots[self.work].status = Some("该服务器不支持 SFTP 文件传输".to_string());
            return;
        };
        let Some(entry) = self.slots[self.work].browser.entries.get(self.slots[self.work].browser.selected).cloned() else {
            self.slots[self.work].status = Some("请先选中要删除的项".to_string());
            return;
        };
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        let cancel = self.fresh_cancel();
        self.choice = Some(Choice {
            title: "删除确认".to_string(),
            lines: vec![
                format!("名称：{}", entry.name),
                format!("路径：{}", entry.path),
                if entry.is_dir {
                    "目录会被递归删除，里面的所有内容一起消失。".to_string()
                } else {
                    "文件会被删除。".to_string()
                },
                "此操作不可恢复。".to_string(),
            ],
            options: vec!["取 消".to_string(), "删 除".to_string()],
            selected: 0,
            shortcuts: &[('n', 0), ('y', 1)],
            danger: true,
            on_pick: Box::new(move |idx| {
                if !matches!(idx, Some(1)) {
                    return;
                }
                let path = entry.path.clone();
                let cancel2 = cancel.clone();
                tokio::spawn(async move {
                    let res = ells_transfer::remove_tree(&sftp, &path, &cancel2)
                        .await
                        .map(|n| format!("已删除 {path}（{n} 项）"))
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(AppEvent::SftpOp { slot, res });
                });
            }),
        });
    }

    fn register_transfer(&mut self, label: String, direction: &'static str) {
        // 完成的传输保留在本次会话里（顶部"传输进度 x/x"要统计总数），
        // 只在异常多时裁掉最旧的，防止长会话内存无限增长
        while self.slots[self.work].browser.transfers.iter().filter(|t| t.done).count() >= 50 {
            let Some(idx) = self.slots[self.work].browser.transfers.iter().position(|t| t.done) else {
                break;
            };
            self.slots[self.work].browser.transfers.remove(idx);
        }
        self.slots[self.work].browser.transfers.push(TransferItem {
            label,
            direction,
            progress: None,
            done: false,
            error: None,
        });
    }

    fn on_zmodem_event(&mut self, ev: crate::zmodem::ZmodemEvent) {
        use crate::zmodem::ZmodemEvent as Z;
        // 系统对话框一次只能开一个，而且不该在用户没看的那个标签上抢焦点：
        // 后台标签先挂起，切回该标签或关掉当前弹窗后再继续。
        let background = self.work != self.active;
        if !matches!(ev, Z::Missing { .. }) && (self.dialog_open || background) {
            let name = match &ev {
                Z::Send { .. } => "sz",
                Z::Receive => "rz",
                _ => "zmodem",
            };
            self.slots[self.work].status = Some(if background {
                format!("已拦截 {name}：切回该标签后继续")
            } else {
                format!("已拦截 {name}：请先完成当前弹窗，完成后会自动继续")
            });
            self.slots[self.work].pending_zmodem = Some(ev);
            return;
        }
        match ev {
            Z::Missing { sz } => {
                let cmd = if sz { "sz" } else { "rz" };
                self.slots[self.work].status = Some(format!(
                    "远端没有 {cmd}：ells 的 sz/rz 转换需要服务器安装 lrzsz（apt/yum install lrzsz）"
                ));
            }
            Z::Send { files } => {
                self.clear_zmodem_soon();
                if self.slots[self.work].sftp.is_none() {
                    self.slots[self.work].status = Some("已拦截 sz，但该连接没有 SFTP 通道".to_string());
                    return;
                }
                // 新的 sz 意图取代可能残留的 rz 等待，避免 SftpCwd 回来时先弹上传框
                self.slots[self.work].rz_pending = false;
                if files.is_empty() {
                    self.slots[self.work].status =
                        Some("已拦截 sz：请在文件浏览器中选择要下载的文件".to_string());
                    self.slots[self.work].sz_pick_mode = true;
                    self.open_browser();
                    return;
                }
                if files.len() == 1 {
                    // 单文件 sz：直接弹系统「另存为」
                    self.slots[self.work].status =
                        Some("已拦截 sz：请在弹出窗口选择保存位置".to_string());
                    let f = files[0].clone();
                    if self.slots[self.work].remote_cwd.is_empty() {
                        self.slots[self.work].sz_saveas_pending = true;
                        self.slots[self.work].sz_pending = files;
                        self.refresh_remote_cwd();
                    } else {
                        let cwd = self.slots[self.work].remote_cwd.clone();
                        self.open_sz_save_as(&f, cwd);
                    }
                    return;
                }
                self.slots[self.work].status =
                    Some(format!("已拦截 sz：{} 个文件改走 SFTP 下载", files.len()));
                if self.slots[self.work].remote_cwd.is_empty() {
                    self.slots[self.work].sz_pending = files;
                    self.refresh_remote_cwd();
                } else {
                    let cwd = self.slots[self.work].remote_cwd.clone();
                    self.start_sz(files, cwd);
                }
            }
            Z::Receive => {
                self.clear_zmodem_soon();
                if self.slots[self.work].sftp.is_none() {
                    self.slots[self.work].status = Some("已拦截 rz，但该连接没有 SFTP 通道".to_string());
                    return;
                }
                self.slots[self.work].sz_pending.clear();
                self.slots[self.work].sz_saveas_pending = false;
                self.slots[self.work].status =
                    Some("已拦截 rz：请在弹出窗口选择要上传的本地文件".to_string());
                if self.slots[self.work].remote_cwd.is_empty() {
                    self.slots[self.work].rz_pending = true;
                    self.refresh_remote_cwd();
                } else {
                    let cwd = self.slots[self.work].remote_cwd.clone();
                    self.open_upload_picker(cwd);
                }
            }
            Z::Unknown => {
                self.clear_zmodem_soon();
                self.slots[self.work].status =
                    Some("检测到 ZMODEM 握手但无法判定方向，已中止远端传输".to_string());
            }
        }
    }

    fn start_sz(&mut self, files: Vec<String>, cwd: String) {
        for f in files {
            let path = if f.starts_with('/') {
                f.clone()
            } else {
                ells_transfer::remote_join(&cwd, &f)
            };
            let name = path.rsplit('/').next().unwrap_or(&path).to_string();
            self.start_download(FileEntry {
                name,
                path,
                is_dir: false,
                size: 0,
            });
        }
    }

    fn refresh_remote_cwd(&mut self) {
        let Some(sftp) = self.slots[self.work].sftp.clone() else { return };
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        tokio::spawn(async move {
            let res = sftp.canonicalize(".").await.map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::SftpCwd { slot, res });
        });
    }

    fn clear_zmodem_soon(&mut self) {
        self.slots[self.work].zclear_seq += 1;
        let seq = self.slots[self.work].zclear_seq;
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(4000)).await;
            let _ = tx.send(AppEvent::ZmodemClear { slot, seq });
        });
    }

    /// Best-effort remote cwd tracking from echoed shell commands.
    fn track_cwd_cmd(&mut self, cmd: &[String]) {
        let Some(first) = cmd.first() else { return };
        if first.rsplit('/').next().unwrap_or(first) != "cd" {
            return;
        }
        match cmd.len() {
            1 => {
                // bare `cd` → home; ask SFTP to resolve it.
                self.slots[self.work].remote_cwd.clear();
                self.refresh_remote_cwd();
            }
            2 => {
                let arg = &cmd[1];
                if arg == "-"
                    || arg.contains(['$', '`', '\'', '"', ';', '&', '|', '*', '\\'])
                {
                    return;
                }
                if arg.starts_with('/') {
                    self.slots[self.work].remote_cwd = arg.clone();
                    return;
                }
                if self.slots[self.work].remote_cwd.is_empty() {
                    return;
                }
                self.slots[self.work].remote_cwd = if arg == ".." {
                    ells_transfer::remote_parent(&self.slots[self.work].remote_cwd)
                } else {
                    ells_transfer::remote_join(&self.slots[self.work].remote_cwd, arg.trim_start_matches("./"))
                };
            }
            _ => {}
        }
    }

    fn handle_browser_key(&mut self, key: &KeyEvent, ctrl: bool) {
        if ctrl && matches!(key.code, KeyCode::Char('s')) {
            self.screen = ScreenKind::Session;
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.slots[self.work].sz_pick_mode = false;
                self.screen = ScreenKind::Session;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.slots[self.work].browser.selected = self.slots[self.work].browser.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                if self.slots[self.work].browser.selected + 1 < self.slots[self.work].browser.entries.len() {
                    self.slots[self.work].browser.selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(entry) = self.slots[self.work].browser.entries.get(self.slots[self.work].browser.selected).cloned() {
                    if entry.is_dir {
                        self.start_listing(entry.path);
                    } else if self.slots[self.work].sz_pick_mode {
                        self.open_save_as(entry);
                    } else {
                        self.start_download(entry);
                    }
                }
            }
            KeyCode::Backspace | KeyCode::Char('h') => {
                if self.slots[self.work].browser.path != "/" {
                    let parent = ells_transfer::remote_parent(&self.slots[self.work].browser.path);
                    self.start_listing(parent);
                }
            }
            KeyCode::Char('u') => {
                let path = self.slots[self.work].browser.path.clone();
                self.open_upload_picker(path)
            }
            KeyCode::Char('U') => {
                let path = self.slots[self.work].browser.path.clone();
                self.open_upload_dir_picker(path)
            }
            KeyCode::Char('d') => {
                if let Some(entry) = self.slots[self.work].browser.entries.get(self.slots[self.work].browser.selected).cloned() {
                    if self.slots[self.work].sz_pick_mode || entry.is_dir {
                        self.open_save_as(entry);
                    } else {
                        self.start_download(entry);
                    }
                }
            }
            KeyCode::Char('r') => {
                let path = self.slots[self.work].browser.path.clone();
                self.start_listing(path);
            }
            KeyCode::Char('m') => self.prompt_mkdir(),
            KeyCode::Char('n') => self.prompt_rename(),
            KeyCode::Char('c') if !ctrl => self.prompt_chmod(),
            KeyCode::Char('D') => self.ask_delete_entry(),
            KeyCode::Char('c') if ctrl => self.cancel_transfers(),
            _ => {
                let _ = ctrl;
            }
        }
    }

    fn handle_unlock_key(&mut self, key: &KeyEvent, ctrl: bool) {
        match key.code {
            KeyCode::Esc => self.done = true,
            KeyCode::Char('c') if ctrl => self.done = true,
            KeyCode::Char(c) if !ctrl && !self.unlock.busy => {
                self.unlock.input.push(c);
                self.unlock.error = None;
            }
            KeyCode::Backspace => {
                self.unlock.input.pop();
            }
            KeyCode::Enter => self.unlock_submit(),
            _ => {}
        }
    }

    fn unlock_submit(&mut self) {
        if self.unlock.busy {
            return;
        }
        let input = self.unlock.input.clone();
        match self.unlock.stage {
            UnlockStage::Open => {
                self.unlock.busy = true;
                let tx = self.event_tx.clone();
                let input = Zeroizing::new(input);
                tokio::spawn(async move {
                    let res = tokio::task::spawn_blocking(move || vault::unlock_vault(&input))
                        .await
                        .map_err(|e| format!("后台任务异常: {e}"))
                        .and_then(|r| r.map_err(|e| e.to_string()));
                    let _ = tx.send(AppEvent::VaultUnlocked(res));
                });
            }
            UnlockStage::CreateFirst => {
                if input.is_empty() {
                    self.unlock.error = Some("主密码不能为空".to_string());
                    return;
                }
                self.unlock.pending_master = Some(input);
                self.unlock.stage = UnlockStage::CreateConfirm;
                self.unlock.input.clear();
            }
            UnlockStage::CreateConfirm => {
                let matched = self.unlock.pending_master.as_deref() == Some(input.as_str());
                // 比对完就抹掉暂存的这份明文：后面用不上了
                self.unlock.pending_master = None;
                if matched {
                    self.unlock.busy = true;
                    let tx = self.event_tx.clone();
                    let input = Zeroizing::new(input);
                    tokio::spawn(async move {
                        let res = tokio::task::spawn_blocking(move || {
                            if vault::vault_exists() {
                                return Err(anyhow::anyhow!("保险库已存在，请勿重复创建"));
                            }
                            let vk = vault::create_vault_key(&input)?;
                            vault::store_vault_key(&Vault::default(), &vault::vault_path()?, &vk)?;
                            Ok(vk)
                        })
                        .await
                        .map_err(|e| format!("后台任务异常: {e}"))
                        .and_then(|r| r.map_err(|e| e.to_string()));
                        let _ = tx.send(AppEvent::VaultCreated(res));
                    });
                } else {
                    self.unlock.error = Some("两次输入的主密码不一致".to_string());
                    self.unlock.stage = UnlockStage::CreateFirst;
                    self.unlock.input.clear();
                }
            }
        }
    }

    /// i：导入 ~/.ssh/config。只新增库里没有的别名，绝不覆盖用户已经填好密码的主机。
    fn import_ssh_config(&mut self) {
        let all = ells_core::sshconfig::load_user_config();
        if all.is_empty() {
            self.status = Some("~/.ssh/config 里没有可导入的主机".to_string());
            return;
        }
        let fresh: Vec<Host> = all
            .into_iter()
            .filter(|h| !self.vault.hosts.iter().any(|e| e.alias == h.alias))
            .collect();
        if fresh.is_empty() {
            self.status = Some("~/.ssh/config 里的主机都已经在了".to_string());
            return;
        }
        let mut lines: Vec<String> = fresh
            .iter()
            .take(8)
            .map(|h| format!("{} → {}（{}）", h.alias, h.target(), h.auth_label()))
            .collect();
        if fresh.len() > 8 {
            lines.push(format!("…共 {} 台", fresh.len()));
        }
        lines.push(String::new());
        lines.push("密码认证的机器导入后需在「编辑」里补密码。".to_string());
        let tx = self.event_tx.clone();
        let count = fresh.len();
        self.choice = Some(Choice {
            title: "导入 ~/.ssh/config".to_string(),
            lines,
            options: vec!["取 消".to_string(), format!("导 入 {count} 台")],
            selected: 1,
            shortcuts: &[('n', 0), ('y', 1)],
            danger: false,
            on_pick: Box::new(move |idx| {
                if matches!(idx, Some(1)) {
                    let _ = tx.send(AppEvent::ImportHosts(fresh));
                }
            }),
        });
    }

    /// x：反向操作——把保险库写成一段 ssh_config。写盘在 `export_ssh_config`。
    fn ask_export_ssh_config(&mut self) {
        if self.vault.hosts.is_empty() {
            self.status = Some("保险库里还没有可导出的主机".to_string());
            return;
        }
        let Some(path) = ells_core::sshconfig::export_path() else {
            self.status = Some("无法定位用户主目录".to_string());
            return;
        };
        let count = self.vault.hosts.len();
        let tx = self.event_tx.clone();
        self.choice = Some(Choice {
            title: "导出 ssh_config".to_string(),
            lines: vec![
                format!("把保险库里的 {count} 台主机写成一段 ssh_config："),
                path.display().to_string(),
                String::new(),
                "只写别名 / 主机 / 端口 / 用户 / 私钥路径 / 转发，密码一条都不写。".to_string(),
                "写的是 ells 目录下的导出件，~/.ssh/config 本体一个字都不动。".to_string(),
                "已有同名文件会被覆盖。".to_string(),
            ],
            options: vec!["取 消".to_string(), "导 出".to_string()],
            selected: 0,
            shortcuts: &[('n', 0), ('y', 1)],
            danger: false,
            on_pick: Box::new(move |idx| {
                if matches!(idx, Some(1)) {
                    let _ = tx.send(AppEvent::ExportSshConfig);
                }
            }),
        });
    }

    /// 真正落盘：原子写，失败只影响这份可重生成的导出件，不碰保险库。
    fn export_ssh_config(&mut self) {
        let Some(path) = ells_core::sshconfig::export_path() else {
            self.status = Some("无法定位用户主目录".to_string());
            return;
        };
        let text = ells_core::sshconfig::to_config_text(&self.vault.hosts);
        let count = self.vault.hosts.len();
        match ells_core::write_atomic(&path, text.as_bytes()) {
            Ok(()) => {
                self.status = Some(format!(
                    "已导出 {count} 台 → {}（不含任何密码）",
                    path.display()
                ));
                let _ = audit::record(
                    AuditKind::Export,
                    "ssh-config",
                    &format!("导出 {count} 台 → {}", path.display()),
                );
            }
            Err(err) => self.status = Some(format!("导出失败：{err}")),
        }
    }

    fn import_hosts(&mut self, hosts: Vec<Host>) {
        let mut imported = 0usize;
        let mut backed_up = 0usize;
        let mut key_errors = Vec::new();
        for host in hosts {
            let mut host = host;
            let alias = host.alias.clone();
            match self.backup_key(&mut host) {
                Ok(copied) => {
                    backed_up += copied as usize;
                    self.vault.upsert(host);
                    imported += 1;
                }
                Err(err) => key_errors.push(format!("{alias}: {err}")),
            }
            self.list.selected = 0;
        }
        if imported > 0 {
            self.save_vault();
            let _ = audit::record(AuditKind::Import, "ssh-config", &format!("导入 {imported} 台"));
        }
        self.status = match key_errors.is_empty() {
            true => Some(format!(
                "已导入 {imported} 台主机{}",
                if backed_up > 0 {
                    format!("（{backed_up} 把私钥已备份到 ~/.ells/keys）")
                } else {
                    String::new()
                }
            )),
            false => Some(format!(
                "已导入 {imported} 台；{} 台私钥备份失败：{}",
                key_errors.len(),
                key_errors.join("；")
            )),
        };
    }

    fn handle_list_key(&mut self, key: &KeyEvent, ctrl: bool) {
        // 过滤词编辑态优先：此时除了退出/退格/字符，其它键位都不该动列表
        if self.filtering {
            return self.handle_filter_key(key);
        }
        let rows = self.visible();
        let len = rows.len();
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
            || (ctrl && matches!(key.code, KeyCode::Char('c')))
        {
            // 后台标签还在传输：先问一句，别让半截文件被当成完整的
            let busy = self
                .slots
                .iter()
                .filter(|s| s.browser.transfers.iter().any(|t| !t.done))
                .count();
            if busy > 0 && !self.quit_confirm {
                self.quit_confirm = true;
                self.status =
                    Some(format!("还有 {busy} 个标签在传输中，再按一次 q 仍然退出"));
                return;
            }
            self.done = true;
            return;
        }
        self.quit_confirm = false;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.list.selected = self.list.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if len > 0 && self.list.selected + 1 < len {
                    self.list.selected += 1;
                }
            }
            KeyCode::Char('a') => {
                // 新增：从当前选中的那台带上结构性字段；没选中就是空表
                let tpl = self.selected_host();
                self.form = FormState::template(tpl.as_ref());
                self.screen = ScreenKind::Form;
            }
            KeyCode::Char('e') => {
                if let Some(host) = self.selected_host() {
                    self.form = FormState::new(Some(&host));
                    self.screen = ScreenKind::Form;
                }
            }
            KeyCode::Char('d') => {
                // 二级确认：先弹确认框，真正删除在 confirm_delete()
                if let Some(host) = self.selected_host() {
                    self.delete_confirm = Some(host.alias.clone());
                    self.confirm_index = 0;
                }
            }
            KeyCode::Char('s') => self.open_settings(),
            KeyCode::Char('i') => self.import_ssh_config(),
            KeyCode::Char('x') => self.ask_export_ssh_config(),
            KeyCode::Char('/') => {
                self.filtering = true;
            }
            KeyCode::Char('t') => {
                self.tunnels_open = true;
                self.tunnels_selected = 0;
            }
            KeyCode::Char('p') => {
                if let Some(alias) = self.selected_host().map(|h| h.alias.clone()) {
                    self.open_rules_for(alias);
                } else {
                    self.status = Some("先选中一台主机，再按 p 编辑它的端口转发".into());
                }
            }
            KeyCode::Char('m') => self.open_ports(),
            KeyCode::Char('h') => self.open_known(),
            KeyCode::Char('l') => self.open_audit(),
            KeyCode::Char('v') => self.open_sessions(),
            KeyCode::Char('f') => self.toggle_favorite(),
            KeyCode::Char(' ') => self.toggle_fold(),
            KeyCode::Char('z') => self.toggle_fold_all(),
            KeyCode::Char('o') => self.cycle_sort(),
            KeyCode::Char('?') | KeyCode::F(1) => self.help_open = true,
            KeyCode::Enter => {
                if let Some(host) = self.selected_host() {
                    self.open_host(host);
                }
            }
            _ => {}
        }
    }

    /// `/` 之后的按键：全部进过滤框。Esc 清空并退出，Enter 只退出（保留过滤词）。
    fn handle_filter_key(&mut self, key: &KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filtering = false;
                self.filter.clear();
                self.list.selected = 0;
            }
            KeyCode::Enter | KeyCode::Tab => {
                self.filtering = false;
                self.list.selected = 0;
            }
            KeyCode::Backspace => {
                self.filter.remove(self.filter.chars().count().saturating_sub(1));
                self.list.selected = 0;
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.list.selected = 0;
            }
            _ => {}
        }
    }

    /// 段头（以及配套的折叠）只在没有过滤词、且排序真的按分组走时出现：
    /// 过滤时结果按模糊分数拍平，分组只会把命中顺序切碎。
    pub(crate) fn headers_shown(&self) -> bool {
        self.filter.trim().is_empty() && self.settings.list_sort.groups_ordered()
    }

    /// 一次算出"画什么"和"哪几台没被折叠"，两者必须同源。绘制帧里直接用它，
    /// 别调 `list_rows()` + `visible()` + `selected_row()` 各算一遍。
    pub(crate) fn rows_and_visible(&self) -> (Vec<ListRow>, Vec<usize>) {
        let order = order_hosts(&self.vault.hosts, &self.filter, self.settings.list_sort);
        build_list_rows(
            &self.vault.hosts,
            &order,
            self.headers_shown(),
            &self.list.folded,
        )
    }

    /// 当前可见主机的顺序（列表下标 → `vault.hosts` 下标，段头不占位）。
    pub(crate) fn visible(&self) -> Vec<usize> {
        self.rows_and_visible().1
    }

    /// 光标挪到主机行上：段头能被 ↑↓ 经过，但不停留。
    fn clamp_selection(&mut self) {
        let len = self.visible().len();
        if len == 0 {
            self.list.selected = 0;
        } else if self.list.selected >= len {
            self.list.selected = len - 1;
        }
    }

    /// Space：折叠/展开光标所在的那一段。折叠只在分组视图里生效，别的排序按
    /// 一下给句提示，不然用户只会觉得"这台机器怎么自己消失了"。
    fn toggle_fold(&mut self) {
        if !self.headers_shown() {
            self.status = Some(
                "当前排序不分段，折叠无效（按 o 换到「默认」或「分组」）".to_string(),
            );
            return;
        }
        if self.visible().is_empty() && !self.list.folded.is_empty() {
            // 全折起来的时候没有光标可用，Space 就是"把他们都放出来"
            self.list.folded.clear();
            self.status = Some("所有分组都被折叠了，已全部展开".to_string());
            return;
        }
        let Some(host) = self.selected_host() else { return };
        let g = group_of(&host);
        if self.list.folded.iter().any(|f| f == &g) {
            self.list.folded.retain(|f| f != &g);
            self.status = Some(format!("已展开 {g}"));
        } else {
            self.list.folded.push(g.clone());
            self.status = Some(format!("已折叠 {g}（Space 展开，z 展开全部）"));
        }
        self.clamp_selection();
    }

    /// z：一次收起/放出所有分组。全折叠时列表只剩段头，正好用来看"我有几组"。
    fn toggle_fold_all(&mut self) {
        if !self.headers_shown() {
            self.status = Some("当前排序不分段，按 o 换到「默认」或「分组」".to_string());
            return;
        }
        if !self.list.folded.is_empty() {
            self.list.folded.clear();
            self.status = Some("已展开全部分组".to_string());
        } else {
            self.list.folded = foldable_groups(&self.vault.hosts);
            self.status = Some("已折叠全部分组（z 展开）".to_string());
        }
        self.clamp_selection();
    }

    /// o：循环排序方式，选择立刻写进 settings.ini，下次启动还是它。
    fn cycle_sort(&mut self) {
        self.settings.list_sort = self.settings.list_sort.next();
        self.settings.save();
        self.list.selected = 0;
        self.status = Some(format!(
            "列表排序：{}（按 o 切换）",
            self.settings.list_sort.label()
        ));
    }

    /// 当前可见行对应的主机（越界返回 None：删除最后一台之后就会这样）。
    fn selected_host(&self) -> Option<Host> {
        let i = *self.visible().get(self.list.selected)?;
        self.vault.hosts.get(i).cloned()
    }

    /// 把列表光标移到某台主机上（列表下标是 `visible()` 的顺序，不能直接用房
    /// 主在 `vault.hosts` 里的下标）。过滤词可能把它藏起来，所以一并清空；
    /// 它所在的那一段被折叠过也要先展开，否则光标只会夹到别的主机上。
    fn focus_host(&mut self, alias: &str) {
        self.filter.clear();
        self.filtering = false;
        if let Some(g) = self.vault.hosts.iter().find(|h| h.alias == alias).map(group_of) {
            self.list.folded.retain(|f| f != &g);
        }
        if let Some(row) = self
            .visible()
            .iter()
            .position(|&i| self.vault.hosts[i].alias == alias)
        {
            self.list.selected = row;
        } else {
            self.list.selected = self
                .list
                .selected
                .min(self.visible().len().saturating_sub(1));
        }
    }

    fn toggle_favorite(&mut self) {
        let Some(host) = self.selected_host() else { return };
        let alias = host.alias.clone();
        let now = host.favorite;
        if let Some(h) = self.vault.hosts.iter_mut().find(|h| h.alias == alias) {
            h.favorite = !now;
        }
        self.save_vault();
        self.status = Some(if now {
            format!("已取消收藏 {alias}")
        } else {
            format!("已收藏 {alias}（列表置顶）")
        });
    }

    /// 隧道面板里对选中主机做的事：没跑就起，在跑就停。
    fn toggle_tunnel(&mut self) {
        let Some(alias) = self.tunnel_rows().get(self.tunnels_selected).cloned() else { return };
        if self.tunnels.is_running(&alias) {
            self.tunnels.stop_one(&alias);
            self.tunnel_state.insert(alias.clone(), TunnelState::Stopped);
            // 监听口随隧道一起没了，映射表不能再显示旧端口
            self.tunnel_ports.remove(&alias);
            let _ = audit::record(AuditKind::Tunnel, &alias, "手动停止");
            self.status = Some(format!("已停止 {alias} 的隧道"));
            return;
        }
        let Some(host) = self.vault.hosts.iter().find(|h| h.alias == alias).cloned() else {
            return;
        };
        if host.forwards.is_empty() {
            self.status = Some(format!("{alias} 没有转发规则，按 Enter 建一条"));
            return;
        }
        self.start_tunnel_for(&host);
    }

    fn stop_all_tunnels(&mut self) {
        let running = self.tunnels.running();
        self.tunnels.stop_all();
        for alias in &running {
            self.tunnel_state.insert(alias.clone(), TunnelState::Stopped);
            self.tunnel_ports.remove(alias);
            let _ = audit::record(AuditKind::Tunnel, alias, "全部停止");
        }
        self.status = Some(format!("已停止 {} 条隧道", running.len()));
    }

    /// 进这台主机的规则表格。没有规则的主机也进得来（列表页 p），否则"添加"没有入口。
    pub(crate) fn open_rules_for(&mut self, alias: String) {
        let forwards = self
            .vault
            .hosts
            .iter()
            .find(|h| h.alias == alias)
            .map(|h| h.forwards.clone())
            .unwrap_or_default();
        self.rules_alias = alias;
        self.rules_rows = if forwards.is_empty() {
            vec![RuleRow::empty()]
        } else {
            rules_from_forwards(&forwards)
        };
        self.rules_row = 0;
        self.rules_col = 0;
        self.rules_error = None;
        self.rules_open = true;
    }

    /// 表格里的撞口提示（每行都要显示，不只看选中行）：编辑器打开时按行算一次。
    pub(crate) fn rules_clashes(&self) -> Vec<String> {
        (0..self.rules_rows.len())
            .map(|i| row_clash_text(&self.rules_rows, &self.vault.hosts, &self.rules_alias, i))
            .collect()
    }

    fn new_rule_row(&mut self) {
        self.rules_rows.push(RuleRow::empty());
        self.rules_row = self.rules_rows.len() - 1;
        self.rules_col = 0;
        self.rules_error = None;
    }

    fn delete_rule_row(&mut self) {
        if self.rules_rows.is_empty() {
            return;
        }
        self.rules_rows.remove(self.rules_row.min(self.rules_rows.len() - 1));
        self.rules_row = self.rules_row.min(self.rules_rows.len().saturating_sub(1));
        self.rules_error = None;
    }

    /// 类型列用 ←/→ 切换，不占按键：目标主机那一格要能打出任意字母。
    fn cycle_rule_kind(&mut self, row: usize, forward: bool) {
        const KINDS: [&str; 3] = ["L", "D", "R"];
        let cur = self.rules_rows[row].kind.to_ascii_uppercase();
        let idx = KINDS.iter().position(|k| *k == cur.as_str()).unwrap_or(0);
        let next = if forward { (idx + 1) % KINDS.len() } else { (idx + KINDS.len() - 1) % KINDS.len() };
        self.rules_rows[row].kind = KINDS[next].to_string();
    }

    /// 往当前格追加一个字符。表格里的格全是文本，合法性留到保存时统一判。
    fn push_rule_char(&mut self, row: usize, ch: char) {
        let mut v = self.rules_rows[row].cell(self.rules_col).to_string();
        v.push(ch);
        self.rules_rows[row].set_cell(self.rules_col, v);
    }

    fn handle_rules_key(&mut self, key: &KeyEvent, ctrl: bool) {
        if self.rules_rows.is_empty() {
            // 空表也要能起手：Ctrl-N 建行，Esc 离开（等于清空规则并放弃）
            match key.code {
                KeyCode::Esc => self.rules_open = false,
                KeyCode::Char('n') if ctrl => self.new_rule_row(),
                _ => {}
            }
            return;
        }
        let row = self.rules_row.min(self.rules_rows.len() - 1);
        match key.code {
            KeyCode::Esc => {
                self.rules_open = false;
                self.rules_error = None;
            }
            KeyCode::Enter => self.save_rules(),
            KeyCode::Up => self.rules_row = row.saturating_sub(1),
            KeyCode::Down => {
                if row + 1 < self.rules_rows.len() {
                    self.rules_row = row + 1;
                }
            }
            KeyCode::Left => {
                if self.rules_col == 0 {
                    self.cycle_rule_kind(row, false);
                } else {
                    self.rules_col -= 1;
                }
            }
            KeyCode::Right => {
                // 最后一格再往右必须停住：越界后焦点画不出来，打字也像是丢了
                if self.rules_col == 0 {
                    self.cycle_rule_kind(row, true);
                } else if self.rules_col + 1 < RULE_COLS.len() {
                    self.rules_col += 1;
                }
            }
            KeyCode::Tab => {
                self.rules_col = (self.rules_col + 1) % RULE_COLS.len();
                if self.rules_col == 0 && row + 1 < self.rules_rows.len() {
                    self.rules_row = row + 1;
                }
            }
            KeyCode::BackTab => {
                if self.rules_col == 0 {
                    self.rules_col = RULE_COLS.len() - 1;
                    self.rules_row = row.saturating_sub(1);
                } else {
                    self.rules_col -= 1;
                }
            }
            KeyCode::Char('n') if ctrl => self.new_rule_row(),
            KeyCode::Char('d') if ctrl => self.delete_rule_row(),
            KeyCode::Char('c') if ctrl => {
                self.rules_rows[row].set_cell(self.rules_col, String::new());
                self.rules_error = None;
            }
            KeyCode::Backspace => {
                let v = self.rules_rows[row].cell(self.rules_col).to_string();
                if let Some(c) = v.chars().next_back() {
                    let mut t = v;
                    t.truncate(t.len() - c.len_utf8());
                    self.rules_rows[row].set_cell(self.rules_col, t);
                }
                self.rules_error = None;
            }
            KeyCode::Char(ch) if self.rules_col == 0 => {
                // 类型列只认 L/D/R，打别的字母等于没按——留个非法值在表里不如不让进
                let up = ch.to_ascii_uppercase();
                if matches!(up, 'L' | 'D' | 'R') {
                    self.rules_rows[row].kind = up.to_string();
                    self.rules_error = None;
                }
            }
            KeyCode::Char(ch) if !ctrl => {
                self.push_rule_char(row, ch);
                self.rules_error = None;
            }
            _ => {}
        }
    }

    /// 保存：把表格写回这台主机的 forwards。正在跑的隧道拿的是启动时刻的快照，
    /// 规则改了不重启就等于没改（端口也不会跟着变），所以这里主动重启一次。
    fn save_rules(&mut self) {
        let (forwards, errors) = rules_to_forwards(&self.rules_rows);
        if !errors.is_empty() {
            self.rules_error = Some(errors.join("；"));
            return;
        }
        let alias = self.rules_alias.clone();
        let Some(found) = self.vault.hosts.iter().find(|h| h.alias == alias).cloned() else {
            self.rules_error = Some(format!("{alias} 已不在保险库里"));
            return;
        };
        let running = self.tunnels.is_running(&alias);
        let changed = found.forwards != forwards;
        let mut host = found;
        host.forwards = forwards;
        let empty = host.forwards.is_empty();
        self.vault.upsert(host.clone());
        self.save_vault();
        let _ = audit::record(AuditKind::VaultSaved, &alias, "转发规则已更新");
        self.rules_open = false;
        self.status = Some(if running && changed && !empty {
            self.tunnels.stop_one(&alias);
            self.tunnel_ports.remove(&alias);
            self.start_tunnel_for(&host);
            format!("{alias}：规则已保存，隧道按新规则重启")
        } else if running && empty {
            self.tunnels.stop_one(&alias);
            self.tunnel_state.insert(alias.clone(), TunnelState::Stopped);
            self.tunnel_ports.remove(&alias);
            format!("{alias}：规则已清空，隧道随之停止")
        } else if running {
            format!("{alias}：规则没变，隧道继续跑")
        } else {
            format!("{alias}：已保存 {} 条规则（按 t 打开面板、空格启停）", host.forwards.len())
        });
    }

    fn open_ports(&mut self) {
        self.ports_open = true;
    }

    fn handle_ports_key(&mut self, key: &KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('m') => self.ports_open = false,
            _ => {}
        }
    }

    /// 面板上的行：有转发规则的主机（没规则的不显示，否则整台机器列表都在）。
    pub(crate) fn tunnel_rows(&self) -> Vec<String> {
        let mut rows: Vec<String> = self
            .vault
            .hosts
            .iter()
            .filter(|h| !h.forwards.is_empty())
            .map(|h| h.alias.clone())
            .collect();
        rows.sort();
        rows
    }

    fn on_tunnel_event(&mut self, event: TunnelEvent) {
        let _ = audit::record(AuditKind::Tunnel, &event.alias, &event.label());
        // 停止/终局时监听口已经不在了，映射表里必须一起消失，不能留个"看起来还在听"的旧端口
        if event.ports.is_empty() || matches!(event.state, TunnelState::Stopped) {
            self.tunnel_ports.remove(&event.alias);
        } else {
            self.tunnel_ports.insert(event.alias.clone(), event.ports.clone());
        }
        self.tunnel_state.insert(event.alias.clone(), event.state.clone());
        // 面板开着就给一行即时反馈；失败必须让用户看见，不能只改状态色
        if matches!(event.state, TunnelState::Failed(_)) {
            self.status = Some(event.label());
        }
    }

    fn open_known(&mut self) {
        self.known_entries = hostkey::list_entries();
        self.known_selected = 0;
        self.known_open = true;
    }

    /// 删除选中的一条 `~/.ells/known_hosts` 记录（`~/.ssh` 的那些只读、删不掉）。
    /// 删了下次连这台会重新问一次指纹——这正是它存在的用途（密钥换了又不想留着旧的）。
    fn delete_known_entry(&mut self) {
        let Some(entry) = self.known_entries.get(self.known_selected).cloned() else { return };
        if !entry.writable {
            self.known_msg = Some("来自 ~/.ssh/known_hosts，ells 不修改它".to_string());
            return;
        }
        match hostkey::delete_entry(&entry.hosts, &entry.algorithm) {
            Ok(()) => {
                let _ = audit::record(
                    AuditKind::HostKeyTrusted,
                    &entry.hosts,
                    &format!("删除记录 {}", entry.algorithm),
                );
                self.known_entries = hostkey::list_entries();
                self.known_selected = self
                    .known_selected
                    .min(self.known_entries.len().saturating_sub(1));
                self.known_msg = None;
                self.status = Some(format!("已删除 {} 的 {} 记录", entry.hosts, entry.algorithm));
            }
            Err(err) => self.known_msg = Some(format!("删除失败：{err}")),
        }
    }

    fn open_audit(&mut self) {
        self.audit_lines = audit::read_last(200);
        self.audit_scroll = 0;
        self.audit_open = true;
    }

    fn handle_known_key(&mut self, key: &KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') => {
                self.known_open = false;
                self.known_msg = None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.known_selected = self.known_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.known_selected + 1 < self.known_entries.len() {
                    self.known_selected += 1;
                }
            }
            KeyCode::Char('d') => self.delete_known_entry(),
            _ => {}
        }
    }

    fn handle_audit_key(&mut self, key: &KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('l') => self.audit_open = false,
            KeyCode::Up => self.audit_scroll = self.audit_scroll.saturating_sub(1),
            KeyCode::Down => {
                let max = self.audit_lines.len().saturating_sub(1);
                self.audit_scroll = (self.audit_scroll + 1).min(max);
            }
            KeyCode::PageUp => self.audit_scroll = self.audit_scroll.saturating_sub(10),
            KeyCode::PageDown => {
                let max = self.audit_lines.len().saturating_sub(1);
                self.audit_scroll = (self.audit_scroll + 10).min(max);
            }
            _ => {}
        }
    }

    /// 打开会话记录页：列最近 50 个日志文件，Enter 看内容。
    fn open_sessions(&mut self) {
        self.sessions_rows = sessionlog::list(50);
        self.sessions_selected = 0;
        self.sessions_text = None;
        self.sessions_scroll = 0;
        self.sessions_open = true;
    }

    /// 读入选中那一份的正文（去掉控制序列，界面里看着干净）。
    fn open_session_preview(&mut self) {
        let Some(row) = self.sessions_rows.get(self.sessions_selected) else {
            self.status = Some("没有可看的会话记录".to_string());
            return;
        };
        let path = row.path.clone();
        self.sessions_text = sessionlog::read_plain(&path).or_else(|| {
            self.status = Some(format!("读不了 {}（文件可能已被清理）", row.name));
            None
        });
        self.sessions_scroll = 0;
    }

    fn handle_sessions_key(&mut self, key: &KeyEvent) {
        // 正在看正文：这一层只翻页，Esc 回到文件列表
        if self.sessions_text.is_some() {
            let lines = self
                .sessions_text
                .as_ref()
                .map(|t| t.lines().count())
                .unwrap_or(0);
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                    self.sessions_text = None;
                    self.sessions_scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.sessions_scroll = self.sessions_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.sessions_scroll = (self.sessions_scroll + 1).min(lines.saturating_sub(1));
                }
                KeyCode::PageUp | KeyCode::Char('u') => {
                    self.sessions_scroll = self.sessions_scroll.saturating_sub(10);
                }
                KeyCode::PageDown | KeyCode::Char('d') | KeyCode::Char(' ') => {
                    self.sessions_scroll = (self.sessions_scroll + 10).min(lines.saturating_sub(1));
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('v') => self.sessions_open = false,
            KeyCode::Up | KeyCode::Char('k') => {
                self.sessions_selected = self.sessions_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.sessions_selected + 1 < self.sessions_rows.len() {
                    self.sessions_selected += 1;
                }
            }
            KeyCode::Enter => self.open_session_preview(),
            KeyCode::Char('d') => self.delete_session_log(),
            _ => {}
        }
    }

    /// 删掉选中的那份日志：只删 `~/.ells/logs` 里我们自己去认的文件名。
    fn delete_session_log(&mut self) {
        let Some(row) = self.sessions_rows.get(self.sessions_selected).cloned() else {
            return;
        };
        match std::fs::remove_file(&row.path) {
            Ok(()) => {
                let _ = audit::record(AuditKind::Settings, "logs", &format!("删除会话记录 {}", row.name));
                self.sessions_rows = sessionlog::list(50);
                self.sessions_selected = self
                    .sessions_selected
                    .min(self.sessions_rows.len().saturating_sub(1));
                self.status = Some(format!("已删除 {}", row.name));
            }
            Err(err) => self.status = Some(format!("删除失败：{err}")),
        }
    }

    fn handle_tunnels_key(&mut self, key: &KeyEvent) {
        let rows = self.tunnel_rows();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('t') => self.tunnels_open = false,
            KeyCode::Up | KeyCode::Char('k') => {
                self.tunnels_selected = self.tunnels_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.tunnels_selected + 1 < rows.len() {
                    self.tunnels_selected += 1;
                }
            }
            // 启停挪到空格/s：Enter 现在是"进这台主机的规则表格"，
            // 与"Enter = 进入/确认"在全程序一致（表单、会话、标签页都是这个语义）。
            KeyCode::Char(' ') | KeyCode::Char('s') => self.toggle_tunnel(),
            KeyCode::Enter => {
                if let Some(alias) = rows.get(self.tunnels_selected).cloned() {
                    self.tunnels_open = false;
                    self.open_rules_for(alias);
                }
            }
            KeyCode::Char('m') => self.open_ports(),
            KeyCode::Char('x') => self.stop_all_tunnels(),
            _ => {}
        }
    }

    fn handle_delete_confirm_key(&mut self, key: &KeyEvent) {
        match key.code {
            KeyCode::Esc => self.delete_confirm = None,
            KeyCode::Tab | KeyCode::Left | KeyCode::Right => {
                self.confirm_index = 1 - self.confirm_index;
            }
            KeyCode::Up | KeyCode::Down => {}
            KeyCode::Enter => {
                if self.confirm_index == 1 {
                    self.confirm_delete();
                } else {
                    self.delete_confirm = None;
                }
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_delete(),
            KeyCode::Char('n') | KeyCode::Char('N') => self.delete_confirm = None,
            _ => {}
        }
    }

    fn confirm_delete(&mut self) {
        if let Some(alias) = self.delete_confirm.take() {
            self.vault.remove(&alias);
            self.save_vault();
            let _ = audit::record(AuditKind::VaultSaved, &alias, "主机已删除");
            self.status = Some(format!("已删除 {alias}"));
            self.list.selected = self
                .list
                .selected
                .min(self.visible().len().saturating_sub(1));
        }
    }

    fn open_homepage(&mut self) {
        open_url(ui::HOMEPAGE_URL);
        self.status = Some("已在浏览器中打开官网 ells.cn".to_string());
    }

    /// 终端标签名随页面联动：ells-功能页名；会话页显示 ells-主机别名。
    fn sync_term_title(&mut self) {
        let title = match self.screen {
            ScreenKind::Unlock => "ells-解锁保险库".to_string(),
            ScreenKind::List => "ells-主机列表".to_string(),
            ScreenKind::Form => {
                if self.form.editing_alias.is_some() {
                    "ells-编辑主机".to_string()
                } else {
                    "ells-新增主机".to_string()
                }
            }
            ScreenKind::Session => match &self.slots[self.active].session {
                // label 形如 `别名 · user@host:port`，标签页只需要别名
                Some(s) => {
                    format!("ells-{}", s.label.split(" · ").next().unwrap_or("会话"))
                }
                None => "ells-会话".to_string(),
            },
            ScreenKind::Browser => "ells-远程文件".to_string(),
        };
        if self.term_title.as_deref() != Some(title.as_str()) {
            term::set_term_title(&title);
            self.term_title = Some(title);
        }
    }

    /// 开始连接。`target` = 落到哪个标签；None = 当前标签空着就用它，否则新开一个。
    fn start_connect(&mut self, host: Host, target: Option<usize>) {
        let idx = match target {
            Some(idx) => idx.min(self.slots.len() - 1),
            None if self.slots[self.active].is_idle() => self.active,
            None => self.push_tab(),
        };
        {
            let slot = &mut self.slots[idx];
            slot.host = Some(host.clone());
            slot.connecting = true;
            // 谁发起这一路都一样：先把还在等表的定时器作废
            slot.reconnect_seq += 1;
            slot.view = ScreenKind::List;
            slot.status = Some(format!("正在连接 {}…", host.alias));
        }
        self.active = idx;
        self.work = idx;
        self.screen = ScreenKind::List;
        self.settings_open = false;
        self.transfer_popup = false;
        self.status = Some(format!("正在连接 {}…", host.alias));
        // 带了转发规则的主机：连接时就顺手把隧道拉起来，不等用户去面板按 Enter。
        // 隧道用自己的连接，会话挂了它也不挂（见 TunnelManager）。
        if !host.forwards.is_empty() && !self.tunnels.is_running(&host.alias) {
            self.start_tunnel_for(&host);
        }
        self.pending_connect = Some((host, idx));
    }

    /// 起某台主机的隧道（连接时自动、面板里手动都走这里）。
    fn start_tunnel_for(&mut self, host: &Host) {
        if host.forwards.is_empty() {
            return;
        }
        let alias = host.alias.clone();
        let vault = Arc::new(self.vault.clone());
        self.tunnels.start(host, vault);
        self.tunnel_state.insert(alias.clone(), TunnelState::Connecting);
        let _ = audit::record(AuditKind::Tunnel, &alias, "启动");
        self.status = Some(format!(
            "正在启动 {alias} 的隧道（{} 条规则）",
            host.forwards.len()
        ));
    }

    /// `ells <别名>` / `s <别名>`：解锁后按别名直连，只尝试一次。
    fn try_direct_connect(&mut self) {
        let Some(alias) = self.direct_alias.take() else {
            return;
        };
        let lower = alias.to_lowercase();
        let host = self
            .vault
            .hosts
            .iter()
            .find(|h| h.alias == alias || h.alias.to_lowercase() == lower)
            .cloned();
        match host {
            Some(host) => self.start_connect(host, None),
            None => self.status = Some(format!("找不到别名为 `{alias}` 的主机")),
        }
    }

    /// 连接放到后台任务：主机密钥确认要求事件环一直能响应按键。
    fn spawn_connect(&mut self, host: Host, idx: usize) {
        let id = self.slots[idx].id;
        let vault = self.vault.clone();
        let policy = self.hostkey.clone();
        let (cols, rows) = term_size();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let res = RemoteSession::connect(&host, &vault, cols, rows.max(2) - 1, &policy)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Connected { slot: id, res });
        });
    }

    /// 连接意外结束：一键重连同一主机（用户按 Ctrl-] 主动断开、或在远端敲
    /// exit/logout 正常退出都不会走到这里）。
    fn offer_reconnect(&mut self, host: Host, reason: Option<String>) {
        self.focus_host(&host.alias);
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        self.choice = Some(Choice {
            title: "连接已断开".to_string(),
            lines: vec![
                format!("主机：{}", host.target()),
                reason.unwrap_or_else(|| "可能是网络中断、服务器重启或空闲超时。".to_string()),
            ],
            options: vec!["重 连".to_string(), "返 回 列 表".to_string()],
            selected: 0,
            shortcuts: &[('r', 0), ('l', 1)],
            danger: false,
            on_pick: Box::new(move |idx| {
                if matches!(idx, Some(0)) {
                    let _ = tx.send(AppEvent::Reconnect { slot, host });
                }
            }),
        });
    }

    /// 异常掉线：先按退避自动重连，关掉自动或次数用尽才弹窗问用户。
    fn on_session_lost(&mut self, idx: usize, host: Host) {
        let params = self.settings.reconnect;
        let up = self.slots[idx]
            .connected_at
            .map(|at| at.elapsed().as_secs())
            .unwrap_or(u64::MAX);
        // 站稳过之后的一次偶发断开从第 1 次重头算；连上就掉的那种"抖"继续往上爬，
        // 否则退避永远停在 1 秒，等于对着一个坏链路猛撞。
        let start = if params.was_stable(up) {
            1
        } else {
            self.slots[idx].reconnect_attempt.saturating_add(1).max(1)
        };
        self.stop_reconnect(idx);
        if params.max_attempts == 0 {
            self.offer_reconnect(host, None);
            return;
        }
        if start > params.max_attempts {
            self.offer_reconnect(
                host,
                Some(format!(
                    "已自动重连 {} 次都没接上，先停下来问一下。",
                    params.max_attempts
                )),
            );
            return;
        }
        self.arm_reconnect(idx, host, start, None);
    }

    /// 布防一次自动重连：睡到点再发 `AutoReconnect`。期间任何主动操作都会顶掉
    /// 代号（`reconnect_seq`），到点的旧定时器因此作废，不会出现"你以为停了、
    /// 它还在后台一遍遍连"。
    fn arm_reconnect(&mut self, idx: usize, host: Host, attempt: u32, reason: Option<String>) {
        let params = self.settings.reconnect;
        let wait = params.delay_with_jitter(attempt, pseudo_spread());
        let slot = &mut self.slots[idx];
        slot.reconnect_seq += 1;
        slot.reconnect_attempt = attempt;
        slot.reconnect_host = Some(host.clone());
        let seq = slot.reconnect_seq;
        let id = slot.id;
        let secs = wait.as_millis().div_ceil(1000).max(1);
        let head = reason.unwrap_or_else(|| "连接已断开".to_string());
        let text = format!(
            "{head} · {secs} 秒后自动重连（第 {attempt}/{} 次）· 按 Ctrl-] 停止",
            params.max_attempts
        );
        slot.status = Some(text.clone());
        if idx == self.active {
            self.status = Some(text);
        }
        let _ = audit::record(
            AuditKind::Connect,
            &host.alias,
            &format!("自动重连排队 第 {attempt}/{} 次", params.max_attempts),
        );
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let _ = tx.send(AppEvent::AutoReconnect {
                slot: id,
                host,
                attempt,
                seq,
            });
        });
    }

    /// 撤掉这一路在等的自动重连。
    fn stop_reconnect(&mut self, idx: usize) {
        let slot = &mut self.slots[idx];
        slot.reconnect_seq += 1;
        slot.reconnect_attempt = 0;
        slot.reconnect_host = None;
    }

    /// 这一标签下一次该等多久：用户正在看的这一格按 5 秒（连着失败就沿阶梯往上退），
    /// 背景那几格降到 `METRICS_IDLE_EVERY`。
    ///
    /// 判据用 `active` 而不是 `work`：处理事件时 `work` 会被挪到这条事件所属的标签上，
    /// 那不代表用户在看它。间隔是在**回包时**定的，所以换标签不必打断任何在途轮次 ——
    /// 后台那一轮回来自然按慢节奏重接。
    fn metrics_interval(&self, idx: usize) -> std::time::Duration {
        metrics_every(idx == self.active, self.slots[idx].metrics.interval())
    }

    /// 这一标签重新被用户看见：那一分钟的等待不该让他对着旧数看，作废在等的定时器、
    /// 立刻接一轮采集（代号一 +1，在途的旧回包一并作废）。切回去就见到新鲜的数。
    fn revive_metrics(&mut self, idx: usize) {
        {
            let slot = &mut self.slots[idx];
            if slot.session.is_none() || slot.metrics.unsupported {
                return;
            }
            slot.metrics.cancel();
        }
        self.arm_metrics(idx, METRICS_FIRST_DELAY);
    }

    /// 布防这一标签的下一次采集：睡 `every` 再发一个带代号的 `MetricsTick`。
    /// 与自动重连同一套路 —— 一次性定时器 + 代号作废，全局没有 tick。
    fn arm_metrics(&mut self, idx: usize, every: std::time::Duration) {
        if !self.settings.metrics {
            return;
        }
        let slot = &mut self.slots[idx];
        if slot.session.is_none() || slot.metrics.unsupported {
            return;
        }
        let seq = slot.metrics.arm();
        let id = slot.id;
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(every).await;
            let _ = tx.send(AppEvent::MetricsTick { slot: id, seq });
        });
    }

    /// 定时器到点：代号对得上才真的去开采集通道。会话已经没了就把这条链断在这里，
    /// 不再布防 —— 一排进度条不值得在已断的连接上重试。
    fn on_metrics_tick(&mut self, idx: usize, seq: u64) {
        {
            let slot = &self.slots[idx];
            if slot.metrics.seq != seq || slot.metrics.unsupported {
                return;
            }
        }
        let Some(target) = self.slots[idx]
            .session
            .as_ref()
            .and_then(|s| s.session.probe_target())
        else {
            self.slots[idx].metrics.cancel();
            return;
        };
        let id = self.slots[idx].id;
        // 磁盘每一轮都要数，但不是每一轮都普查：普查按 60 秒的节奏数轮次（`disk_rounds`），
        // 中间那几轮采集那边只对"当前最满那块"问一次 statvfs。
        let want_survey = self.slots[idx].metrics.disk_survey_due();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let res = target
                .gather(METRICS_TIMEOUT, METRICS_MAX_BYTES, true, want_survey)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::MetricsProbe { slot: id, seq, res });
        });
    }

    /// 采集回来：先认数，再布防下一轮（所以每条会话任何时刻至多一轮在途）。
    /// 代号对不上说明这期间断开过 / 关过标签 / 改过设置，这一包直接丢，
    /// 绝不能让它把已经消失的那一行又画回来。
    fn on_metrics_probe(
        &mut self,
        idx: usize,
        seq: u64,
        res: std::result::Result<ells_core::Probe, String>,
    ) {
        let keep_polling = {
            let slot = &mut self.slots[idx];
            if slot.metrics.seq != seq {
                return;
            }
            match res {
                Ok(probe) => slot.metrics.adopt(&probe),
                Err(_) => slot.metrics.missed(),
            }
        };
        if keep_polling {
            // 通道连着失败到退避顶端时说一句：数字从此一分钟才动一次，不说明原因
            // 就会被当成"这根条坏了"。除此之外不打扰 —— 断开有自己的状态行。
            if self.slots[idx].metrics.just_capped() {
                self.slots[idx].status = Some(
                    "这台主机的指标通道连续采集失败，已放慢到 60 秒一轮 · 恢复后自动回到 5 秒"
                        .to_string(),
                );
            }
            // 刚差出基线、还欠一个 CPU 数：这一轮之后两秒就接力（用户正看着的才值得抢）。
            let every = metrics_next(
                self.slots[idx].metrics.cpu_pending,
                idx == self.active,
                self.slots[idx].metrics.interval(),
            );
            self.arm_metrics(idx, every);
        } else {
            // 一行忽然消失总得说句为什么：这一格的状态行本来就说不清是网络还是机器
            self.slots[idx].status = Some(
                "这台主机采不到指标，已停止轮询 · 底部那一行还给终端".to_string(),
            );
        }
        self.sync_footer_rows(idx);
    }

    /// 指标开关（含"这台采不到"）变了：把每一标签的视口高度对齐到实际会画的行数，
    /// 并按需接上/停掉采集。切标签不改 —— 行数本来就是按每个标签自己的状态算的。
    ///
    /// 这里**不**把判定采不到的机器救回来：判一次就停手，只有用户特意重新打开
    /// （`set_metrics_setting`）才再给那台一次机会。否则改个主题都要那台机器重采三轮、
    /// 那一行闪回来又消失。
    fn sync_metrics(&mut self) {
        for idx in 0..self.slots.len() {
            let on = self.settings.metrics && self.slots[idx].session.is_some();
            if !on {
                self.slots[idx].metrics.cancel();
            }
            if on && !self.slots[idx].metrics.is_polling() {
                // 只有用户看得见的那一格抢第一轮；背景那几格按自己的降频节奏接上
                let every = if idx == self.active {
                    METRICS_FIRST_DELAY
                } else {
                    self.metrics_interval(idx)
                };
                self.arm_metrics(idx, every);
            }
            self.sync_footer_rows(idx);
        }
    }

    /// 设置页那一行的唯一入口：打开时把"采不到"的判定收回（用户特意再开一次，
    /// 就当新机器待见一回），关掉就把所有在途轮次作废。
    fn set_metrics_setting(&mut self, on: bool) {
        self.settings.metrics = on;
        if on {
            for idx in 0..self.slots.len() {
                if self.slots[idx].metrics.unsupported {
                    self.slots[idx].metrics.reset();
                }
            }
        }
        self.sync_metrics();
    }

    /// 底部指标行占不占那一行：连着、开着、且还没判定采不到。
    /// 判定采不到之后要把这一行还给终端 —— 白占一行高度比看不到数字更糟。
    pub(crate) fn footer_rows(&self, idx: usize) -> u16 {
        let slot = &self.slots[idx];
        if self.settings.metrics && slot.session.is_some() && !slot.metrics.unsupported {
            1
        } else {
            0
        }
    }

    fn sync_footer_rows(&mut self, idx: usize) {
        let rows = self.footer_rows(idx);
        if let Some(s) = self.slots[idx].session.as_mut() {
            s.set_footer_rows(rows);
        }
    }

    /// 定时器到点：代号和次数都对得上才真的去连，否则这就是条过期定时器。
    fn on_reconnect_timer(&mut self, host: Host, attempt: u32, seq: u64) {
        let idx = self.work;
        {
            let slot = &self.slots[idx];
            if slot.reconnect_seq != seq || slot.reconnect_attempt != attempt {
                return;
            }
        }
        // start_connect 会顶掉代号（让这条链上更早的定时器全部作废），所以这里
        // 连完再把"本轮第几次"补回去：连接失败时才知道该接着爬哪一档退避。
        let for_next = host.clone();
        self.start_connect(host, Some(idx));
        let slot = &mut self.slots[idx];
        slot.reconnect_attempt = attempt;
        slot.reconnect_host = Some(for_next);
    }

    /// 连接失败后接着爬退避：还有额度就继续自动，用完才弹窗。
    fn on_connect_failed(&mut self, idx: usize, err: &str) {
        if self.slots[idx].reconnect_attempt == 0 {
            return;
        }
        // 主机密钥没过：那是要用户拍板的事，自动重试等于把同一个确认框连着弹三次
        if key_trouble(err) {
            self.stop_reconnect(idx);
            return;
        }
        let params = self.settings.reconnect;
        let Some(host) = self.slots[idx].reconnect_host.clone() else {
            self.stop_reconnect(idx);
            return;
        };
        let next = self.slots[idx].reconnect_attempt.saturating_add(1);
        if next <= params.max_attempts {
            self.arm_reconnect(idx, host, next, Some(format!("重连失败：{err}")));
            return;
        }
        self.stop_reconnect(idx);
        if idx == self.active {
            self.offer_reconnect(
                host,
                Some(format!(
                    "已自动重连 {} 次都没接上，先停下来问一下。",
                    params.max_attempts
                )),
            );
        }
    }

    fn on_connected(&mut self, res: std::result::Result<RemoteSession, String>) {
        let event_tx = self.event_tx.clone();
        let idx = self.work;
        // 后台标签连上了不该抢界面：用户可能正在另一路上打字
        let foreground = idx == self.active;
        // 记一笔要用别名，而失败分支会把 slot.host 清空，所以在动任何状态前抄下来。
        let alias = self.slots[idx]
            .host
            .as_ref()
            .map(|h| h.alias.clone())
            .unwrap_or_else(|| "?".to_string());
        let mut connected = false;
        let mut failed: Option<String> = None;
        match res {
            Ok(mut session) => {
                let slot = &mut self.slots[idx];
                slot.connecting = false;
                let id = slot.id;
                // 会话标题直接用主机别名，重连/多标签下都比旧的全局缓存可靠
                let label = slot
                    .host
                    .as_ref()
                    .map(|h| format!("{} · {}", h.alias, h.target()))
                    .unwrap_or_else(|| "会话".to_string());
                // 会话落盘：连着就开一个文件，用户关了就收；写不进去只少一份记录
                slot.log = if self.settings.session_log {
                    sessionlog::Recorder::start(&alias).map(|mut lg| {
                        lg.note(&format!("会话已建立 · {label}"));
                        lg
                    })
                } else {
                    None
                };
                if let Some(rx) = session.take_output() {
                    events::spawn_remote_pump(rx, event_tx, id);
                }
                slot.sftp = session.sftp();
                let (cols, rows) = term_size();
                // 新会话先归零上一台机器的数；那一行的位置从连上就占好，
                // 免得出数的一瞬间视口高度跳一行
                slot.metrics.reset();
                let footer = if self.settings.metrics { 1 } else { 0 };
                slot.session = Some(SessionState::new(label, session, rows, cols, footer));
                slot.remote_cwd.clear();
                slot.sz_pending.clear();
                slot.rz_pending = false;
                slot.search = None;
                // 新连接从零开始：清掉上一个会话的传输记录
                slot.browser.transfers.clear();
                slot.cancel = Cancel::default();
                slot.status = None;
                slot.view = ScreenKind::Session;
                // 连上了：这一轮退避到此为止，并记下时刻 —— 下次断开算不算
                // "站稳后的偶发"就看这个
                slot.connected_at = Some(std::time::Instant::now());
                slot.reconnect_attempt = 0;
                slot.reconnect_host = None;
                connected = true;
            }
            Err(err) => {
                let msg = format!("连接失败: {err}");
                let _ = audit::record(AuditKind::ConnectFailed, &alias, &err);
                let slot = &mut self.slots[idx];
                slot.connecting = false;
                slot.host = None;
                slot.status = Some(msg.clone());
                self.status = Some(msg);
                failed = Some(err);
            }
        }
        if connected {
            // "最近使用"排序靠这个时间戳，连上就写，不写列表永远排不出新旧
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if let Some(h) = self.vault.hosts.iter_mut().find(|h| h.alias == alias) {
                h.last_connected = now;
            }
            self.save_vault();
            let _ = audit::record(AuditKind::Connect, &alias, "会话已建立");
            // 底部那排指标：连上立刻探一次（内存/负载/磁盘当场有数），差出基线后两秒
            // 补上 CPU，之后 5 秒一轮（磁盘那条腿 60 秒一次）
            self.arm_metrics(idx, METRICS_FIRST_DELAY);
        }
        if foreground && connected {
            self.screen = ScreenKind::Session;
            self.status = None;
            self.transfer_popup = false;
        }
        if let Some(err) = failed {
            self.on_connect_failed(idx, &err);
        }
        self.refresh_remote_cwd();
    }

    /// 首次见到 / 密钥变更：把决策交给用户，连接任务在等这把密钥的回答。
    fn ask_host_key(&mut self, prompt: HostKeyPrompt) {
        let changed = prompt.trust == KeyTrust::Changed;
        let lines = vec![
            format!("主机：{}:{}", prompt.host, prompt.port),
            format!("算法：{}", prompt.algorithm),
            format!("指纹：{}", prompt.fingerprint),
            if changed {
                "与已记录的密钥不一致：可能是服务器重装，也可能是中间人攻击。请先在服务器侧核对指纹。"
            } else {
                "首次连接该主机。请与云控制台或管理员核对指纹后再接受。"
            }
            .to_string(),
        ];
        let responder = prompt.responder;
        // 决策落在弹窗回调里（回调碰不到 App），审计就在那里直接写文件
        let (audit_host, audit_algo, audit_fp) = (
            format!("{}:{}", prompt.host, prompt.port),
            prompt.algorithm.clone(),
            prompt.fingerprint.clone(),
        );
        let audit_changed = changed;
        self.choice = Some(Choice {
            title: if changed { "主机密钥已变更" } else { "确认主机密钥" }.to_string(),
            lines,
            options: if changed {
                vec!["拒 绝".to_string(), "我已核对，更新记录".to_string()]
            } else {
                vec!["拒 绝".to_string(), "接受并记录".to_string()]
            },
            // 密钥变更时默认停在"拒绝"；首次连接按 TOFU 默认"接受"
            selected: if changed { 0 } else { 1 },
            shortcuts: &[('n', 0), ('y', 1)],
            danger: changed,
            on_pick: Box::new(move |idx| {
                let accept = matches!(idx, Some(1));
                let _ = responder.send(accept);
                let _ = audit::record(
                    if audit_changed {
                        AuditKind::HostKeyChanged
                    } else {
                        AuditKind::HostKeyTrusted
                    },
                    &audit_host,
                    &format!(
                        "{} {audit_algo} {}{audit_fp}",
                        if accept { "接受" } else { "拒绝" },
                        if audit_changed && !accept { "（拒绝更新）" } else { "" }
                    ),
                );
            }),
        });
    }

    /// 目标已存在：取消 / 改名保留双方 / 覆盖。默认落在"改名"，不预设丢数据。
    fn ask_conflict(&mut self, prompt: ConflictPrompt) {
        let mut lines = vec![
            format!("文件：{}", prompt.name),
            format!("目标：{}", prompt.target),
        ];
        // 冲突也可能来自后台标签：不说清楚用户会以为弹窗是凭空出现的
        if self.work != self.active {
            lines.insert(0, format!("标签：{}", self.slots[self.work].title()));
        }
        if let Some(size) = prompt.size {
            lines.push(format!("已存在：{}", ui::human_size(size)));
        }
        lines.push(format!(
            "覆盖会丢掉原有内容，改名会把两份都留下（{}）。",
            if prompt.location == "远端" { "远端" } else { "本地" }
        ));
        let responder = prompt.responder;
        self.choice = Some(Choice {
            title: format!("{}已存在同名文件", prompt.location),
            lines,
            options: vec![
                "取 消".to_string(),
                "改名保留双方".to_string(),
                "覆盖原文件".to_string(),
            ],
            selected: 1,
            shortcuts: &[('n', 0), ('r', 1), ('o', 2)],
            danger: false,
            on_pick: Box::new(move |idx| {
                let decision = match idx {
                    Some(2) => Conflict::Overwrite,
                    Some(1) => Conflict::Rename,
                    _ => Conflict::Cancel,
                };
                let _ = responder.send(decision);
            }),
        });
    }

    fn answer_choice(&mut self, idx: Option<usize>) {
        if let Some(choice) = self.choice.take() {
            (choice.on_pick)(idx);
        }
    }

    fn handle_choice_key(&mut self, key: &KeyEvent) {
        let Some(choice) = &self.choice else {
            return;
        };
        let n = choice.options.len().max(1);
        let moved = match key.code {
            KeyCode::Left => Some((choice.selected + n - 1) % n),
            KeyCode::Right | KeyCode::Tab => Some((choice.selected + 1) % n),
            _ => None,
        };
        if let Some(next) = moved {
            if let Some(c) = self.choice.as_mut() {
                c.selected = next;
            }
            return;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => {
                let selected = self.choice.as_ref().map(|c| c.selected).unwrap_or(0);
                self.answer_choice(Some(selected));
            }
            KeyCode::Esc => self.answer_choice(None),
            KeyCode::Char(c) => {
                let hit = self
                    .choice
                    .as_ref()
                    .and_then(|ch| ch.shortcuts.iter().find(|(k, _)| *k == c))
                    .map(|(_, idx)| *idx);
                if let Some(idx) = hit {
                    self.answer_choice(Some(idx));
                }
            }
            _ => {}
        }
    }

    fn handle_prompt_key(&mut self, key: &KeyEvent, ctrl: bool) {
        match key.code {
            KeyCode::Esc => self.answer_prompt(None),
            KeyCode::Enter => {
                let raw = self.prompt.as_ref().map(|p| p.buffer.clone()).unwrap_or_default();
                let value = raw.trim();
                let allow_empty = self.prompt.as_ref().is_some_and(|p| p.allow_empty);
                if value.is_empty() && !allow_empty {
                    if let Some(p) = self.prompt.as_mut() {
                        p.error = Some("内容不能为空".to_string());
                    }
                    return;
                }
                self.answer_prompt(Some(value.to_string()));
            }
            KeyCode::Backspace => {
                if let Some(p) = self.prompt.as_mut() {
                    p.buffer.pop();
                    p.error = None;
                }
            }
            KeyCode::Delete => {
                if let Some(p) = self.prompt.as_mut() {
                    p.buffer.clear();
                    p.error = None;
                }
            }
            KeyCode::Char(c) if !ctrl && !c.is_control() => {
                if let Some(p) = self.prompt.as_mut() {
                    p.buffer.push(c);
                    p.error = None;
                }
            }
            _ => {}
        }
    }

    fn answer_prompt(&mut self, value: Option<String>) {
        if let Some(prompt) = self.prompt.take() {
            (prompt.on_done)(value);
        }
    }

    /// 粘贴：会话页原样交给远端（与原生终端一致，换行由远端 shell 处理）；
    /// 表单/解锁页去掉全部控制字符，避免多行粘贴把单行输入框撑成一段乱码。
    fn handle_paste(&mut self, text: &str) {
        if self.choice.is_some() {
            return;
        }
        if self.screen == ScreenKind::Session {
            if !text.is_empty() {
                if let Some(s) = &mut self.slots[self.work].session {
                    s.handle_paste(text);
                }
            }
            return;
        }
        let clean = sanitize_input(text);
        if clean.is_empty() {
            return;
        }
        match self.screen {
            ScreenKind::Unlock if !self.unlock.busy => {
                self.unlock.input.push_str(&clean);
                self.unlock.error = None;
            }
            ScreenKind::Form if self.form.footer.is_none() => {
                if let Some(field) = self.form.fields.get_mut(self.form.focus) {
                    field.value.push_str(&clean);
                }
                self.form.error = None;
            }
            _ => {}
        }
    }

    fn handle_form_key(&mut self, key: &KeyEvent, ctrl: bool) {
        if self.form.jump_picker.is_some() {
            self.handle_jump_picker_key(key);
            return;
        }
        let vis = self.form.visible();
        let focus = self.form.focus;
        let kind = self.form.fields.get(focus).map(|f| f.kind);
        let role = self.form.fields.get(focus).map(|f| f.role);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                // Ctrl-C clears the focused field instead of quitting;
                // the auth selector resets to its default rather than
                // going empty (an empty auth used to silently save as agent).
                if let Some(field) = self.form.fields.get_mut(focus) {
                    if field.role == FieldRole::Auth {
                        field.value = "password".to_string();
                    } else {
                        field.value.clear();
                    }
                }
                self.form.clear_hidden();
            }
            KeyCode::Char('f') if ctrl => {
                self.open_picker();
            }
            KeyCode::Char('r') if ctrl && kind == Some(FieldKind::Secret) => {
                // Ctrl-R 显形/遮回：十次"存完连不上"有八次是密码末尾多敲了一个字符，
                // 不让用户看一眼自己敲的东西就只能反复重填。
                self.form.reveal_secret = !self.form.reveal_secret;
            }
            KeyCode::Char('j') if ctrl && role == Some(FieldRole::Jump) => {
                self.open_jump_picker();
            }
            KeyCode::Esc => {
                self.form.error = None;
                self.form.footer = None;
                self.screen = ScreenKind::List;
            }
            KeyCode::Tab | KeyCode::Down => {
                match self.form.footer {
                    None => {
                        if let Some(pos) = vis.iter().position(|&i| i == focus) {
                            if pos + 1 < vis.len() {
                                self.form.focus = vis[pos + 1];
                            } else {
                                self.form.footer = Some(0);
                            }
                        } else if let Some(&first) = vis.first() {
                            self.form.focus = first;
                        }
                    }
                    Some(0) => self.form.footer = Some(1),
                    Some(_) => {
                        self.form.footer = None;
                        if let Some(&first) = vis.first() {
                            self.form.focus = first;
                        }
                    }
                }
            }
            KeyCode::Up => {
                match self.form.footer {
                    None => {
                        if let Some(pos) = vis.iter().position(|&i| i == focus) {
                            if pos > 0 {
                                self.form.focus = vis[pos - 1];
                            }
                        }
                    }
                    Some(1) => self.form.footer = Some(0),
                    Some(_) => {
                        self.form.footer = None;
                        if let Some(&last) = vis.last() {
                            self.form.focus = last;
                        }
                    }
                }
            }
            KeyCode::Left | KeyCode::Right => {
                if let Some(f) = self.form.footer {
                    self.form.footer = Some(1 - f);
                } else if kind == Some(FieldKind::AuthChoice) {
                    let dir = if key.code == KeyCode::Right { 1 } else { -1 };
                    self.form.cycle_auth(dir);
                    self.form.error = None;
                }
            }
            KeyCode::Enter => match self.form.footer {
                // 保存只能通过按钮（聚焦后回车或鼠标点击）
                Some(0) => self.form_submit(),
                Some(_) => {
                    self.form.error = None;
                    self.form.footer = None;
                    self.screen = ScreenKind::List;
                }
                None => match role {
                    // 选择型输入框：Enter 打开选择器，换行只用上下键
                    Some(FieldRole::KeyPath) => self.open_picker(),
                    Some(FieldRole::Jump) => self.open_jump_picker(),
                    _ => match vis.iter().position(|&i| i == focus) {
                        Some(pos) if pos + 1 < vis.len() => self.form.focus = vis[pos + 1],
                        Some(_) => self.form.footer = Some(0),
                        None => {
                            if let Some(&first) = vis.first() {
                                self.form.focus = first;
                            }
                        }
                    },
                },
            },
            KeyCode::Char(c) if !ctrl => {
                let mut target = focus;
                if self.form.footer.is_some() {
                    // 按钮聚焦态直接打字：回到第一个输入框并录入
                    self.form.footer = None;
                    if let Some(&first) = vis.first() {
                        self.form.focus = first;
                        target = first;
                    }
                }
                if let Some(field) = self.form.fields.get_mut(target) {
                    field.value.push(c);
                }
                self.form.error = None;
            }
            KeyCode::Backspace | KeyCode::Delete => {
                if self.form.footer.is_some() {
                    return;
                }
                if let Some(field) = self.form.fields.get_mut(focus) {
                    // A file path is never edited char-by-char: clear it in one go.
                    if field.role == FieldRole::KeyPath {
                        field.value.clear();
                    } else {
                        field.value.pop();
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_jump_picker_key(&mut self, key: &KeyEvent) {
        match key.code {
            KeyCode::Up => self.move_jump_picker(false),
            KeyCode::Down | KeyCode::Tab => self.move_jump_picker(true),
            KeyCode::Enter => self.confirm_jump_picker(),
            KeyCode::Esc | KeyCode::Char('j') => {
                self.form.jump_picker = None;
            }
            _ => {}
        }
    }

    fn move_jump_picker(&mut self, down: bool) {
        let Some(picker) = self.form.jump_picker.as_mut() else {
            return;
        };
        if down {
            if picker.selected + 1 < picker.items.len() {
                picker.selected += 1;
            }
        } else {
            picker.selected = picker.selected.saturating_sub(1);
        }
    }

    fn confirm_jump_picker(&mut self) {
        let alias = match &self.form.jump_picker {
            Some(picker) if !picker.items.is_empty() => {
                let i = picker.selected.min(picker.items.len() - 1);
                picker.items[i].0.clone()
            }
            _ => return,
        };
        self.form.jump_picker = None;
        if let Some(f) = self
            .form
            .fields
            .iter_mut()
            .find(|f| f.role == FieldRole::Jump)
        {
            f.value = alias;
        }
    }

    fn open_jump_picker(&mut self) {
        let current = self.form.editing_alias.clone();
        let mut items: Vec<(String, String)> = vec![("".into(), "（无跳板机 · 直连）".into())];
        items.extend(self.vault.hosts.iter().filter_map(|h| {
            if current.as_deref() == Some(h.alias.as_str()) {
                return None; // cannot jump through itself
            }
            Some((h.alias.clone(), format!("{} · {}", h.alias, h.target())))
        }));
        let current_jump = self.form.value_of(FieldRole::Jump).to_string();
        let selected = items
            .iter()
            .position(|(a, _)| *a == current_jump)
            .unwrap_or(0);
        self.form.jump_picker = Some(JumpPicker { items, selected });
    }

    fn open_picker(&mut self) {
        let field = self.form.focus;
        if self
            .form
            .fields
            .get(field)
            .map(|f| f.role != FieldRole::KeyPath)
            .unwrap_or(true)
        {
            self.form.error = Some("只有\"私钥路径\"可以用文件选择框".to_string());
            return;
        }
        let current = self
            .form
            .fields
            .get(field)
            .map(|f| f.value.clone())
            .unwrap_or_default();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            let start = match current.rfind(['/', '\\']) {
                Some(pos) if pos > 0 => {
                    let p = PathBuf::from(&current[..pos]);
                    if p.is_dir() {
                        p
                    } else {
                        home.clone()
                    }
                }
                _ => home,
            };
            let path = tokio::task::spawn_blocking(move || {
                crate::dialog::pick_file(
                    "选择私钥文件",
                    start,
                    vec![
                        (
                            "私钥文件 (*.pem *.key *.ppk)".to_string(),
                            vec!["pem".to_string(), "key".to_string(), "ppk".to_string()],
                        ),
                        ("所有文件".to_string(), vec!["*".to_string()]),
                    ],
                )
            })
            .await
            .ok()
            .flatten();
            let _ = tx.send(AppEvent::PickedFile { field, path });
        });
    }

    /// Copy a freshly-chosen private key into `~/.ells/keys` so deleting or
    /// moving the original file cannot break the connection.
    fn backup_key(&self, host: &mut Host) -> std::result::Result<bool, String> {
        let Auth::PrimaryKey { path, passphrase } = &host.auth else {
            return Ok(false);
        };
        let src = PathBuf::from(path);
        let keys_dir = dirs::home_dir()
            .ok_or("无法定位用户主目录")?
            .join(".ells")
            .join("keys");
        if src.starts_with(&keys_dir) {
            return Ok(false); // already backed up
        }
        std::fs::create_dir_all(&keys_dir).map_err(|e| format!("创建密钥目录失败: {e}"))?;
        let fname = src
            .file_name()
            .ok_or("私钥路径无效")?
            .to_string_lossy()
            .into_owned();
        let alias_safe: String = host
            .alias
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let dest = keys_dir.join(format!("{alias_safe}_{fname}"));
        std::fs::copy(&src, &dest).map_err(|e| format!("备份私钥失败: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600));
        }
        host.auth = Auth::PrimaryKey {
            path: dest.to_string_lossy().into_owned(),
            passphrase: passphrase.clone(),
        };
        Ok(true)
    }

    fn form_submit(&mut self) {
        let prior = self
            .form
            .editing_alias
            .as_ref()
            .and_then(|old| self.vault.hosts.iter().find(|h| &h.alias == old))
            .cloned();
        match self.form.build_host(prior.as_ref()) {
            Ok(mut host) => {
                let alias = host.alias.clone();
                match self.backup_key(&mut host) {
                    Ok(copied) => {
                        if let Some(old) = self.form.editing_alias.take() {
                            self.vault.remove(&old);
                        }
                        self.vault.upsert(host);
                        self.save_vault();
                        let _ = audit::record(
                            AuditKind::VaultSaved,
                            &alias,
                            if prior.is_some() { "主机已更新" } else { "主机已新增" },
                        );
                        self.focus_host(&alias);
                        self.status = Some(if copied {
                            format!("已保存 {alias}（私钥已备份到 ~/.ells/keys）")
                        } else {
                            format!("已保存 {alias}")
                        });
                        self.screen = ScreenKind::List;
                    }
                    Err(err) => {
                        self.form.error = Some(err);
                    }
                }
            }
            Err(err) => {
                self.form.error = Some(err);
            }
        }
    }

    fn save_vault(&self) {
        let Some(vk) = &self.vault_key else {
            return; // dev mode has no vault to persist
        };
        if let Err(err) = vault::save_vault_key(&self.vault, vk) {
            tracing::warn!(%err, "保存保险库失败");
        }
    }
}

impl FormState {
    fn blank() -> Self {
        Self {
            fields: Vec::new(),
            focus: 0,
            footer: None,
            editing_alias: None,
            error: None,
            jump_picker: None,
            reveal_secret: false,
        }
    }

    fn new(host: Option<&Host>) -> Self {
        let auth_value = match host.map(|h| &h.auth) {
            Some(Auth::Password) | None => "password",
            Some(Auth::PrimaryKey { .. }) => "key",
            Some(Auth::Agent) => "agent",
        };
        let key_path = match host.map(|h| &h.auth) {
            Some(Auth::PrimaryKey { path, .. }) => path.clone(),
            _ => String::new(),
        };
        let key_pass = match host.map(|h| &h.auth) {
            Some(Auth::PrimaryKey {
                passphrase: Some(p),
                ..
            }) => p.clone(),
            _ => String::new(),
        };
        let field = |role, label, kind, value: String| Field {
            role,
            label,
            value,
            kind,
        };
        Self {
            fields: vec![
                field(FieldRole::Alias, "别名", FieldKind::Text, host.map(|h| h.alias.clone()).unwrap_or_default()),
                field(FieldRole::Hostname, "主机", FieldKind::Text, host.map(|h| h.hostname.clone()).unwrap_or_default()),
                field(FieldRole::Port, "端口", FieldKind::Number, host.map(|h| h.port.to_string()).unwrap_or_else(|| "22".into())),
                field(FieldRole::User, "用户", FieldKind::Text, host.map(|h| h.user.clone()).unwrap_or_default()),
                field(FieldRole::Auth, "认证方式 ←/→", FieldKind::AuthChoice, auth_value.to_string()),
                field(FieldRole::Password, "密码", FieldKind::Secret, host.and_then(|h| h.password.clone()).unwrap_or_default()),
                field(FieldRole::KeyPath, "私钥路径 ctrl-f", FieldKind::Text, key_path),
                field(FieldRole::KeyPass, "私钥口令(可选)", FieldKind::Secret, key_pass),
                field(FieldRole::Jump, "跳板机(可选) ctrl-j", FieldKind::Text, host.and_then(|h| h.jump.clone()).unwrap_or_default()),
                field(FieldRole::Group, "分组(可选)", FieldKind::Text, host.and_then(|h| h.group.clone()).unwrap_or_default()),
                field(FieldRole::Tags, "标签(逗号分隔)", FieldKind::Text, host.map(|h| h.tags.join(", ")).unwrap_or_default()),
                field(
                    FieldRole::Forwards,
                    "转发 -L 8080:127.0.0.1:5432 -D 1080",
                    FieldKind::Text,
                    host.map(|h| {
                        h.forwards
                            .iter()
                            .map(|f| f.label())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default(),
                ),
            ],
            focus: 0,
            footer: None,
            editing_alias: host.map(|h| h.alias.clone()),
            error: None,
            jump_picker: None,
            reveal_secret: false,
        }
    }

    /// 从一台已有主机"新建同类"：带上结构性字段（用户/端口/认证方式/私钥路径/
    /// 跳板/分组/标签/转发），别名、主机名和密码留空——身份和凭据必须自己填，
    /// 免得把上一台的口令悄悄复制进新条目里。
    fn template(host: Option<&Host>) -> Self {
        let mut form = Self::new(host);
        if host.is_some() {
            form.editing_alias = None;
            for f in &mut form.fields {
                if matches!(
                    f.role,
                    FieldRole::Alias
                        | FieldRole::Hostname
                        | FieldRole::Password
                        | FieldRole::KeyPass
                ) {
                    f.value.clear();
                }
            }
        }
        form
    }

    /// Field indices currently shown for the selected auth method.
    pub fn visible(&self) -> Vec<usize> {
        let auth = self.auth_value();
        self.fields
            .iter()
            .enumerate()
            .filter(|(_, f)| match f.role {
                FieldRole::Password => auth == "password",
                FieldRole::KeyPath | FieldRole::KeyPass => auth == "key",
                _ => true,
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn auth_value(&self) -> &str {
        self.value_of(FieldRole::Auth)
    }

    fn value_of(&self, role: FieldRole) -> &str {
        self.fields
            .iter()
            .find(|f| f.role == role)
            .map(|f| f.value.as_str())
            .unwrap_or("")
    }

    fn cycle_auth(&mut self, dir: i32) {
        const OPTS: [&str; 3] = ["password", "key", "agent"];
        let idx = OPTS
            .iter()
            .position(|o| *o == self.auth_value())
            .unwrap_or(0);
        let next = (((idx as i32 + dir) % OPTS.len() as i32 + OPTS.len() as i32) as usize)
            % OPTS.len();
        if let Some(f) = self
            .fields
            .iter_mut()
            .find(|f| f.role == FieldRole::Auth)
        {
            f.value = OPTS[next].to_string();
        }
        self.clear_hidden();
    }

    /// Switching auth method wipes the now-hidden credential fields so stale
    /// secrets never leak into the saved host.
    fn clear_hidden(&mut self) {
        let vis = self.visible();
        for (i, f) in self.fields.iter_mut().enumerate() {
            if !vis.contains(&i)
                && matches!(f.role, FieldRole::Password | FieldRole::KeyPath | FieldRole::KeyPass)
            {
                f.value.clear();
            }
        }
        if !vis.contains(&self.focus) {
            self.focus = self
                .fields
                .iter()
                .position(|f| f.role == FieldRole::Auth)
                .unwrap_or(0);
        }
    }

    /// `prior` 是编辑中的原主机：收藏、备注、最近连接时间这些不在表单里的字段
    /// 必须从它继承，否则用户每编辑一次就把自己的标记清干净。
    fn build_host(&self, prior: Option<&Host>) -> std::result::Result<Host, String> {
        let alias = self.value_of(FieldRole::Alias).trim().to_string();
        let hostname = self.value_of(FieldRole::Hostname).trim().to_string();
        let port: u16 = self
            .value_of(FieldRole::Port)
            .trim()
            .parse()
            .map_err(|_| "端口必须是 1-65535 的数字".to_string())?;
        let user = self.value_of(FieldRole::User).trim().to_string();
        // 只点名缺的那几项：三个都填了只差用户时，报"别名、主机、用户是必填项"
        // 等于让用户把已经填对的重新看一遍。
        let mut missing: Vec<&str> = Vec::new();
        if alias.is_empty() {
            missing.push("别名");
        }
        if hostname.is_empty() {
            missing.push("主机");
        }
        if user.is_empty() {
            missing.push("用户");
        }
        if !missing.is_empty() {
            return Err(format!("{}是必填项", missing.join("、")));
        }
        let auth = match self.auth_value() {
            "password" => {
                if self.value_of(FieldRole::Password).is_empty() {
                    return Err("密码认证需要填写密码".to_string());
                }
                Auth::Password
            }
            "key" => {
                if self.value_of(FieldRole::KeyPath).trim().is_empty() {
                    return Err("密钥认证需要填写私钥路径".to_string());
                }
                let pass = self.value_of(FieldRole::KeyPass);
                Auth::PrimaryKey {
                    path: self.value_of(FieldRole::KeyPath).trim().to_string(),
                    passphrase: if pass.is_empty() {
                        None
                    } else {
                        Some(pass.to_string())
                    },
                }
            }
            "agent" => Auth::Agent,
            _ => return Err("认证方式为空，请用 ←/→ 重新选择".to_string()),
        };
        let jump = self.value_of(FieldRole::Jump).trim().to_string();
        if !jump.is_empty() && jump == alias {
            return Err("跳板机不能是该主机自身".to_string());
        }
        let (forwards, bad) = Forward::parse_specs(self.value_of(FieldRole::Forwards));
        if !bad.is_empty() {
            return Err(format!("转发规则看不懂：{}", bad.join("、")));
        }
        let group = self.value_of(FieldRole::Group).trim().to_string();
        let tags: Vec<String> = self
            .value_of(FieldRole::Tags)
            .split([',', '，', ' '])
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| t.trim_start_matches('#').to_string())
            .filter(|t| !t.is_empty())
            .collect();
        Ok(Host {
            alias,
            hostname,
            port,
            user,
            auth,
            password: if self.auth_value() == "password" {
                Some(self.value_of(FieldRole::Password).to_string())
            } else {
                None
            },
            jump: if jump.is_empty() { None } else { Some(jump) },
            note: prior.and_then(|p| p.note.clone()),
            forwards,
            group: if group.is_empty() { None } else { Some(group) },
            tags,
            favorite: prior.is_some_and(|p| p.favorite),
            last_connected: prior.map(|p| p.last_connected).unwrap_or(0),
        })
    }
}

/// 本地默认下载目录（拿不到就落到当前目录）。
fn download_home() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 远端目录里已占用的名字。listing 失败时退化为只含冲突名，仍能选出未占用候选。
async fn remote_taken(
    sftp: &SftpSession,
    dir: &str,
    fallback: &str,
) -> std::collections::HashSet<String> {
    let mut taken: std::collections::HashSet<String> = ells_transfer::list(sftp, dir)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.name)
        .collect();
    if taken.is_empty() {
        taken.insert(fallback.to_string());
    }
    taken
}

/// 本地目录里已占用的名字（语义与 remote_taken 相同）。
fn local_taken(dir: &Path, fallback: &str) -> std::collections::HashSet<String> {
    let mut taken = std::collections::HashSet::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                taken.insert(name.to_string());
            }
        }
    }
    if taken.is_empty() {
        taken.insert(fallback.to_string());
    }
    taken
}

/// 目标已存在时把决定交给 UI（取消 / 改名 / 覆盖），返回最终使用的名字；
/// None = 用户放弃。UI 不回答（比如会话断了）也按放弃处理，绝不默默覆盖。
async fn ask_overwrite<T: Fn(&str) -> bool>(
    tx: &mpsc::UnboundedSender<AppEvent>,
    slot: u32,
    location: &'static str,
    name: &str,
    target: &str,
    size: Option<u64>,
    taken: T,
) -> Option<String> {
    let (responder, answer) = oneshot::channel();
    if tx
        .send(AppEvent::Conflict {
            slot,
            prompt: ConflictPrompt {
                location,
                name: name.to_string(),
                target: target.to_string(),
                size,
                responder,
            },
        })
        .is_err()
    {
        return None;
    }
    match answer.await.ok()? {
        Conflict::Cancel => None,
        Conflict::Overwrite => Some(name.to_string()),
        Conflict::Rename => Some(ells_transfer::unique_in(name, taken)),
    }
}

/// 一次上传（文件或整个目录树）：先问覆盖，再真跑，名字可能因"改名"而变。
async fn run_upload(
    sftp: Arc<SftpSession>,
    local: PathBuf,
    dest: String,
    name: String,
    ptx: mpsc::UnboundedSender<Progress>,
    tx: &mpsc::UnboundedSender<AppEvent>,
    slot: u32,
    cancel: &Cancel,
) -> std::result::Result<String, (String, String)> {
    let is_dir = match std::fs::metadata(&local) {
        Ok(md) => md.is_dir(),
        Err(err) => {
            return Err((name, format!("无法读取本地路径 {}：{err}", local.display())));
        }
    };
    let target = ells_transfer::remote_join(&dest, &name);
    let mut final_name = name.clone();
    match ells_transfer::remote_meta(&sftp, &target).await {
        Ok(Some(meta)) => {
            let taken = remote_taken(&sftp, &dest, &name).await;
            let chosen =
                ask_overwrite(tx, slot, "远端", &name, &target, Some(meta.size), |c| {
                    taken.contains(c)
                })
                .await;
            match chosen {
                Some(n) => final_name = n,
                None => {
                    let msg = format!("已取消上传：{name}");
                    return Err((name, msg));
                }
            }
        }
        Ok(None) => {}
        Err(err) => return Err((name, format!("{err:#}"))),
    }
    let _ = tx.send(AppEvent::SftpStarted {
        slot,
        label: final_name.clone(),
        direction: "上传",
    });
    let res = if is_dir {
        ells_transfer::upload_tree(&sftp, &local, &dest, &final_name, ptx, cancel)
            .await
            .map(|_| ())
    } else {
        let remote = ells_transfer::remote_join(&dest, &final_name);
        ells_transfer::upload(&sftp, &local, remote, ptx, cancel).await
    };
    match res {
        Ok(()) => Ok(final_name),
        Err(err) => Err((final_name, format!("{err:#}"))),
    }
}

/// 一次下载（文件或整个目录树）。`confirm_overwrite` = false 用于系统另存为
/// （原生对话框自己已经问过覆盖，不能再问一遍）。
async fn run_download(
    sftp: Arc<SftpSession>,
    entry: FileEntry,
    dest_dir: PathBuf,
    name: String,
    confirm_overwrite: bool,
    ptx: mpsc::UnboundedSender<Progress>,
    tx: &mpsc::UnboundedSender<AppEvent>,
    slot: u32,
    cancel: &Cancel,
) -> std::result::Result<String, (String, String)> {
    let label = entry.name.clone();
    // 类型以服务器为准：sz 只给了路径，浏览器条目的 is_dir 可能已经过期
    let is_dir = match ells_transfer::remote_meta(&sftp, &entry.path).await {
        Ok(Some(meta)) => meta.is_dir,
        _ => entry.is_dir,
    };
    let mut final_name = name.clone();
    let target = dest_dir.join(&name);
    if confirm_overwrite && ells_transfer::local_exists(&target) {
        let size = std::fs::metadata(&target)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len());
        let shown = target.display().to_string();
        let taken = local_taken(&dest_dir, &name);
        let chosen =
            ask_overwrite(tx, slot, "本地", &name, &shown, size, |c| taken.contains(c)).await;
        match chosen {
            Some(n) => final_name = n,
            None => {
                let msg = format!("已取消下载：{label}");
                return Err((label, msg));
            }
        }
    }
    let _ = tx.send(AppEvent::SftpStarted {
        slot,
        label: final_name.clone(),
        direction: "下载",
    });
    let res = if is_dir {
        ells_transfer::download_tree(&sftp, &entry.path, &dest_dir, &final_name, ptx, cancel)
            .await
            .map(|_| ())
    } else {
        ells_transfer::download(
            &sftp,
            entry.path.clone(),
            &dest_dir,
            ptx,
            &final_name,
            cancel,
        )
        .await
        .map(|_| ())
    };
    match res {
        Ok(()) => Ok(final_name),
        Err(err) => Err((final_name, format!("{err:#}"))),
    }
}

fn term_size() -> (u16, u16) {
    crossterm::terminal::size()
        .map(|(c, r)| (c as u16, r as u16))
        .unwrap_or((80, 24))
}

/// 鼠标按下坐标 (column,row) 是否落在矩形内（终端单元格，0 基）。
fn hit(r: Rect, column: u16, row: u16) -> bool {
    column >= r.x
        && column < r.x.saturating_add(r.width)
        && row >= r.y
        && row < r.y.saturating_add(r.height)
}

/// 保活间隔在 15/30/60/120/300 之间步进（已是区间外自定义值时向档位靠拢）。
fn step_keepalive(cur: u64, up: bool) -> u64 {
    const STEPS: [u64; 5] = [15, 30, 60, 120, 300];
    if up {
        STEPS.iter().copied().find(|s| *s > cur).unwrap_or(cur)
    } else {
        STEPS.iter().copied().rev().find(|s| *s < cur).unwrap_or(cur)
    }
}

/// 点击/回车时循环切换保活档位：15→30→60→120→300→15。
fn cycle_keepalive(cur: u64) -> u64 {
    const STEPS: [u64; 5] = [15, 30, 60, 120, 300];
    match STEPS.iter().position(|s| *s == cur) {
        Some(i) => STEPS[(i + 1) % STEPS.len()],
        None => STEPS[0],
    }
}

/// 自动重连的次数档位：`0` = 关掉自动重连（回到弹窗问），最后一档当"无限"。
/// 只把"几次"这件事放进界面，退避的秒数/抖动留在 `settings.ini`：
/// 前者人人要用，后者是少数人的调参，摆在弹窗里只会把设置页变成表单。
const RECONNECT_LADDER: [u32; 6] = [0, 1, 2, 3, 5, ells_core::reconnect::MAX_ATTEMPTS_CAP];

/// ←/→ 微调重连次数：不在档位上的值（手改过 ini）回到最近的一档。
fn step_reconnect_attempts(cur: u32, up: bool) -> u32 {
    if up {
        RECONNECT_LADDER.iter().copied().find(|v| *v > cur).unwrap_or(cur)
    } else {
        RECONNECT_LADDER
            .iter()
            .copied()
            .rev()
            .find(|v| *v < cur)
            .unwrap_or(cur)
    }
}

/// Enter/点击：在档位里循环，越界回到"关"。
fn cycle_reconnect_attempts(cur: u32) -> u32 {
    match RECONNECT_LADDER.iter().position(|v| *v == cur) {
        Some(i) => RECONNECT_LADDER[(i + 1) % RECONNECT_LADDER.len()],
        None => RECONNECT_LADDER[1],
    }
}

/// 去掉全部控制字符（含 \r \n \t）：单行输入框只接受可打印内容。
fn sanitize_input(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// 用系统默认浏览器打开 URL（URL 为编译期常量，无注入风险）。
fn open_url(url: &str) {
    let spawned = if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(err) = spawned {
        tracing::warn!(%err, "打开默认浏览器失败");
    }
}

#[cfg(test)]
mod form_tests {
    use super::*;

    fn sample() -> Host {
        Host {
            alias: "web-prod".into(),
            hostname: "10.0.0.1".into(),
            port: 2222,
            user: "deploy".into(),
            auth: Auth::Agent,
            password: Some("hunter2".into()),
            jump: Some("bastion".into()),
            group: Some("生产".into()),
            tags: vec!["db".into(), "eu".into()],
            forwards: vec![Forward::Local {
                bind: None,
                listen_port: 5432,
                dest_host: "127.0.0.1".into(),
                dest_port: 5432,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn template_carries_structure_but_not_identity_or_secrets() {
        let f = FormState::template(Some(&sample()));
        assert_eq!(f.editing_alias, None, "模板必须是新增，不是编辑那台");
        assert_eq!(f.value_of(FieldRole::User), "deploy");
        assert_eq!(f.value_of(FieldRole::Port), "2222");
        assert_eq!(f.value_of(FieldRole::Jump), "bastion");
        assert_eq!(f.value_of(FieldRole::Group), "生产");
        assert_eq!(f.value_of(FieldRole::Tags), "db, eu");
        assert!(
            f.value_of(FieldRole::Forwards).contains("-L 5432:127.0.0.1:5432"),
            "转发作家没带过来: {}",
            f.value_of(FieldRole::Forwards)
        );
        for role in [
            FieldRole::Alias,
            FieldRole::Hostname,
            FieldRole::Password,
            FieldRole::KeyPass,
        ] {
            assert_eq!(f.value_of(role), "", "{role:?} 应该留空让用户自己填");
        }
    }

    #[test]
    fn template_without_selection_is_blank() {
        let f = FormState::template(None);
        assert_eq!(f.editing_alias, None);
        assert_eq!(f.value_of(FieldRole::Port), "22");
        assert_eq!(f.value_of(FieldRole::Group), "");
        // 空表提交要挡下来，不能写出一个没有别名的主机
        assert_eq!(f.build_host(None).unwrap_err(), "别名、主机、用户是必填项");
    }
}

#[cfg(test)]
mod reconnect_tests {
    use super::*;

    #[test]
    fn ladder_cycles_through_off_and_infinite() {
        assert_eq!(cycle_reconnect_attempts(0), 1);
        assert_eq!(cycle_reconnect_attempts(3), 5);
        assert_eq!(cycle_reconnect_attempts(5), ells_core::reconnect::MAX_ATTEMPTS_CAP);
        assert_eq!(cycle_reconnect_attempts(ells_core::reconnect::MAX_ATTEMPTS_CAP), 0);
        // 手改 ini 改成档位外的值：Enter 回到"1 次"而不是停在野值上
        assert_eq!(cycle_reconnect_attempts(4), 1);
    }

    #[test]
    fn arrows_step_within_the_ladder() {
        assert_eq!(step_reconnect_attempts(0, false), 0, "已经是最小还往左就停住");
        assert_eq!(step_reconnect_attempts(0, true), 1);
        assert_eq!(step_reconnect_attempts(3, true), 5);
        assert_eq!(
            step_reconnect_attempts(ells_core::reconnect::MAX_ATTEMPTS_CAP, true),
            ells_core::reconnect::MAX_ATTEMPTS_CAP,
            "无限再往右也不许绕回 0（那是关掉）"
        );
        assert_eq!(step_reconnect_attempts(4, false), 3);
    }

    #[test]
    fn key_refusal_is_not_a_network_problem() {
        // 用户在密钥确认弹窗里按"拒绝"后，russh 送回的就是这两句
        assert!(key_trouble("建立连接失败: Unknown server key"));
        assert!(key_trouble("The server key changed at line 12"));
        assert!(key_trouble("主机密钥与记录不一致"));
        assert!(!key_trouble("Connection refused (os error 10061)"));
    }
}

#[cfg(test)]
mod list_tests {
    use super::*;

    fn host(alias: &str, group: Option<&str>) -> Host {
        Host {
            alias: alias.into(),
            hostname: "10.0.0.1".into(),
            port: 22,
            user: "root".into(),
            group: group.map(str::to_string),
            ..Default::default()
        }
    }

    fn rows(hosts: &[Host], query: &str, sort: ListSort, folded: &[&str]) -> Vec<ListRow> {
        let folded: Vec<String> = folded.iter().map(|s| s.to_string()).collect();
        let order = order_hosts(hosts, query, sort);
        build_list_rows(hosts, &order, query.trim().is_empty() && sort.groups_ordered(), &folded).0
    }

    fn aliases<'h>(order: &[usize], hosts: &'h [Host]) -> Vec<&'h str> {
        order.iter().map(|&i| hosts[i].alias.as_str()).collect()
    }

    /// 段头每组只出现一次，计数写的是这一段里的主机数。段与段之间按组名排，
    /// 所以拉丁名（redis）在汉字名（生产）前面——这是 `group_rank` 的既有行为。
    #[test]
    fn one_header_per_group_with_counts() {
        let hosts = vec![
            host("web", Some("生产")),
            host("db", Some("生产")),
            host("cache", Some("redis")),
        ];
        let got = rows(&hosts, "", ListSort::Grouped, &[]);
        assert_eq!(
            got,
            vec![
                ListRow::Header { group: "redis".into(), count: 1, folded: false },
                ListRow::Host(2),
                ListRow::Header { group: "生产".into(), count: 2, folded: false },
                ListRow::Host(1),
                ListRow::Host(0),
            ]
        );
    }

    /// 折叠只藏主机行：段头留着，计数还是全段的数，展开前后数字不该变。
    #[test]
    fn folding_keeps_the_header_and_its_count() {
        let hosts = vec![
            host("web", Some("生产")),
            host("db", Some("生产")),
            host("cache", Some("redis")),
        ];
        let got = rows(&hosts, "", ListSort::Grouped, &["生产"]);
        assert_eq!(
            got,
            vec![
                ListRow::Header { group: "redis".into(), count: 1, folded: false },
                ListRow::Host(2),
                ListRow::Header { group: "生产".into(), count: 2, folded: true },
            ]
        );
        let (_, visible) = {
            let order = order_hosts(&hosts, "", ListSort::Grouped);
            build_list_rows(&hosts, &order, true, &["生产".to_string()])
        };
        assert_eq!(aliases(&visible, &hosts), ["cache"], "折叠段的主机不能再占光标位");
    }

    /// 没分组的恒在最后一列，段头写「未分组」，和排序键同一套说法。
    #[test]
    fn ungrouped_hosts_share_the_tail_section() {
        let hosts = vec![host("b", None), host("a", Some("  ")), host("c", Some("生产"))];
        let got = rows(&hosts, "", ListSort::Grouped, &[]);
        let headers: Vec<&str> = got
            .iter()
            .filter_map(|r| match r {
                ListRow::Header { group, .. } => Some(group.as_str()),
                ListRow::Host(_) => None,
            })
            .collect();
        assert_eq!(headers, ["生产", "未分组"]);
    }

    /// 不分组排的两种模式没有段头可折：主机按那个键序一条不落铺平。
    #[test]
    fn non_group_sorts_have_no_headers_and_ignore_folds() {
        let hosts = vec![host("web", Some("生产")), host("db", Some("生产")), host("a", None)];
        for sort in [ListSort::Recent, ListSort::Alias] {
            let got = rows(&hosts, "", sort, &["生产"]);
            assert!(
                got.iter().all(|r| matches!(r, ListRow::Host(_))),
                "{sort:?} 不该画段头"
            );
            assert_eq!(got.len(), 3, "{sort:?} 折叠不该生效");
        }
        assert_eq!(
            aliases(&order_hosts(&hosts, "", ListSort::Alias), &hosts),
            ["a", "db", "web"]
        );
    }

    /// 最近使用：连过的在前，从没连过（0）的落到最后。
    #[test]
    fn recent_sort_keeps_never_connected_last() {
        let mut a = host("a", None);
        let b = host("b", None);
        let mut c = host("c", None);
        a.last_connected = 100;
        c.last_connected = 900;
        let hosts = vec![a, b, c];
        assert_eq!(
            aliases(&order_hosts(&hosts, "", ListSort::Recent), &hosts),
            ["c", "a", "b"]
        );
    }

    /// 过滤时按分数拍平：分组、收藏、最近使用统统让位给命中顺序，而且折叠着的
    /// 分组不能把命中主机藏起来——用户找的就是它。
    #[test]
    fn filter_flattens_to_score_order_without_headers() {
        let mut fav = host("web-prod", Some("生产"));
        fav.favorite = true;
        let hosts = vec![fav, host("web", Some("redis")), host("note", None)];
        let got = rows(&hosts, "web", ListSort::Grouped, &["生产"]);
        assert!(got.iter().all(|r| matches!(r, ListRow::Host(_))));
        let order: Vec<usize> = got
            .iter()
            .filter_map(|r| match r {
                ListRow::Host(i) => Some(*i),
                ListRow::Header { .. } => None,
            })
            .collect();
        let mut hit = aliases(&order, &hosts);
        hit.sort();
        assert_eq!(hit, ["web", "web-prod"], "折叠段里的命中也得留在列表上");
    }

    #[test]
    fn sort_cycle_returns_to_the_start() {
        let mut sort = ListSort::Grouped;
        for _ in 0..4 {
            sort = sort.next();
        }
        assert_eq!(sort, ListSort::Grouped);
        assert_eq!(ListSort::parse("recent"), Some(ListSort::Recent));
        assert_eq!(ListSort::parse("写错了"), None);
    }

    #[test]
    fn foldable_groups_lists_each_section_once() {
        let hosts = vec![
            host("web", Some("生产")),
            host("db", Some("生产")),
            host("x", None),
        ];
        assert_eq!(foldable_groups(&hosts), ["生产", "未分组"]);
    }
}

#[cfg(test)]
mod required_tests {
    use super::*;

    fn set(form: &mut FormState, role: FieldRole, value: &str) {
        if let Some(field) = form.fields.iter_mut().find(|f| f.role == role) {
            field.value = value.to_string();
        }
    }

    /// 报错只点缺的那一项的名：已经填对的不该再被用户重看一遍。
    #[test]
    fn missing_field_error_names_only_what_is_empty() {
        let mut form = FormState::new(None);
        set(&mut form, FieldRole::Alias, "web");
        set(&mut form, FieldRole::Hostname, "10.0.0.1");
        set(&mut form, FieldRole::Auth, "agent");
        assert_eq!(form.build_host(None).unwrap_err(), "用户是必填项");
        set(&mut form, FieldRole::User, "root");
        assert!(form.build_host(None).is_ok());
        // 三项全空才是过去那句笼统的"别名、主机、用户是必填项"
        let blank = FormState::new(None);
        assert_eq!(blank.build_host(None).unwrap_err(), "别名、主机、用户是必填项");
    }

    /// ＊ 标的必须 exactly 是 build_host 会拦下来的那三项，多个少个都是骗人。
    #[test]
    fn stars_mark_exactly_the_checked_fields() {
        for role in [FieldRole::Alias, FieldRole::Hostname, FieldRole::User] {
            assert!(role.required(), "{role:?} 必须带 ＊");
        }
        for role in [
            FieldRole::Port,
            FieldRole::Auth,
            FieldRole::Password,
            FieldRole::KeyPath,
            FieldRole::KeyPass,
            FieldRole::Jump,
            FieldRole::Group,
            FieldRole::Tags,
            FieldRole::Forwards,
        ] {
            assert!(!role.required(), "{role:?} 不该带 ＊");
        }
    }

    /// 「生产」和「生产 」是同一个分组：提示里必须只出现一次、台数合在一起算，
    /// 否则用户照着提示敲，列表页还是裂成两个段头。
    #[test]
    fn group_counts_merges_the_whitespace_variants() {
        let mut a = Host { alias: "a".into(), ..Default::default() };
        let mut b = Host { alias: "b".into(), ..Default::default() };
        a.group = Some("生产".into());
        b.group = Some("生产 ".into());
        let c = Host { alias: "c".into(), ..Default::default() };
        assert_eq!(
            group_counts(&[a, b, c]),
            vec![("生产".to_string(), 2), ("未分组".to_string(), 1)]
        );
    }
}

#[cfg(test)]
mod rules_tests {
    use super::*;

    fn row(listen: &str, dest_host: &str, dest_port: &str) -> RuleRow {
        RuleRow {
            kind: "L".into(),
            bind: String::new(),
            listen: listen.into(),
            dest_host: dest_host.into(),
            dest_port: dest_port.into(),
        }
    }

    fn host_with(alias: &str, port: u16) -> Host {
        Host {
            alias: alias.into(),
            forwards: vec![Forward::Local {
                bind: None,
                listen_port: port,
                dest_host: "10.0.0.9".into(),
                dest_port: 22,
            }],
            ..Default::default()
        }
    }

    /// 表格存回规则必须一条不差：静态口、自动口、`-D`、`-R` 都要原样回去。
    /// 自动口在表格里是"本地端口空着"，存回去是 `listen_port: 0`——留空=系统分配就靠这一条撑着。
    #[test]
    fn table_round_trips_every_kind() {
        let forwards = vec![
            Forward::Local { bind: None, listen_port: 8080, dest_host: "127.0.0.1".into(), dest_port: 5432 },
            Forward::Local { bind: None, listen_port: 0, dest_host: "db".into(), dest_port: 5432 },
            Forward::Dynamic { bind: Some("*".into()), listen_port: 1080 },
            Forward::Dynamic { bind: None, listen_port: 0 },
            Forward::Remote { bind: None, listen_port: 9090, dest_host: "internal".into(), dest_port: 22 },
        ];
        let rows = rules_from_forwards(&forwards);
        assert_eq!(rows[1].listen, "", "自动口不能显示成 0");
        assert_eq!(rows[3].kind, "D");
        assert_eq!(rows[4].kind, "R", "-R 存得进来就该看得改，别抹掉");
        assert_eq!(rows[2].bind, "*");
        let (back, bad) = rules_to_forwards(&rows);
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(back, forwards);
    }

    /// 全空的行直接丢掉，填了一半的必须拦住；报的行号是表里的位置，
    /// 前面有空白行也不能错位——用户是对着屏幕第几行改的。
    #[test]
    fn blank_rows_vanish_but_half_filled_ones_report_their_own_line_number() {
        let rows = vec![
            RuleRow::empty(),
            row("8080", "db", ""),
            RuleRow { kind: "x".into(), ..RuleRow::empty() },
            row("http", "db", "22"),
        ];
        let (ok, bad) = rules_to_forwards(&rows);
        assert!(ok.is_empty());
        assert_eq!(
            bad,
            vec![
                "第 2 行：目标端口不能空着",
                "第 3 行：类型只能是 L、D 或 R",
                "第 4 行：本地端口要么留空（自动分配），要么写成数字",
            ]
        );
    }

    /// 只按了 `Ctrl-N`、一个字没敲的那一行不算"填了一半"：默认的类型 `L` 不是用户写的。
    /// 这条挂了就等于告诉用户"你新增了一行错误"。
    #[test]
    fn a_fresh_row_alone_saves_as_nothing_at_all() {
        let (ok, bad) = rules_to_forwards(&[RuleRow::empty()]);
        assert!(ok.is_empty());
        assert!(bad.is_empty(), "{bad:?}");
    }

    /// 只点缺的那一项的名：主机填了、端口空着，就不该再提"目标主机"。与 build_host 同一口径。
    #[test]
    fn missing_destination_names_only_what_is_empty() {
        let (_, bad) = rules_to_forwards(&[RuleRow { dest_host: "db".into(), ..RuleRow::empty() }]);
        assert_eq!(bad, vec!["第 1 行：目标端口不能空着"]);
        let (_, bad) = rules_to_forwards(&[RuleRow { bind: "*".into(), ..RuleRow::empty() }]);
        assert_eq!(bad, vec!["第 1 行：目标主机、目标端口不能空着"]);
    }

    /// `-D` 没有目标，光一个本地口就是完整规则；它不该被当成"填了一半"。
    #[test]
    fn dynamic_needs_no_destination() {
        let rows = vec![RuleRow { kind: "D".into(), listen: "1080".into(), ..RuleRow::empty() }];
        let (ok, bad) = rules_to_forwards(&rows);
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(ok, vec![Forward::Dynamic { bind: None, listen_port: 1080 }]);
    }

    /// 撞口提示要把对手一次点全：本表另一行说"本表第 N 行"，别的主机说别名，顺序是先表内后表外。
    #[test]
    fn clash_text_lists_every_rival_in_one_breath() {
        let rows = vec![row("8080", "a", "1"), row("8080", "b", "2")];
        let hosts = vec![host_with("db-prod", 8080)];
        assert_eq!(
            row_clash_text(&rows, &hosts, "web", 0),
            "⚠ 与 本表第 2 行、db-prod 同用 8080 — 留空本地口即可自动分配"
        );
    }

    /// 正在编辑的这台主机自己在保险库里也有一条 8080，提示不能把它算成对手——
    /// 自己跟自己"撞"是保存前就要停掉的旧规则，说成撞口会让人去改另一台。
    #[test]
    fn clash_text_never_names_the_host_being_edited() {
        let rows = vec![row("8080", "db", "5432")];
        let hosts = vec![host_with("web", 8080), host_with("db-prod", 8080)];
        assert_eq!(
            row_clash_text(&rows, &hosts, "web", 0),
            "⚠ 与 db-prod 同用 8080 — 留空本地口即可自动分配"
        );
    }

    /// 留空和显式写 0 都不该被标 ⚠：用户要的正是"谁也不撞"，标上反而像出错了。
    #[test]
    fn an_auto_row_never_clashes_with_anything() {
        let rows = vec![row("", "a", "1"), row("0", "b", "2")];
        let hosts = vec![host_with("db-prod", 8080)];
        for (i, r) in rows.iter().enumerate() {
            assert!(r.port().is_none(), "第 {} 行是自动口，不该有固定端口", i + 1);
            assert!(row_clash_text(&rows, &hosts, "web", i).is_empty());
        }
    }

    /// 没写口的行、越界的下标都要安静地返回空串：表格里每行都调它，panic 会把整个 TUI 打回 shell。
    #[test]
    fn clash_text_is_quiet_about_rows_it_cannot_look_at() {
        let rows = vec![row("8080", "a", "1")];
        let hosts = vec![host_with("db-prod", 8080)];
        assert_eq!(row_clash_text(&rows, &hosts, "web", 7), "");
        assert_eq!(row_clash_text(&[], &hosts, "web", 0), "");
        // 只有自己、没有对手的一行也是干净的
        assert_eq!(row_clash_text(&rows, &[], "web", 0), "");
    }
}

#[cfg(test)]
mod metrics_tests {
    use super::*;
    use ells_core::Probe;

    /// 一轮完整数据：CPU 累计 1000 jiffies（闲 850）、内存用掉 75%、最满的盘 88%。
    const ROUND1: &str = concat!(
        "ellsm1\n",
        "cpu  100 0 50 800 50 0 0 0\n",
        "MemTotal: 1000 kB\n",
        "MemAvailable: 250 kB\n",
        "/dev/sda1 1000 880 120 88% /data\n",
    );
    /// 下一轮：总共走了 100 个 jiffies，其中 60 个在闲 ⇒ 忙碌 40%。
    const ROUND2: &str = concat!(
        "ellsm1\n",
        "cpu  130 0 60 840 70 0 0 0\n",
        "MemTotal: 1000 kB\n",
        "MemAvailable: 250 kB\n",
        "/dev/sda1 1000 880 120 88% /data\n",
    );

    /// 磁盘到点的那一轮：df 给了一个新数，界面上那一格要跟着换。
    const ROUND_DISK_91: &str = concat!(
        "ellsm1\n",
        "cpu  130 0 60 840 70 0 0 0\n",
        "MemTotal: 1000 kB\n",
        "MemAvailable: 250 kB\n",
        "/dev/sda1 1000 910 90 91% /data\n",
    );
    /// 没到点的那一轮：`gather` 不去读 mounts、也不跑 df，所以只有 /proc 那三样。
    const ROUND_NO_DISK: &str = concat!(
        "ellsm1\n",
        "cpu  100 0 50 800 50 0 0 0\n",
        "MemTotal: 1000 kB\n",
        "MemAvailable: 250 kB\n",
        "0.42 0.31 0.19 1/234 5678\n",
    );

    fn probe(text: &str) -> ells_core::Probe {
        Probe::parse(text)
    }

    /// CPU 那一格必须是**两轮之差**：单看一轮只有开机以来的平均值，那根条几乎不动，
    /// 看着像坏了。所以首轮宁可可空也不报那个假数。
    #[test]
    fn the_first_round_has_no_cpu_yet_but_the_rest_is_real() {
        let mut m = Metrics::default();
        assert!(m.adopt(&probe(ROUND1)));
        assert_eq!(m.cpu, None, "没有可差的基线，就该空着");
        assert_eq!(m.mem, Some(75));
        let disk = m.disk.clone().expect("有 df 就该有磁盘");
        assert_eq!((disk.mount.as_str(), disk.percent), ("/data", 88));
        assert_eq!((disk.used_kb, disk.total_kb), (880, 1000));
    }

    #[test]
    fn the_second_round_computes_cpu_from_the_diff() {
        let mut m = Metrics::default();
        m.adopt(&probe(ROUND1));
        m.adopt(&probe(ROUND2));
        assert_eq!(m.cpu, Some(40));
    }

    /// 没有 /proc 的机器（FreeBSD、某些容器）：CPU 这一格永远空着，但磁盘是真的 ——
    /// 有真数据就不算失败轮，不能被计进"采不到"。
    #[test]
    fn a_machine_without_proc_keeps_the_disk_and_is_not_a_miss() {
        let mut m = Metrics::default();
        for _ in 0..METRICS_GIVE_UP {
            assert!(m.adopt(&probe("ellsm1\n/dev/sda1 1000 500 500 50% /\n")));
        }
        assert!(!m.unsupported, "有数可画就不该停止轮询");
        assert_eq!(m.cpu, None);
        assert_eq!(m.disk.map(|d| d.percent), Some(50));
    }

    /// 整台机器什么都不给：连续这么多轮就认输，停止占那条连接的通道。
    #[test]
    fn three_empty_rounds_stop_polling() {
        let mut m = Metrics::default();
        assert!(m.adopt(&probe("")));
        assert!(m.adopt(&probe("")));
        assert!(!m.unsupported, "第二次还不算");
        assert!(!m.adopt(&probe("")), "第三次该停了");
        assert!(m.unsupported);
        assert!(!m.is_polling(), "判定采不到之后不能还在轮询");
    }

    /// 中间成功一次就把"这台采不到"的计数清掉：偶发一次 df 卡住不该让用户永远看不到指标。
    #[test]
    fn one_good_round_clears_the_empty_counter() {
        let mut m = Metrics::default();
        assert!(m.adopt(&probe("")));
        assert!(m.adopt(&probe("")));
        assert!(m.adopt(&probe(ROUND1)), "成功一轮就把计数清掉");
        assert!(m.adopt(&probe("")));
        assert!(m.adopt(&probe("")));
        assert!(!m.adopt(&probe("")), "重新数满三轮才判定采不到");
        assert!(m.unsupported);
    }

    /// 通道级失败只退避、不判死：网络抖一下、服务器瞬间过载、跳板掐一下，都不该让
    /// 底部那一行永远消失（判死只留给"回包是空的"那种真的没东西可采的机器）。
    #[test]
    fn transport_failures_back_off_instead_of_giving_up() {
        let mut m = Metrics::default();
        assert_eq!(m.interval(), METRICS_EVERY, "没失败过就是用户定的节奏");
        for (round, secs) in [(1, 10u64), (2, 30), (3, 60), (4, 60), (5, 60)] {
            assert!(m.missed(), "第 {round} 次通道失败之后还要接着采");
            assert!(!m.unsupported, "通道失败不该被判成这台采不到");
            assert_eq!(m.interval(), std::time::Duration::from_secs(secs));
        }
    }

    /// 退到顶端的那一刻说一句，之后就闭嘴：状态行反复刷比数字慢一分钟更扰人。
    #[test]
    fn the_capped_moment_is_reported_once() {
        let mut m = Metrics::default();
        m.missed();
        m.missed();
        assert!(!m.just_capped());
        m.missed();
        assert!(m.just_capped(), "第三次失败正好退到 60 秒，该说一句");
        m.missed();
        assert!(!m.just_capped(), "第四次不该再重复那句话");
    }

    /// 恢复是自动的：任意一轮拿到数，间隔就落回 5 秒，不用用户去拨开关。
    #[test]
    fn one_good_round_ends_the_backoff() {
        let mut m = Metrics::default();
        for _ in 0..3 {
            m.missed();
        }
        assert_eq!(m.interval(), std::time::Duration::from_secs(60));
        assert!(m.adopt(&probe(ROUND1)));
        assert_eq!(m.interval(), METRICS_EVERY, "拿到数就说明通道也是通的");
        assert_eq!(
            m.disk.as_ref().map(|d| d.percent),
            Some(88),
            "退避期间那一格保留的是上次的数"
        );
    }

    /// 背景标签降到一分钟一轮：八个标签一起开着时，每台服务器看到的是"一分钟一次"，
    /// 而不是"每五秒八次"。用户正看着的那一格一秒都不让 —— 那根条就是要跟着动。
    #[test]
    fn background_tabs_downshift_and_the_focused_one_does_not() {
        assert_eq!(metrics_every(true, METRICS_EVERY), METRICS_EVERY);
        assert_eq!(metrics_every(false, METRICS_EVERY), METRICS_IDLE_EVERY);
        // 退避过的间隔照样降频，但绝不比一分钟更快
        let mut m = Metrics::default();
        m.missed();
        m.missed();
        assert_eq!(metrics_every(true, m.interval()), std::time::Duration::from_secs(30));
        assert_eq!(metrics_every(false, m.interval()), METRICS_IDLE_EVERY);
    }

    /// 用户刚进来时最想要的就是 CPU：第一轮只拿到基线，所以那一轮之后**两秒**就接力，
    /// 而不是让他等满三个 5 秒。背景那几格不抢（没人看），差完数之后也回到正常节奏。
    #[test]
    fn the_baseline_round_hands_the_cpu_cell_a_short_relay() {
        let mut m = Metrics::default();
        assert!(m.adopt(&probe(ROUND1)));
        assert_eq!(m.cpu, None);
        assert!(m.cpu_pending, "这一轮只有基线，下一轮该抢");
        assert_eq!(
            metrics_next(m.cpu_pending, true, m.interval()),
            METRICS_CPU_WINDOW,
            "正看着的这一格两秒后就出 CPU"
        );
        assert_eq!(
            metrics_next(m.cpu_pending, false, m.interval()),
            METRICS_IDLE_EVERY,
            "没人看的那一格不该为了一格 CPU 抢起来"
        );
        assert!(m.adopt(&probe(ROUND2)));
        assert_eq!(m.cpu, Some(40));
        assert!(!m.cpu_pending, "数已经差出来了，节奏交回分频");
        assert_eq!(metrics_next(m.cpu_pending, true, m.interval()), METRICS_EVERY);
    }

    /// 没有 `/proc/stat` 的机器（FreeBSD、被加固过的容器）永远不会 `cpu_pending`，
    /// 所以不会两秒一次猛采还差不出数 —— 空转的代价由那条短窗口规则自己付掉。
    #[test]
    fn a_machine_without_a_cpu_never_spins_on_the_short_window() {
        let mut m = Metrics::default();
        let no_cpu = "ellsm1\nMemTotal: 1000 kB\nMemAvailable: 250 kB\n";
        assert!(m.adopt(&probe(no_cpu)));
        assert!(!m.cpu_pending, "没有基线可言，也就没有接力");
        assert!(m.adopt(&probe(no_cpu)));
        assert_eq!(metrics_next(m.cpu_pending, true, m.interval()), METRICS_EVERY);
    }

    /// 切回来时重接的那一条要用一个新代号：背景期间在途的那一轮（连同它 60 秒的旧定时器）
    /// 都要作废，否则两轮回包抢同一格，晚到的那份会把数画旧。
    #[test]
    fn reviving_a_tab_invalidates_the_round_it_was_waiting_on() {
        let mut m = Metrics::default();
        let waiting = m.arm();
        m.cancel();
        let revived = m.arm();
        assert_ne!(waiting, revived, "旧定时器和旧回包都对不上新代号了");
        assert!(m.is_polling(), "重接之后这条链又有人在等了");
    }

    /// 代号必须跨会话单调往上走：重连后 seq 从 0 重来的话，上一台机器那只还在飞的
    /// 回包就会对上新会话的代号，把已经消失的那一行画回去。
    #[test]
    fn reset_keeps_the_generation_running_forward() {
        let mut m = Metrics::default();
        let seq = m.arm();
        m.adopt(&probe(ROUND1));
        m.reset();
        assert!(!m.has_data(), "上一台机器的数要清干净");
        assert!(!m.is_polling());
        let next = m.arm();
        assert!(next > seq, "新会话的代号要比旧的大");
    }

    /// cancel 之后在途的那一包必须作废：会话都没了，不该再把数写进已经不存在的行。
    #[test]
    fn cancel_invalidates_the_round_in_flight() {
        let mut m = Metrics::default();
        let seq = m.arm();
        m.cancel();
        assert_ne!(m.seq, seq, "在途回包带的代号对不上了");
        assert!(!m.is_polling());
    }

    /// 分频现在是两件事：每一轮都想要磁盘那个数（跟单：只问最满那块一次 statvfs），
    /// 但**普查**（重读 mounts + 逐块 statvfs，兜底一条 df）仍然 12 轮才一次。
    /// 普查是最贵的一条腿，而挂载点集合一分钟里几乎不动。
    #[test]
    fn only_every_twelfth_round_does_a_disk_survey() {
        let mut m = Metrics::default();
        let mut surveyed = Vec::new();
        for round in 1..=(METRICS_DISK_EVERY_ROUNDS * 2 + 1) {
            if m.disk_survey_due() {
                surveyed.push(round);
            }
            assert!(m.adopt(&probe(ROUND_NO_DISK)));
        }
        assert_eq!(surveyed, vec![1, 13, 25], "连上的第一轮普查一次，之后每 12 轮一次");
    }

    /// 跟单轮也要换数：这才是方案 B 的意义 —— 那一格 5 秒一档，只是每档只问一块盘。
    #[test]
    fn a_followed_disk_refreshes_the_cell_without_a_survey() {
        let mut m = Metrics::default();
        m.adopt(&probe(ROUND1));
        assert_eq!(m.disk.as_ref().map(|d| d.percent), Some(88));
        assert!(!m.disk_survey_due(), "刚普查过，下一轮只跟单");
        // 这一轮带回的是同一块盘的新数（有人往里写东西了）。
        m.adopt(&probe(ROUND_DISK_91));
        assert_eq!(
            m.disk.as_ref().map(|d| (d.mount.as_str(), d.percent)),
            Some(("/data", 91)),
            "跟单的轮次也得把数换上去，否则 5 秒一档是假的"
        );
    }

    /// 没数的那一轮要**留着上次的数**：那一格空掉比旧一秒看起来都像坏了。
    #[test]
    fn the_disk_cell_keeps_its_number_between_queries() {
        let mut m = Metrics::default();
        m.adopt(&probe(ROUND1));
        assert_eq!(m.disk.as_ref().map(|d| d.percent), Some(88));
        for _ in 0..(METRICS_DISK_EVERY_ROUNDS - 1) {
            assert!(m.adopt(&probe(ROUND_NO_DISK)));
            assert_eq!(m.disk.as_ref().map(|d| d.percent), Some(88));
        }
        assert!(m.disk_survey_due(), "11 轮之后正好到点");
        m.adopt(&probe(ROUND_DISK_91));
        assert_eq!(
            m.disk.as_ref().map(|d| (d.mount.as_str(), d.percent)),
            Some(("/data", 91)),
            "到点的那一轮要换新数"
        );
    }

    /// 普查轮到点就问完了，**问到没问到都往后数 12 轮**。
    ///
    /// 这条守的是方案 B 最坏的一种退化：一台没有 `statvfs@openssh.com` 的服务器上，
    /// 如果"这一轮没数"就把倒计时留在 0，那下一轮又是一次普查 + 一条 `df` —— 60 秒的
    /// 节奏当场变成 5 秒，每 5 秒 fork 一个 shell，正是这套 SFTP 主路径要避免的东西。
    #[test]
    fn a_survey_with_no_answer_still_counts_down() {
        let mut m = Metrics::default();
        assert!(m.disk_survey_due());
        assert!(m.adopt(&probe(ROUND_NO_DISK)), "只有 /proc 的机器照样算拿到东西");
        assert!(!m.disk_survey_due(), "没问到也要把普查点推回 60 秒之后");
        assert!(m.adopt(&probe(ROUND_NO_DISK)));
    }

    /// 通道级失败的那一轮根本没走到 adopt，倒计时不能被一次超时偷偷推后。
    #[test]
    fn a_failed_round_leaves_the_disk_countdown_alone() {
        let mut m = Metrics::default();
        assert!(m.disk_survey_due());
        assert!(m.missed());
        assert!(m.disk_survey_due(), "通道失败的那一轮没问到，倒计时不该动");
        m.adopt(&probe(ROUND1));
        assert!(!m.disk_survey_due(), "问到之后就按节奏数着");
        assert!(m.missed());
        assert_eq!(m.disk.as_ref().map(|d| d.percent), Some(88), "旧数还在");
    }

    /// 反过来也得守住的：缓存里那块盘不能替一台已经什么都采不到的机器说话，
    /// 否则"判定采不到"永远轮不到，那一行赖在底部一直不还给终端。
    #[test]
    fn a_cached_disk_does_not_keep_a_dead_machine_polling() {
        let mut m = Metrics::default();
        m.adopt(&probe(ROUND1));
        assert!(m.adopt(&probe("")));
        assert!(m.adopt(&probe("")));
        assert!(!m.adopt(&probe("")), "三轮空手就该停，不管磁盘那一格缓存着谁");
        assert!(m.unsupported);
        assert!(m.has_data(), "轮询停了，但那一轮之前画过的数不该被抹掉");
    }

    #[test]
    fn a_new_session_asks_for_the_disk_again() {
        let mut m = Metrics::default();
        m.adopt(&probe(ROUND1));
        assert!(!m.disk_survey_due());
        m.reset();
        assert!(m.disk_survey_due(), "换了一条连接，第一轮就要重做普查");
        assert_eq!(m.disk, None, "上一台机器的盘不能留给新会话");
    }

    /// 负载是第四个数，走的是快的那条腿：它和 CPU% 各说一件事（占用 vs 排队）。
    #[test]
    fn loadavg_fills_its_own_field_every_round() {
        let mut m = Metrics::default();
        m.adopt(&probe(ROUND_NO_DISK));
        assert_eq!(
            m.load.map(|l| l.display()),
            Some("0.42/0.31/0.19".to_string())
        );
        m.adopt(&probe(ROUND1));
        assert_eq!(m.load, None, "这台机器不给 loadavg 就该空着，不是 0");
    }
}

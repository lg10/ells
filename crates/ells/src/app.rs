use anyhow::Result;
use crossterm::event::{
    EnableBracketedPaste, EnableMouseCapture, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::enable_raw_mode;
use crossterm::execute;
use crossterm::terminal::EnterAlternateScreen;
use ells_core::host::{Auth, Host};
use ells_core::ssh::RemoteSession;
use ells_core::vault::{self, VaultKey};
use ells_core::{HostKeyPolicy, HostKeyPrompt, KeyTrust, Vault};
use ells_transfer::{self, Cancel, FileEntry, Progress};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use russh_sftp::client::SftpSession;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use crate::events::{self, AppEvent};
use crate::keybinds::{Action, Chord};
use crate::session::{SessionAction, SessionState, TermMode};
use crate::settings::Settings;
use crate::term;
use crate::ui;

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
}

pub struct Field {
    pub role: FieldRole,
    pub label: &'static str,
    pub value: String,
    pub kind: FieldKind,
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
    /// 设置弹窗中当前聚焦的选项行（0=高亮 1=保活 2=主密码开关 3=修改主密码 4=快捷键 5=保存 6=取消）。
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
    /// 主机密钥策略：连接任务用它发问，UI 用它的通道回答。
    hostkey: HostKeyPolicy,
    /// 传输详情弹窗是否打开（顶部聚合进度条触发）。
    pub transfer_popup: bool,
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
    /// 上一次写入终端标签名的文本（变化才重发 OSC 0）。
    term_title: Option<String>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    event_rx: mpsc::UnboundedReceiver<AppEvent>,
}

pub async fn run(alias: Option<String>, dev: bool, yes: bool) -> Result<()> {
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

    let mut app = App::startup(tx, rx, alias, dev, HostKeyPolicy::new(hkey_tx, yes));
    let result = app.loop_run(&mut terminal).await;

    term::restore_terminal();
    terminal.show_cursor()?;
    result
}

impl App {
    fn startup(
        tx: mpsc::UnboundedSender<AppEvent>,
        rx: mpsc::UnboundedReceiver<AppEvent>,
        alias: Option<String>,
        dev: bool,
        hostkey: HostKeyPolicy,
    ) -> Self {
        let settings = Settings::load();
        // 保险库、known_hosts、自动解锁凭据都在 ~/.ells：先把它收紧到仅当前用户可读
        ells_core::harden_config_dir();
        ells_core::ssh::set_keepalive_interval(settings.keepalive_secs);
        let base = Self {
            screen: ScreenKind::List,
            unlock: UnlockState {
                input: String::new(),
                error: None,
                stage: UnlockStage::Open,
                pending_master: None,
                busy: false,
            },
            list: ListState { selected: 0 },
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
            hostkey,
            transfer_popup: false,
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
            term_title: None,
            event_tx: tx,
            event_rx: rx,
        };
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
            if !passthrough {
                terminal.draw(|f| ui::draw(f, self))?;
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
                AppEvent::RemoteClosed { .. } => {
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
                        // 已有弹窗时不再叠加：一次只弹一个，且会孤儿掉前一个的应答通道
                        if self.choice.is_none() {
                            self.offer_reconnect(host);
                        }
                    }
                }
                AppEvent::HostKey(prompt) => self.ask_host_key(prompt),
                AppEvent::Connected { res, .. } => self.on_connected(res),
                AppEvent::Reconnect { host, .. } => self.start_connect(host, Some(self.work)),
                AppEvent::Conflict { prompt, .. } => self.ask_conflict(prompt),
                AppEvent::ImportHosts(hosts) => self.import_hosts(hosts),
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
            // 帮助页是全屏信息层：只认退出键，其余一律不落到下层页面
            match key.code {
                KeyCode::Esc
                | KeyCode::Enter
                | KeyCode::Char('?')
                | KeyCode::Char('q')
                | KeyCode::F(1) => self.help_open = false,
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
        if self.transfer_popup {
            // 详情弹窗是只读信息层：任意按键即关闭。
            self.transfer_popup = false;
            return;
        }
        if self.delete_confirm.is_some() {
            self.handle_delete_confirm_key(&key);
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
            | AppEvent::RemoteClosed { slot }
            | AppEvent::Connected { slot, .. }
            | AppEvent::Reconnect { slot, .. }
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
            let [hl_r, ka_r, mp_r, change_r, kb_r, save_r, cancel_r] =
                ui::settings_hit_rects(self.last_area);
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
            } else if hit(save_r, column, row) {
                self.save_settings();
            } else if hit(cancel_r, column, row) {
                self.cancel_settings();
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
                } else if hit(ui::session_emu_rect(self.last_area), column, row) {
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
        let area = ui::session_emu_rect(self.last_area);
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
                self.settings_focus = (self.settings_focus + 1).min(6);
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.apply_settings_focus(),
            KeyCode::Left => match self.settings_focus {
                0 => self.settings.highlight = !self.settings.highlight,
                1 => {
                    self.settings.keepalive_secs =
                        step_keepalive(self.settings.keepalive_secs, false);
                }
                2 => self.toggle_master_setting(),
                _ => {}
            },
            KeyCode::Right => match self.settings_focus {
                0 => self.settings.highlight = !self.settings.highlight,
                1 => {
                    self.settings.keepalive_secs =
                        step_keepalive(self.settings.keepalive_secs, true);
                }
                2 => self.toggle_master_setting(),
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
    /// 2 主密码保护开关、3 进入修改主密码输入、4 打开快捷键面板、5 保存、6 取消
    fn apply_settings_focus(&mut self) {
        match self.settings_focus {
            0 => self.settings.highlight = !self.settings.highlight,
            1 => {
                self.settings.keepalive_secs = cycle_keepalive(self.settings.keepalive_secs);
            }
            2 => self.toggle_master_setting(),
            3 => self.begin_master_change(),
            4 => self.open_keybinds(),
            5 => self.save_settings(),
            _ => self.cancel_settings(),
        }
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
        ells_core::ssh::set_keepalive_interval(self.settings.keepalive_secs);
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
        let label = if let Some(mut s) = self.slots[self.work].session.take() {
            s.close();
            Some(s.label.clone())
        } else {
            None
        };
        let slot = &mut self.slots[self.work];
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
            let _ = tx.send(AppEvent::SftpDone { slot, res });
            let _ = pump.await;
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
            let _ = tx.send(AppEvent::SftpDone { slot, res });
            let _ = pump.await;
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
        let len = self.vault.hosts.len();
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
                self.form = FormState::new(None);
                self.screen = ScreenKind::Form;
            }
            KeyCode::Char('e') => {
                if let Some(host) = self.vault.hosts.get(self.list.selected).cloned() {
                    self.form = FormState::new(Some(&host));
                    self.screen = ScreenKind::Form;
                }
            }
            KeyCode::Char('d') => {
                // 二级确认：先弹确认框，真正删除在 confirm_delete()
                if let Some(host) = self.vault.hosts.get(self.list.selected) {
                    self.delete_confirm = Some(host.alias.clone());
                    self.confirm_index = 0;
                }
            }
            KeyCode::Char('s') => self.open_settings(),
            KeyCode::Char('i') => self.import_ssh_config(),
            KeyCode::Char('?') | KeyCode::F(1) => self.help_open = true,
            KeyCode::Enter => {
                if let Some(host) = self.vault.hosts.get(self.list.selected).cloned() {
                    self.open_host(host);
                }
            }
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
            self.status = Some(format!("已删除 {alias}"));
            self.list.selected =
                self.list.selected.min(self.vault.hosts.len().saturating_sub(1));
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
            slot.view = ScreenKind::List;
            slot.status = Some(format!("正在连接 {}…", host.alias));
        }
        self.active = idx;
        self.work = idx;
        self.screen = ScreenKind::List;
        self.settings_open = false;
        self.transfer_popup = false;
        self.status = Some(format!("正在连接 {}…", host.alias));
        self.pending_connect = Some((host, idx));
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

    /// 连接意外结束：一键重连同一主机（用户按 Ctrl-] 主动断开不会走到这里）。
    fn offer_reconnect(&mut self, host: Host) {
        if let Some(idx) = self.vault.hosts.iter().position(|h| h.alias == host.alias) {
            self.list.selected = idx;
        }
        let tx = self.event_tx.clone();
        let slot = self.slot_id();
        self.choice = Some(Choice {
            title: "连接已断开".to_string(),
            lines: vec![
                format!("主机：{}", host.target()),
                "可能是网络中断、服务器重启或空闲超时。".to_string(),
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

    fn on_connected(&mut self, res: std::result::Result<RemoteSession, String>) {
        let event_tx = self.event_tx.clone();
        let idx = self.work;
        // 后台标签连上了不该抢界面：用户可能正在另一路上打字
        let foreground = idx == self.active;
        let mut connected = false;
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
                if let Some(rx) = session.take_output() {
                    events::spawn_remote_pump(rx, event_tx, id);
                }
                slot.sftp = session.sftp();
                let (cols, rows) = term_size();
                slot.session = Some(SessionState::new(label, session, rows, cols));
                slot.remote_cwd.clear();
                slot.sz_pending.clear();
                slot.rz_pending = false;
                slot.search = None;
                // 新连接从零开始：清掉上一个会话的传输记录
                slot.browser.transfers.clear();
                slot.cancel = Cancel::default();
                slot.status = None;
                slot.view = ScreenKind::Session;
                connected = true;
            }
            Err(err) => {
                let msg = format!("连接失败: {err}");
                let slot = &mut self.slots[idx];
                slot.connecting = false;
                slot.host = None;
                slot.status = Some(msg.clone());
                self.status = Some(msg);
            }
        }
        if foreground && connected {
            self.screen = ScreenKind::Session;
            self.status = None;
            self.transfer_popup = false;
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
                let _ = responder.send(matches!(idx, Some(1)));
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
        match self.form.build_host() {
            Ok(mut host) => {
                let alias = host.alias.clone();
                match self.backup_key(&mut host) {
                    Ok(copied) => {
                        if let Some(old) = self.form.editing_alias.take() {
                            self.vault.remove(&old);
                        }
                        self.list.selected = self
                            .vault
                            .hosts
                            .iter()
                            .position(|h| h.alias == alias)
                            .unwrap_or(0);
                        self.vault.upsert(host);
                        self.save_vault();
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
            ],
            focus: 0,
            footer: None,
            editing_alias: host.map(|h| h.alias.clone()),
            error: None,
            jump_picker: None,
        }
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

    fn build_host(&self) -> std::result::Result<Host, String> {
        let alias = self.value_of(FieldRole::Alias).trim().to_string();
        let hostname = self.value_of(FieldRole::Hostname).trim().to_string();
        let port: u16 = self
            .value_of(FieldRole::Port)
            .trim()
            .parse()
            .map_err(|_| "端口必须是 1-65535 的数字".to_string())?;
        let user = self.value_of(FieldRole::User).trim().to_string();
        if alias.is_empty() || hostname.is_empty() || user.is_empty() {
            return Err("别名、主机、用户是必填项".to_string());
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
            note: None,
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

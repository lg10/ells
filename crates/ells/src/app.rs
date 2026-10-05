use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use ells_core::host::{Auth, Host};
use ells_core::ssh::RemoteSession;
use ells_core::vault::{self, VaultKey};
use ells_core::Vault;
use ells_transfer::{self, FileEntry, Progress};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use russh_sftp::client::SftpSession;
use std::io::{stdout, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::events::{self, AppEvent};
use crate::session::{SessionAction, SessionState, TermMode};
use crate::settings::Settings;
use crate::ui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenKind {
    Unlock,
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

pub struct App {
    pub screen: ScreenKind,
    pub unlock: UnlockState,
    pub list: ListState,
    pub form: FormState,
    pub browser: BrowserState,
    pub session: Option<SessionState>,
    pub sftp: Option<Arc<SftpSession>>,
    /// Tracked remote shell working directory (for hijacked sz/rz transfers).
    remote_cwd: String,
    /// sz files waiting for remote_cwd to resolve.
    sz_pending: Vec<String>,
    /// rz waiting for remote_cwd before opening the native picker.
    rz_pending: bool,
    /// Destination directory for the next upload (browser path or remote cwd).
    upload_dest: Option<String>,
    /// Browser was opened by an argument-less `sz` to pick a download target.
    sz_pick_mode: bool,
    /// Single-file `sz` waiting for remote_cwd before opening Save As.
    sz_saveas_pending: bool,
    /// A native file dialog is currently on screen (only one at a time).
    dialog_open: bool,
    /// ZMODEM event that arrived while a dialog was open; replayed after close.
    pending_zmodem: Option<crate::zmodem::ZmodemEvent>,
    /// Generation counter for the ZMODEM swallow-window timer (sliding).
    zclear_seq: u64,
    pub settings: Settings,
    /// 全局设置弹窗是否打开（会话界面顶部「设置」按钮触发）。
    pub settings_open: bool,
    /// 设置弹窗中当前聚焦的选项行（0=高亮 1=保活 2=主密码开关 3=修改主密码 4=保存 5=取消）。
    pub settings_focus: usize,
    /// 本次运行解锁用过的明文主密码（改密/关闭保护时写入自动解锁凭据需要它）。
    pub master_secret: Option<String>,
    /// 自动解锁时暂存的凭据，解锁成功后转入 master_secret。
    auto_master: Option<String>,
    pub mp_stage: MpStage,
    pub mp_buf: String,
    pub mp_busy: bool,
    mp_first: String,
    /// 主机列表删除的二级确认（存待删别名）。
    pub delete_confirm: Option<String>,
    /// 删除确认弹窗聚焦按钮（0=保留 1=删除）。
    pub confirm_index: usize,
    /// 传输详情弹窗是否打开（顶部聚合进度条触发）。
    pub transfer_popup: bool,
    /// 最近一次绘制的终端区域，用于把鼠标坐标映射到顶部按钮。
    pub last_area: Rect,
    pub vault: Vault,
    pub vault_key: Option<VaultKey>,
    pub pending_connect: Option<Host>,
    pub direct_alias: Option<String>,
    pub pending_unlock_action: bool,
    pub status: Option<String>,
    pub done: bool,
    /// 上一次写入终端标签名的文本（变化才重发 OSC 0）。
    term_title: Option<String>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    event_rx: mpsc::UnboundedReceiver<AppEvent>,
}

pub async fn run(alias: Option<String>, dev: bool) -> Result<()> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let (tx, rx) = mpsc::unbounded_channel::<AppEvent>();
    events::spawn_input_stream(tx.clone());

    let mut app = App::startup(tx, rx, alias, dev);
    let result = app.loop_run(&mut terminal).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    // 空标题 = 让终端回落到默认标签名（Windows Terminal/iTerm2 均如此处理）
    set_term_title("");
    result
}

impl App {
    fn startup(
        tx: mpsc::UnboundedSender<AppEvent>,
        rx: mpsc::UnboundedReceiver<AppEvent>,
        alias: Option<String>,
        dev: bool,
    ) -> Self {
        let settings = Settings::load();
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
            browser: BrowserState {
                path: String::new(),
                entries: Vec::new(),
                selected: 0,
                scroll: 0,
                loading: false,
                error: None,
                transfers: Vec::new(),
            },
            session: None,
            sftp: None,
            remote_cwd: String::new(),
            sz_pending: Vec::new(),
            rz_pending: false,
            upload_dest: None,
            sz_pick_mode: false,
            sz_saveas_pending: false,
            dialog_open: false,
            pending_zmodem: None,
            zclear_seq: 0,
            settings,
            settings_open: false,
            settings_focus: 0,
            master_secret: None,
            auto_master: None,
            mp_stage: MpStage::Idle,
            mp_buf: String::new(),
            mp_first: String::new(),
            mp_busy: false,
            delete_confirm: None,
            confirm_index: 0,
            transfer_popup: false,
            last_area: Rect::ZERO,
            vault: Vault::default(),
            vault_key: None,
            pending_connect: None,
            direct_alias: alias,
            pending_unlock_action: false,
            status: None,
            done: false,
            term_title: None,
            event_tx: tx,
            event_rx: rx,
        };
        if dev {
            let mut app = base;
            app.vault = vault::load_dev_vault().unwrap_or_default();
            app.status = Some("开发模式：读取 ~/.ells/hosts.dev.toml".to_string());
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
                app.auto_master = Some(master.clone());
                app.unlock.busy = true;
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
            self.sync_term_title();
            let passthrough = self.screen == ScreenKind::Session
                && self.session.as_ref().map(|s| s.mode) == Some(TermMode::Passthrough);
            if !passthrough {
                terminal.draw(|f| ui::draw(f, self))?;
            }
            // Connect only AFTER the "正在连接…" frame is on screen.
            if let Some(host) = self.pending_connect.take() {
                self.connect(host).await;
                continue;
            }
            let Some(ev) = self.event_rx.recv().await else {
                break;
            };
            match ev {
                AppEvent::Key(key) => self.handle_key(key).await,
                AppEvent::Paste(text) => {
                    if let Some(s) = &mut self.session {
                        s.handle_paste(&text);
                    }
                }
                AppEvent::Resize(cols, rows) => {
                    if let Some(s) = &mut self.session {
                        s.handle_resize(cols, rows);
                    }
                }
                AppEvent::MousePress { column, row } => self.handle_mouse(column, row),
                AppEvent::MouseDrag { column, row } => self.handle_mouse_drag(column, row),
                AppEvent::MouseRelease { column, row } => self.handle_mouse_release(column, row),
                AppEvent::MouseScroll { delta } => self.handle_scroll(delta),
                AppEvent::RemoteData(bytes) => {
                    let mut zev = None;
                    let mut cmds = Vec::new();
                    if let Some(s) = &mut self.session {
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
                    if self.session.as_ref().is_some_and(|s| s.is_intercepting()) {
                        self.clear_zmodem_soon();
                    }
                }
                AppEvent::RemoteClosed => {
                    if let Some(mut s) = self.session.take() {
                        s.close();
                        self.status = Some(format!("[{}] 会话已结束", s.label));
                        self.screen = ScreenKind::List;
                        self.list.selected = 0;
                    }
                    self.sftp = None;
                    self.remote_cwd.clear();
                    self.sz_pending.clear();
                    self.rz_pending = false;
                    self.sz_pick_mode = false;
                    self.sz_saveas_pending = false;
                    self.dialog_open = false;
                    self.pending_zmodem = None;
                    self.settings_open = false;
                    self.transfer_popup = false;
                }
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
                                None => Some(self.unlock.input.clone()),
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
                            self.master_secret = Some(self.unlock.input.clone());
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
                            self.vault_key = Some(vk);
                            self.master_secret = Some(new_master.clone());
                            self.mp_stage = MpStage::Idle;
                            self.mp_buf.clear();
                            self.mp_first.clear();
                            if !self.settings.master_password_enabled {
                                let _ = crate::settings::write_master_backup(&new_master);
                            }
                            self.status = Some("主密码已修改，下次启动用新密码".to_string());
                        }
                        Err(msg) => {
                            self.status = Some(format!("修改主密码失败：{msg}"));
                        }
                    }
                }
                AppEvent::PickedUpload(path) => {
                    self.dialog_open = false;
                    match path {
                        Some(p) => self.start_upload(PathBuf::from(p)),
                        None => {
                            self.status = Some("已取消上传（未选择文件）".to_string());
                            self.upload_dest = None;
                        }
                    }
                    if let Some(pending) = self.pending_zmodem.take() {
                        self.on_zmodem_event(pending);
                    }
                }
                AppEvent::PickedSave { entry, path } => {
                    self.dialog_open = false;
                    match path {
                        Some(p) => {
                            if self.sz_pick_mode {
                                self.sz_pick_mode = false;
                                self.screen = ScreenKind::Session;
                            }
                            self.start_download_to(entry, PathBuf::from(p));
                        }
                        None => {
                            self.status = Some("已取消下载（未选择保存位置）".to_string());
                        }
                    }
                    if let Some(pending) = self.pending_zmodem.take() {
                        self.on_zmodem_event(pending);
                    }
                }
                AppEvent::SftpCwd(Ok(dir)) => {
                    self.remote_cwd = dir.clone();
                    if self.rz_pending {
                        self.rz_pending = false;
                        self.open_upload_picker(dir.clone());
                    }
                    if !self.sz_pending.is_empty() {
                        let files = std::mem::take(&mut self.sz_pending);
                        if files.len() == 1 && std::mem::take(&mut self.sz_saveas_pending) {
                            self.open_sz_save_as(&files[0], dir);
                        } else {
                            self.sz_saveas_pending = false;
                            self.start_sz(files, dir);
                        }
                    }
                }
                AppEvent::SftpCwd(Err(err)) => {
                    self.status = Some(format!("无法解析远端目录: {err}"));
                    // 目录拿不到就别让 pending 状态过夜，否则会串到下一次 sz/rz
                    self.rz_pending = false;
                    self.sz_pending.clear();
                    self.sz_saveas_pending = false;
                }
                AppEvent::ZmodemClear(seq) => {
                    if seq == self.zclear_seq {
                        // 无条件复位：裸 sz 只触发检测不进入吞流，但 watcher 的
                        // fired 状态同样需要清掉，否则下次 sz/rz 不会被检测。
                        if let Some(s) = &mut self.session {
                            s.end_intercept();
                        }
                    }
                }
                AppEvent::SftpHome(res) => {
                    match res {
                        Ok(dir) => self.start_listing(dir),
                        Err(err) => {
                            self.browser.loading = false;
                            self.browser.error = Some(err);
                        }
                    }
                }
                AppEvent::SftpListed(res) => {
                    self.browser.loading = false;
                    match res {
                        Ok(entries) => {
                            self.browser.entries = entries;
                            self.browser.selected =
                                self.browser.selected.min(self.browser.entries.len().saturating_sub(1));
                            self.browser.scroll = self.browser.scroll.min(self.browser.selected);
                            self.browser.error = None;
                        }
                        Err(err) => self.browser.error = Some(err),
                    }
                }
                AppEvent::SftpProgress(pr) => {
                    if let Some(item) = self
                        .browser
                        .transfers
                        .iter_mut()
                        .find(|t| t.label == pr.label && !t.done)
                    {
                        item.progress = Some(pr);
                    } else {
                        let label = pr.label.clone();
                        self.browser.transfers.push(TransferItem {
                            label,
                            direction: "?",
                            progress: Some(pr),
                            done: false,
                            error: None,
                        });
                    }
                }
                AppEvent::SftpDone(res) => match res {
                    Ok(label) => {
                        let dir = if let Some(item) = self
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
                        self.status = Some(format!("{dir}完成：{label}"));
                        if self.screen == ScreenKind::Browser && !self.browser.loading {
                            self.start_listing(self.browser.path.clone());
                        }
                    }
                    Err((label, msg)) => {
                        if let Some(item) = self
                            .browser
                            .transfers
                            .iter_mut()
                            .find(|t| t.label == label && !t.done)
                        {
                            item.done = true;
                            item.error = Some(msg.clone());
                        }
                        self.status = Some(msg);
                    }
                },
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
        // char must never reach text inputs.
        let key = match key.code {
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL) && c.is_control() =>
            {
                match char::from_u32(c as u32 + 0x60) {
                    Some(letter) if c as u32 >= 0x01 && (c as u32) <= 0x1a => {
                        KeyEvent::new(KeyCode::Char(letter), KeyModifiers::CONTROL)
                    }
                    _ => return,
                }
            }
            _ => key,
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.settings_open {
            self.handle_settings_key(&key);
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
        match self.screen {
            ScreenKind::Session => self.handle_session_key(&key, ctrl).await,
            ScreenKind::Unlock => self.handle_unlock_key(&key, ctrl),
            ScreenKind::List => self.handle_list_key(&key, ctrl),
            ScreenKind::Form => {
                self.handle_form_key(&key, ctrl);
            }
            ScreenKind::Browser => self.handle_browser_key(&key, ctrl),
        }
    }

    async fn handle_session_key(&mut self, key: &KeyEvent, ctrl: bool) {
        if ctrl && matches!(key.code, KeyCode::Char('s')) {
            self.open_browser();
            return;
        }
        let action = match &mut self.session {
            Some(s) => s.handle_key(key),
            None => SessionAction::Keep,
        };
        if action == SessionAction::Detach {
            self.detach_session("已返回列表");
        }
    }

    /// 顶部按钮行/弹窗的鼠标命中测试。
    fn handle_mouse(&mut self, column: u16, row: u16) {
        if self.settings_open {
            if self.mp_stage != MpStage::Idle {
                // 改密输入中：鼠标不参与，只认键盘
                return;
            }
            let [hl_r, ka_r, mp_r, change_r, save_r, cancel_r] = ui::settings_hit_rects(self.last_area);
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
                let [settings_r, upload_r, download_r, progress_r] =
                    ui::header_button_rects(self.last_area);
                if hit(settings_r, column, row) {
                    self.settings_open = true;
                    self.settings_focus = 0;
                } else if hit(upload_r, column, row) {
                    self.trigger_upload();
                } else if hit(download_r, column, row) {
                    self.trigger_download();
                } else if hit(progress_r, column, row) && !self.browser.transfers.is_empty() {
                    self.transfer_popup = true;
                } else if hit(ui::session_emu_rect(self.last_area), column, row) {
                    // 终端区按下 = 开始拖选（鼠标捕获后原生选择失效，由 ells 自绘）
                    if let Some(s) = &mut self.session {
                        s.begin_selection(column, row);
                    }
                }
            }
            ScreenKind::Browser => {
                let list = ui::browser_layout(self.last_area)[1];
                if row >= list.y && row < list.y.saturating_add(list.height) {
                    let idx = (row - list.y) as usize + self.browser.scroll;
                    if idx < self.browser.entries.len() {
                        self.browser.selected = idx;
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
                if hit(ui::homepage_rect(self.last_area), column, row) {
                    self.open_homepage();
                } else if hit(ui::list_settings_rect(self.last_area), column, row) {
                    self.settings_open = true;
                    self.settings_focus = 0;
                }
            }
            _ => {}
        }
    }

    fn handle_mouse_drag(&mut self, column: u16, row: u16) {
        if self.screen != ScreenKind::Session {
            return;
        }
        if let Some(s) = &mut self.session {
            s.update_selection(column, row);
        }
    }

    fn handle_mouse_release(&mut self, column: u16, row: u16) {
        if self.screen != ScreenKind::Session {
            return;
        }
        let area = ui::session_emu_rect(self.last_area);
        let text = match &mut self.session {
            Some(s) => {
                s.update_selection(column, row);
                s.selected_text(area)
            }
            None => return,
        };
        if !text.is_empty() {
            copy_osc52(&text);
            let n = text.chars().count();
            self.status = Some(format!("已复制 {n} 个字符（OSC 52 剪贴板）"));
        }
    }

    /// 滚轮：会话=回看历史；浏览器=移动选择（列表窗口跟随滚动）。
    fn handle_scroll(&mut self, delta: i8) {
        match self.screen {
            ScreenKind::Session => {
                if let Some(s) = &mut self.session {
                    s.handle_wheel(delta);
                }
            }
            ScreenKind::Browser => {
                if self.browser.entries.is_empty() {
                    return;
                }
                let height = ui::browser_layout(self.last_area)[1].height.max(1) as usize;
                let len = self.browser.entries.len();
                if delta > 0 {
                    self.browser.selected = (self.browser.selected + 1).min(len - 1);
                } else {
                    self.browser.selected = self.browser.selected.saturating_sub(1);
                }
                let b = &mut self.browser;
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
        if self.sftp.is_none() {
            self.status = Some("该服务器不支持 SFTP 文件传输".to_string());
            return;
        }
        if self.remote_cwd.is_empty() {
            self.rz_pending = true;
            self.refresh_remote_cwd();
        } else {
            let dir = self.remote_cwd.clone();
            self.open_upload_picker(dir);
        }
    }

    /// 顶部「下载」按钮 = sz 功能：打开远端文件浏览器选择下载目标。
    fn trigger_download(&mut self) {
        if self.sftp.is_none() {
            self.status = Some("该服务器不支持 SFTP 文件传输".to_string());
            return;
        }
        self.sz_pick_mode = true;
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
                self.settings_focus = (self.settings_focus + 1).min(5);
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
    /// 2 主密码保护开关、3 进入修改主密码输入、4 保存、5 取消
    fn apply_settings_focus(&mut self) {
        match self.settings_focus {
            0 => self.settings.highlight = !self.settings.highlight,
            1 => {
                self.settings.keepalive_secs = cycle_keepalive(self.settings.keepalive_secs);
            }
            2 => self.toggle_master_setting(),
            3 => self.begin_master_change(),
            4 => self.save_settings(),
            _ => self.cancel_settings(),
        }
    }

    fn save_settings(&mut self) {
        self.settings.save();
        ells_core::ssh::set_keepalive_interval(self.settings.keepalive_secs);
        // 主密码开关联动本地自动解锁凭据：关闭=写入，开启=删除
        if self.settings.master_password_enabled {
            crate::settings::clear_master_backup();
        } else if let Some(master) = &self.master_secret {
            if let Err(err) = crate::settings::write_master_backup(master) {
                self.status = Some(format!("设置已保存，但免密凭据写入失败: {err}"));
                self.settings_open = false;
                self.reset_master_edit();
                return;
            }
        }
        self.settings_open = false;
        self.reset_master_edit();
        self.status = Some("设置已保存（保活间隔对下次连接生效）".to_string());
    }

    fn cancel_settings(&mut self) {
        // 丢弃未保存的改动，回到磁盘上的当前值
        self.settings = Settings::load();
        ells_core::ssh::set_keepalive_interval(self.settings.keepalive_secs);
        self.settings_open = false;
        self.reset_master_edit();
    }

    fn reset_master_edit(&mut self) {
        self.mp_stage = MpStage::Idle;
        self.mp_buf.clear();
        self.mp_first.clear();
    }

    fn detach_session(&mut self, status: &str) {
        if let Some(mut s) = self.session.take() {
            s.close();
            self.status = Some(format!("[{}] {status}", s.label));
        }
        self.sftp = None;
        self.remote_cwd.clear();
        self.sz_pending.clear();
        self.rz_pending = false;
        self.sz_pick_mode = false;
        self.sz_saveas_pending = false;
        self.settings_open = false;
        self.transfer_popup = false;
        self.screen = ScreenKind::List;
        self.list.selected = 0;
    }

    fn open_browser(&mut self) {
        let Some(sftp) = self.sftp.clone() else {
            self.status = Some("该服务器不支持 SFTP 文件传输".to_string());
            return;
        };
        self.browser.reset();
        self.browser.loading = true;
        self.screen = ScreenKind::Browser;
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let res = sftp
                .canonicalize(".")
                .await
                .map_err(|e| format!("无法解析远端目录: {e}"));
            let _ = tx.send(AppEvent::SftpHome(res));
        });
    }

    fn start_listing(&mut self, dir: String) {
        let Some(sftp) = self.sftp.clone() else {
            return;
        };
        self.browser.path = dir.clone();
        self.browser.loading = true;
        self.browser.error = None;
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let res = ells_transfer::list(&sftp, &dir)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::SftpListed(res));
        });
    }

    fn open_upload_picker(&mut self, dest: String) {
        if self.sftp.is_none() {
            return;
        }
        self.dialog_open = true;
        self.upload_dest = Some(dest);
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            let path = tokio::task::spawn_blocking(move || {
                crate::dialog::pick_file("选择要上传的文件", home, Vec::new())
            })
            .await
            .ok()
            .flatten();
            let _ = tx.send(AppEvent::PickedUpload(path));
        });
    }

    fn start_upload(&mut self, local: PathBuf) {
        let Some(sftp) = self.sftp.clone() else {
            self.status = Some(format!("上传失败：{local:?} 该连接没有 SFTP 通道"));
            return;
        };
        let Some(name) = local.file_name().map(|s| s.to_string_lossy().into_owned()) else {
            self.status = Some("无法解析所选文件名".to_string());
            return;
        };
        let dest = self
            .upload_dest
            .take()
            .unwrap_or_else(|| self.browser.path.clone());
        let remote = ells_transfer::remote_join(&dest, &name);
        self.register_transfer(name.clone(), "上传");
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let (ptx, mut prx) = mpsc::unbounded_channel::<Progress>();
            let pump_tx = tx.clone();
            let pump = tokio::spawn(async move {
                while let Some(pr) = prx.recv().await {
                    let _ = pump_tx.send(AppEvent::SftpProgress(pr));
                }
            });
            let res = match ells_transfer::upload(&sftp, &local, remote, ptx).await {
                Ok(()) => Ok(name),
                Err(err) => Err((name, format!("{err:#}"))),
            };
            let _ = tx.send(AppEvent::SftpDone(res));
            let _ = pump.await;
        });
    }

    fn start_download(&mut self, entry: FileEntry) {
        let dest_dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let name = entry.name.clone();
        self.spawn_download(entry, dest_dir, name);
    }

    /// Download to an explicit local path chosen via the system "Save As" dialog.
    fn start_download_to(&mut self, entry: FileEntry, dest: PathBuf) {
        let dest_dir = dest
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let name = dest
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.name.clone());
        self.spawn_download(entry, dest_dir, name);
    }

    fn spawn_download(&mut self, entry: FileEntry, dest_dir: PathBuf, name: String) {
        let Some(sftp) = self.sftp.clone() else {
            return;
        };
        self.register_transfer(entry.name.clone(), "下载");
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let (ptx, mut prx) = mpsc::unbounded_channel::<Progress>();
            let pump_tx = tx.clone();
            let pump = tokio::spawn(async move {
                while let Some(pr) = prx.recv().await {
                    let _ = pump_tx.send(AppEvent::SftpProgress(pr));
                }
            });
            let res = match ells_transfer::download(&sftp, entry.path, &dest_dir, ptx, &name)
                .await
            {
                Ok(path) => {
                    let _ = path;
                    Ok(entry.name)
                }
                Err(err) => Err((entry.name, format!("{err:#}"))),
            };
            let _ = tx.send(AppEvent::SftpDone(res));
            let _ = pump.await;
        });
    }

    /// `sz <one-file>`: resolve the remote path and open the native Save As dialog.
    fn open_sz_save_as(&mut self, file: &str, cwd: String) {
        let path = if file.starts_with('/') {
            file.to_string()
        } else {
            ells_transfer::remote_join(&cwd, file)
        };
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        self.open_save_as(FileEntry {
            name,
            path,
            is_dir: false,
            size: 0,
        });
    }

    fn open_save_as(&mut self, entry: FileEntry) {
        if self.sftp.is_none() {
            self.status = Some("该服务器不支持 SFTP 文件传输".to_string());
            return;
        }
        self.dialog_open = true;
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let dir = dirs::download_dir()
                .or_else(dirs::home_dir)
                .unwrap_or_else(|| PathBuf::from("."));
            let name = entry.name.clone();
            let path = tokio::task::spawn_blocking(move || {
                crate::dialog::save_file("保存下载文件", dir, &name)
            })
            .await
            .ok()
            .flatten();
            let _ = tx.send(AppEvent::PickedSave { entry, path });
        });
    }

    fn register_transfer(&mut self, label: String, direction: &'static str) {
        // 完成的传输保留在本次会话里（顶部"传输进度 x/x"要统计总数），
        // 只在异常多时裁掉最旧的，防止长会话内存无限增长
        while self.browser.transfers.iter().filter(|t| t.done).count() >= 50 {
            let Some(idx) = self.browser.transfers.iter().position(|t| t.done) else {
                break;
            };
            self.browser.transfers.remove(idx);
        }
        self.browser.transfers.push(TransferItem {
            label,
            direction,
            progress: None,
            done: false,
            error: None,
        });
    }

    fn on_zmodem_event(&mut self, ev: crate::zmodem::ZmodemEvent) {
        use crate::zmodem::ZmodemEvent as Z;
        if !matches!(ev, Z::Missing { .. }) && self.dialog_open {
            let name = match &ev {
                Z::Send { .. } => "sz",
                Z::Receive => "rz",
                _ => "zmodem",
            };
            self.status = Some(format!("已拦截 {name}：请先完成当前弹窗，完成后会自动继续"));
            self.pending_zmodem = Some(ev);
            return;
        }
        match ev {
            Z::Missing { sz } => {
                let cmd = if sz { "sz" } else { "rz" };
                self.status = Some(format!(
                    "远端没有 {cmd}：ells 的 sz/rz 转换需要服务器安装 lrzsz（apt/yum install lrzsz）"
                ));
            }
            Z::Send { files } => {
                self.clear_zmodem_soon();
                if self.sftp.is_none() {
                    self.status = Some("已拦截 sz，但该连接没有 SFTP 通道".to_string());
                    return;
                }
                // 新的 sz 意图取代可能残留的 rz 等待，避免 SftpCwd 回来时先弹上传框
                self.rz_pending = false;
                if files.is_empty() {
                    self.status =
                        Some("已拦截 sz：请在文件浏览器中选择要下载的文件".to_string());
                    self.sz_pick_mode = true;
                    self.open_browser();
                    return;
                }
                if files.len() == 1 {
                    // 单文件 sz：直接弹系统「另存为」
                    self.status = Some("已拦截 sz：请在弹出窗口选择保存位置".to_string());
                    let f = files[0].clone();
                    if self.remote_cwd.is_empty() {
                        self.sz_saveas_pending = true;
                        self.sz_pending = files;
                        self.refresh_remote_cwd();
                    } else {
                        let cwd = self.remote_cwd.clone();
                        self.open_sz_save_as(&f, cwd);
                    }
                    return;
                }
                self.status = Some(format!("已拦截 sz：{} 个文件改走 SFTP 下载", files.len()));
                if self.remote_cwd.is_empty() {
                    self.sz_pending = files;
                    self.refresh_remote_cwd();
                } else {
                    let cwd = self.remote_cwd.clone();
                    self.start_sz(files, cwd);
                }
            }
            Z::Receive => {
                self.clear_zmodem_soon();
                if self.sftp.is_none() {
                    self.status = Some("已拦截 rz，但该连接没有 SFTP 通道".to_string());
                    return;
                }
                self.sz_pending.clear();
                self.sz_saveas_pending = false;
                self.status = Some("已拦截 rz：请在弹出窗口选择要上传的本地文件".to_string());
                if self.remote_cwd.is_empty() {
                    self.rz_pending = true;
                    self.refresh_remote_cwd();
                } else {
                    let cwd = self.remote_cwd.clone();
                    self.open_upload_picker(cwd);
                }
            }
            Z::Unknown => {
                self.clear_zmodem_soon();
                self.status = Some("检测到 ZMODEM 握手但无法判定方向，已中止远端传输".to_string());
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
        let Some(sftp) = self.sftp.clone() else { return };
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let res = sftp.canonicalize(".").await.map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::SftpCwd(res));
        });
    }

    fn clear_zmodem_soon(&mut self) {
        self.zclear_seq += 1;
        let seq = self.zclear_seq;
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(4000)).await;
            let _ = tx.send(AppEvent::ZmodemClear(seq));
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
                self.remote_cwd.clear();
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
                    self.remote_cwd = arg.clone();
                    return;
                }
                if self.remote_cwd.is_empty() {
                    return;
                }
                self.remote_cwd = if arg == ".." {
                    ells_transfer::remote_parent(&self.remote_cwd)
                } else {
                    ells_transfer::remote_join(&self.remote_cwd, arg.trim_start_matches("./"))
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
                self.sz_pick_mode = false;
                self.screen = ScreenKind::Session;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.browser.selected = self.browser.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                if self.browser.selected + 1 < self.browser.entries.len() {
                    self.browser.selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(entry) = self.browser.entries.get(self.browser.selected).cloned() {
                    if entry.is_dir {
                        self.start_listing(entry.path);
                    } else if self.sz_pick_mode {
                        self.open_save_as(entry);
                    } else {
                        self.start_download(entry);
                    }
                }
            }
            KeyCode::Backspace | KeyCode::Char('h') => {
                if self.browser.path != "/" {
                    let parent = ells_transfer::remote_parent(&self.browser.path);
                    self.start_listing(parent);
                }
            }
            KeyCode::Char('u') => {
                let path = self.browser.path.clone();
                self.open_upload_picker(path)
            }
            KeyCode::Char('d') => {
                if let Some(entry) = self.browser.entries.get(self.browser.selected).cloned() {
                    if !entry.is_dir {
                        if self.sz_pick_mode {
                            self.open_save_as(entry);
                        } else {
                            self.start_download(entry);
                        }
                    }
                }
            }
            KeyCode::Char('r') => {
                let path = self.browser.path.clone();
                self.start_listing(path);
            }
            KeyCode::Char('c') if ctrl => {}
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
                if self.unlock.pending_master.as_deref() == Some(input.as_str()) {
                    self.unlock.busy = true;
                    let tx = self.event_tx.clone();
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
                    self.unlock.pending_master = None;
                    self.unlock.input.clear();
                }
            }
        }
    }

    fn handle_list_key(&mut self, key: &KeyEvent, ctrl: bool) {
        // One-shot: after unlocking, honour `ells <alias>` direct connect.
        if std::mem::take(&mut self.pending_unlock_action) {
            if let Some(alias) = self.direct_alias.clone() {
                if let Some(host) = self.vault.find(&alias).cloned() {
                    self.start_connect(host);
                    return;
                }
                self.status = Some(format!("找不到别名为 `{alias}` 的主机"));
            }
            self.direct_alias = None;
        }

        let len = self.vault.hosts.len();
        match key.code {
            KeyCode::Esc => self.done = true,
            KeyCode::Char('q') => self.done = true,
            KeyCode::Char('c') if ctrl => self.done = true,
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
            KeyCode::Char('s') => {
                self.settings_open = true;
                self.settings_focus = 0;
            }
            KeyCode::Enter => {
                if let Some(host) = self.vault.hosts.get(self.list.selected).cloned() {
                    self.start_connect(host);
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
            ScreenKind::Session => match &self.session {
                // label 形如 `别名 · user@host:port`，标签页只需要别名
                Some(s) => {
                    format!("ells-{}", s.label.split(" · ").next().unwrap_or("会话"))
                }
                None => "ells-会话".to_string(),
            },
            ScreenKind::Browser => "ells-远程文件".to_string(),
        };
        if self.term_title.as_deref() != Some(title.as_str()) {
            set_term_title(&title);
            self.term_title = Some(title);
        }
    }

    fn start_connect(&mut self, host: Host) {
        self.status = Some(format!("正在连接 {}…", host.alias));
        self.pending_connect = Some(host);
    }

    async fn connect(&mut self, host: Host) {
        let label = format!("{} · {}", host.alias, host.target());
        let (cols, rows) = term_size();
        let vault = self.vault.clone();
        let result = RemoteSession::connect(&host, &vault, cols, rows.max(2) - 1).await;
        match result {
            Ok(mut session) => {
                if let Some(rx) = session.take_output() {
                    events::spawn_remote_pump(rx, self.event_tx.clone());
                }
                self.sftp = session.sftp();
                let state = SessionState::new(label, session, rows, cols);
                self.session = Some(state);
                self.screen = ScreenKind::Session;
                self.status = None;
                self.remote_cwd.clear();
                self.sz_pending.clear();
                self.rz_pending = false;
                // 新连接从零开始：清掉上一个会话的传输记录
                self.browser.transfers.clear();
                self.transfer_popup = false;
                self.refresh_remote_cwd();
            }
            Err(err) => {
                self.status = Some(format!("连接失败: {err:#}"));
            }
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

/// 通过 OSC 52 转义序列把文本放进终端模拟器的剪贴板
/// （Windows Terminal / iTerm2 / kitty 等原生支持，无需系统剪贴板依赖）。
fn copy_osc52(text: &str) {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = stdout();
    let _ = write!(out, "\x1b]52;c;{b64}\x1b\\");
    let _ = out.flush();
}

/// OSC 0 设置终端标签页/窗口标题（Windows Terminal、iTerm2、kitty 等均支持）。
fn set_term_title(title: &str) {
    let mut out = stdout();
    let _ = write!(out, "\x1b]0;{title}\x07");
    let _ = out.flush();
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

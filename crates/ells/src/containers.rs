//! 容器视图的状态与纯投影。网络与解析在 `ells_core`，这里只管"打开着的那一屏"。

use chrono::{DateTime, Local, Utc};
pub use ells_core::docker::ContainerAction;
use ells_core::docker::{
    dedupe_ports, ContainerDetail, ContainerStats, DockerContainer, PortMapping,
};
use std::collections::HashSet;

/// 底栏一句提示的存活时间：`docker ps` 打完之后提示一会儿就收，
/// 免得用户早就去干别的了，底栏还挂着"F4 看容器"。
const HINT_TTL: std::time::Duration = std::time::Duration::from_secs(12);

/// 收起时每个容器下最多列几条端口；超出的折成 `+N`。
const PREVIEW_PORTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DockerTab {
    #[default]
    Containers,
    Ports,
}

/// 端口中心视图的一行：一条**已发布**的映射（没发布的暴露口不进这张表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortRow {
    pub name: String,
    pub id: String,
    pub ip: String,
    pub public_port: u16,
    pub private_port: u16,
    pub proto: String,
}

/// 容器列表里真正要画的一行。父行是容器，子行是端口或"+N"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListLine {
    Container { index: usize },
    Port { container: usize, port: PortMapping },
    More { container: usize, hidden: usize },
}

/// 操作菜单的一项：详情单独一类，其余都是容器动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuChoice {
    Detail,
    Act(ContainerAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionMenu {
    pub container_id: String,
    pub container_name: String,
    pub choices: Vec<MenuChoice>,
    pub selected: usize,
    /// 选中的动作在等二次确认；`None` 表示还在选。
    pub confirm: Option<ContainerAction>,
    pub busy: bool,
    pub error: Option<String>,
}

impl ActionMenu {
    pub fn current(&self) -> Option<MenuChoice> {
        self.choices.get(self.selected).copied()
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.choices.is_empty() {
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, self.choices.len() as isize - 1) as usize;
    }

    /// 菜单正文行数——绘制与鼠标命中测试必须用同一个数。
    pub fn body_rows(&self) -> usize {
        if self.confirm.is_some() {
            2 + usize::from(self.error.is_some())
        } else if self.busy {
            2
        } else {
            self.choices.len() + 1 + usize::from(self.error.is_some())
        }
    }
}

/// 按容器状态决定菜单里能做什么。详情永远排第一，回车误触也不会改状态。
pub fn menu_choices(state: &str) -> Vec<MenuChoice> {
    let mut out = vec![MenuChoice::Detail];
    match state {
        "running" => out.extend([
            MenuChoice::Act(ContainerAction::Restart),
            MenuChoice::Act(ContainerAction::Stop),
            MenuChoice::Act(ContainerAction::Pause),
            MenuChoice::Act(ContainerAction::Kill),
        ]),
        "paused" => out.extend([
            MenuChoice::Act(ContainerAction::Unpause),
            MenuChoice::Act(ContainerAction::Stop),
        ]),
        "restarting" => out.push(MenuChoice::Act(ContainerAction::Stop)),
        "exited" | "created" | "dead" => out.extend([
            MenuChoice::Act(ContainerAction::Start),
            MenuChoice::Act(ContainerAction::Remove),
        ]),
        "removing" => {}
        _ => out.extend([
            MenuChoice::Act(ContainerAction::Start),
            MenuChoice::Act(ContainerAction::Stop),
        ]),
    }
    out
}

pub fn menu_choice_label(choice: MenuChoice) -> &'static str {
    match choice {
        MenuChoice::Detail => "详情（含资源占用）",
        MenuChoice::Act(action) => action_label(action),
    }
}

pub fn action_label(action: ContainerAction) -> &'static str {
    match action {
        ContainerAction::Start => "启动",
        ContainerAction::Stop => "停止",
        ContainerAction::Restart => "重启",
        ContainerAction::Kill => "强制停止",
        ContainerAction::Pause => "暂停",
        ContainerAction::Unpause => "继续",
        ContainerAction::Remove => "删除容器",
    }
}

/// 改状态之前的二次确认；启动/继续/暂停可逆，不拦。
pub fn action_needs_confirm(action: ContainerAction) -> bool {
    matches!(
        action,
        ContainerAction::Stop
            | ContainerAction::Restart
            | ContainerAction::Kill
            | ContainerAction::Remove
    )
}

#[derive(Debug, Default)]
pub struct DetailView {
    pub open: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub data: Option<ContainerDetail>,
    pub scroll: usize,
    pub container_name: String,
}

/// 详情弹窗的一行：区块标题、字段或条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailLine {
    Section(&'static str),
    Field { label: &'static str, value: String },
    Item(String),
}

#[derive(Debug, Default)]
pub struct DockerView {
    pub open: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub items: Vec<DockerContainer>,
    pub selected: usize,
    /// 连 `docker ps -a`（含已停止）还是只看运行中的
    pub all: bool,
    /// 展开了端口的容器 ID（可以同时展开多个）
    pub(crate) expanded: HashSet<String>,
    /// 容器列表从第几行开始画（展开后行数会变，得能单独滚）
    pub scroll: usize,
    pub tab: DockerTab,
    pub menu: Option<ActionMenu>,
    pub detail: DetailView,
    /// 这一轮列表请求的代号：关掉、刷新、切 `all` 都 +1，旧回包因此作废
    pub(crate) seq: u64,
    /// 详情请求的代号：关掉详情、再看另一个容器都 +1
    pub(crate) detail_seq: u64,
}

impl DockerView {
    /// 发起一轮查询（也用于刷新 / 切换 `all`）：旧回包从此对不上代号。
    pub fn begin_fetch(&mut self, all: bool) -> u64 {
        self.all = all;
        self.open = true;
        self.loading = true;
        self.error = None;
        self.seq += 1;
        self.seq
    }

    /// 关掉视图并作废在途回包。
    pub fn close(&mut self) {
        self.open = false;
        self.loading = false;
        self.seq += 1;
        self.menu = None;
        self.detail = DetailView::default();
        self.detail_seq += 1;
    }

    /// 断开 / 关标签：连数据一起清掉。代号继续往上走，在途的旧回包不能对上新会话。
    pub fn reset(&mut self) {
        let (seq, detail_seq) = (self.seq, self.detail_seq);
        *self = DockerView::default();
        self.seq = seq;
        self.detail_seq = detail_seq;
    }

    pub fn adopt(&mut self, seq: u64, res: Result<Vec<DockerContainer>, String>) -> bool {
        if seq != self.seq {
            return false;
        }
        self.loading = false;
        match res {
            Ok(items) => {
                self.items = items;
                self.error = None;
            }
            Err(e) => {
                self.items.clear();
                self.error = Some(e);
            }
        }
        // 刷新后还在的容器保留展开状态，已经没了的 ID 清掉。
        let ids: HashSet<String> = self.items.iter().map(|c| c.id.clone()).collect();
        self.expanded.retain(|id| ids.contains(id));
        if self
            .menu
            .as_ref()
            .is_some_and(|menu| !ids.contains(&menu.container_id))
        {
            self.menu = None;
        }
        if self.selected >= self.rows_len() {
            self.selected = self.rows_len().saturating_sub(1);
        }
        let total = self.line_count();
        self.scroll = self.scroll.min(total.saturating_sub(1));
        true
    }

    /// 可选中的行数（容器页是容器数，端口页是映射数）。
    pub fn rows_len(&self) -> usize {
        match self.tab {
            DockerTab::Containers => self.items.len(),
            DockerTab::Ports => self.ports().len(),
        }
    }

    /// 实际绘制的行数：展开的端口也要算进去。
    pub fn line_count(&self) -> usize {
        match self.tab {
            DockerTab::Containers => self.list_lines().len(),
            DockerTab::Ports => self.ports().len(),
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        let len = self.rows_len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, len as isize - 1) as usize;
    }

    pub fn toggle_tab(&mut self) {
        self.tab = match self.tab {
            DockerTab::Containers => DockerTab::Ports,
            DockerTab::Ports => DockerTab::Containers,
        };
        self.selected = 0;
        self.scroll = 0;
        self.menu = None;
    }

    pub fn running_count(&self) -> usize {
        self.items.iter().filter(|c| c.is_running()).count()
    }

    /// 端口中心：只留发布了宿主口的映射，IPv4/IPv6 通配双绑折叠，
    /// 按宿主端口升序——找"8080 被谁占了"是第一诉求。
    pub fn ports(&self) -> Vec<PortRow> {
        let mut rows: Vec<PortRow> = self
            .items
            .iter()
            .flat_map(|c| {
                dedupe_ports(&c.ports)
                    .into_iter()
                    .filter_map(|p| p.public_port.map(|public| (c, public, p)))
                    .map(|(c, public_port, p)| PortRow {
                        name: c.name.clone(),
                        id: c.id.clone(),
                        ip: p.ip,
                        public_port,
                        private_port: p.private_port,
                        proto: p.proto,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        rows.sort_by(|a, b| {
            a.public_port
                .cmp(&b.public_port)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.proto.cmp(&b.proto))
        });
        rows
    }

    /// 容器页的完整行投影：父行 + 收起时最多 3 条端口 + `+N`。
    pub fn list_lines(&self) -> Vec<ListLine> {
        if self.tab != DockerTab::Containers {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (index, container) in self.items.iter().enumerate() {
            out.push(ListLine::Container { index });
            let ports = dedupe_ports(&container.ports);
            if ports.is_empty() {
                continue;
            }
            let expanded = self.expanded.contains(&container.id);
            let shown = if expanded {
                ports.len()
            } else {
                ports.len().min(PREVIEW_PORTS)
            };
            for port in ports.iter().take(shown) {
                out.push(ListLine::Port {
                    container: index,
                    port: port.clone(),
                });
            }
            if !expanded && ports.len() > shown {
                out.push(ListLine::More {
                    container: index,
                    hidden: ports.len() - shown,
                });
            }
        }
        out
    }

    pub fn is_expanded(&self, index: usize) -> bool {
        self.items
            .get(index)
            .is_some_and(|c| self.expanded.contains(&c.id))
    }

    /// 3 条以内没有可展开的内容，点击不改变状态。
    pub fn toggle_expanded(&mut self) {
        let Some(container) = self.items.get(self.selected) else {
            return;
        };
        if dedupe_ports(&container.ports).len() <= PREVIEW_PORTS {
            return;
        }
        if !self.expanded.remove(&container.id) {
            self.expanded.insert(container.id.clone());
        }
    }

    /// 父行端口列的那一小段：`▸ 5 条` / `▾ 5 条` / `3 条` / `—`。
    pub fn port_badge(&self, index: usize) -> String {
        let Some(container) = self.items.get(index) else {
            return "—".to_string();
        };
        let count = dedupe_ports(&container.ports).len();
        if count == 0 {
            return "—".to_string();
        }
        if count <= PREVIEW_PORTS {
            return format!("{count} 条");
        }
        if self.expanded.contains(&container.id) {
            format!("▾ {count} 条")
        } else {
            format!("▸ {count} 条")
        }
    }

    /// 选中容器在行投影里的起始行。
    pub fn selected_line_index(&self, lines: &[ListLine]) -> usize {
        lines
            .iter()
            .position(|line| {
                matches!(line, ListLine::Container { index } if *index == self.selected)
            })
            .unwrap_or(0)
    }

    /// 保证选中容器（连同展开的端口）在视口里；比视口还高时优先露出父行。
    pub fn ensure_selected_visible(&mut self, visible: usize) {
        if self.tab != DockerTab::Containers {
            return;
        }
        let lines = self.list_lines();
        if lines.is_empty() || visible == 0 {
            self.scroll = 0;
            return;
        }
        let start = self.selected_line_index(&lines);
        let end = lines[start + 1..]
            .iter()
            .position(|line| matches!(line, ListLine::Container { .. }))
            .map(|offset| start + 1 + offset)
            .unwrap_or(lines.len());
        let max = lines.len().saturating_sub(visible);
        if end.saturating_sub(start) > visible {
            self.scroll = start.min(max);
            return;
        }
        self.scroll = self.scroll.min(max);
        if start < self.scroll {
            self.scroll = start;
        } else if end > self.scroll + visible {
            self.scroll = (end - visible).min(max);
        }
    }

    pub fn scroll_by(&mut self, delta: isize, visible: usize) {
        if self.tab != DockerTab::Containers {
            return;
        }
        let total = self.line_count();
        if visible == 0 || total <= visible {
            self.scroll = 0;
            return;
        }
        let max = (total - visible) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as usize;
    }

    /// 打开选中行的操作菜单。端口页也能对映射所属的容器操作。
    pub fn open_menu(&mut self) -> bool {
        let (id, name, state) = match self.tab {
            DockerTab::Containers => {
                let Some(container) = self.items.get(self.selected) else {
                    return false;
                };
                (
                    container.id.clone(),
                    container.name.clone(),
                    container.state.clone(),
                )
            }
            DockerTab::Ports => {
                let rows = self.ports();
                let Some(row) = rows.get(self.selected) else {
                    return false;
                };
                let state = self
                    .items
                    .iter()
                    .find(|c| c.id == row.id)
                    .map(|c| c.state.clone())
                    .unwrap_or_default();
                (row.id.clone(), row.name.clone(), state)
            }
        };
        let choices = menu_choices(&state);
        if choices.is_empty() {
            return false;
        }
        self.menu = Some(ActionMenu {
            container_id: id,
            container_name: name,
            choices,
            selected: 0,
            confirm: None,
            busy: false,
            error: None,
        });
        true
    }

    pub fn begin_detail(&mut self, name: &str) -> u64 {
        self.menu = None;
        self.detail = DetailView {
            open: true,
            loading: true,
            container_name: name.to_string(),
            ..DetailView::default()
        };
        self.detail_seq += 1;
        self.detail_seq
    }

    pub fn close_detail(&mut self) {
        self.detail = DetailView::default();
        self.detail_seq += 1;
    }

    pub fn adopt_detail(
        &mut self,
        seq: u64,
        res: Result<ContainerDetail, String>,
    ) -> bool {
        if seq != self.detail_seq || !self.detail.open {
            return false;
        }
        self.detail.loading = false;
        match res {
            Ok(data) => {
                self.detail.data = Some(data);
                self.detail.error = None;
            }
            Err(e) => {
                self.detail.data = None;
                self.detail.error = Some(e);
            }
        }
        self.detail.scroll = 0;
        true
    }

    pub fn detail_line_count(&self) -> usize {
        self.detail
            .data
            .as_ref()
            .map(|data| detail_lines(data).len())
            .unwrap_or(0)
    }

    pub fn scroll_detail(&mut self, delta: isize, visible: usize) {
        let total = self.detail_line_count();
        if visible == 0 || total <= visible {
            self.detail.scroll = 0;
            return;
        }
        let max = (total - visible) as isize;
        self.detail.scroll = (self.detail.scroll as isize + delta).clamp(0, max) as usize;
    }

    /// 详情滚到底：可见行数由界面给。
    pub fn detail_scroll_end(&mut self, visible: usize) {
        self.detail.scroll = self
            .detail_line_count()
            .saturating_sub(visible.max(1));
    }

    /// 复制选中行：TSV，粘到表格软件里就是一列对一格。
    pub fn copy_text(&self) -> Option<String> {
        match self.tab {
            DockerTab::Containers => {
                let c = self.items.get(self.selected)?;
                Some(format!(
                    "{}\t{}\t{}\t{}\t{}",
                    c.name,
                    c.image,
                    c.state,
                    c.status,
                    ports_summary(&c.ports)
                ))
            }
            DockerTab::Ports => {
                let r = self.ports().get(self.selected)?.clone();
                Some(format!(
                    "{}\t{}\t{}\t{}/{}",
                    r.name, r.ip, r.public_port, r.private_port, r.proto
                ))
            }
        }
    }
}

/// 一个容器全部端口的紧凑写法：`8080→80/tcp, 9000/tcp（仅暴露）`。
pub fn ports_summary(ports: &[PortMapping]) -> String {
    let ports = dedupe_ports(ports);
    if ports.is_empty() {
        return "—".to_string();
    }
    ports
        .iter()
        .map(|p| match p.public_port {
            Some(public) => format!("{public}→{}/{}", p.private_port, p.proto),
            None => format!("{}/{}（仅暴露）", p.private_port, p.proto),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// 绑定地址的人话：通配地址意味着这台机器对外可达。
pub fn exposure(ip: &str) -> &str {
    match ip {
        "" => "仅暴露",
        "0.0.0.0" | "::" => "全网可达",
        ip => ip,
    }
}

/// 详情弹窗的分区内容。纯投影，时间用 `Utc::now()`，测试走 `detail_lines_at`。
pub fn detail_lines(detail: &ContainerDetail) -> Vec<DetailLine> {
    detail_lines_at(detail, Utc::now())
}

pub fn detail_lines_at(detail: &ContainerDetail, now: DateTime<Utc>) -> Vec<DetailLine> {
    let mut out = Vec::new();
    out.push(DetailLine::Section("基本"));
    push_field(
        &mut out,
        "名称",
        if detail.name.is_empty() {
            "—".to_string()
        } else {
            detail.name.clone()
        },
    );
    push_field(&mut out, "容器 ID", short_digest(&detail.id));
    push_field(&mut out, "镜像", image_text(detail));
    let mut state = if detail.state.is_empty() {
        "—".to_string()
    } else {
        detail.state.clone()
    };
    if let Some(health) = &detail.health {
        state.push_str(" · ");
        state.push_str(health);
    }
    if detail.oom_killed {
        state.push_str(" · 因内存不足被杀");
    }
    push_field(&mut out, "状态", state);
    push_field(&mut out, "创建时间", format_time(detail.created));
    push_field(&mut out, "启动时间", format_time(detail.started_at));
    push_field(&mut out, "运行时长", uptime_text(detail, now));
    push_field(
        &mut out,
        "重启策略",
        restart_policy_text(&detail.restart_policy, detail.restart_max_retry),
    );
    push_field(&mut out, "重启次数", detail.restart_count.to_string());
    if !detail.is_active() {
        push_field(
            &mut out,
            "退出码",
            detail
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "—".to_string()),
        );
        if detail.finished_at.is_some() {
            push_field(&mut out, "停止时间", format_time(detail.finished_at));
        }
    }

    out.push(DetailLine::Section("资源占用"));
    if let Some(stats) = &detail.stats {
        out.extend(stats_lines(stats));
    } else if let Some(error) = &detail.stats_error {
        push_field(&mut out, "读取失败", error.clone());
    } else if !detail.is_active() {
        out.push(DetailLine::Item("容器未运行，没有实时资源数据".to_string()));
    } else {
        out.push(DetailLine::Item("没有取到资源数据".to_string()));
    }

    out.push(DetailLine::Section("网络"));
    if detail.networks.is_empty() {
        out.push(DetailLine::Item("—".to_string()));
    } else {
        for network in &detail.networks {
            out.push(DetailLine::Item(if network.ip.is_empty() {
                network.name.clone()
            } else {
                format!("{} · {}", network.name, network.ip)
            }));
        }
    }

    out.push(DetailLine::Section("端口"));
    let ports = dedupe_ports(&detail.ports);
    if ports.is_empty() {
        out.push(DetailLine::Item("—".to_string()));
    } else {
        for port in &ports {
            out.push(DetailLine::Item(port_text(port)));
        }
    }

    out.push(DetailLine::Section("挂载"));
    if detail.mounts.is_empty() {
        out.push(DetailLine::Item("—".to_string()));
    } else {
        for mount in &detail.mounts {
            let mode = if mount.read_only { "只读" } else { "读写" };
            let kind = if mount.kind.is_empty() {
                mode.to_string()
            } else {
                format!("{mode} · {}", mount.kind)
            };
            out.push(DetailLine::Item(format!(
                "{} → {}（{kind}）",
                mount.source, mount.destination
            )));
        }
    }

    out.push(DetailLine::Section("命令"));
    if let Some(entrypoint) = &detail.entrypoint {
        push_field(&mut out, "Entrypoint", entrypoint.clone());
    }
    if let Some(cmd) = &detail.cmd {
        push_field(&mut out, "命令", cmd.clone());
    }
    push_field(
        &mut out,
        "用户",
        if detail.user.is_empty() {
            "默认（root）".to_string()
        } else {
            detail.user.clone()
        },
    );
    push_field(
        &mut out,
        "工作目录",
        if detail.working_dir.is_empty() {
            "/".to_string()
        } else {
            detail.working_dir.clone()
        },
    );
    out
}

/// `c` 复制详情：TSV，区块名单独一行。
pub fn detail_copy_text(detail: &ContainerDetail) -> String {
    detail_lines(detail)
        .into_iter()
        .map(|line| match line {
            DetailLine::Section(name) => format!("【{name}】"),
            DetailLine::Field { label, value } => format!("{label}\t{value}"),
            DetailLine::Item(value) => format!("\t{value}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn push_field(out: &mut Vec<DetailLine>, label: &'static str, value: String) {
    out.push(DetailLine::Field { label, value });
}

fn image_text(detail: &ContainerDetail) -> String {
    match (detail.image.is_empty(), detail.image_id.is_empty()) {
        (true, true) => "—".to_string(),
        (true, false) => short_digest(&detail.image_id),
        (false, true) => detail.image.clone(),
        (false, false) => format!("{}（{}）", detail.image, short_digest(&detail.image_id)),
    }
}

fn short_digest(id: &str) -> String {
    if id.is_empty() {
        return "—".to_string();
    }
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    id.chars().take(12).collect()
}

fn uptime_text(detail: &ContainerDetail, now: DateTime<Utc>) -> String {
    if detail.is_active() {
        return detail
            .started_at
            .map(|started| format_duration(started, now))
            .unwrap_or_else(|| "—".to_string());
    }
    detail
        .finished_at
        .map(|finished| format!("已停止（{}）", format_time(Some(finished))))
        .unwrap_or_else(|| "—".to_string())
}

fn restart_policy_text(policy: &str, max_retry: u64) -> String {
    match policy {
        "" | "no" => "不自动重启".to_string(),
        "always" => "总是重启".to_string(),
        "unless-stopped" => "除非手动停止，否则重启".to_string(),
        "on-failure" if max_retry > 0 => format!("失败时重启（最多 {max_retry} 次）"),
        "on-failure" => "失败时重启".to_string(),
        other => other.to_string(),
    }
}

fn stats_lines(stats: &ContainerStats) -> Vec<DetailLine> {
    let mut out = Vec::new();
    if let Some(cpu) = stats.cpu_percent {
        push_field(&mut out, "CPU", format!("{cpu:.1}%"));
    }
    match (stats.mem_used, stats.mem_limit) {
        (Some(used), Some(limit)) => {
            let percent = stats
                .mem_percent
                .map(|p| format!("（{p:.1}%）"))
                .unwrap_or_default();
            push_field(
                &mut out,
                "内存",
                format!("{} / {}{percent}", format_bytes(used), format_bytes(limit)),
            );
        }
        (Some(used), None) => push_field(&mut out, "内存", format_bytes(used)),
        _ => {}
    }
    if stats.net_rx.is_some() || stats.net_tx.is_some() {
        push_field(
            &mut out,
            "网络",
            format!(
                "接收 {} · 发送 {}",
                opt_bytes(stats.net_rx),
                opt_bytes(stats.net_tx)
            ),
        );
    }
    if let Some(pids) = stats.pids {
        push_field(&mut out, "进程", pids.to_string());
    }
    if stats.block_read.is_some() || stats.block_write.is_some() {
        push_field(
            &mut out,
            "磁盘 I/O",
            format!(
                "读 {} · 写 {}",
                opt_bytes(stats.block_read),
                opt_bytes(stats.block_write)
            ),
        );
    }
    if out.is_empty() {
        out.push(DetailLine::Item("没有取到可用的资源数据".to_string()));
    }
    out
}

fn opt_bytes(value: Option<u64>) -> String {
    value
        .map(format_bytes)
        .unwrap_or_else(|| "—".to_string())
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_time(time: Option<DateTime<Utc>>) -> String {
    time.map(|t| {
        t.with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    })
    .unwrap_or_else(|| "—".to_string())
}

pub fn format_duration(from: DateTime<Utc>, to: DateTime<Utc>) -> String {
    let seconds = (to - from).num_seconds().max(0);
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days} 天 {hours} 小时")
    } else if hours > 0 {
        format!("{hours} 小时 {minutes} 分")
    } else if minutes > 0 {
        format!("{minutes} 分")
    } else {
        format!("{seconds} 秒")
    }
}

fn port_text(port: &PortMapping) -> String {
    match port.public_port {
        Some(public) => format!(
            "{public} → {}/{} · {}",
            port.private_port,
            port.proto,
            exposure(&port.ip)
        ),
        None => format!(
            "{}/{} · {}",
            port.private_port,
            port.proto,
            exposure(&port.ip)
        ),
    }
}

/// 这一屏输出像不像 `docker ps` 的表头。只认**同一行**里同时出现
/// CONTAINER ID / IMAGE 和 STATUS 或 PORTS：偶然打印到这几个单词的日志不算。
pub fn looks_like_docker_ps(text: &str) -> bool {
    text.lines().any(|line| {
        line.contains("CONTAINER ID")
            && line.contains("IMAGE")
            && (line.contains("STATUS") || line.contains("PORTS") || line.contains("COMMAND"))
    })
}

/// 滚动扫描窗口：`docker ps` 的表头完全可能被拆在两个输出块里
/// （SSH 的包边界不按行切），所以留一小截尾巴再匹配。
#[derive(Debug, Default)]
pub struct DockerHint {
    tail: Vec<u8>,
}

impl DockerHint {
    const MAX: usize = 8 * 1024;

    /// 喂一段远端输出；这一次是否**新**认出表头。
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return false;
        }
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > Self::MAX {
            let cut = self.tail.len() - Self::MAX;
            self.tail.drain(..cut);
        }
        if looks_like_docker_ps(&String::from_utf8_lossy(&self.tail)) {
            // 认出来就把尾巴清掉：紧接着的表格行不该反复刷新提示。
            self.tail.clear();
            return true;
        }
        false
    }
}

#[derive(Debug, Default)]
pub struct DockerHintClock {
    hint: DockerHint,
    until: Option<std::time::Instant>,
}

impl DockerHintClock {
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.hint.feed(bytes) {
            self.until = Some(std::time::Instant::now() + HINT_TTL);
        }
    }

    pub fn active(&self) -> bool {
        self.until.is_some_and(|t| std::time::Instant::now() < t)
    }

    pub fn clear(&mut self) {
        self.until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ells_core::docker::{MountInfo, NetworkInfo, PortMapping};

    fn c(name: &str, state: &str, ports: Vec<PortMapping>) -> DockerContainer {
        DockerContainer {
            id: format!("{name:0<12}"),
            name: name.to_string(),
            image: "img".to_string(),
            state: state.to_string(),
            status: "status".to_string(),
            ports,
        }
    }

    fn p(ip: &str, public: Option<u16>, private: u16) -> PortMapping {
        PortMapping {
            ip: ip.to_string(),
            private_port: private,
            public_port: public,
            proto: "tcp".to_string(),
        }
    }

    fn many_ports(count: u16) -> Vec<PortMapping> {
        (1..=count)
            .map(|i| p("0.0.0.0", Some(8000 + i), i))
            .collect()
    }

    fn detail_fixture() -> ContainerDetail {
        let at = Utc.with_ymd_and_hms(2026, 11, 4, 10, 0, 0).unwrap();
        ContainerDetail {
            id: "0123456789abcdef0123".to_string(),
            name: "web".to_string(),
            image: "nginx:latest".to_string(),
            image_id: "sha256:abcdef0123456789".to_string(),
            state: "running".to_string(),
            health: Some("healthy".to_string()),
            created: Some(at),
            started_at: Some(at),
            finished_at: None,
            exit_code: Some(0),
            restart_count: 2,
            restart_policy: "unless-stopped".to_string(),
            restart_max_retry: 0,
            networks: vec![NetworkInfo {
                name: "bridge".to_string(),
                ip: "172.17.0.2".to_string(),
            }],
            mounts: vec![MountInfo {
                source: "/srv/www".to_string(),
                destination: "/usr/share/nginx/html".to_string(),
                read_only: false,
                kind: "bind".to_string(),
            }],
            ports: vec![p("0.0.0.0", Some(8080), 80)],
            entrypoint: Some("/docker-entrypoint.sh".to_string()),
            cmd: Some("nginx -g daemon off;".to_string()),
            user: "root".to_string(),
            working_dir: "/usr/share/nginx/html".to_string(),
            oom_killed: false,
            stats: Some(ContainerStats {
                cpu_percent: Some(12.5),
                mem_used: Some(128 * 1024 * 1024),
                mem_limit: Some(1024 * 1024 * 1024),
                mem_percent: Some(12.5),
                net_rx: Some(1024),
                net_tx: Some(2048),
                pids: Some(3),
                block_read: Some(4096),
                block_write: Some(0),
            }),
            stats_error: None,
        }
    }

    #[test]
    fn detects_docker_ps_header_even_with_ansi() {
        let colored = "\x1b[1mCONTAINER ID\x1b[0m   IMAGE     COMMAND   CREATED   STATUS    PORTS     NAMES\r\n";
        assert!(looks_like_docker_ps(colored));
        assert!(!looks_like_docker_ps("$ ls -la\r\ntotal 0\r\n"));
    }

    #[test]
    fn hint_survives_split_chunks_and_fires_once() {
        let mut h = DockerHint::default();
        assert!(!h.feed(b"output\r\nCONTAINER"));
        assert!(h.feed(b" ID   IMAGE   COMMAND   STATUS   PORTS\r\nabc web\r\n"));
        assert!(!h.feed(b"remaining rows\r\n"));
    }

    #[test]
    fn port_rows_are_published_only_deduped_and_sorted() {
        let view = DockerView {
            items: vec![
                c(
                    "web",
                    "running",
                    vec![
                        p("0.0.0.0", Some(8080), 80),
                        p("::", Some(8080), 80),
                        p("", None, 9000),
                    ],
                ),
                c("db", "running", vec![p("127.0.0.1", Some(5432), 5432)]),
            ],
            ..Default::default()
        };
        let rows = view.ports();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].public_port, 5432, "按宿主端口升序");
        assert_eq!(rows[1].name, "web");
        assert_eq!(rows[1].ip, "0.0.0.0");
        assert!(!rows[1].id.is_empty(), "端口行要带上容器 ID，才能开操作菜单");
    }

    #[test]
    fn reset_does_not_let_a_stale_result_back_in() {
        let mut view = DockerView::default();
        let old = view.begin_fetch(false);
        view.reset();
        let fresh = view.begin_fetch(false);
        assert_ne!(old, fresh);
        assert!(!view.adopt(old, Ok(vec![c("ghost", "running", vec![])])));
        assert!(view.items.is_empty());
    }

    #[test]
    fn stale_results_are_ignored() {
        let mut view = DockerView::default();
        let fresh = view.begin_fetch(false);
        let stale = view.begin_fetch(true);
        assert_ne!(fresh, stale);
        assert!(!view.adopt(fresh, Ok(vec![c("old", "running", vec![])])));
        assert!(view.adopt(stale, Ok(vec![c("new", "running", vec![])])));
        assert_eq!(view.items[0].name, "new");
        assert!(!view.loading);
    }

    #[test]
    fn selection_clamps_after_shrink() {
        let mut view = DockerView {
            items: vec![c("a", "running", vec![]), c("b", "running", vec![])],
            selected: 1,
            ..Default::default()
        };
        let seq = view.begin_fetch(false);
        view.adopt(seq, Ok(vec![c("a", "running", vec![])]));
        assert_eq!(view.selected, 0);
        view.move_by(-3);
        assert_eq!(view.selected, 0);
        view.move_by(99);
        assert_eq!(view.selected, 0);
    }

    #[test]
    fn copy_text_is_tab_separated() {
        let mut view = DockerView {
            items: vec![c("web", "running", vec![p("0.0.0.0", Some(8080), 80)])],
            ..Default::default()
        };
        assert_eq!(
            view.copy_text().unwrap(),
            "web\timg\trunning\tstatus\t8080→80/tcp"
        );
        view.tab = DockerTab::Ports;
        assert_eq!(view.copy_text().unwrap(), "web\t0.0.0.0\t8080\t80/tcp");
    }

    #[test]
    fn summary_marks_exposed_only_ports() {
        let ports = vec![p("0.0.0.0", Some(8080), 80), p("", None, 9000)];
        assert_eq!(ports_summary(&ports), "8080→80/tcp, 9000/tcp（仅暴露）");
        assert_eq!(ports_summary(&[]), "—");
    }

    #[test]
    fn exposure_reads_as_human() {
        assert_eq!(exposure("0.0.0.0"), "全网可达");
        assert_eq!(exposure("::"), "全网可达");
        assert_eq!(exposure("127.0.0.1"), "127.0.0.1");
        assert_eq!(exposure(""), "仅暴露");
    }

    #[test]
    fn collapsed_list_shows_three_ports_and_hidden_count() {
        let view = DockerView {
            items: vec![c("web", "running", many_ports(5))],
            ..Default::default()
        };
        let lines = view.list_lines();
        assert_eq!(lines.len(), 5, "父行 + 3 条端口 + 还有 2 条");
        assert_eq!(
            lines
                .iter()
                .filter(|line| matches!(line, ListLine::Port { .. }))
                .count(),
            3
        );
        assert!(lines.contains(&ListLine::More {
            container: 0,
            hidden: 2
        }));
        assert_eq!(view.port_badge(0), "▸ 5 条");
    }

    #[test]
    fn expanding_lists_every_port_and_collapsing_restores_preview() {
        let mut view = DockerView {
            items: vec![c("web", "running", many_ports(5))],
            ..Default::default()
        };
        view.toggle_expanded();
        assert!(view.is_expanded(0));
        assert_eq!(view.list_lines().len(), 6);
        assert!(!view
            .list_lines()
            .iter()
            .any(|line| matches!(line, ListLine::More { .. })));
        assert_eq!(view.port_badge(0), "▾ 5 条");
        view.toggle_expanded();
        assert!(!view.is_expanded(0));
        assert_eq!(view.list_lines().len(), 5);
    }

    #[test]
    fn short_port_lists_do_not_toggle() {
        let mut view = DockerView {
            items: vec![c("web", "running", many_ports(3))],
            ..Default::default()
        };
        view.toggle_expanded();
        assert!(!view.is_expanded(0));
        assert_eq!(view.port_badge(0), "3 条");
    }

    #[test]
    fn expansion_survives_refresh_and_is_pruned_when_container_goes_away() {
        let mut view = DockerView {
            items: vec![c("web", "running", many_ports(5))],
            ..Default::default()
        };
        view.toggle_expanded();
        let seq = view.begin_fetch(false);
        view.adopt(seq, Ok(vec![c("web", "running", many_ports(5))]));
        assert!(view.is_expanded(0), "刷新后同一个 ID 仍保留展开");

        let seq = view.begin_fetch(false);
        view.adopt(seq, Ok(vec![c("other", "running", many_ports(5))]));
        assert!(!view.is_expanded(0));
        assert!(view.expanded.is_empty());
    }

    #[test]
    fn menu_choices_follow_container_state() {
        let running = menu_choices("running");
        assert_eq!(running[0], MenuChoice::Detail, "详情永远排第一");
        assert!(running.contains(&MenuChoice::Act(ContainerAction::Restart)));
        assert!(running.contains(&MenuChoice::Act(ContainerAction::Stop)));
        assert!(running.contains(&MenuChoice::Act(ContainerAction::Pause)));
        assert!(running.contains(&MenuChoice::Act(ContainerAction::Kill)));
        assert!(!running.contains(&MenuChoice::Act(ContainerAction::Start)));

        let paused = menu_choices("paused");
        assert!(paused.contains(&MenuChoice::Act(ContainerAction::Unpause)));
        assert!(paused.contains(&MenuChoice::Act(ContainerAction::Stop)));

        let exited = menu_choices("exited");
        assert!(exited.contains(&MenuChoice::Act(ContainerAction::Start)));
        assert!(exited.contains(&MenuChoice::Act(ContainerAction::Remove)));
        assert!(!exited.contains(&MenuChoice::Act(ContainerAction::Restart)));
    }

    #[test]
    fn only_state_changing_actions_need_confirmation() {
        assert!(action_needs_confirm(ContainerAction::Stop));
        assert!(action_needs_confirm(ContainerAction::Restart));
        assert!(action_needs_confirm(ContainerAction::Kill));
        assert!(action_needs_confirm(ContainerAction::Remove));
        assert!(!action_needs_confirm(ContainerAction::Start));
        assert!(!action_needs_confirm(ContainerAction::Pause));
        assert!(!action_needs_confirm(ContainerAction::Unpause));
    }

    #[test]
    fn menu_opens_on_selected_container_and_moves_within_bounds() {
        let mut view = DockerView {
            items: vec![
                c("web", "running", many_ports(1)),
                c("db", "exited", vec![]),
            ],
            selected: 1,
            ..Default::default()
        };
        assert!(view.open_menu());
        let menu = view.menu.as_mut().unwrap();
        assert_eq!(menu.container_name, "db");
        assert_eq!(menu.choices[0], MenuChoice::Detail);
        menu.move_by(99);
        assert_eq!(menu.selected, menu.choices.len() - 1);
        menu.move_by(-99);
        assert_eq!(menu.selected, 0);
    }

    #[test]
    fn detail_swallows_stale_results() {
        let mut view = DockerView::default();
        let first = view.begin_detail("web");
        let second = view.begin_detail("db");
        assert!(!view.adopt_detail(first, Ok(detail_fixture())));
        assert!(view.detail.data.is_none());
        assert!(view.adopt_detail(second, Ok(detail_fixture())));
        assert_eq!(view.detail.data.as_ref().unwrap().name, "web");
        assert!(!view.detail.loading);

        view.close_detail();
        assert!(!view.adopt_detail(second, Ok(detail_fixture())));
        assert!(!view.detail.open);
    }

    #[test]
    fn detail_lines_show_sections_resource_usage_and_ports() {
        let detail = detail_fixture();
        let at = Utc.with_ymd_and_hms(2026, 11, 4, 12, 13, 0).unwrap();
        let lines = detail_lines_at(&detail, at);
        assert!(lines.contains(&DetailLine::Section("基本")));
        assert!(lines.contains(&DetailLine::Section("资源占用")));
        assert!(lines.contains(&DetailLine::Section("端口")));

        let field = |label: &str| {
            lines.iter().find_map(|line| match line {
                DetailLine::Field { label: l, value } if *l == label => Some(value.clone()),
                _ => None,
            })
        };
        assert_eq!(field("CPU").as_deref(), Some("12.5%"));
        assert_eq!(
            field("内存").as_deref(),
            Some("128.0 MiB / 1.0 GiB（12.5%）")
        );
        assert_eq!(field("运行时长").as_deref(), Some("2 小时 13 分"));
        assert_eq!(
            field("重启策略").as_deref(),
            Some("除非手动停止，否则重启")
        );
        assert!(field("镜像").unwrap().contains("nginx:latest"));
        assert!(lines
            .iter()
            .any(|line| matches!(line, DetailLine::Item(v) if v.contains("8080 → 80/tcp"))));
    }

    #[test]
    fn stopped_detail_says_no_live_stats() {
        let mut detail = detail_fixture();
        detail.state = "exited".to_string();
        detail.stats = None;
        detail.finished_at = detail.started_at;
        let lines = detail_lines_at(
            &detail,
            Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap(),
        );
        assert!(lines.contains(&DetailLine::Item(
            "容器未运行，没有实时资源数据".to_string()
        )));
    }

    #[test]
    fn byte_and_duration_formatting() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        let from = Utc.with_ymd_and_hms(2026, 11, 4, 10, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2026, 11, 6, 13, 5, 0).unwrap();
        assert_eq!(format_duration(from, to), "2 天 3 小时");
    }

    #[test]
    fn scrolling_keeps_the_selected_block_visible() {
        let items: Vec<DockerContainer> = (0..10)
            .map(|i| c(&format!("c{i}"), "running", many_ports(5)))
            .collect();
        let mut view = DockerView {
            items,
            selected: 7,
            ..Default::default()
        };
        for container in &view.items {
            view.expanded.insert(container.id.clone());
        }
        let lines = view.list_lines();
        let start = view.selected_line_index(&lines);
        view.ensure_selected_visible(10);
        assert!(view.scroll <= start && start < view.scroll + 10, "选中块首个父行要露出来");
        assert!(start + 6 <= view.scroll + 10 || start + 6 - start > 10);
    }

    #[test]
    fn scrolling_pins_a_block_taller_than_the_viewport() {
        let mut view = DockerView {
            items: vec![c("web", "running", many_ports(15))],
            ..Default::default()
        };
        view.toggle_expanded();
        let lines = view.list_lines();
        let start = view.selected_line_index(&lines);
        view.scroll = 9;
        view.ensure_selected_visible(10);
        assert_eq!(view.scroll, start, "16 行的块放不下时优先从父行开始画");

        view.scroll_by(100, 10);
        assert_eq!(view.scroll, lines.len() - 10);
        view.scroll_by(-100, 10);
        assert_eq!(view.scroll, 0);
    }
}

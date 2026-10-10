use ells_term::vt100;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Gauge, List, ListItem, ListState as RtListState, Paragraph, Wrap,
};
use ratatui::Frame;

use crate::app::{App, Choice, FieldKind, FieldRole, ListRow, Prompt, RULE_COLS, ScreenKind, Slot, UnlockStage};
use crate::session::TermMode;
use crate::theme;

/// "这一行是当前行"的样式：画底色的主题用灰条，不画底色的主题（跟随终端 / 高对比 / 浅色底）
/// 只剩粗体几乎认不出来，所以改用反显——和设置面板的聚焦行同一套写法。
fn select_style() -> Style {
    let mut s = Style::default().bg(theme::band()).add_modifier(Modifier::BOLD);
    if theme::band() == Color::Reset {
        s = s.add_modifier(Modifier::REVERSED);
    }
    s
}

pub fn draw(f: &mut Frame, app: &mut App) {
    app.last_area = f.area();
    match app.screen {
        ScreenKind::Unlock => draw_unlock(f, app),
        ScreenKind::List => draw_list(f, app),
        ScreenKind::Form => draw_form(f, app),
        ScreenKind::Session => draw_session(f, app),
        ScreenKind::Browser => draw_browser(f, app),
    }
    // 更新进度/待重启弹窗先画：它盖住列表与设置页，但确认弹窗仍要在它之上可答
    if app.update.modal() {
        draw_update_popup(f, app);
    }
    // 确认弹窗永远盖在最上层：它可能出现在任何页面（连接中 / 传输中）
    if let Some(choice) = &app.choice {
        draw_choice(f, choice);
    }
    if let Some(prompt) = &app.prompt {
        draw_prompt(f, prompt);
    }
    // 帮助页在最上层：它由任意页面唤起，且期间不响应其它键位
    if app.help_open {
        let area = app.last_area;
        draw_help(f, area, app);
    }
}

/// `v0.1.0 · 构建 2026-10-04 18:00` — build time is injected by build.rs.
fn version_tag() -> String {
    format!(
        "v{} · 构建 {}",
        env!("CARGO_PKG_VERSION"),
        env!("ELLS_BUILD_TIME")
    )
}

pub const HOMEPAGE_URL: &str = "https://ells.cn";

/// 顶部第 1 行右端的官网徽章按钮（列表页与会话页共用）。
/// 显示宽 14："官网" 4 列 + "(ells.cn)" 10 列。
pub fn homepage_rect(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(area.width).saturating_sub(14),
        y: area.y,
        width: 14,
        height: 1,
    }
}

fn draw_homepage_badge(f: &mut Frame, area: Rect) {
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "官网(ells.cn)",
            Style::default()
                .fg(theme::on_accent())
                .bg(theme::select_bg())
                .add_modifier(Modifier::BOLD),
        ))),
        homepage_rect(area),
    );
}

/// 主机列表底部键位行右端的【设置】按钮（绘制与命中测试共用）。
/// 布局固定为 [列表 Min(3) · 键位行 Length(1) · 状态行 Length(2)]，键位行即倒数第 3 行。
pub fn list_settings_rect(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(area.width).saturating_sub(8),
        y: area.y.saturating_add(area.height).saturating_sub(3),
        width: 8,
        height: 1,
    }
}

/// 可更新徽标的矩形：第 0 行、官网徽章（右端 14 列）左侧留一格。
/// 宽度由文案自己算出来，所以绘制与命中测试必须传同一份文案。
pub fn update_badge_rect(area: Rect, label: &str) -> Rect {
    let w = (display_width(label) as u16)
        .min(area.width.saturating_sub(17))
        .max(1);
    Rect {
        x: area
            .x
            .saturating_add(area.width)
            .saturating_sub(15u16.saturating_add(w)),
        y: area.y,
        width: w,
        height: 1,
    }
}

fn draw_unlock(f: &mut Frame, app: &mut App) {
    let creating = app.unlock.stage != UnlockStage::Open;
    // 首次设主密码要多几行"不可找回"警示：框子跟着长高、边框换黄色，
    // 让用户一眼看出这是"正在定规矩"而不是"输入密码"。
    let area = centered(if creating { 56 } else { 50 }, if creating { 11 } else { 9 }, f.area());
    f.buffer_mut().set_style(area, Style::default().bg(theme::bg()));
    let u = &app.unlock;
    let stage_name = match u.stage {
        UnlockStage::Open => "解锁保险库",
        UnlockStage::CreateFirst => "设置主密码",
        UnlockStage::CreateConfirm => "确认主密码",
    };
    let title = format!(" ells · {stage_name} · {} ", version_tag());
    let accent = if creating { theme::warn() } else { theme::text() };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(accent))
        .title_style(Style::default().fg(accent).add_modifier(Modifier::BOLD))
        .style(Style::default().fg(theme::text()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let masked: String = u.input.chars().map(|_| '•').collect();
    // 两种阶段共用同一套 5 行切分：解锁页把第一行留空，省掉两套下标
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(if creating { 3 } else { 0 }),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
    if creating {
        let warn = Paragraph::new(vec![
            Line::from(Span::styled(
                " ! 主密码忘记后无法找回，ells 不做任何找回。",
                Style::default().fg(theme::err()).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "   只能重置保险库重来：主机与凭据都要重填。",
                Style::default().fg(theme::err()),
            )),
            Line::from(Span::styled(
                "   主密码只加密本机保险库，不会发往任何服务器。",
                Style::default().fg(theme::dim()),
            )),
        ]);
        f.render_widget(warn, chunks[0]);
    }
    let first_line = if u.busy {
        Span::styled(
            "正在解密/创建保险库，请稍候（约 1 秒）…",
            Style::default().fg(theme::warn()).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(format!("主密码：{masked}"), Style::default().fg(theme::accent()))
    };
    f.render_widget(Paragraph::new(Line::from(first_line)), chunks[1]);
    if let Some(err) = &u.error {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                err.clone(),
                Style::default().fg(theme::err()),
            ))),
            chunks[2],
        );
    }
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            if creating {
                "Enter 下一步 · Esc 退出 · 请务必记牢"
            } else {
                "Enter 提交 · Esc 退出"
            },
            Style::default().add_modifier(Modifier::DIM),
        ))),
        chunks[3],
    );
}

/// 主机列表页几何：品牌行 / 标签条 / 主机面板 / 键位行 / 状态行。
/// 标签条在列表页也要能点（那是回到"已经连着的会话"的入口），所以绘制与命中测试共用它。
pub fn list_layout(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(2),
        ])
        .split(area)
}

/// 别名列：过滤时把命中的那几个字符点亮，用户才知道 `web` 凭什么排在
/// `dev-worker` 前面。列宽照旧补到 14 列，汉字按两列算。
const ALIAS_COL: usize = 14;

fn alias_spans(alias: &str, hits: &[usize]) -> Vec<Span<'static>> {
    let base = Style::default()
        .fg(theme::warn())
        .add_modifier(Modifier::BOLD);
    let hit = Style::default()
        .fg(theme::accent())
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    if hits.is_empty() {
        return vec![Span::styled(format!("{:<ALIAS_COL$}", alias), base)];
    }
    let chars: Vec<char> = alias.chars().collect();
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_hit = hits.contains(&0);
    for (idx, c) in chars.iter().enumerate() {
        let is_hit = hits.contains(&idx);
        if is_hit != run_hit && !run.is_empty() {
            out.push(Span::styled(
                std::mem::take(&mut run),
                if run_hit { hit } else { base },
            ));
        }
        run_hit = is_hit;
        run.push(*c);
    }
    if !run.is_empty() {
        out.push(Span::styled(run, if run_hit { hit } else { base }));
    }
    let pad = ALIAS_COL.saturating_sub(display_width(alias));
    if pad > 0 {
        out.push(Span::raw(" ".repeat(pad)));
    }
    out
}

/// 「最近使用」的相对时间。列表右端只放得下几个字符，绝对时间既挤又得在心里
/// 做减法；`ts == 0` 是从来没连过，和"很久以前连过"是两件事，得单独说。
fn last_seen(now: u64, ts: u64) -> String {
    if ts == 0 {
        return "从未".to_string();
    }
    let mins = now.saturating_sub(ts) / 60;
    match mins {
        0 => "刚刚".to_string(),
        1..=59 => format!("{mins}分"),
        60..=1439 => format!("{}小时", mins / 60),
        1440..=2879 => "昨天".to_string(),
        2880..=43199 => format!("{}天", mins / 1440),
        _ => format!("{}月", mins / 43200),
    }
}

/// 主机的会话状态标记：● 已连着 / ○ 正在连 / · 空闲。
/// 一台主机可能有多路会话，取第一个命中的活动标签。
fn live_mark(app: &App, alias: &str) -> (char, Color) {
    for slot in &app.slots {
        if slot.host.as_ref().map(|h| h.alias.as_str()) != Some(alias) {
            continue;
        }
        if slot.session.is_some() {
            return ('●', theme::ok());
        }
        if slot.connecting {
            return ('○', theme::warn());
        }
    }
    ('·', theme::dim())
}

/// 某台主机隧道的标记：▲ 已就绪 / ◐ 连接中或重连中 / ▼ 失败或已停 / ○ 未启动。
fn tunnel_mark(app: &App, alias: &str) -> (char, Color) {
    match app.tunnel_state.get(alias) {
        Some(ells_core::tunnel::TunnelState::Up) => ('▲', theme::ok()),
        Some(ells_core::tunnel::TunnelState::Connecting)
        | Some(ells_core::tunnel::TunnelState::Retrying { .. }) => ('◐', theme::warn()),
        Some(_) => ('▼', theme::err()),
        None => ('○', theme::dim()),
    }
}

/// 隧道面板的三块几何：面板本身、顶部的"端口映射"按钮、下面的列表。
/// 画和命中测试共用这一份，改一处不会让另一处跑偏。
pub fn tunnel_rects(area: Rect) -> (Rect, Rect, Rect) {
    let panel = centered(72, 15, area);
    let inner = Block::bordered().inner(panel);
    let chip_row = Rect { x: inner.x, y: inner.y, width: inner.width, height: 1 };
    let list = Rect {
        x: inner.x,
        y: inner.y + 1,
        width: inner.width,
        height: inner.height.saturating_sub(1),
    };
    (panel, chip_row, list)
}

/// 隧道面板顶部的"端口映射"按钮矩形。按钮靠右，显示宽 12（"端口映射" 8 + "（m）" 4）。
/// 面板比按钮还窄时跟着面板缩，不能伸出边框——命中测试用的是同一个矩形。
pub fn tunnel_ports_chip_rect(area: Rect) -> Rect {
    let (_, chip_row, _) = tunnel_rects(area);
    let width = 12.min(chip_row.width);
    Rect {
        x: chip_row.x + chip_row.width - width,
        y: chip_row.y,
        width,
        height: 1,
    }
}

/// 隧道面板：`t` 打开，列出所有带转发规则的主机与它们的实况。
fn draw_tunnels(f: &mut Frame, app: &mut App) {
    let (area, chip_row, list_area) = tunnel_rects(app.last_area);
    f.buffer_mut().set_style(area, Style::default().bg(theme::bg()));
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" 端口转发 ")
        .title_style(Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD));
    f.render_widget(block, area);

    let clashes = ells_core::host::all_port_clashes(&app.vault.hosts);
    let chip = if clashes.is_empty() {
        Line::from(Span::styled(" 端口映射（m）", Style::default().fg(theme::alt())))
    } else {
        // 有主机共用同一个本地口：这句必须出现在面板上，不然用户要等到启动失败才知道
        let ports = clashes
            .iter()
            .map(|(p, _)| p.to_string())
            .collect::<Vec<_>>()
            .join("、");
        Line::from(vec![
            Span::styled(
                format!("⚠ {ports} 被多台主机共用 "),
                Style::default().fg(theme::warn()),
            ),
            Span::styled("端口映射（m）", Style::default().fg(theme::alt())),
        ])
    };
    f.render_widget(Paragraph::new(chip).alignment(ratatui::layout::Alignment::Right), chip_row);

    let rows = app.tunnel_rows();
    let lines: Vec<Line> = if rows.is_empty() {
        vec![
            Line::from(Span::styled(
                " 没有主机配置转发规则 — 列表页选中主机按 p 用表格建，或按 e 在「转发」栏写",
                Style::default().fg(theme::dim()),
            )),
            Line::from(Span::styled(
                " 本地端口留空就交给系统分配，两台主机想要同一个口也不会互挤",
                Style::default().fg(theme::muted()),
            )),
        ]
    } else {
        rows.iter()
            .map(|alias| {
                let host = app.vault.hosts.iter().find(|h| &h.alias == alias);
                let (mark, color) = tunnel_mark(app, alias);
                let mut spans = vec![
                    Span::styled(format!("{mark} "), Style::default().fg(color)),
                    Span::styled(
                        format!("{:<14}", alias),
                        Style::default().fg(theme::warn()).add_modifier(Modifier::BOLD),
                    ),
                ];
                if let Some(h) = host {
                    // 显示 display()：自动口写成"自动"，而不是像填错的 `-L 0:db:5432`
                    spans.push(Span::styled(
                        h.forwards
                            .iter()
                            .map(|x| x.display())
                            .collect::<Vec<_>>()
                            .join("  "),
                        Style::default().fg(theme::muted()),
                    ));
                }
                // 实际监听口：自动分配的那次结果只有隧道自己知道
                if let Some(ports) = app.tunnel_ports.get(alias) {
                    spans.push(Span::styled(
                        format!(" ⇒ {}", ports.iter().map(|p| p.endpoint()).collect::<Vec<_>>().join(" ")),
                        Style::default().fg(theme::ok()),
                    ));
                }
                if let Some(state) = app.tunnel_state.get(alias) {
                    spans.push(Span::styled(
                        format!(" {}", state_label(state)),
                        Style::default().fg(color),
                    ));
                }
                Line::from(spans)
            })
            .collect()
    };
    let selected = app.tunnels_selected.min(rows.len().saturating_sub(1));
    let items: Vec<ListItem> = lines.into_iter().map(ListItem::new).collect();
    let mut ls = RtListState::default();
    if !rows.is_empty() {
        ls.select(Some(selected));
    }
    f.render_stateful_widget(
        List::new(items).highlight_style(select_style()).highlight_symbol("> "),
        list_area,
        &mut ls,
    );
    f.render_widget(
        Paragraph::new(" 空格/s 启停 · Enter 编辑规则表格 · m 端口映射 · x 全部停止 · t/Esc 关闭 ")
            .style(Style::default().fg(theme::dim())),
        Rect {
            x: area.x,
            y: area.y.saturating_add(area.height),
            width: area.width,
            height: 1,
        },
    );
}

fn state_label(state: &ells_core::tunnel::TunnelState) -> &'static str {
    use ells_core::tunnel::TunnelState as S;
    match state {
        S::Connecting => "连接中",
        S::Up => "已就绪",
        S::Retrying { .. } => "重连中",
        S::Failed(_) => "失败",
        S::Stopped => "已停止",
    }
}

/// 规则表格的列宽（显示列数），与 `rules_cell_rects` 同源。
const RULE_W: [usize; 5] = [4, 12, 10, 22, 8];

/// 面板矩形 + 屏幕上真正画出来的那些格子（带各自的表内行号）：
/// 画表格和鼠标命中测试共用这一份几何，滚动窗口也只在这里算一次。
/// 分成两处算过一次事故：绘制按窗口重排了 y，命中却还在用未滚动的行号，滚两行再点第 3 行改的是第 1 行。
pub fn rules_cell_rects(area: Rect, rows: usize, selected: usize) -> (Rect, Vec<(usize, Vec<Rect>)>) {
    let panel = centered(70, (rows + 9) as u16, area);
    let inner = Block::bordered().inner(panel);
    let visible = inner.height.saturating_sub(1) as usize;
    let offset = selected.min(rows.saturating_sub(1)).saturating_sub(visible.saturating_sub(1));
    // 窄终端上列宽要被面板裁住：格子画到边框外面，鼠标命中就会指向一个看不见的地方
    let limit = inner.x + inner.width;
    let mut window = Vec::new();
    for i in offset..rows.min(offset + visible) {
        let y = inner.y + 1 + (i - offset) as u16;
        let mut x = inner.x + 2;
        let mut cols = Vec::with_capacity(RULE_COLS.len());
        for w in RULE_W {
            let cx = x.min(limit);
            let cw = (w as u16).min(limit.saturating_sub(cx));
            cols.push(Rect { x: cx, y, width: cw, height: 1 });
            x = cx + cw + 1;
        }
        window.push((i, cols));
    }
    (panel, window)
}

/// 规则表格编辑器：隧道面板 Enter / 列表页 p 打开。
fn draw_rules(f: &mut Frame, app: &mut App) {
    let rows = app.rules_rows.len();
    let (area, window) = rules_cell_rects(app.last_area, rows, app.rules_row);
    f.buffer_mut().set_style(area, Style::default().bg(theme::bg()));
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(format!(" 转发规则 · {} ", app.rules_alias))
        .title_style(Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // 表头：列名按 RULE_W 对齐，和下面的格子严格同列
    let mut head: Vec<Span> = Vec::with_capacity(RULE_COLS.len());
    for (i, name) in RULE_COLS.iter().enumerate() {
        let w = RULE_W[i] + 1;
        head.push(Span::styled(
            pad_display(name, w - 1),
            Style::default().fg(theme::dim()).add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(head)),
        Rect { x: inner.x + 2, y: inner.y, width: inner.width.saturating_sub(2), height: 1 },
    );

    let clashes = app.rules_clashes();
    // 行数超过面板高度时让选中行保持在视野内，末尾几行照样能改：窗口由 rules_cell_rects 算
    for (i, cols) in &window {
        let row = &app.rules_rows[*i];
        for (col, rect) in cols.iter().enumerate() {
            let raw = if col == 2 && row.listen.trim().is_empty() {
                "自动".to_string()
            } else {
                row.cell(col).to_string()
            };
            let focused = *i == app.rules_row && col == app.rules_col;
            let style = if focused {
                select_style()
            } else if col == 2 && row.listen.trim().is_empty() {
                Style::default().fg(theme::dim())
            } else {
                Style::default().fg(theme::text())
            };
            f.render_widget(Paragraph::new(Span::styled(clip_display(&raw, RULE_W[col]), style)), *rect);
        }
        // 光标行标记 + 撞口标记：整句提示在表格下面一行，这里只放"这行有问题"的记号
        let mark = if *i == app.rules_row { ">" } else { " " };
        let y = cols.first().map(|r| r.y).unwrap_or(inner.y + 1);
        f.render_widget(
            Paragraph::new(Span::styled(
                mark,
                Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD),
            )),
            Rect { x: inner.x, y, width: 1, height: 1 },
        );
        if clashes.get(*i).is_some_and(|c| !c.is_empty()) {
            let last = cols.last().copied().unwrap_or(inner);
            f.render_widget(
                Paragraph::new(Span::styled("⚠", Style::default().fg(theme::warn()))),
                Rect { x: (last.x + last.width + 1).min(inner.x + inner.width - 1), y, width: 1, height: 1 },
            );
        }
    }

    // 表格下方：选中行的撞口整句 → 保存错误 → 键位与自动口说明
    let foot = inner.y + 1 + window.len() as u16;
    let mut line = |y: u16, text: String, color: Color| {
        f.render_widget(
            Paragraph::new(Span::styled(text, Style::default().fg(color))),
            Rect { x: inner.x, y, width: inner.width, height: 1 },
        );
    };
    let clash = clashes.get(app.rules_row).cloned().unwrap_or_default();
    let mut y = foot + 1;
    if !clash.is_empty() {
        line(y, clash, theme::warn());
        y += 1;
    }
    if let Some(err) = &app.rules_error {
        line(y, err.clone(), theme::err());
        y += 1;
    }
    line(y, " ↑↓ 行 · ←→/Tab 格 · 第 1 列 ←/→ 换类型 · Ctrl-N 加行 · Ctrl-D 删行 · Enter 保存 · Esc 放弃 ".to_string(), theme::dim());
    line(
        y + 1,
        " 本地端口留空 = 系统分配空闲口（多台主机同口时用它）· -R 存得进来但起不来 ".to_string(),
        theme::muted(),
    );
}

/// 端口映射弹窗的面板矩形（画与命中测试同源）。
pub fn ports_rect(area: Rect) -> Rect {
    centered(76, 16, area)
}

/// 端口映射总表：`m` 打开，或点隧道面板上的"端口映射"。
/// 这里只报隧道实际绑上的口——自动分配的结果只有它自己知道。
fn draw_ports(f: &mut Frame, app: &mut App) {
    let area = ports_rect(app.last_area);
    f.buffer_mut().set_style(area, Style::default().bg(theme::bg()));
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" 端口映射（实际监听口） ")
        .title_style(Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut aliases: Vec<&String> = app.tunnel_ports.keys().collect();
    aliases.sort();
    let lines: Vec<Line> = if aliases.is_empty() {
        vec![
            Line::from(Span::styled(
                " 现在没有任何正在监听的本地端口",
                Style::default().fg(theme::dim()),
            )),
            Line::from(Span::styled(
                " 按 t 打开端口转发面板 · 空格启动一条 · Enter 用表格改规则",
                Style::default().fg(theme::muted()),
            )),
        ]
    } else {
        let mut out = vec![Line::from(vec![
            Span::styled(pad_display("主机", 15), Style::default().fg(theme::dim()).add_modifier(Modifier::BOLD)),
            Span::styled(pad_display("规则", 30), Style::default().fg(theme::dim()).add_modifier(Modifier::BOLD)),
            Span::styled(pad_display("实际监听", 24), Style::default().fg(theme::dim()).add_modifier(Modifier::BOLD)),
            Span::styled("状态", Style::default().fg(theme::dim()).add_modifier(Modifier::BOLD)),
        ])];
        for alias in aliases {
            let state = app
                .tunnel_state
                .get(alias)
                .map(state_label)
                .unwrap_or("—");
            let ports = app.tunnel_ports[alias].clone();
            for (i, p) in ports.iter().enumerate() {
                let name = if i == 0 { alias.as_str() } else { "" };
                out.push(Line::from(vec![
                    Span::styled(
                        pad_display(&clip_display(name, 14), 15),
                        Style::default().fg(theme::warn()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        pad_display(&p.spec, 30),
                        Style::default().fg(theme::muted()),
                    ),
                    Span::styled(
                        pad_display(&p.endpoint(), 24),
                        Style::default().fg(if p.auto { theme::alt() } else { theme::ok() }),
                    ),
                    Span::styled(state.to_string(), Style::default().fg(theme::text())),
                ]));
            }
        }
        out
    };
    f.render_widget(Paragraph::new(lines), inner);
    f.render_widget(
        Paragraph::new(" m/Esc 关闭 · 状态列是隧道的最新状态 · 撞口时把本地端口留空 ").style(Style::default().fg(theme::dim())),
        Rect {
            x: area.x,
            y: area.y.saturating_add(area.height),
            width: area.width,
            height: 1,
        },
    );
}

/// known_hosts 面板：`h` 打开。ells 自己的记录可删，`~/.ssh` 的只读展示。
fn draw_known(f: &mut Frame, app: &mut App) {
    let area = centered(72, 16, app.last_area);
    f.buffer_mut().set_style(area, Style::default().bg(theme::bg()));
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(format!(" 已知主机密钥（{} 条） ", app.known_entries.len()))
        .title_style(Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let items: Vec<ListItem> = app
        .known_entries
        .iter()
        .map(|e| {
            let mut spans = vec![Span::styled(
                format!("{:<24}", e.hosts),
                Style::default().fg(theme::warn()),
            )];
            spans.push(Span::styled(
                format!("{:<14}", e.algorithm),
                Style::default().fg(theme::muted()),
            ));
            spans.push(Span::raw(e.fingerprint.clone()));
            spans.push(Span::styled(
                if e.writable { " ells" } else { " ~/.ssh 只读" },
                Style::default().fg(theme::dim()),
            ));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut ls = RtListState::default();
    if !app.known_entries.is_empty() {
        ls.select(Some(app.known_selected.min(app.known_entries.len() - 1)));
    }
    f.render_stateful_widget(
        List::new(items)
            .highlight_style(select_style())
            .highlight_symbol("> "),
        inner,
        &mut ls,
    );
    let hint = match &app.known_msg {
        Some(msg) => format!(" {msg} "),
        None => " ↑↓ 选择 · Enter/d 删除(仅 ells 的记录) · Esc 关闭 ".to_string(),
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::default().fg(if app.known_msg.is_some() {
            theme::warn()
        } else {
            theme::dim()
        })),
        Rect {
            x: area.x,
            y: area.y.saturating_add(area.height),
            width: area.width,
            height: 1,
        },
    );
}

/// 审计记录面板：`l` 打开，显示 `~/.ells/audit.log` 末尾若干行。
fn draw_audit(f: &mut Frame, app: &mut App) {
    let area = centered(78, 18, app.last_area);
    f.buffer_mut().set_style(area, Style::default().bg(theme::bg()));
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" 操作记录（最近） ")
        .title_style(Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let text = if app.audit_lines.is_empty() {
        " 还没有记录".to_string()
    } else {
        app.audit_lines.join("\n")
    };
    f.render_widget(
        Paragraph::new(text)
            .scroll((app.audit_scroll as u16, 0))
            .style(Style::default().fg(theme::muted())),
        inner,
    );
    f.render_widget(
        Paragraph::new(" ↑↓ 滚动 · PgUp/PgDn 翻页 · Esc 关闭 ")
            .style(Style::default().fg(theme::dim())),
        Rect {
            x: area.x,
            y: area.y.saturating_add(area.height),
            width: area.width,
            height: 1,
        },
    );
}

/// 把 `2026-10-09T14:03:22+08:00` 收成 `2026-10-09 14:03`：一屏放不下完整时区戳。
fn session_time(stamp: &str) -> String {
    let head: String = stamp.chars().take(16).collect();
    head.replace('T', " ")
}

/// 会话记录面板：`v` 打开。先看 `~/.ells/logs` 的文件列表，Enter 看正文（已去控制序列）。
fn draw_sessions(f: &mut Frame, app: &mut App) {
    let area = centered(78, 18, app.last_area);
    f.buffer_mut().set_style(area, Style::default().bg(theme::bg()));
    f.render_widget(Clear, area);
    let title = match &app.sessions_text {
        Some(_) => {
            let name = app
                .sessions_rows
                .get(app.sessions_selected)
                .map(|e| e.name.clone())
                .unwrap_or_default();
            format!(" 会话记录 · {name} ")
        }
        None => format!(" 会话记录（{} 份） ", app.sessions_rows.len()),
    };
    let block = Block::bordered()
        .title(title)
        .title_style(Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(block, area);

    match &app.sessions_text {
        Some(text) => {
            let shown: String = text
                .lines()
                .skip(app.sessions_scroll)
                .take(inner.height as usize)
                .collect::<Vec<_>>()
                .join("\n");
            f.render_widget(
                Paragraph::new(shown).style(Style::default().fg(theme::muted())),
                inner,
            );
        }
        None => {
            let items: Vec<ListItem> = app
                .sessions_rows
                .iter()
                .map(|e| {
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{:<17} ", session_time(&e.mtime)),
                            Style::default().fg(theme::dim()),
                        ),
                        Span::styled(
                            format!("{:>8}  ", human_size(e.size)),
                            Style::default().fg(theme::muted()),
                        ),
                        Span::raw(e.name.clone()),
                    ]))
                })
                .collect();
            let mut ls = RtListState::default();
            if !app.sessions_rows.is_empty() {
                ls.select(Some(app.sessions_selected.min(app.sessions_rows.len() - 1)));
            }
            f.render_stateful_widget(
                List::new(items)
                    .highlight_style(select_style())
                    .highlight_symbol("> "),
                inner,
                &mut ls,
            );
        }
    }

    let hint = if app.sessions_text.is_some() {
        " ↑↓/jk 滚动 · PgUp/PgDn(du/空格) 翻页 · Esc/Enter 返回列表 "
    } else if app.sessions_rows.is_empty() {
        " 还没有会话记录（连上一台就会开始写） "
    } else {
        " ↑↓/jk 选择 · Enter 查看 · d 删除这一份 · Esc 关闭 "
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::default().fg(theme::dim())),
        Rect {
            x: area.x,
            y: area.y.saturating_add(area.height),
            width: area.width,
            height: 1,
        },
    );
}

fn draw_list(f: &mut Frame, app: &mut App) {
    let chunks = list_layout(f.area());
    let area = f.area();

    // 第 0 行：品牌行（右端官网徽章单独画，避免被本段文字盖住）。
    // 顺手把"这个库里有多少台、几个分组"写在启动就看到的地方：新装的用户
    // 最想知道的是自己的主机到底进来了没有。
    let groups = crate::app::foldable_groups(&app.vault.hosts).len();
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " ells ",
                Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "主机列表 · {} 台 · {groups} 组 · {}",
                    app.vault.hosts.len(),
                    version_tag()
                ),
                Style::default().fg(theme::dim()),
            ),
        ]))
        .style(Style::default().bg(theme::bg())),
        chunks[0],
    );
    // 第 1 行：标签条（与会话页同一份几何）
    let titles: Vec<String> = app.slots.iter().map(Slot::title).collect();
    draw_tab_bar(f, area, &titles, app.active, &app.settings.keybinds);

    // 一帧只算一次：段头、可见主机、高亮行号必须同源，分开算光标会指到隔壁组
    let (rows, visible_hosts) = app.rows_and_visible();
    let selected_row = visible_hosts
        .get(app.list.selected)
        .copied()
        .and_then(|host| {
            rows.iter()
                .position(|r| matches!(r, ListRow::Host(i) if *i == host))
        });
    let total = app.vault.hosts.len();
    let live = app.slots.iter().filter(|s| s.session.is_some()).count();
    let filtering = !app.filter.trim().is_empty();
    let title = if total == 0 {
        " 主机 ".to_string()
    } else if filtering {
        format!(
            " 主机 (共 {total} · 命中 {}/{} · 过滤 {}) ",
            visible_hosts.len(),
            total,
            app.filter.trim()
        )
    } else {
        format!(
            " 主机 (共 {total} · 排序 {}{}) ",
            app.settings.list_sort.label(),
            if live > 0 {
                format!(" · 在线 {live}")
            } else {
                String::new()
            }
        )
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD));
    let inner = block.inner(chunks[2]);
    f.render_widget(block, chunks[2]);

    if rows.is_empty() {
        let hint = if total == 0 {
            " 还没有主机 — 按 a 新增，或按 i 从 ~/.ssh/config 导入"
        } else {
            " 没有匹配的主机 — Esc 清空过滤"
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(theme::dim()),
            ))),
            inner,
        );
    } else if visible_hosts.is_empty() {
        // 全折叠：一条主机都没剩下，但段头还在，别让界面看起来像库空了
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " 全部分组已折叠 — 按 z 展开",
                Style::default().fg(theme::dim()),
            ))),
            inner,
        );
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let indent = app.headers_shown();
        let items: Vec<ListItem> = rows
            .iter()
            .map(|row| match row {
                ListRow::Header { group, count, folded } => ListItem::new(Line::from(vec![
                    Span::styled(
                        // ▾ 展开着 / ▸ 收起来了，箭头方向就是按键之后的样子
                        format!("{} {} ", if *folded { "▸" } else { "▾" }, group),
                        Style::default().fg(theme::alt()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if *folded {
                            format!("({count} 台已折叠)")
                        } else {
                            format!("({count})")
                        },
                        Style::default().fg(theme::muted()),
                    ),
                ])),
                ListRow::Host(i) => {
                    let h = &app.vault.hosts[*i];
                    let (mark, mark_color) = live_mark(app, &h.alias);
                    let mut spans: Vec<Span> = Vec::new();
                    if indent {
                        // 段头之下缩进两列，一眼看出这台属于上面那一段
                        spans.push(Span::raw("  "));
                    }
                    spans.push(Span::styled(
                        format!("{mark} "),
                        Style::default().fg(mark_color).add_modifier(Modifier::BOLD),
                    ));
                    // 收藏的排在前头靠星号说明为什么它在上面
                    spans.push(Span::styled(
                        if h.favorite { "★ " } else { "  " },
                        Style::default().fg(theme::warn()),
                    ));
                    let hits = if filtering {
                        ells_core::filter::match_positions(
                            app.filter.trim(),
                            &app.vault.hosts[*i].alias,
                        )
                    } else {
                        Vec::new()
                    };
                    spans.extend(alias_spans(&h.alias, &hits));
                    spans.push(Span::raw(h.target()));
                    spans.push(Span::styled(
                        format!("  [{}]", h.auth_label()),
                        Style::default().fg(theme::muted()),
                    ));
                    // 段头已经写了组名，行内就不重复占地方
                    if !indent && let Some(g) = &h.group {
                        spans.push(Span::styled(
                            format!(" #{g}"),
                            Style::default().fg(theme::alt()),
                        ));
                    }
                    for t in h.tags.iter().take(3) {
                        spans.push(Span::styled(
                            format!(" {t}"),
                            Style::default().fg(theme::muted()),
                        ));
                    }
                    if let Some(j) = &h.jump {
                        spans.push(Span::styled(
                            format!(" 经 {j}"),
                            Style::default().fg(theme::alt()),
                        ));
                    }
                    // 转发规则数 + 隧道实况：起不来必须在列表上看见，否则用户以为通了
                    if !h.forwards.is_empty() {
                        let (tmark, tcolor) = tunnel_mark(app, &h.alias);
                        spans.push(Span::styled(
                            format!(" {tmark}{}条转发", h.forwards.len()),
                            Style::default().fg(tcolor),
                        ));
                    }
                    // 右端一列「最近使用」：谁还在手上过一目了然，不用点进去猜
                    let stamp = last_seen(now, h.last_connected);
                    let used = spans
                        .iter()
                        .map(|s| display_width(&s.content))
                        .sum::<usize>()
                        + display_width(&stamp);
                    let pad = (inner.width as usize).saturating_sub(used).saturating_sub(2);
                    if pad > 0 {
                        spans.push(Span::raw(" ".repeat(pad)));
                    }
                    spans.push(Span::styled(stamp, Style::default().fg(theme::dim())));
                    ListItem::new(Line::from(spans))
                }
            })
            .collect();
        let mut ls = RtListState::default();
        ls.select(selected_row);
        f.render_stateful_widget(
            List::new(items)
                .highlight_style(select_style())
                .highlight_symbol("> "),
            inner,
            &mut ls,
        );
    }

    let keys = format!(
        " ↑↓ 选择 · Enter 连接 · a 新增 · e 编辑 · d 删除 · i/x 导入导出 · p 转发 · m 映射 · / 过滤 · f 收藏 · Space/z 折叠 · o 排序 · t 隧道 · h 密钥 · l 记录 · {}/{}/{} 切标签 · ? 帮助 · q 退出 ",
        app.settings.keybinds.display(crate::keybinds::Action::NewTab),
        app.settings.keybinds.display(crate::keybinds::Action::NextTab),
        app.settings.keybinds.display(crate::keybinds::Action::PrevTab),
    );
    f.render_widget(
        Paragraph::new(keys).style(Style::default().fg(theme::dim())),
        chunks[3],
    );
    // 键位行右端：【设置】按钮（实心青底，与会话页按钮同风格）
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "【设置】",
            Style::default()
                .fg(theme::on_accent())
                .bg(theme::accent_bg())
                .add_modifier(Modifier::BOLD),
        ))),
        list_settings_rect(f.area()),
    );
    // 过滤态占掉状态行：用户必须看得见自己敲进了什么，否则光标像在瞎跑
    if app.filtering {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" 过滤 ", Style::default().fg(theme::dim())),
                Span::styled(
                    format!("{}▏", app.filter),
                    Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" Enter 保持 · Esc 清空", Style::default().fg(theme::dim())),
            ])),
            chunks[4],
        );
    } else {
        match &app.status {
        Some(status) => f.render_widget(
            Paragraph::new(status.clone()).style(Style::default().fg(theme::ok())),
            chunks[4],
        ),
        // 空闲时用状态行讲清标记的含义，省得用户以为 ● 是装饰
        None => {
            let legend = format!(
                " ● 已连接（Enter 或点顶部标签切回那一格）· ○ 正在连接 · 断开用 {}（只断这一格） ",
                app.settings.keybinds.display(crate::keybinds::Action::CloseTab)
            );
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    legend,
                    Style::default().fg(theme::dim()),
                ))),
                chunks[4],
            );
        }
    }
    }

    // 顶部右端官网徽章
    draw_homepage_badge(f, f.area());
    // 可更新徽标紧挨官网徽章左侧：只在后台确认有新版本后才出现
    if let Some(label) = app.update.badge() {
        let rect = update_badge_rect(f.area(), &label);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                label,
                Style::default()
                    .fg(theme::on_accent())
                    .bg(theme::warn())
                    .add_modifier(Modifier::BOLD),
            ))),
            rect,
        );
    }

    // 删除二级确认弹窗最后渲染，盖住列表
    if let Some(alias) = app.delete_confirm.clone() {
        draw_delete_confirm(f, app.confirm_index, &alias);
    }
    if app.tunnels_open {
        draw_tunnels(f, app);
    }
    if app.rules_open {
        draw_rules(f, app);
    }
    if app.ports_open {
        draw_ports(f, app);
    }
    if app.known_open {
        draw_known(f, app);
    }
    if app.audit_open {
        draw_audit(f, app);
    }
    if app.sessions_open {
        draw_sessions(f, app);
    }
    if app.settings_open {
        draw_settings_stack(f, app);
    }
}

/// 删除确认弹窗的两个按钮矩形（与 draw_delete_confirm 几何一致，供命中测试）。
pub fn delete_confirm_rects(area: Rect) -> [Rect; 2] {
    let p = centered(44, 7, area);
    let ix = p.x + 1;
    let iy = p.y + 1;
    let iw = p.width.saturating_sub(2);
    [
        Rect { x: ix.saturating_add(iw / 2).saturating_sub(16), y: iy + 3, width: 14, height: 1 },
        Rect { x: ix.saturating_add(iw / 2).saturating_add(2), y: iy + 3, width: 14, height: 1 },
    ]
}

fn draw_delete_confirm(f: &mut Frame, confirm_index: usize, alias: &str) {
    let rects = delete_confirm_rects(f.area());
    let panel = centered(44, 7, f.area());
    f.buffer_mut().set_style(panel, Style::default().bg(theme::bg()));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 删除确认 ")
        .title_style(Style::default().fg(theme::err()).add_modifier(Modifier::BOLD));
    f.render_widget(Clear, panel);
    f.render_widget(block, panel);
    let inner_x = panel.x + 1;
    let inner_w = panel.width.saturating_sub(2);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" 确定删除主机 “{alias}” 吗？"),
            Style::default().fg(theme::warn()),
        ))),
        Rect { x: inner_x, y: panel.y + 1, width: inner_w, height: 1 },
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " 此操作不可恢复。",
            Style::default().fg(theme::dim()),
        ))),
        Rect { x: inner_x, y: panel.y + 2, width: inner_w, height: 1 },
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "【 保 留 】",
            Style::default()
                .fg(theme::text())
                .bg(theme::band())
                .add_modifier(if confirm_index == 0 { Modifier::BOLD } else { Modifier::empty() })
                .add_modifier(if confirm_index == 0 {
                    Modifier::UNDERLINED
                } else {
                    Modifier::empty()
                }),
        ))),
        rects[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "【 删 除 】",
            Style::default()
                .fg(theme::on_accent())
                .bg(theme::err())
                .add_modifier(if confirm_index == 1 { Modifier::BOLD } else { Modifier::empty() })
                .add_modifier(if confirm_index == 1 {
                    Modifier::UNDERLINED
                } else {
                    Modifier::empty()
                }),
        ))),
        rects[1],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ←→/Tab 切换 · Enter 确认 · Esc 取消 · y 删 n 留",
            Style::default().fg(theme::dim()),
        ))),
        Rect { x: inner_x, y: panel.y + panel.height.saturating_sub(2), width: inner_w, height: 1 },
    );
}

/// 帮助页：分区块列出全部键位。内容是编译期常量，宽度不够时自动换行，
/// 一屏装不下就靠 `app.help_scroll` 翻页。
fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let binds = &app.settings.keybinds;
    let panel = centered(area.width.saturating_sub(2), area.height.saturating_sub(2), area);
    f.render_widget(Clear, panel);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(" ells 快捷键 ")
            .title_style(
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            ),
        panel,
    );
    use crate::keybinds::Action;
    let list_body = "↑↓/jk 选择 · Enter 连接（这台已经连着就切回它那一格标签）· a 新增 · e 编辑 · d 删除（二次确认）· s 设置 · i 导入 ~/.ssh/config · x 导出 ssh_config · p 用表格编辑这台的转发规则 · m 端口映射总表 · / 过滤 · f 收藏置顶 · Space 折叠/展开当前分组 · z 一键折叠全部 · o 循环排序（默认/最近使用/别名/分组，记住上次选的）· t 端口转发 · h 已知主机密钥 · l 操作记录 · v 会话记录 · ?/F1 帮助 · q/Ctrl-C 退出；行首 ● 已连接 / ○ 正在连接，右端一列是最近使用时间，顶部标签条可以直接点".to_string();
    let organize_body = "/ 之后直接打字即过滤（别名、主机、用户、分组、标签、备注都算），按分数排序并点亮别名里命中的字符，Enter 保留过滤词、Esc 清空；Space 折叠/展开光标所在分组，z 一键收起全部，f 给选中的主机加星。排序按 o 循环四档：默认（分组 → 组内收藏 → 组内最近使用 → 别名）· 最近使用 · 别名 · 分组（组内只按别名），选中的那档写在 settings.ini 的 list_sort=，下次启动还是它；分组/默认两档画段头「▾ 生产 (3)」，过滤时段头收起、折叠也一并失效。表单里的「分组」决定它排在哪儿、「标签」能被过滤命中。".to_string();
    let tunnel_body = "端口转发写在主机表单的「转发」栏里，一条 SSH 连接承载这台机器的全部规则：-L 8080:127.0.0.1:5432（本地口经服务器送进去）、-D 1080（本地 SOCKS5）、裸写 8080:db:5432 也算本地转发；-R 存得进来、导出也带上，但 ells 起不来它，会明确报「暂不支持」而不是静默失效。本地端口可以留空（或写 0）＝交给系统分配，跟 ssh -L 0:db:5432 同义：两台主机都想占 8080 时留空就不互挤，实际落在哪个口按 m 看「端口映射」总表（隧道面板顶部的「端口映射」也能点）。写死的端口真撞上时，启动失败会点名是谁占着（另一台 ells 主机的哪条规则，还是外部程序），界面里编辑时也会提前在行尾标 ⚠。t 打开隧道面板看每台实况（▲ 已就绪 ◐ 连/重连中 ▼ 失败），空格或 s 启停，Enter 进表格改这台的规则（列表页按 p 也能直接开），x 全停。本地监听口在拨号前就占好，会话断开不影响隧道，它自己按 1/2/5/15/30/60 秒退避重连。".to_string();
    let tab_body = format!(
        "{} 新建标签 · {} 下一个 · {} 上一个 · 鼠标点顶部标签条切换、点末尾 + 新建 · {} 关闭当前标签（只断这一格）· {} 回主机列表（连接保持不断，再按一次回到原来的页面）· 列表页与会话页、浏览器页顶部都有标签条",
        binds.display(Action::NewTab),
        binds.display(Action::NextTab),
        binds.display(Action::PrevTab),
        binds.display(Action::CloseTab),
        binds.display(Action::HostList),
    );
    let term_body = format!(
        "直接打字即发往远端 · {} 文件浏览器 · {} 内嵌/直通 · {} 关闭标签 · {} 整屏重绘 · 滚轮回看 · 拖选复制（OSC 52）· {} 搜索历史输出（回看时按 / 同样可用，n/N 上下条）· {} 回主机列表 · F1 帮助",
        binds.display(Action::Browser),
        binds.display(Action::Passthrough),
        binds.display(Action::CloseTab),
        binds.display(Action::Redraw),
        binds.display(Action::Search),
        binds.display(Action::HostList),
    );
    let sections: [(&str, std::borrow::Cow<str>); 16] = [
        ("主机列表", std::borrow::Cow::Owned(list_body)),
        ("分组与过滤", std::borrow::Cow::Owned(organize_body)),
        ("端口转发", std::borrow::Cow::Owned(tunnel_body)),
        ("主机密钥与记录", std::borrow::Cow::Borrowed("h 打开已知主机密钥：ells 自己记的（~/.ells/known_hosts）可以删，删掉下次连这台会重新问一次指纹；~/.ssh/known_hosts 只显示、永不改动。l 打开操作记录：连接/断开、密钥被信任或变更、传输、隧道、保险库保存、改主密码都写进 ~/.ells/audit.log（北京时间、只追加、超过 1 MiB 自动转存，记录里不会出现任何密码或私钥）。")),
        ("会话记录", std::borrow::Cow::Borrowed("v 打开会话记录：每连上一台机器把终端原始输出另存一份 ~/.ells/logs/<别名>-<时间>.log（原样存 ANSI，退回去用 less -R 也能看颜色；界面里查看时已去掉控制序列）。单份 8 MiB 封顶、只留最近 30 天，列表页 d 删单份。开关在「设置 → 会话记录」，关掉之后新连接不再落盘；这份文件里是你在远端敲过的命令和远端打印的一切，别人拿到本机就能读到，介意就关掉。")),
        ("主机指标", std::borrow::Cow::Borrowed("会话页最底下那一行是远端主机的 CPU / 内存 / 磁盘：磁盘取用得最满的那个真实挂载点的容量使用率，数字直接照抄 df 的 Capacity 列（自己算会因为 ext4 预留块和 df 差几个点），旁边写上是哪个挂载点、已用/总量。三格里只放标签、条和百分比，两句补充的话一律排在整行末尾，摆得下才写 —— 挂在某一格后面会把那一格撑得比旁边的宽。采集用的就是那条已经认证好的连接：先在这条连接已有的 SFTP 会话上读 /proc/stat、/proc/meminfo、/proc/loadavg、/proc/mounts，磁盘用 statvfs@openssh.com 问本地盘（网络盘一律不问：一次卡死的 statvfs 会把整条 SFTP 会话连文件传输一起拖住）；没有 SFTP、服务器不支持那个扩展或读不到东西时才回落一条一次性 exec（df -Pk），而且只填还空着的那几格。不另开连接、不碰你正在敲的那个 PTY、也不在远端留任何进程。连上立刻跑第一轮，内存、磁盘、负载当场有数；节奏是分开的：/proc 那三样 5 秒一轮，磁盘那一格 5 秒也跟着换数，但每轮只对「当前最满那块」问一次 statvfs（挂载点表缓存在这条连接上，不然每 5 秒都要重读一遍 /proc/mounts 再逐块问）；每 60 秒才重做一次普查 —— 重读挂载点、逐块问一遍、重新决定哪块最满（问盘最贵，而挂载点集合一分钟里几乎不动）。服务器没有 statvfs 扩展时跟单轮一个请求都不发，那一格沿用上次的数，只在 60 秒的普查点补一条 df —— 绝不让 df 变成每 5 秒一次。没在看的那些标签降到 60 秒一轮，切回去当场重采一轮。CPU 必须两次快照做差才是瞬时值，所以那一格比别的晚两步 —— 基线拿到后两秒就接力，进去约两秒见到第一个数，之后跟着 5 秒一轮；没有 /proc 的机器（FreeBSD、精简容器）一直是 —，磁盘照常。采不到就画 —，绝不画 0%；连续三轮什么都拿不到就判定这台不支持，停掉轮询、把这一行还给终端。通道级失败（超时、开不了通道）不算这台采不到，只是把节奏退到 5 → 10 → 30 → 60 秒，任意一轮拿到数就自动回到 5 秒。开关在「设置 → 主机指标」。")),
        ("导入与导出", std::borrow::Cow::Borrowed("i 从 ~/.ssh/config 导入：只新增库里没有的别名，已经存过的主机一个字都不覆盖（密码认证的那几台导入后要在「编辑」里补密码）。x 将保险库写成一段 ssh_config，落点固定在 ~/.ells/ssh_config.export：只写别名 / 主机 / 端口 / 用户 / IdentityFile / ProxyJump / 转发，密码和主密码永不进这份文件；~/.ssh/config 本体 ells 从不动笔，要用就自己把那几行粘过去。命令行走 ells export（打到标准输出）或 ells export --out 路径。")),
        ("多标签会话", std::borrow::Cow::Owned(tab_body)),
        ("会话终端", std::borrow::Cow::Owned(term_body)),
        ("改键", std::borrow::Cow::Owned(format!(
            "会话与标签页按键可在「设置 → 快捷键设置」里自定义（列表页按 s、会话页按顶部【设置】）：Enter 选中某项后按下新按键即可绑定，只接受 F2–F9 或带 Ctrl/Alt 的组合键（F1 留给帮助页，F10–F12 常被终端或系统吃掉），改完即时写入 ~/.ells/settings.ini；撞到已占用的键会自动互换，【恢复默认】一键还原。跨平台：mac/Linux 终端把 Ctrl-] 这类组合发成与 Ctrl-5 同一个字节，已自动归一，默认键在三个平台都能触发；本机注意点——{}",
            crate::keybinds::platform_note(),
        ))),
        ("外观主题", std::borrow::Cow::Borrowed("设置页「界面主题」按 Enter 或 ←→ 循环四套：跟随终端（一处底色都不画，全用终端自己的主题，mac 终端/iTerm2/WezTerm/Windows Terminal 都合适，**四个平台默认都是它**）· 深色（画死黑底灰条，终端配色自己拿不准时的保底）· 高对比（去掉灰色小字，靠粗体与反显分层）· 浅色底（白底终端用深字）。切换即时预览，【保 存】才写入 settings.ini 的 theme=，【取消】还原。")),
        ("文件浏览器", std::borrow::Cow::Borrowed("↑↓/滚轮 选择 · Enter 进入目录或下载 · u 上传文件 · U 上传整个目录 · d 下载 · m 新建目录 · n 重命名 · c 改权限（八进制 600/0644，只改权限位，不动属主和时间）· D 删除（递归，先确认）· Ctrl-C 取消全部在途传输 · r 刷新 · Backspace 上级 · Esc 返回终端")),
        ("主机表单", std::borrow::Cow::Borrowed("Tab/↓ 下一个字段 · ↑ 上一个 · ←→ 切换认证方式 · Ctrl-F 选私钥 · Ctrl-J 选跳板机 · Ctrl-C 清空当前字段 · 光标在「密码/私钥口令」上按 Ctrl-R 让它显形（再按一次遮回去）· Enter 在\"私钥路径/跳板机\"上直接打开选择器，保存要点【保 存】或聚焦后回车。带 ＊ 的别名、主机、用户是必填项，提交失败时只点缺的那几项的名。「分组」只影响列表归类与排序，光标停在这一栏时下面会列出已有分组和各自台数，敲同名即归入、敲新名即新建（末尾空格不算新分组）；「标签」用逗号分隔、过滤时按 #标签 命中；「转发」写 ssh 风格的 -L/-D 规则（见【端口转发】），想一列一列对着填就在列表页按 p（或隧道面板里按 Enter）进表格编辑器，本地端口那一格留空就是交给系统分配；这三栏留空即无。编辑已有主机不会丢掉它的收藏、备注和最近连接时间；按 a 新增时会把列表里选中的那台的用户/端口/认证方式/私钥路径/跳板/分组/标签/转发带过来，但别名、主机名和密码/口令一律留空 —— 身份和凭据要自己填")),
        ("解锁保险库", std::borrow::Cow::Borrowed("输入主密码后回车；首次使用会要求输入两遍。主密码不可找回，忘记只能删除 ~/.ells/vault.bin 重来。")),
        ("确认弹窗", std::borrow::Cow::Borrowed("←→/Tab 切换选项 · Enter 确认 · Esc 取消。传输冲突默认停在\"改名保留双方\"；主机密钥变更默认停在\"拒绝\"。")),
        ("命令行", std::borrow::Cow::Borrowed("ells 打开列表；ells <别名> 直连；ells --dev 读 ~/.ells/hosts.dev.toml；ells -y 首次主机密钥自动接受（密钥变更仍然拒绝）。配置在 ~/.ells/。")),
    ];
    let mut lines: Vec<Line> = Vec::new();
    for (title, body) in sections {
        lines.push(Line::from(Span::styled(
            format!("【{title}】"),
            Style::default()
                .fg(theme::warn())
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            body.into_owned(),
            Style::default().fg(theme::muted()),
        )));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        " Esc / q / ? / F1 关闭",
        Style::default().fg(theme::dim()),
    )));
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.help_scroll as u16, 0)),
        Rect {
            x: panel.x + 1,
            y: panel.y + 1,
            width: panel.width.saturating_sub(2),
            height: panel.height.saturating_sub(2),
        },
    );
    // 底部提示行盖在面板最后一行上，滚动键位要说得清
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                " ↑↓/jk 滚动 · PgDn/空格 下一页 · Esc/q/? 关闭（第 {} 屏）",
                app.help_scroll + 1
            ),
            Style::default().fg(theme::dim()),
        ))),
        Rect {
            x: panel.x + 1,
            y: panel.y + panel.height.saturating_sub(1),
            width: panel.width.saturating_sub(2),
            height: 1,
        },
    );
}

/// 文本输入弹窗面板矩形（绘制与鼠标命中测试共用）。
pub fn prompt_rect(area: Rect) -> Rect {
    centered(56, 5, area)
}

/// 通用文本输入弹窗：远端新建目录 / 重命名 / 会话内搜索共用。
fn draw_prompt(f: &mut Frame, prompt: &Prompt) {
    let panel = prompt_rect(f.area());
    f.render_widget(Clear, panel);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", prompt.title))
            .title_style(
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            ),
        panel,
    );
    let inner_x = panel.x + 1;
    let inner_w = panel.width.saturating_sub(2);
    let prefix = format!("{}：", prompt.label);
    // 输入超过一行宽度时保留尾部（用户在改文件名时关心的是后半段）
    let room = inner_w
        .saturating_sub(prefix.chars().count() as u16 + 2)
        .max(1) as usize;
    let mut tail: Vec<char> = prompt.buffer.chars().rev().take(room).collect();
    tail.reverse();
    let shown: String = tail.into_iter().collect();
    let text = Line::from(vec![
        Span::styled(prefix, Style::default().fg(theme::dim())),
        Span::styled(shown, Style::default().fg(theme::text())),
        Span::styled(
            "▏",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(
        Paragraph::new(text),
        Rect { x: inner_x, y: panel.y + 1, width: inner_w, height: 1 },
    );
    let note = match &prompt.error {
        Some(err) => (format!(" {err}"), theme::warn()),
        None => {
            let mut hint = " Enter 确认 · Esc 取消".to_string();
            if let Some(extra) = prompt.hint {
                hint.push_str(" · ");
                hint.push_str(extra);
            }
            (hint, theme::dim())
        }
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(note.0, Style::default().fg(note.1)))),
        Rect { x: inner_x, y: panel.y + 2, width: inner_w, height: 1 },
    );
}

/// 确认弹窗几何：面板 + 等宽按钮槽。绘制与鼠标命中测试必须同源，否则点按钮会打偏。
pub fn choice_rects(area: Rect, lines: usize, options: usize) -> (Rect, Vec<Rect>) {
    let options = (options as u16).max(1);
    let panel = centered(62, lines as u16 + 4, area);
    let slot = panel.width.saturating_sub(2) / options;
    let buttons = (0..options)
        .map(|i| Rect {
            x: panel.x + 1 + slot * i,
            y: panel.y + panel.height.saturating_sub(3),
            width: slot,
            height: 1,
        })
        .collect();
    (panel, buttons)
}

/// 通用确认弹窗：主机密钥确认与传输覆盖确认共用一套绘制（语义由 Choice 自己带）。
fn draw_choice(f: &mut Frame, choice: &Choice) {
    let area = f.area();
    let (panel, buttons) = choice_rects(area, choice.lines.len(), choice.options.len());
    f.render_widget(Clear, panel);
    let danger = choice.danger;
    let title_color = if danger { theme::err() } else { theme::accent() };
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", choice.title))
            .title_style(
                Style::default()
                    .fg(title_color)
                    .add_modifier(Modifier::BOLD),
            ),
        panel,
    );
    let inner = Rect {
        x: panel.x + 1,
        y: panel.y + 1,
        width: panel.width.saturating_sub(2),
        height: panel.height.saturating_sub(4),
    };
    let text: Vec<Line> = choice
        .lines
        .iter()
        .map(|l| {
            Line::from(Span::styled(
                l.clone(),
                Style::default().fg(if danger { theme::warn() } else { theme::muted() }),
            ))
        })
        .collect();
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
    for (i, rect) in buttons.iter().enumerate() {
        let Some(label) = choice.options.get(i) else {
            continue;
        };
        let selected = i == choice.selected;
        // 危险弹窗里"让步"的那个选项永远标红，即便焦点不在它上面
        let accept_risky = danger && i > 0;
        let style = Style::default()
            .fg(if selected {
                theme::on_accent()
            } else if accept_risky {
                theme::err()
            } else {
                theme::muted()
            })
            .bg(if selected { theme::select_bg() } else { Color::Reset })
            .add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(format!("【 {label} 】"), style))),
            *rect,
        );
    }
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ←→/Tab 切换 · Enter 确认 · Esc 取消",
            Style::default().fg(theme::dim()),
        ))),
        Rect {
            x: panel.x + 1,
            y: panel.y + panel.height.saturating_sub(2),
            width: panel.width.saturating_sub(2),
            height: 1,
        },
    );
}

/// 表单面板矩形：n 个输入行 + 错误行 + 空行 + 按钮行 + 2 行提示。
/// 绘制与鼠标命中测试共用，保证几何一致。
pub fn form_inner(area: Rect, vis_len: usize) -> Rect {
    let p = centered(64, (vis_len as u16) + 7, area);
    Rect {
        x: p.x + 1,
        y: p.y + 1,
        width: p.width.saturating_sub(2),
        height: p.height.saturating_sub(2),
    }
}

/// 表单底部【保 存】【取 消】按钮矩形（位于 inner.y + n + 2 行）。
pub fn form_button_rects(inner: Rect, vis_len: usize) -> [Rect; 2] {
    let cx = inner.x + inner.width / 2;
    let y = inner.y + (vis_len as u16) + 2;
    [
        Rect { x: cx.saturating_sub(15), y, width: 14, height: 1 },
        Rect { x: cx.saturating_add(1), y, width: 14, height: 1 },
    ]
}

/// 分组项的行内提示：已有分组、各组台数、这次保存会归进哪一段。
/// 只在光标停在分组项上、且没有错误要显示时出现——报错永远优先。
fn group_hint(app: &App) -> Option<String> {
    if app.form.footer.is_some() {
        return None;
    }
    if app.form.fields.get(app.form.focus).map(|f| f.role) != Some(FieldRole::Group) {
        return None;
    }
    let typed = app
        .form
        .fields
        .iter()
        .find(|f| f.role == FieldRole::Group)
        .map(|f| f.value.trim().to_string())
        .unwrap_or_default();
    let counts = crate::app::group_counts(&app.vault.hosts);
    if counts.is_empty() {
        return Some(if typed.is_empty() {
            "还没有分组：起个名字，列表页就按它分段（留空归入「未分组」）".to_string()
        } else {
            format!("还没有分组：将新建「{typed}」（留空归入「未分组」）")
        });
    }
    let listed = counts
        .iter()
        .map(|(g, n)| format!("{g}({n})"))
        .collect::<Vec<_>>()
        .join(" · ");
    Some(if typed.is_empty() {
        format!("已有分组 {listed} — 照敲即归入，留空则在「未分组」")
    } else if counts.iter().any(|(g, _)| g == &typed) {
        format!("已归入「{typed}」— 全部分组：{listed}")
    } else {
        format!("将新建分组「{typed}」— 已有：{listed}")
    })
}

fn draw_form(f: &mut Frame, app: &mut App) {
    let vis = app.form.visible();
    let n = vis.len();
    let area = centered(64, (n as u16) + 7, f.area());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 主机 ")
        .title_style(Style::default().fg(theme::accent()));
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    const LABEL_W: usize = 16;
    let avail = (inner.width as usize).saturating_sub(LABEL_W + 4).max(4);
    let lines: Vec<Line> = vis
        .iter()
        .map(|&i| {
            let field = &app.form.fields[i];
            let focused = app.form.footer.is_none() && i == app.form.focus;
            let full = match field.kind {
                // 遮着的口令只有按过 Ctrl-R 才显形；默认永远是掩码
                FieldKind::Secret if !field.value.is_empty() && !app.form.reveal_secret => {
                    field.value.chars().map(|_| '•').collect::<String>()
                }
                FieldKind::AuthChoice => auth_label(&field.value).to_string(),
                _ => field.value.clone(),
            };
            // Caret is always at the end (insert/backspace), so show the tail.
            let shown = if full.chars().count() > avail {
                let tail: String = full
                    .chars()
                    .rev()
                    .take(avail.saturating_sub(1).max(1))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                format!("…{tail}")
            } else {
                full
            };
            let label_style = Style::default().fg(if focused { theme::accent() } else { theme::text() });
            let must = crate::app::FieldRole::required(field.role);
            Line::from(vec![
                // ＊ 标必填三项；其余行留同样两列的空白，值那一列才对得齐
                Span::styled(
                    if must { "＊" } else { "  " },
                    Style::default()
                        .fg(if must { theme::err() } else { theme::dim() })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(pad_display(field.label, LABEL_W), label_style),
                Span::styled(
                    if shown.is_empty() && !focused {
                        "…".to_string()
                    } else {
                        shown
                    },
                    Style::default()
                        .fg(if focused { theme::warn() } else { theme::muted() })
                        .add_modifier(if focused {
                            Modifier::REVERSED
                        } else {
                            Modifier::empty()
                        }),
                ),
            ])
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines),
        Rect { x: inner.x, y: inner.y, width: inner.width, height: n as u16 },
    );
    if let Some(err) = &app.form.error {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                err.clone(),
                Style::default().fg(theme::err()),
            ))),
            Rect { x: inner.x, y: inner.y + n as u16, width: inner.width, height: 1 },
        );
    } else if let Some(hint) = group_hint(app) {
        // 分组是敲出来的，多一个空格就是一个新段头：光标停在这一项时把已有分组
        // 和它们的台数摆出来，顺便说清这次回车到底归进哪一段。
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(theme::muted()),
            ))),
            Rect { x: inner.x, y: inner.y + n as u16, width: inner.width, height: 1 },
        );
    }

    // 保存/取消按钮：保存是唯一提交入口（聚焦回车或鼠标点击）
    let [save_r, cancel_r] = form_button_rects(inner, n);
    let footer = app.form.footer;
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "【 保 存 】",
            Style::default()
                .fg(theme::on_accent())
                .bg(theme::accent_bg())
                .add_modifier(if footer == Some(0) { Modifier::BOLD } else { Modifier::empty() })
                .add_modifier(if footer == Some(0) {
                    Modifier::UNDERLINED
                } else {
                    Modifier::empty()
                }),
        ))),
        save_r,
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "【 取 消 】",
            Style::default()
                .fg(theme::text())
                .bg(theme::band())
                .add_modifier(if footer == Some(1) { Modifier::BOLD } else { Modifier::empty() })
                .add_modifier(if footer == Some(1) {
                    Modifier::UNDERLINED
                } else {
                    Modifier::empty()
                }),
        ))),
        cancel_r,
    );

    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                " Tab/↓ 移项 · Enter 选私钥/跳板机 · ←/→ 换认证 · 口令项 Ctrl-R 显形",
                Style::default().fg(theme::dim()),
            )),
            Line::from(Span::styled(
                " ＊ 为必填 · 鼠标点击定位 · 按钮 Enter/点击 保存 · 私钥口令仅加密私钥才填",
                Style::default().fg(theme::dim()),
            )),
        ]),
        Rect { x: inner.x, y: inner.y + (n as u16) + 3, width: inner.width, height: 2 },
    );

    if let Some(picker) = &app.form.jump_picker {
        let h = (picker.items.len() as u16) + 4;
        let area = centered(52, h.min(f.area().height.saturating_sub(2)), f.area());
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" 选择跳板机 · ↑↓ 选择 · Enter 确认 · Esc 关闭 ")
            .title_style(Style::default().fg(theme::accent()));
        let pinner = block.inner(area);
        f.render_widget(Clear, area);
        f.render_widget(block, area);
        let items: Vec<ListItem> = picker
            .items
            .iter()
            .enumerate()
            .map(|(i, (_, label))| {
                let style = if i == picker.selected {
                    Style::default().fg(theme::warn()).add_modifier(Modifier::REVERSED)
                } else {
                    Style::default().fg(theme::muted())
                };
                ListItem::new(Line::from(Span::styled(format!(" {label}"), style)))
            })
            .collect();
        f.render_widget(List::new(items), pinner);
    }
}

/// Chinese label for an auth method value.
fn auth_label(v: &str) -> &'static str {
    match v {
        "key" => "密钥",
        "agent" => "ssh-agent",
        _ => "密码",
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{:.1}{}", value, UNITS[unit])
    }
}

/// 浏览器共享布局：路径行 / 文件列表 / 底部提示行。绘制与鼠标命中测试都依赖它，
/// 两边必须使用同一份定义。
pub fn browser_layout(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area)
}

fn draw_browser(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = browser_layout(area);
    // 标签条要读绑定表，而下面 `b` 已经可变借用了 app.slots —— 先拷一份（KeyBinds 是 Copy）
    let binds = app.settings.keybinds;
    let active = app.active;
    let titles: Vec<String> = app.slots.iter().map(Slot::title).collect();
    let height = chunks[2].height.max(1) as usize;
    let b = &mut app.slots[app.active].browser;
    // 键盘移动选择后，滚动窗口在绘制时统一夹住（滚轮/点击路径已自行维护）
    if b.selected >= b.scroll + height {
        b.scroll = b.selected - height + 1;
    }
    if b.selected < b.scroll {
        b.scroll = b.selected;
    }

    let path_line = Line::from(vec![
        Span::styled(
            format!(" 远端文件 {} ", b.path),
            Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD),
        ),
        if b.loading {
            Span::styled("加载中…", Style::default().fg(theme::warn()))
        } else {
            Span::styled(
                format!("{} 项", b.entries.len()),
                Style::default().fg(theme::dim()),
            )
        },
    ]);
    f.render_widget(
        Paragraph::new(path_line).style(Style::default().bg(theme::band())),
        chunks[0],
    );
    // 第 1 行：标签条（列表页/会话页/浏览器页同一套几何，点标签就切过去）
    draw_tab_bar(f, area, &titles, active, &binds);

    let items: Vec<ListItem> = b
        .entries
        .iter()
        .map(|e| {
            let kind = if e.is_dir { "/" } else { "" };
            let size = if e.is_dir {
                String::new()
            } else {
                human_size(e.size)
            };
            let label = format!("{}{}", e.name, kind);
            ListItem::new(Line::from(vec![Span::styled(
                format!("{:<44}{}", label, size),
                Style::default().fg(if e.is_dir { theme::accent() } else { theme::text() }),
            )]))
        })
        .collect();
    let list = List::new(items).highlight_style(select_style());
    let mut ls = RtListState::default().with_offset(b.scroll);
    ls.select(if b.entries.is_empty() {
        None
    } else {
        Some(b.selected.min(b.entries.len() - 1))
    });
    f.render_stateful_widget(list, chunks[2], &mut ls);

    // 传输进度不再占用浏览器界面：统一在会话顶部的聚合条/详情弹窗查看
    if let Some(err) = &b.error {
        f.render_widget(
            Paragraph::new(format!(" {err}")).style(Style::default().fg(theme::err())),
            chunks[3],
        );
    } else {
        let keys =
            " ↑↓/滚轮 选择 · Enter 进入/下载 · u 上传文件 · U 上传目录 · d 下载 · m 新建目录 · n 重命名 · c 改权限 · D 删除 · Ctrl-C 取消传输 · r 刷新 · Esc 返回终端 ";
        f.render_widget(
            Paragraph::new(keys).style(Style::default().fg(theme::dim())),
            chunks[3],
        );
    }
}

/// Pad `s` with spaces so its terminal display width reaches `width`
/// (CJK glyphs count as 2 columns).
fn pad_display(s: &str, width: usize) -> String {
    let mut out = s.to_string();
    for _ in display_width(s)..width {
        out.push(' ');
    }
    out
}

fn display_width(s: &str) -> usize {
    s.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum()
}

fn is_wide(c: char) -> bool {
    let u = c as u32;
    (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE30..=0xFE6F).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x1F300..=0x1FAFF).contains(&u)
        || (0x20000..=0x3FFFD).contains(&u)
}

fn draw_session(f: &mut Frame, app: &mut App) {
    let idx = app.active;
    if let Some(sg) = app.slots[idx].session.as_mut() {
        let off = sg.scroll;
        sg.emu.set_scrollback(off);
    }
    let Some(s) = &app.slots[idx].session else { return };
    // 底部指标行占不占这一行，由 App 一处决定（会话页绘制与鼠标命中都用它），
    // 和 SessionState 里算视口高度时用的那个数必须同源，否则正好裁掉远端最后一行
    let footer = app.footer_rows(idx);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(HEADER_ROWS),
            Constraint::Min(1),
            Constraint::Length(footer),
        ])
        .split(f.area());

    let mode_tag = match s.mode {
        TermMode::Embedded => "内嵌",
        TermMode::Passthrough => "直通",
    };
    // 顶部按键提示跟着用户的自定义绑定走（KeyBinds 是 Copy，避开与 s 的借用冲突）
    let binds = app.settings.keybinds;
    use crate::keybinds::Action;
    let key_hint = format!(
        "{} 切模式 · {} 文件 · {} 重绘 · {} 搜索 · {} 新标签 · {}/{} 切换 · {} 关闭标签 · {} 列表",
        binds.display(Action::Passthrough),
        binds.display(Action::Browser),
        binds.display(Action::Redraw),
        binds.display(Action::Search),
        binds.display(Action::NewTab),
        binds.display(Action::NextTab),
        binds.display(Action::PrevTab),
        binds.display(Action::CloseTab),
        binds.display(Action::HostList),
    );
    let header = Line::from(vec![
        Span::styled(" ells ", Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD)),
        Span::styled(format!("● {} ", s.label), Style::default().fg(theme::warn())),
        Span::styled(
            format!("● {mode_tag} "),
            Style::default()
                .fg(if s.mode == TermMode::Passthrough {
                    theme::err()
                } else {
                    theme::ok()
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(key_hint, Style::default().fg(theme::dim())),
    ]);
    let title_row = Rect {
        x: chunks[0].x,
        y: chunks[0].y,
        width: chunks[0].width,
        height: 1,
    };
    f.render_widget(Paragraph::new(header).style(Style::default().bg(theme::band())), title_row);
    // 第 0 行右端：官网徽章（点击在浏览器打开 https://ells.cn）
    draw_homepage_badge(f, f.area());

    // 第 1 行：标签条（点标签切换，点 + 新建）
    let titles: Vec<String> = app.slots.iter().map(Slot::title).collect();
    draw_tab_bar(f, chunks[0], &titles, app.active, &app.settings.keybinds);

    // 第 2 行：一次性状态提示（连接/拦截/完成/取消）；空闲时给操作指引。
    let note_row = Rect {
        x: chunks[0].x,
        y: chunks[0].y + 2,
        width: chunks[0].width,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(Line::from("")).style(Style::default().bg(theme::bg())),
        note_row,
    );
    if let Some(search) = &app.slots[idx].search {
        let line = Line::from(Span::styled(
            format!(
                " 搜索「{}」：第 {}/{} 个命中 · n 下一个 · N 上一个 · Esc 退出",
                search.query,
                search.cursor + 1,
                search.hits.len()
            ),
            Style::default().fg(theme::accent()).bg(theme::bg()),
        ));
        f.render_widget(Paragraph::new(line), note_row);
    } else if s.scroll > 0 {
        let line = Line::from(Span::styled(
            format!(" 回看历史：已向上 {n} 行 · 滚轮回底部 · 任意按键回到实时", n = s.scroll),
            Style::default().fg(theme::alt()).bg(theme::bg()),
        ));
        f.render_widget(Paragraph::new(line), note_row);
    } else if let Some(note) = app.slots[idx].status.as_ref().or(app.status.as_ref()) {
        let line = Line::from(Span::styled(
            format!(" {note}"),
            Style::default().fg(theme::warn()).bg(theme::bg()),
        ));
        f.render_widget(Paragraph::new(line), note_row);
    } else {
        let line = Line::from(Span::styled(
            " 点击上方按钮或输入 sz/rz 传输文件 · 滚轮回看输出 · 拖选复制",
            Style::default().fg(theme::dim()).bg(theme::bg()),
        ));
        f.render_widget(Paragraph::new(line), note_row);
    }

    // Third header row: [设置] [上传] [下载] + aggregate transfer progress bar.
    draw_header_buttons(f, app);

    if s.mode != TermMode::Embedded {
        return;
    }

    let area = chunks[1];
    let screen = s.emu.screen();
    let (emu_rows, emu_cols) = s.emu.size();
    let rows = emu_rows.min(area.height);
    let cols = emu_cols.min(area.width);
    let highlight_on = app.settings.highlight;
    let sel = s.selection_rect();
    let buf = f.buffer_mut();
    for r in 0..rows {
        // 逐行构建与单元格对齐的文本（宽字符续格用 '\0' 占位）用于高亮匹配
        let colors: Vec<Option<Color>> = if highlight_on {
            let mut text = String::with_capacity(cols as usize);
            for c in 0..cols {
                match screen.cell(r, c) {
                    Some(cell) if cell.is_wide_continuation() => text.push('\0'),
                    Some(cell) => {
                        let sym = cell.contents();
                        text.push(sym.chars().next().unwrap_or(' '));
                    }
                    None => text.push(' '),
                }
            }
            crate::highlight::line_colors(&text)
        } else {
            Vec::new()
        };
        for c in 0..cols {
            let x = area.x + c;
            let y = area.y + r;
            let Some(cell) = screen.cell(r, c) else {
                buf[(x, y)].set_symbol(" ");
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut symbol = cell.contents();
            if symbol.is_empty() {
                symbol = " ";
            }
            let mut style = Style::default();
            if let Some(color) = map_color(cell.fgcolor()) {
                style = style.fg(color);
            }
            if let Some(color) = map_color(cell.bgcolor()) {
                style = style.bg(color);
            }
            // 高亮只覆盖"默认前景色"的单元格，不干扰程序自己发的 ANSI 颜色
            if matches!(cell.fgcolor(), vt100::Color::Default) {
                if let Some(Some(color)) = colors.get(c as usize) {
                    style = style.fg(*color);
                }
            }
            let mut mods = Modifier::empty();
            if cell.bold() {
                mods |= Modifier::BOLD;
            }
            if cell.dim() {
                mods |= Modifier::DIM;
            }
            if cell.italic() {
                mods |= Modifier::ITALIC;
            }
            if cell.underline() {
                mods |= Modifier::UNDERLINED;
            }
            if cell.inverse() {
                mods |= Modifier::REVERSED;
            }
            style = style.add_modifier(mods);
            if let Some(((x0, y0), (x1, y1))) = sel {
                if x >= x0 && x <= x1 && y >= y0 && y <= y1 {
                    style = style.add_modifier(Modifier::REVERSED);
                }
            }
            buf[(x, y)].set_symbol(symbol).set_style(style);
        }
    }
    // 搜索命中行整行压暗加粗：跳到哪一行必须一眼看得见
    if let Some(search) = &app.slots[app.active].search {
        if search.view_row < rows {
            buf.set_style(
                Rect {
                    x: area.x,
                    y: area.y + search.view_row,
                    width: area.width,
                    height: 1,
                },
                select_style(),
            );
        }
    }
    // 回看历史时隐藏光标：光标属于实时视图，不应出现在 scrollback 画面上。
    if s.scroll == 0 {
        let (cy, cx) = screen.cursor_position();
        if screen.hide_cursor() {
            f.set_cursor_position((area.right().saturating_sub(1), area.bottom().saturating_sub(1)));
        } else if cy < rows && cx < cols {
            f.set_cursor_position((area.x + cx, area.y + cy));
        }
    }
    // 底部不再叠加状态浮层：状态统一显示在顶部第二行，避免三处重复。
    if footer > 0 {
        draw_metrics_bar(f, app, chunks[2]);
    }
    // 弹窗最后渲染：内嵌终端的逐格写入会覆盖先画的浮层
    if app.settings_open {
        draw_settings_stack(f, app);
    }
    if app.transfer_popup {
        draw_transfer_popup(f, app);
    }
}

/// 底部指标行的一格：标签 + 百分比 + 条 + 可选说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricCell {
    pub label: &'static str,
    /// `None` 画 `—`：这台机器没给这项数据（或这一轮还没跑完）。
    /// 折成 0% 会变成"这台很空闲"的假象，那比空白更坏。
    pub percent: Option<u8>,
    pub detail: String,
}

/// 一行能给出的那几格。顺序固定：CPU、内存、磁盘。
///
/// 说明（`detail`）是这一格的附加值，装得下才写：CPU 那格带 1/5/15 分钟负载，
/// 磁盘那格带挂载点和已用/总量。
pub fn metric_cells(m: &crate::app::Metrics) -> Vec<MetricCell> {
    let disk = m.disk.as_ref();
    vec![
        MetricCell {
            label: "CPU",
            percent: m.cpu,
            detail: m
                .load
                .map(|l| inline(&format!("负载 {}", l.display())))
                .unwrap_or_default(),
        },
        MetricCell { label: "内存", percent: m.mem, detail: String::new() },
        MetricCell {
            label: "磁盘",
            percent: disk.map(|d| d.percent),
            detail: inline(
                &disk
                    .map(|d| {
                        format!(
                            "{} {}/{}",
                            d.mount,
                            human_size(d.used_kb * 1024),
                            human_size(d.total_kb * 1024)
                        )
                    })
                    .unwrap_or_default(),
            ),
        },
    ]
}

/// 把远端来的文本压成一行：挂载点名字里真可能有制表符和换行（`/proc/mounts` 里是
/// `\040\011\012` 这种转义，`unescape_mount` 会把它们还原成真的空白字符）。底部那
/// 一行只有一格高，控制字符既会把行撑断，也会把"画出来的宽度"和"算出来的宽度"岔开。
fn inline(raw: &str) -> String {
    raw.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// 一根条的填充格数：向上取整，1% 也要看得见一格。
fn bar_filled(percent: Option<u8>, width: usize) -> usize {
    match percent {
        Some(p) if width > 0 => ((p as usize * width).div_ceil(100)).clamp(0, width),
        _ => 0,
    }
}

/// 底部那排条的分段（文本 + 语气），绘制与单测共用同一份算法。
///
/// 三格**只由"标签 + 条 + 百分比"构成，三根条一样长**：说明（磁盘的挂载点、CPU 的
/// 负载）一律排在整行最后，不塞进各自那一格里。挂在一格后面，那一格就会比旁边宽出
/// 一截，用户第一眼看到的不是数字变了，而是"这根条怎么这么长"。
///
/// 宽度不够时按顺序降级：先把条缩短、缩到 0 就只剩"标签 百分比"，再把行尾的说明
/// 整句丢掉（先丢负载，再丢磁盘那句）。被挤掉的永远是装饰，不是数字。
pub fn metric_parts(m: &crate::app::Metrics, width: u16) -> Vec<(String, MetricTone)> {
    // 连上到第一轮回来之间一个数都没有：明说"在采"，别摆三条空槽看着像坏了
    if !m.has_data() {
        return vec![(" 远端主机指标：正在采集第一轮…".to_string(), MetricTone::Dim)];
    }
    let cells = metric_cells(m);
    let avail = width as usize;
    // 每格的固定开销：` 标签 `（两侧各一个空格）+ 百分比文本；分隔符 ` · ` 占 3 列。
    let text_fixed: usize = cells
        .iter()
        .map(|c| {
            let pct = c.percent.map(|p| p.to_string().len() + 1).unwrap_or(1);
            display_width(c.label) + 2 + pct
        })
        .sum::<usize>();
    // 连"标签 百分比"都摆不下时，分隔符先从 ` · ` 退成一个空格：
    // 挤掉的永远是装饰，不是数字
    let sep = if text_fixed + (cells.len() - 1) * 3 <= avail {
        " · "
    } else {
        " "
    };
    let fixed = text_fixed + (cells.len() - 1) * display_width(sep);
    // 每根条后面还要留一个空格才不贴住百分比，所以平分之后各让出一格。
    // 上限 12：再宽也只是好看，那点余量留给各格的说明更有用。
    let bar = (avail.saturating_sub(fixed) / cells.len())
        .saturating_sub(1)
        .min(12);
    // 说明共用同一段余量（条定完之后剩下的那些列），分配顺序从右往左：磁盘那句
    // "哪座挂载点、用了多少"最要紧，先给它，再到 CPU 那句负载。摆不下就整句不写 ——
    // 截半截的挂载点比没有更难读。排在行尾时的间距按实际算：第一句前面两个空格，
    // 之后每句前面一个 " · "，所以行宽不会超。
    let mut slack = avail.saturating_sub(
        fixed + cells.len() * if bar > 0 { bar + 1 } else { 0 },
    );
    let mut tail: Vec<&MetricCell> = Vec::new();
    for i in (0..cells.len()).rev() {
        let w = display_width(&cells[i].detail);
        let gap = if tail.is_empty() { 2 } else { 3 };
        if w > 0 && w + gap <= slack {
            slack -= w + gap;
            tail.push(&cells[i]);
        }
    }
    let mut parts: Vec<(String, MetricTone)> = Vec::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            parts.push((sep.to_string(), MetricTone::Dim));
        }
        parts.push((format!(" {} ", cell.label), MetricTone::Label));
        let tone = MetricTone::of(cell.percent);
        let pct = cell
            .percent
            .map(|p| format!("{p}%"))
            .unwrap_or_else(|| "—".to_string());
        if bar > 0 {
            let filled = bar_filled(cell.percent, bar);
            parts.push((
                format!("{}{}", "█".repeat(filled), "░".repeat(bar - filled)),
                tone.as_bar(),
            ));
            parts.push((" ".to_string(), MetricTone::Dim));
        }
        parts.push((pct, tone));
    }
    // 整行的装饰都在这之后：三根条已经摆完，等宽的那部分是用户读的数，剩下的才是话。
    for (n, cell) in tail.iter().enumerate() {
        let gap = if n == 0 { "  " } else { " · " };
        parts.push((format!("{gap}{}", cell.detail), MetricTone::Dim));
    }
    parts
}

/// 一格的配色：<70 正常、70~89 提醒、≥90 危险；没有数就是灰。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricTone {
    Label,
    Dim,
    Ok,
    Warn,
    Err,
    /// 进度条那一截的四档：字色和不带 `Bar` 的那一档一样，多垫一层自己的轨道底色。
    /// 条不能靠"色带透出来"画 —— 深色主题的色带是浅灰，`░` 网点透出来还是浅灰，
    /// 整根条就和背景一个亮度（mac 上默认不画底色所以看不出来，Windows 上一眼就穿）。
    BarDim,
    BarOk,
    BarWarn,
    BarErr,
}

impl MetricTone {
    fn of(percent: Option<u8>) -> MetricTone {
        match percent {
            Some(p) if p >= 90 => MetricTone::Err,
            Some(p) if p >= 70 => MetricTone::Warn,
            Some(_) => MetricTone::Ok,
            None => MetricTone::Dim,
        }
    }

    /// 同一档换成"条"的那一档。
    fn as_bar(self) -> MetricTone {
        match self {
            MetricTone::Ok => MetricTone::BarOk,
            MetricTone::Warn => MetricTone::BarWarn,
            MetricTone::Err => MetricTone::BarErr,
            _ => MetricTone::BarDim,
        }
    }

    fn color(self) -> Color {
        match self {
            MetricTone::Label => theme::alt(),
            MetricTone::Dim | MetricTone::BarDim => theme::dim(),
            MetricTone::Ok | MetricTone::BarOk => theme::ok(),
            MetricTone::Warn | MetricTone::BarWarn => theme::warn(),
            MetricTone::Err | MetricTone::BarErr => theme::err(),
        }
    }

    /// 样式单独抽成吃 `track` 的纯函数：主题是一份进程级全局状态，测试里改它会和
    /// 别的用例打架，所以这里把"用哪个轨道色"留在外面。
    fn style_with(self, track: Color) -> Style {
        match self {
            MetricTone::BarDim
            | MetricTone::BarOk
            | MetricTone::BarWarn
            | MetricTone::BarErr => Style::default().fg(self.color()).bg(track),
            tone => Style::default().fg(tone.color()),
        }
    }

    fn style(self) -> Style {
        self.style_with(theme::track())
    }
}

/// 会话页最底下那一行：远端主机的 CPU / 内存 / 磁盘。
/// 底色用色带，和远端输出明确分开 —— 这行是 ells 的装饰，不是屏幕上的文字。
fn draw_metrics_bar(f: &mut Frame, app: &App, area: Rect) {
    let slot = &app.slots[app.active];
    if slot.session.is_none() {
        return;
    }
    let line = Line::from(
        metric_parts(&slot.metrics, area.width)
            .into_iter()
            .map(|(text, tone)| Span::styled(text, tone.style()))
            .collect::<Vec<Span>>(),
    );
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme::band())),
        area,
    );
}

/// 会话顶部第 3 行的可点击区域：【设置】【上传】【下载】【列表】 + 聚合进度条。
/// 鼠标事件用它做命中测试，绘制用它摆位置，两者必须一致。
pub fn header_button_rects(area: Rect) -> [Rect; 5] {
    let y = area.y + 3;
    let btn = |x: u16| Rect { x: area.x + x, y, width: 8, height: 1 };
    let settings = btn(1);
    let upload = btn(10);
    let download = btn(19);
    let host_list = btn(28);
    // 四个按钮占到第 36 列，进度条只能从它右边开始算（窄屏也不能盖住【列表】）
    let w = (area.width.saturating_sub(40)).clamp(0, 44).max(1);
    let progress = Rect {
        x: (area.x + area.width)
            .saturating_sub(w + 2)
            .max(area.x.saturating_add(37)),
        y,
        width: w,
        height: 1,
    };
    [settings, upload, download, host_list, progress]
}

fn draw_header_buttons(f: &mut Frame, app: &App) {
    let area = f.area();
    let [settings, upload, download, host_list, progress] = header_button_rects(area);
    // 整行黑底，和上方提示行连成一块"标题栏"
    f.render_widget(
        Paragraph::new(Line::from("")).style(Style::default().bg(theme::bg())),
        Rect { x: area.x, y: settings.y, width: area.width, height: 1 },
    );
    // 实心青底黑字，视觉上像可点击的按钮
    let btn_style = Style::default()
        .fg(theme::on_accent())
        .bg(theme::accent_bg())
        .add_modifier(Modifier::BOLD);
    for (rect, label) in [(settings, "设置"), (upload, "上传"), (download, "下载"), (host_list, "列表")] {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(format!("【{label}】"), btn_style))),
            rect,
        );
    }

    let transfers = &app.slots[app.active].browser.transfers;
    let total = transfers.len();
    let done = transfers.iter().filter(|t| t.done).count();
    let failed = transfers.iter().filter(|t| t.error.is_some()).count();
    let active: Vec<_> = transfers.iter().filter(|t| !t.done && t.error.is_none()).collect();
    let counter = format!("传输进度 ({done}/{total})");
    if !active.is_empty() {
        let mut got: u64 = 0;
        let mut sum: Option<u64> = Some(0);
        for t in &active {
            match &t.progress {
                Some(p) => {
                    got += p.transferred;
                    sum = match (sum, p.total) {
                        (Some(a), Some(b)) => Some(a + b),
                        _ => None,
                    };
                }
                None => sum = None,
            }
        }
        let ratio = sum
            .filter(|tt| *tt > 0)
            .map(|tt| (got as f64 / tt as f64).clamp(0.0, 1.0))
            .unwrap_or(0.0);
        let pct = sum
            .map(|_| format!(" {:.0}%", ratio * 100.0))
            .unwrap_or_default();
        let label = format!("{counter}{pct}");
        f.render_widget(
            Gauge::default()
                .ratio(ratio)
                .label(label)
                .gauge_style(Style::default().fg(theme::warn()).bg(theme::bg())),
            progress,
        );
    } else if total > 0 && failed > 0 {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {counter} · {failed} 个失败 · 点击查看 "),
                Style::default().fg(theme::err()).bg(theme::bg()),
            ))),
            progress,
        );
    } else if total > 0 {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {counter} · 点击查看 "),
                Style::default().fg(theme::ok()).bg(theme::bg()),
            ))),
            progress,
        );
    }
}

/// 内嵌终端可视区（顶部标题栏之下、底部指标行之上的全部空间）。
/// 绘制与鼠标命中测试共用：`footer_rows` 传错就会让"点得到"和"看得到"差一行，
/// 那一行点上去像在终端里选了个空气。
pub fn session_emu_rect(area: Rect, footer_rows: u16) -> Rect {
    Rect {
        x: area.x,
        y: area.y + HEADER_ROWS,
        width: area.width,
        height: area.height.saturating_sub(HEADER_ROWS + footer_rows),
    }
}

/// 会话页顶部标题栏的行数：模式行 / 标签条 / 状态行 / 按钮行。
/// `SessionState::header_rows` 必须与它一致，否则远端画面会被裁掉一行。
pub const HEADER_ROWS: u16 = 4;
/// 标签条在标题栏里的行号（0 起）。第 0 行留给模式行和官网徽章。
pub const TAB_ROW: u16 = 1;
/// 标签名最多占多少列（CJK 按 2 列算），超出截断。
const TAB_TITLE_MAX: usize = 12;
/// 相邻标签之间的分隔条宽度。这一列不属于任何标签，点它不切标签。
const TAB_GAP: u16 = 1;
/// 标签最短列宽，保证单字符别名也点得中。
const TAB_MIN: u16 = 6;

/// 单个标签的列宽：` 1:标题 `，随标题实际长度走（不再定宽 14，两个标签时不会隔得老远）。
fn tab_cell_width(idx: usize, title: &str) -> u16 {
    let label = display_width(&clip_display(title, TAB_TITLE_MAX)) as u16;
    let num = idx.to_string().len() as u16;
    (label + num + 3).max(TAB_MIN)
}

/// 标签条几何：绘制与鼠标命中必须用同一份定义。宽度从左边放不下时起就不再列出后面的标签。
pub fn tab_rects(area: Rect, titles: &[String]) -> Vec<(usize, Rect)> {
    let y = area.y + TAB_ROW;
    let mut x = area.x + 1;
    let mut out = Vec::with_capacity(titles.len());
    for (idx, title) in titles.iter().enumerate() {
        let width = tab_cell_width(idx, title);
        if x + width > area.x + area.width {
            break;
        }
        out.push((idx, Rect { x, y, width, height: 1 }));
        x += width + TAB_GAP;
    }
    out
}

/// 标签条末尾的「+」：新建标签。
pub fn tab_new_rect(area: Rect, titles: &[String]) -> Rect {
    let x = tab_rects(area, titles)
        .last()
        .map(|(_, r)| r.right() + TAB_GAP)
        .unwrap_or(area.x + 1);
    Rect {
        x: x.min(area.x + area.width.saturating_sub(3)),
        y: area.y + TAB_ROW,
        width: 3,
        height: 1,
    }
}

/// 标签条：当前标签实心高亮，后台标签灰底，超出宽度的标签不画（改后的功能键仍能循环）。
fn draw_tab_bar(
    f: &mut Frame,
    area: Rect,
    titles: &[String],
    active: usize,
    binds: &crate::keybinds::KeyBinds,
) {
    let y = area.y + TAB_ROW;
    f.render_widget(
        Paragraph::new(Line::from("")).style(Style::default().bg(theme::band())),
        Rect { x: area.x, y, width: area.width, height: 1 },
    );
    let rects = tab_rects(area, titles);
    for (i, (idx, rect)) in rects.iter().enumerate() {
        let title = clip_display(titles.get(*idx).map(String::as_str).unwrap_or(""), TAB_TITLE_MAX);
        let style = if *idx == active {
            Style::default().fg(theme::on_accent()).bg(theme::accent_bg()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::text()).bg(theme::bg())
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {}:{} ", idx + 1, title),
                style,
            ))),
            *rect,
        );
        // 分隔条画在两个标签之间那一列上：它不属于任何标签，所以窄标签也不会黏在一起
        if i + 1 < rects.len() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "│",
                    Style::default().fg(theme::rule()).bg(theme::band()),
                ))),
                Rect { x: rect.right(), y, width: TAB_GAP, height: 1 },
            );
        }
    }
    let plus = tab_new_rect(area, titles);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " + ",
            Style::default().fg(theme::accent()).bg(theme::band()),
        ))),
        plus,
    );
    let hint = format!(
        " {} 新建 · {}/{} 切换 · {} 关闭标签 ",
        binds.display(crate::keybinds::Action::NewTab),
        binds.display(crate::keybinds::Action::NextTab),
        binds.display(crate::keybinds::Action::PrevTab),
        binds.display(crate::keybinds::Action::CloseTab),
    );
    if display_width(&hint) + 2 <= area.width.saturating_sub(plus.right()) as usize {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(theme::muted()).bg(theme::band()),
            ))),
            Rect {
                x: plus.right() + 1,
                y,
                width: area.x + area.width - (plus.right() + 1),
                height: 1,
            },
        );
    }
}

/// 按显示宽度截断（CJK 两列），用于标签名。
fn clip_display(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = if is_wide(c) { 2 } else { 1 };
        if w + cw > width.saturating_sub(1) {
            out.push('…');
            break;
        }
        w += cw;
        out.push(c);
    }
    out
}

/// 设置弹窗面板。命中测试与绘制必须同源，加一行只改这一处。
fn settings_panel(area: Rect) -> Rect {
    centered(56, 14, area)
}

/// 设置弹窗的 12 个可点击行：高亮 / 保活 / 主密码开关 / 修改主密码 / 快捷键 / 主题 /
/// 检查更新 / 会话记录 / 主机指标 / 自动重连 / 保存 / 取消。必须与 draw_settings_overlay 的几何完全一致。
pub fn settings_hit_rects(area: Rect) -> [Rect; 12] {
    let p = settings_panel(area);
    let ix = p.x + 1;
    let iy = p.y + 1;
    let iw = p.width.saturating_sub(2);
    [
        Rect { x: ix, y: iy, width: iw, height: 1 },
        Rect { x: ix, y: iy + 1, width: iw, height: 1 },
        Rect { x: ix, y: iy + 2, width: iw, height: 1 },
        Rect { x: ix, y: iy + 3, width: iw, height: 1 },
        Rect { x: ix, y: iy + 4, width: iw, height: 1 },
        Rect { x: ix, y: iy + 5, width: iw, height: 1 },
        Rect { x: ix, y: iy + 6, width: iw, height: 1 },
        Rect { x: ix, y: iy + 7, width: iw, height: 1 },
        Rect { x: ix, y: iy + 8, width: iw, height: 1 },
        Rect { x: ix, y: iy + 9, width: iw, height: 1 },
        Rect { x: ix.saturating_add(iw / 2).saturating_sub(16), y: iy + 11, width: 14, height: 1 },
        Rect { x: ix.saturating_add(iw / 2).saturating_add(2), y: iy + 11, width: 14, height: 1 },
    ]
}

/// 快捷键面板的可点击行：每个动作一行 + 恢复默认 + 返回设置。
/// 必须与 draw_keybinds_overlay 的几何完全一致（行数是 Action::ALL 推出来的）。
pub fn keybinds_hit_rects(area: Rect) -> [Rect; crate::keybinds::Action::ALL.len() + 2] {
    use crate::keybinds::Action;
    // 数组长度必须是常量表达式，所以这里写 Action::ALL.len()，运行时索引用 n
    const N: usize = Action::ALL.len();
    let n = N;
    let p = centered(60, n as u16 + 7, area);
    let ix = p.x + 1;
    let iy = p.y + 1;
    let iw = p.width.saturating_sub(2);
    let mut rects = [Rect::ZERO; N + 2];
    for (idx, slot) in rects.iter_mut().enumerate().take(n) {
        *slot = Rect { x: ix, y: iy + idx as u16, width: iw, height: 1 };
    }
    // 行 n=操作提示、n+1=本机提示、n+2=结果行，n+3 才是两个按钮
    rects[n] = Rect { x: ix, y: iy + n as u16 + 3, width: 14, height: 1 };
    rects[n + 1] =
        Rect { x: ix.saturating_add(iw).saturating_sub(14), y: iy + n as u16 + 3, width: 14, height: 1 };
    rects
}

fn draw_settings_overlay(f: &mut Frame, app: &App) {
    let rects = settings_hit_rects(f.area());
    let panel = settings_panel(f.area());
    f.buffer_mut().set_style(panel, Style::default().bg(theme::bg()));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 全局设置 ")
        .title_style(Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD));
    f.render_widget(Clear, panel);
    f.render_widget(block, panel);
    let st = &app.settings;
    let focus = app.settings_focus;
    let row_base = |focused: bool| {
        let mut s = Style::default().fg(theme::text()).bg(theme::bg());
        if focused {
            s = s.add_modifier(Modifier::REVERSED);
        }
        s
    };
    // 行 0：高亮开关
    let hl_color = if st.highlight { theme::ok() } else { theme::err() };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" 终端输出高亮（docker / 日志级别）：", row_base(focus == 0)),
            Span::styled(
                if st.highlight { " 开 " } else { " 关 " },
                row_base(focus == 0).fg(hl_color).add_modifier(Modifier::BOLD),
            ),
        ])),
        rects[0],
    );
    // 行 1：保活间隔
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" SSH 空闲保活间隔：", row_base(focus == 1)),
            Span::styled(
                format!(" {} 秒 ", st.keepalive_secs),
                row_base(focus == 1)
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("（15/30/60/120/300）", row_base(focus == 1).fg(theme::dim())),
        ])),
        rects[1],
    );
    // 行 2：主密码保护开关
    let mp_color = if st.master_password_enabled { theme::ok() } else { theme::warn() };
    let mp_note = if st.master_password_enabled {
        " 开 · 启动需输入主密码 "
    } else {
        " 关 · 启动自动解锁（凭据存本地） "
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" 主密码保护：", row_base(focus == 2)),
            Span::styled(
                mp_note,
                row_base(focus == 2).fg(mp_color).add_modifier(Modifier::BOLD),
            ),
        ])),
        rects[2],
    );
    // 行 3：修改主密码（含输入态）
    let line3 = match app.mp_stage {
        crate::app::MpStage::Idle => Line::from(vec![
            Span::styled(" 修改主密码：", row_base(focus == 3)),
            Span::styled(
                if app.master_secret.is_some() {
                    " Enter/点击 开始输入 "
                } else {
                    " 开发模式无主密码 "
                },
                row_base(focus == 3).fg(theme::dim()),
            ),
        ]),
        _ => {
            let masked: String = app.mp_buf.chars().map(|_| '•').collect();
            let prompt = if app.mp_stage == crate::app::MpStage::First {
                " 新主密码："
            } else {
                " 再次输入确认："
            };
            let tail = if app.mp_busy {
                " 重新加密中…"
            } else {
                " Enter 继续 · Esc 取消"
            };
            Line::from(vec![
                Span::styled(prompt, row_base(true)),
                Span::styled(masked, row_base(true).fg(theme::warn())),
                Span::styled(tail, row_base(false).fg(theme::dim())),
            ])
        }
    };
    f.render_widget(Paragraph::new(line3), rects[3]);
    // 行 4：快捷键设置子面板入口
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" 快捷键设置：", row_base(focus == 4)),
            Span::styled(
                format!(
                    " {} 项可改 · Enter/点击 打开 ",
                    crate::keybinds::Action::ALL.len()
                ),
                row_base(focus == 4).fg(theme::dim()),
            ),
        ])),
        rects[4],
    );
    // 行 5：界面主题（改完立即预览，保存才落盘）
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" 界面主题：", row_base(focus == 5)),
            Span::styled(
                format!(" {} ", st.theme.label()),
                row_base(focus == 5).fg(theme::accent()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "←→/Enter 切换",
                row_base(focus == 5).fg(theme::dim()),
            ),
        ])),
        rects[5],
    );
    // 行 6：自动更新开关 + 检查结果（Enter 立即检查，有更新则进确认）
    let checked = app
        .update
        .checked_at
        .map(crate::update::age_label)
        .unwrap_or_else(|| "从未".to_string());
    let update_note = if app.update.checking {
        " 正在检查… ".to_string()
    } else if let Some(tag) = app.update.latest.as_deref() {
        format!(" 最新 {tag} · Enter 更新 ")
    } else if app.update.error.is_some() {
        " 检查失败（见下方） ".to_string()
    } else {
        format!(" 已是最新 · 上次检查 {checked} ")
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" 自动更新：", row_base(focus == 6)),
            Span::styled(
                if st.auto_update { " 开 " } else { " 关 " },
                row_base(focus == 6)
                    .fg(if st.auto_update { theme::ok() } else { theme::dim() })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(update_note, row_base(focus == 6).fg(theme::accent())),
        ])),
        rects[6],
    );
    // 行 7：会话记录开关（终端输出落盘到 ~/.ells/logs，列表页 v 查看）
    let log_color = if st.session_log { theme::ok() } else { theme::dim() };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" 会话记录（终端输出落盘）：", row_base(focus == 7)),
            Span::styled(
                if st.session_log { " 开 " } else { " 关 " },
                row_base(focus == 7).fg(log_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " ~/.ells/logs · 单份 8 MiB · 留 30 天 ",
                row_base(focus == 7).fg(theme::dim()),
            ),
        ])),
        rects[7],
    );
    // 行 8：会话页底部那排远端主机指标（关掉就把这一行还给终端）
    let mt_color = if st.metrics { theme::ok() } else { theme::dim() };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" 主机指标（CPU/内存/磁盘）：", row_base(focus == 8)),
            Span::styled(
                if st.metrics { " 开 " } else { " 关 " },
                row_base(focus == 8).fg(mt_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " 5 秒一轮 · 磁盘跟单 · 60 秒普查 · 同一条连接 ",
                row_base(focus == 8).fg(theme::dim()),
            ),
        ])),
        rects[8],
    );
    // 行 9：会话断线后的自动重连次数（退避参数在 settings.ini 里配）
    let rc_color = if st.reconnect.max_attempts == 0 { theme::dim() } else { theme::ok() };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" 自动重连（会话断开后）：", row_base(focus == 9)),
            Span::styled(
                format!(" {} ", st.reconnect.attempts_label()),
                row_base(focus == 9).fg(rc_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " 次 · 1s 起指数退避 · 细调改 ini ",
                row_base(focus == 9).fg(theme::dim()),
            ),
        ])),
        rects[9],
    );
    // 行 10：操作提示（更新检查失败时这行让给它，长文案才放得下）
    let hint = match &app.update.error {
        Some(err) => format!(" 检查更新失败：{err}"),
        None => " ↑↓ 选择 · Enter/点击 修改 · ←→ 微调 · Esc 取消".to_string(),
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default()
                .fg(if app.update.error.is_some() { theme::warn() } else { theme::dim() })
                .bg(theme::bg()),
        ))),
        Rect { x: rects[0].x, y: rects[0].y + 10, width: rects[0].width, height: 1 },
    );
    // 行 11：保存 / 取消按钮
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "【 保 存 】",
            Style::default()
                .fg(theme::on_accent())
                .bg(theme::accent_bg())
                .add_modifier(if focus == 10 { Modifier::BOLD } else { Modifier::empty() })
                .add_modifier(if focus == 10 { Modifier::UNDERLINED } else { Modifier::empty() }),
        ))),
        rects[10],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "【 取 消 】",
            Style::default()
                .fg(theme::text())
                .bg(theme::band())
                .add_modifier(if focus == 11 { Modifier::BOLD } else { Modifier::empty() })
                .add_modifier(if focus == 11 { Modifier::UNDERLINED } else { Modifier::empty() }),
        ))),
        rects[11],
    );
}

fn draw_keybinds_overlay(f: &mut Frame, app: &App) {
    let n = crate::keybinds::Action::ALL.len();
    let rects = keybinds_hit_rects(f.area());
    let panel = centered(60, n as u16 + 7, f.area());
    f.buffer_mut().set_style(panel, Style::default().bg(theme::bg()));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 快捷键设置 ")
        .title_style(Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD));
    f.render_widget(Clear, panel);
    f.render_widget(block, panel);
    let binds = &app.settings.keybinds;
    let row_base = |focused: bool| {
        let mut s = Style::default().fg(theme::text()).bg(theme::bg());
        if focused {
            s = s.add_modifier(Modifier::REVERSED);
        }
        s
    };
    for (idx, action) in crate::keybinds::Action::ALL.iter().enumerate() {
        let focused = app.keybinds_focus == idx;
        let recording = app.keybinds_recording == Some(*action);
        let badge = if recording {
            " 请按下新按键 · Esc 取消 ".to_string()
        } else {
            format!(" {} ", binds.display(*action))
        };
        let badge_style = if recording {
            row_base(focused).fg(theme::warn()).add_modifier(Modifier::BOLD)
        } else {
            row_base(focused).fg(theme::accent()).add_modifier(Modifier::BOLD)
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {}", pad_display(action.label(), 18)), row_base(focused)),
                Span::styled(badge, badge_style),
            ])),
            rects[idx],
        );
    }
    // 操作提示 + 本机注意点 + 一次性结果行
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑↓ 选择 · Enter/点击 改键 · 只能绑 F2–F9 或 Ctrl/Alt 组合键",
            Style::default().fg(theme::dim()).bg(theme::bg()),
        ))),
        Rect { x: rects[0].x, y: rects[0].y + n as u16, width: rects[0].width, height: 1 },
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", clip_display(crate::keybinds::platform_note(), rects[0].width as usize - 2)),
            Style::default().fg(theme::warn()).bg(theme::bg()),
        ))),
        Rect { x: rects[0].x, y: rects[0].y + n as u16 + 1, width: rects[0].width, height: 1 },
    );
    let msg = match &app.keybinds_msg {
        Some(text) => text.clone(),
        None => " 改键即时生效并保存 · 与已占用的键会自动互换".to_string(),
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", clip_display(&msg, rects[0].width as usize - 2)),
            Style::default()
                .fg(if app.keybinds_msg.is_some() { theme::ok() } else { theme::dim() })
                .bg(theme::bg()),
        ))),
        Rect { x: rects[0].x, y: rects[0].y + n as u16 + 2, width: rects[0].width, height: 1 },
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "【恢复默认】",
            Style::default()
                .fg(theme::text())
                .bg(theme::band())
                .add_modifier(if app.keybinds_focus == n { Modifier::BOLD } else { Modifier::empty() }),
        ))),
        rects[n],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "【 返 回 】",
            Style::default()
                .fg(theme::on_accent())
                .bg(theme::accent_bg())
                .add_modifier(if app.keybinds_focus == n + 1 { Modifier::BOLD } else { Modifier::empty() }),
        ))),
        rects[n + 1],
    );
}

/// 设置弹窗 + 快捷键子面板：子面板要盖在设置之上，所以必须在两者都开时后画。
pub fn draw_settings_stack(f: &mut Frame, app: &App) {
    if !app.settings_open {
        return;
    }
    draw_settings_overlay(f, app);
    if app.keybinds_open {
        draw_keybinds_overlay(f, app);
    }
}

fn draw_transfer_popup(f: &mut Frame, app: &App) {
    let items: Vec<_> = app.slots[app.active].browser.transfers.iter().rev().take(8).collect();
    let height = ((items.len() as u16) * 2 + 3)
        .min(f.area().height.saturating_sub(2))
        .max(5);
    let area = centered(64, height, f.area());
    f.buffer_mut().set_style(area, Style::default().bg(theme::bg()));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 传输详情 · 点击或按键关闭 ")
        .title_style(Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    for (idx, item) in items.iter().enumerate() {
        let row = inner.y + (idx as u16) * 2;
        if row + 1 >= inner.y + inner.height {
            break;
        }
        let status = if let Some(e) = &item.error {
            format!("失败: {e}")
        } else if item.done {
            "完成".to_string()
        } else {
            match &item.progress {
                Some(p) => format!(
                    "{}/{} · {:.0}KB/s",
                    human_size(p.transferred),
                    p.total.map(human_size).unwrap_or_else(|| "?".into()),
                    p.bytes_per_sec / 1024.0
                ),
                None => "准备中…".to_string(),
            }
        };
        let color = if item.error.is_some() {
            theme::err()
        } else if item.done {
            theme::ok()
        } else {
            theme::warn()
        };
        let text = Rect { x: inner.x, y: row, width: inner.width, height: 1 };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {} {} · {status}", item.direction, item.label),
                Style::default().fg(color),
            ))),
            text,
        );
        let bar = Rect {
            x: inner.x + 1,
            y: row + 1,
            width: inner.width.saturating_sub(2),
            height: 1,
        };
        let ratio = item
            .progress
            .as_ref()
            .and_then(|p| p.ratio())
            .unwrap_or(if item.done { 1.0 } else { 0.0 })
            .clamp(0.0, 1.0);
        f.render_widget(
            Gauge::default()
                .ratio(ratio)
                .gauge_style(Style::default().fg(color).bg(theme::bg())),
            bar,
        );
    }
}

/// 更新弹窗面板（进度/结果 + 按钮），绘制与命中测试同源。
pub fn update_popup_rect(area: Rect) -> Rect {
    centered(56, 7, area)
}

/// 更新弹窗的两个按钮：下载中只有第一个（【取消下载】），
/// 待重启时左【立即重启】右【稍后】。
pub fn update_button_rects(area: Rect) -> [Rect; 2] {
    let p = update_popup_rect(area);
    let ix = p.x + 1;
    let iw = p.width.saturating_sub(2);
    let y = p.y + p.height.saturating_sub(2);
    [
        Rect { x: ix.saturating_add(iw / 2).saturating_sub(16), y, width: 16, height: 1 },
        Rect { x: ix.saturating_add(iw / 2).saturating_add(2), y, width: 16, height: 1 },
    ]
}

fn draw_update_popup(f: &mut Frame, app: &App) {
    let area = update_popup_rect(f.area());
    let buttons = update_button_rects(f.area());
    let u = &app.update;
    let (title, head, note, ratio, button) = if u.downloading {
        let asset = crate::update::asset_name().unwrap_or("更新包");
        let r = ratio_of(u.transferred, u.total);
        (
            format!(" 正在更新 · {asset} "),
            format!(
                " 已下载 {}{}（{}%）",
                human_size(u.transferred),
                u.total
                    .map(|t| format!(" / {}", human_size(t)))
                    .unwrap_or_default(),
                (r * 100.0) as u8
            ),
            " 校验通过才会替换文件，中途可取消。".to_string(),
            r,
            "【 取消下载 】",
        )
    } else {
        let tag = u.applied_tag.clone().unwrap_or_default();
        let live = app.slots.iter().filter(|s| s.session.is_some()).count();
        (
            format!(" 更新完成 · {tag} "),
            format!(
                " 新版本已就位，当前进程仍是 v{}",
                crate::update::current_version()
            ),
            if live > 0 {
                format!(" 重启会断开这 {live} 路会话；也可以先退出再运行 ells。")
            } else {
                " 重启后即为新版本；也可以先退出再运行 ells。".to_string()
            },
            1.0,
            "【 立即重启 】",
        )
    };
    f.buffer_mut().set_style(area, Style::default().bg(theme::bg()));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD));
    f.render_widget(Clear, area);
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(head, Style::default().fg(theme::text())))),
        Rect { x: inner.x, y: inner.y, width: inner.width, height: 1 },
    );
    f.render_widget(
        Gauge::default()
            .ratio(ratio.clamp(0.0, 1.0))
            .gauge_style(Style::default().fg(theme::accent()).bg(theme::band())),
        Rect { x: inner.x, y: inner.y + 1, width: inner.width, height: 1 },
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(note, Style::default().fg(theme::dim())))),
        Rect { x: inner.x, y: inner.y + 2, width: inner.width, height: 1 },
    );
    let style = |main: bool| {
        Style::default()
            .fg(if main { theme::on_accent() } else { theme::text() })
            .bg(if main { theme::accent_bg() } else { theme::band() })
            .add_modifier(Modifier::BOLD)
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(button, style(true)))),
        buttons[0],
    );
    if !u.downloading {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("【 稍 后 】", style(false)))),
            buttons[1],
        );
    }
}

/// 进度比例：total 未知时按 0 画（Gauge 全空，至少不是假完成）。
fn ratio_of(done: u64, total: Option<u64>) -> f64 {
    match total {
        Some(t) if t > 0 => done as f64 / t as f64,
        _ => 0.0,
    }
}

fn map_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(n) => Some(Color::Indexed(n)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tabs_hug_their_titles_with_one_column_between() {
        let area = Rect::new(0, 0, 80, 24);
        let titles = ts(&["web", "db-server"]);
        let rects = tab_rects(area, &titles);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].1.width, tab_cell_width(0, "web"));
        // 关键就是这一列间隙：过去固定 14 列时两个短标签中间空了 9 列
        assert_eq!(rects[1].1.x - rects[0].1.right(), TAB_GAP);
        assert!(rects[0].1.width < TAB_TITLE_MAX as u16);
        // 「+」紧跟在最后一个标签的分隔条之后
        assert_eq!(tab_new_rect(area, &titles).x, rects[1].1.right() + TAB_GAP);
    }

    #[test]
    fn cell_width_never_clips_what_it_draws() {
        // 列宽算小了文字会被裁掉；只允许比文字宽（短名字靠 TAB_MIN 兜底多留空格）
        for title in ["web", "", "生产数据库服务器", &"x".repeat(40)] {
            let label = clip_display(title, TAB_TITLE_MAX);
            let drawn = display_width(&format!(" 1:{label} ")) as u16;
            assert!(
                tab_cell_width(0, title) >= drawn,
                "「{title}」列宽 {} 装不下画出的 {} 列",
                tab_cell_width(0, title),
                drawn,
            );
            if drawn >= TAB_MIN {
                assert_eq!(tab_cell_width(0, title), drawn, "「{title}」列宽要贴着文字走");
            }
        }
        // 再长的名字也封顶，不会一路把后面的标签挤下屏
        assert_eq!(tab_cell_width(0, &"x".repeat(40)), tab_cell_width(0, &"y".repeat(300)));
    }

    #[test]
    fn narrow_terms_keep_the_gap_and_drop_the_tail() {
        let titles = ts(&["alpha", "bravo", "charlie"]);
        let rects = tab_rects(Rect::new(0, 0, 24, 24), &titles);
        assert_eq!(rects.len(), 2, "第三个放不下就必须整个丢掉");
        for pair in rects.windows(2) {
            assert_eq!(pair[1].1.x - pair[0].1.right(), TAB_GAP);
        }
        assert!(rects.last().unwrap().1.right() <= 24);
        assert!(tab_rects(Rect::new(0, 0, 8, 24), &titles).len() < titles.len());
    }

    #[test]
    fn settings_rows_stay_in_order_with_the_buttons_last() {
        let area = Rect::new(0, 0, 80, 24);
        let panel = settings_panel(area);
        let r = settings_hit_rects(area);
        // 10 个可编辑项必须是连续的 10 行（… 会话记录 / 主机指标 / 自动重连）：
        // 加一行时鼠标命中不能错位，画的那一行也不能点到别的行为
        for i in 1..10 {
            assert_eq!(r[i].y, r[i - 1].y + 1, "第 {i} 行和上一行不挨着");
        }
        assert_eq!(r[9].y, panel.y + 10);
        // 第 11 行是操作提示（不可点），按钮在它下面同一行
        assert_eq!(r[10].y, r[11].y);
        assert_eq!(r[10].y, panel.y + 12);
        for (i, rect) in r.iter().enumerate() {
            assert!(rect.right() <= panel.right(), "第 {i} 行超出面板右边界");
            assert!(rect.bottom() <= panel.bottom(), "第 {i} 行超出面板下边界");
        }
    }

    #[test]
    fn update_badge_never_overlaps_the_homepage_one() {
        let label = "【v0.1.6 可更新】";
        for area in [Rect::new(0, 0, 80, 24), Rect::new(0, 0, 40, 20), Rect::new(0, 0, 18, 10)] {
            let badge = update_badge_rect(area, label);
            assert_eq!(badge.y, area.y, "徽标必须留在第 0 行");
            assert!(badge.height == 1);
            assert!(
                badge.right() <= homepage_rect(area).x,
                "{area:?} 下徽标压到官网徽章了：{:?} vs {:?}",
                badge,
                homepage_rect(area)
            );
            assert!(badge.x >= area.x, "{area:?} 下徽标跑出了左边界");
        }
        // 宽度跟着文案走（CJK 括号各算 2 列），命中测试才画得准
        assert_eq!(
            update_badge_rect(Rect::new(0, 0, 80, 24), label).width,
            display_width(label) as u16
        );
    }

    /// 「昨天」跨的是自然日还是 24 小时都行，但档位边界必须写死在测试里：
    /// 相对时间一错，用户就会把三年没动的机器当成刚用过的。
    #[test]
    fn recency_steps_are_monotonic() {
        let now = 1_700_000_000;
        assert_eq!(last_seen(now, 0), "从未");
        assert_eq!(last_seen(now, now - 30), "刚刚");
        assert_eq!(last_seen(now, now - 5 * 60), "5分");
        assert_eq!(last_seen(now, now - 3 * 3600), "3小时");
        assert_eq!(last_seen(now, now - 30 * 3600), "昨天");
        assert_eq!(last_seen(now, now - 5 * 86_400), "5天");
        assert_eq!(last_seen(now, now - 70 * 86_400), "2月");
        // 时钟被调回过去（last_connected 在未来）不许 panic，也不许说成"从未"
        assert_eq!(last_seen(now, now + 999), "刚刚");
    }

    /// 命中的字符要单独成段，好让颜色贴得上字；不命中时必须回到补满 14 列的整串。
    #[test]
    fn alias_runs_split_at_the_hits() {
        let joined = |spans: &[Span<'static>]| -> String {
            spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        };
        let hit = vec![0usize, 1];
        let spans = alias_spans("web", &hit);
        assert_eq!(joined(&spans).trim_end(), "web");
        assert_eq!(display_width(&joined(&spans)), ALIAS_COL, "别名列要补到固定宽度");
        // 汉字别名：按字符下标切分，不能把一个汉字拆成两半
        let cn = alias_spans("生产库", &[1]);
        assert_eq!(joined(&cn).trim_end(), "生产库");
        assert!(cn.len() >= 2, "命中段必须被拆开：{} 段", cn.len());
        // 一个字符都没命中（比如靠备注命中的）就照原样画，不多切段
        assert_eq!(joined(&alias_spans("web", &[])), format!("{:<14}", "web"));
    }

    #[test]
    fn update_popup_buttons_stay_inside_the_panel() {
        let area = Rect::new(0, 0, 80, 24);
        let panel = update_popup_rect(area);
        let [main, alt] = update_button_rects(area);
        for (i, r) in [main, alt].iter().enumerate() {
            assert!(r.right() <= panel.right(), "按钮 {i} 超出右边界");
            assert!(r.bottom() <= panel.bottom(), "按钮 {i} 超出下边界");
            assert!(r.x >= panel.x + 1);
        }
        // 两个按钮不许重叠，也没人能把它们和面板边框画到同一格
        assert!(main.right() < alt.x);
    }

    /// 规则表格的几何是绘制和鼠标命中的唯一来源，所以它必须自己保证：
    /// 格子不许压边框、选中行永远在窗口里（否则滚到底就点不着、看不见光标）、列不许互相盖住。
    /// 窄终端上这三条最容易破——面板被 `centered` 缩过之后，列宽总和比面板还宽。
    #[test]
    fn rule_table_cells_stay_in_panel_and_keep_the_selected_row_visible() {
        for (w, h) in [(120u16, 40u16), (80, 24), (64, 20), (40, 12), (20, 6), (6, 4)] {
            let area = Rect::new(0, 0, w, h);
            for rows in [0usize, 1, 8, 30] {
                let mut selected: Vec<usize> = (0..rows).step_by(3).collect();
                if rows > 0 {
                    selected.push(rows - 1);
                }
                for sel in selected {
                    let (panel, window) = rules_cell_rects(area, rows, sel);
                    let inner = Block::bordered().inner(panel);
                    // 面板连一行数据都放不下时（4 行高的终端）只能是空窗口，不许硬画到边框上
                    if inner.height < 2 {
                        assert!(window.is_empty(), "{w}x{h} / {rows} 行放不下还画了 {:?}", window.len());
                        continue;
                    }
                    assert!(
                        window.iter().any(|(i, _)| *i == sel),
                        "{w}x{h} 终端、{rows} 行、选中第 {sel} 行却不在窗口里：{:?}",
                        window.iter().map(|(i, _)| *i).collect::<Vec<_>>()
                    );
                    let mut last_i: Option<usize> = None;
                    for (i, cols) in &window {
                        assert_eq!(cols.len(), RULE_COLS.len(), "第 {i} 行的列数对不上列名");
                        assert!(last_i.is_none_or(|p| *i > p), "行号必须递增且不重复");
                        last_i = Some(*i);
                        let mut right: Option<u16> = None;
                        for (c, r) in cols.iter().enumerate() {
                            assert!(r.x >= inner.x, "第 {i} 行第 {c} 格贴到左边框上了");
                            assert!(r.right() <= inner.x + inner.width, "第 {i} 行第 {c} 格出了右边框");
                            assert!(
                                r.y >= inner.y + 1 && r.bottom() <= inner.y + inner.height,
                                "第 {i} 行画到表头或下边框上：y={}",
                                r.y
                            );
                            // 窄面板上尾部列会被压成 0 宽（点不到，也画不出字），只要求不许互相压过去
                            assert!(right.is_none_or(|p| r.x >= p), "第 {c} 格压到了前一格身上");
                            right = Some(r.right());
                        }
                    }
                }
            }
        }
    }

    /// 面板够宽时列宽就该是设计值：把 22 列的目标主机压成 8 列，用户敲的字全在裁掉的后面。
    #[test]
    fn rule_table_uses_full_column_widths_on_a_normal_terminal() {
        let (_, window) = rules_cell_rects(Rect::new(0, 0, 100, 30), 4, 0);
        for (_, cols) in &window {
            for (c, r) in cols.iter().enumerate() {
                assert_eq!(r.width, RULE_W[c] as u16, "第 {c} 列宽度被人改了");
            }
            // 相邻列之间正好空一格，表头和 ⚠ 记号都靠这一列站位
            for pair in cols.windows(2) {
                assert_eq!(pair[1].x - pair[0].right(), 1);
            }
        }
    }

    /// 隧道面板顶部的"端口映射"按钮和端口弹窗都必须落在各自面板内：
    /// 命中测试拿的是同一份矩形，跑出边框就等于点开了一个看不见的弹窗。
    #[test]
    fn port_mapping_chip_and_popup_fit_their_frames() {
        for (w, h) in [(120u16, 40u16), (80, 24), (64, 20), (40, 12), (20, 8)] {
            let area = Rect::new(0, 0, w, h);
            let (panel, chip_row, _list) = tunnel_rects(area);
            let chip = tunnel_ports_chip_rect(area);
            assert_eq!(chip.y, chip_row.y, "按钮不在顶部那一行");
            assert_eq!(chip.height, 1);
            assert!(chip.right() <= panel.right(), "{w}x{h} 下按钮出了面板右边");
            assert!(chip.x >= chip_row.x, "{w}x{h} 下按钮跑到了面板左侧外面");
            let popup = ports_rect(area);
            assert!(popup.right() <= area.right() && popup.bottom() <= area.bottom());
        }
    }

    /// 两轮真实探针，走的是和运行时完全相同的路（`Probe::parse` → `adopt`）：
    /// `Metrics` 的记账字段是私有的，测试自己拼数字就等于测了个假对象。
    /// ROUND1 是基线，CPU 要到 ROUND2 才做得出差：总共走 100 个 jiffies、58 个在闲 ⇒ 42%。
    const M_ROUND1: &str = concat!(
        "ellsm1\n",
        "cpu  100 0 50 800 50 0 0 0\n",
        "MemTotal: 1000 kB\n",
        "MemAvailable: 390 kB\n",
        "0.42 0.31 0.19 1/234 5678\n",
        "/dev/sda1 2000 1760 240 88% /data\n",
    );
    const M_ROUND2: &str = concat!(
        "ellsm1\n",
        "cpu  140 0 52 850 58 0 0 0\n",
        "MemTotal: 1000 kB\n",
        "MemAvailable: 390 kB\n",
        "0.42 0.31 0.19 1/234 5678\n",
        "/dev/sda1 2000 1760 240 88% /data\n",
    );

    fn filled_metrics() -> crate::app::Metrics {
        let mut m = crate::app::Metrics::default();
        m.adopt(&ells_core::Probe::parse(M_ROUND1));
        m.adopt(&ells_core::Probe::parse(M_ROUND2));
        m
    }

    fn metric_line(m: &crate::app::Metrics, width: u16) -> String {
        metric_parts(m, width).into_iter().map(|(t, _)| t).collect()
    }

    /// 把整行切成"三格 + 行尾装饰"。用来钉死一件事：格子里只允许有标签、条和百分比，
    /// 说明一律在行尾 —— 说明一塞回某一格后面，那一格就比旁边的格宽出一截，用户第一眼
    /// 看到的是"这根条怎么长一点"，而不是数变了没有。
    fn metric_groups(m: &crate::app::Metrics, width: u16) -> (Vec<String>, String) {
        let mut groups: Vec<String> = Vec::new();
        let mut tail = String::new();
        let mut in_tail = false;
        let mut after_sep = true;
        for (text, _) in metric_parts(m, width) {
            // 行尾装饰的第一个 span 以两个空格开头，从它开始后面全算装饰
            if text.starts_with("  ") {
                in_tail = true;
            }
            if in_tail {
                tail.push_str(&text);
            } else if text == " · " {
                after_sep = true;
            } else if after_sep || groups.is_empty() {
                groups.push(text.clone());
                after_sep = false;
            } else {
                groups.last_mut().expect("还没有任何一组").push_str(&text);
            }
        }
        (groups, tail)
    }

    /// 底部那一行是画的、也是命中测试算可视区时用的同一个宽度：
    /// 文字超出这一行，ratatui 会把最右边的数字裁掉，磁盘那个数首当其冲。
    #[test]
    fn the_metric_bar_never_overflows_its_row() {
        // 31 列是"标签 + 三个百分比"摆得下的下限，再窄就已经装不下数字本身了
        let m = filled_metrics();
        for width in 31u16..=200 {
            let drawn = display_width(&metric_line(&m, width));
            assert!(drawn <= width as usize, "{width} 列下画出 {drawn} 列");
        }
    }

    /// 用户要的就是百分比：三个数一个都不能在降级里丢掉。
    #[test]
    fn the_metric_bar_shows_the_measured_percentages() {
        let m = filled_metrics();
        assert_eq!((m.cpu, m.mem, m.disk.as_ref().map(|d| d.percent)), (Some(42), Some(61), Some(88)));
        let line = metric_line(&m, 120);
        for pct in ["42%", "61%", "88%"] {
            assert!(line.contains(pct), "「{line}」里没有 {pct}");
        }
        // 宽屏上条要真的画出来（12 格封顶；`█`/`░` 各占 3 字节，按字符数才准）
        assert!(metric_parts(&m, 120)
            .iter()
            .any(|(t, _)| t.chars().count() == 12 && t.contains('█')));
    }

    /// 宽屏上负载要出现，而且排在整行最后 —— 它说的是"有多少活儿在排队"，是这一行的
    /// 装饰，不该把 CPU 那一格撑宽。
    #[test]
    fn the_load_trails_the_row_instead_of_widening_cpu() {
        let m = filled_metrics();
        assert_eq!(metric_cells(&m)[0].detail, "负载 0.42/0.31/0.19");
        let wide = metric_line(&m, 120);
        assert!(wide.contains("负载 0.42/0.31/0.19"), "「{wide}」");
        assert!(
            wide.find("负载").unwrap() > wide.find("磁盘").unwrap(),
            "「{wide}」负载还挂在 CPU 那一格里"
        );
        // 三格只由"标签 + 条 + 百分比"构成，条一律 12 格；两格之间的 1 列差别是
        // "内存/磁盘"这两个汉字比 "CPU" 宽，不是谁塞了说明进去。
        let (groups, tail) = metric_groups(&m, 120);
        assert_eq!(
            groups,
            vec![
                " CPU ██████░░░░░░ 42%".to_string(),
                " 内存 ████████░░░░ 61%".to_string(),
                " 磁盘 ███████████░ 88%".to_string(),
            ],
            "「{wide}」格子里混进说明了"
        );
        assert_eq!(tail, "  /data 1.7MB/2.0MB · 负载 0.42/0.31/0.19");
    }

    /// 没有 `/proc/stat` 的机器：CPU 永远差不出基线，那一格是 `—`，不是 `0%`。
    /// 0% 看着像"这台机器很闲"，而真相是没采到 —— 假安静比空白危险。
    #[test]
    fn a_missing_proc_shows_a_dash_not_a_fake_zero() {
        let mut m = crate::app::Metrics::default();
        let no_proc = "ellsm1\n/dev/sda1 2000 1760 240 88% /data\n";
        m.adopt(&ells_core::Probe::parse(no_proc));
        m.adopt(&ells_core::Probe::parse(no_proc));
        assert_eq!(metric_cells(&m)[0].percent, None);
        let line = metric_line(&m, 120);
        assert!(line.contains('—'), "「{line}」");
        assert!(!line.contains("0%"), "「{line}」把没采到画成了 0%");
        assert!(line.contains("88%"), "没有 /proc 也该有磁盘");
    }

    /// 磁盘的说明（哪个挂载点、用了多少）是加分项：装得下才写，写半截不如不写。
    #[test]
    fn the_disk_detail_shows_up_only_when_it_fits() {
        let m = filled_metrics();
        assert!(metric_line(&m, 120).contains("/data"));
        assert!(!metric_line(&m, 40).contains("/data"));
        // 但窄屏上数字仍在，且从 CPU 开头（截断只会吃掉尾巴）
        let narrow = metric_line(&m, 40);
        assert!(narrow.starts_with(" CPU "), "「{narrow}」");
        assert!(narrow.contains("42%") && narrow.contains("61%") && narrow.contains("88%"));
    }

    /// 余量只够一句说明时先给磁盘那句：它解释的是用户正盯着的那个数（哪座盘、用了多少），
    /// 负载是新增的装饰，让位。窄到两句都摆不下时两个都消失，数字一个不少。
    #[test]
    fn a_tight_row_keeps_the_disk_detail_and_drops_the_load() {
        let m = filled_metrics();
        let line = metric_line(&m, 100);
        assert!(line.contains("/data"), "「{line}」磁盘的说明被挤掉了");
        assert!(!line.contains("负载"), "「{line}」只够一句说明时该让给磁盘");
        for pct in ["42%", "61%", "88%"] {
            assert!(line.contains(pct), "「{line}」里没有 {pct}");
        }
    }

    /// 挂载点名里真可能有换行和制表符（`/proc/mounts` 把它们转义成 `\012\011`，
    /// `unescape_mount` 再还原成真的空白字符）。底部那一行只有一格高：控制字符得压成
    /// 空格，不然画出来的行数和算出来的宽度会岔开，整页几何一起歪。
    #[test]
    fn a_mount_name_with_control_characters_stays_on_one_line() {
        let mut m = crate::app::Metrics::default();
        m.adopt(&ells_core::Probe::parse(M_ROUND1));
        m.disk = Some(crate::app::DiskGauge {
            mount: "/data\tnightly\nbuild".into(),
            percent: 88,
            used_kb: 1760,
            total_kb: 2000,
        });
        let line = metric_line(&m, 120);
        assert!(!line.contains('\n') && !line.contains('\t'), "「{line:?}」");
        assert!(line.contains("/data nightly build"), "「{line}」");
        assert!(display_width(&line) <= 120, "「{line}」");
    }

    /// 刚连上、第一轮还没回来时给一句人话，而不是三条空槽 —— 空槽看着像坏了。
    #[test]
    fn an_unmeasured_slot_says_its_still_collecting() {
        let line = metric_line(&crate::app::Metrics::default(), 120);
        assert!(line.contains("采集"), "「{line}」");
        assert!(!line.contains('%'));
    }

    /// 配色分档要在界面上说得出话：常年 95% 的盘必须红。
    #[test]
    fn a_nearly_full_disk_is_the_danger_color() {
        let mut m = crate::app::Metrics::default();
        let round = "ellsm1\n/dev/sda1 2000 1900 100 95% /data\n";
        m.adopt(&ells_core::Probe::parse(round));
        let parts = metric_parts(&m, 120);
        let disk = parts.iter().find(|(t, _)| t == "95%").expect("没有 95% 这一格");
        assert_eq!(disk.1, MetricTone::Err);
    }

    /// Windows 上"条被背景藏起来"的正解：条那一截自带轨道底色，其余格子一律不画底。
    /// 深色主题的色带是浅灰，`░` 网点透出来还是浅灰，整根条就和背景一个亮度；
    /// 而"跟随终端"那套一处底都不画，多垫一层反倒脏，所以轨道色只在画死底色的主题里有值。
    #[test]
    fn only_the_bar_buys_its_own_background() {
        let track = Color::Black;
        let bar = MetricTone::Warn.as_bar();
        let style = bar.style_with(track);
        assert_eq!(style.bg, Some(track), "条必须垫住自己的轨道色");
        assert_eq!(style.fg, Some(theme::warn()), "字色还是跟着语气");
        for tone in [MetricTone::Label, MetricTone::Dim, MetricTone::Ok, MetricTone::Err] {
            assert_eq!(tone.style_with(track).bg, None, "{tone:?} 不该画底色");
        }
        // 语气判定不能因为套了一层"条"就变味：95% 的盘，条和数字得是同一个红
        assert_eq!(MetricTone::of(Some(95)).as_bar(), MetricTone::BarErr);
        assert_eq!(MetricTone::of(Some(95)).color(), MetricTone::BarErr.color());
    }

    /// 可视区高度 = 总高 − 标题栏 − 指标行，两者之和绝不能超出屏幕：
    /// 差一行，远端画面就被裁掉一行，而用户只会觉得"窗口变小了"。
    #[test]
    fn the_footer_row_and_emu_view_never_overlap() {
        for h in 10u16..=40 {
            let area = Rect::new(0, 0, 80, h);
            for footer in [0u16, 1] {
                let emu = session_emu_rect(area, footer);
                assert_eq!(emu.y, area.y + HEADER_ROWS);
                assert_eq!(emu.height + HEADER_ROWS + footer, h);
                assert!(emu.bottom() + footer <= area.bottom());
            }
        }
    }
}

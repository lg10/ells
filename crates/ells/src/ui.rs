use ells_term::vt100;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Gauge, List, ListItem, ListState as RtListState, Paragraph, Wrap,
};
use ratatui::Frame;

use crate::app::{App, Choice, FieldKind, Prompt, ScreenKind, Slot, UnlockStage};
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
        draw_help(f, area, &app.settings.keybinds);
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

fn draw_list(f: &mut Frame, app: &mut App) {
    let chunks = list_layout(f.area());
    let area = f.area();

    // 第 0 行：品牌行（右端官网徽章单独画，避免被本段文字盖住）
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " ells ",
                Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("主机列表 · {}", version_tag()), Style::default().fg(theme::dim())),
        ]))
        .style(Style::default().bg(theme::bg())),
        chunks[0],
    );
    // 第 1 行：标签条（与会话页同一份几何）
    let titles: Vec<String> = app.slots.iter().map(Slot::title).collect();
    draw_tab_bar(f, area, &titles, app.active, &app.settings.keybinds);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " 主机{} ",
            if app.vault.hosts.is_empty() {
                String::new()
            } else {
                format!("({} 台)", app.vault.hosts.len())
            }
        ))
        .title_style(Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD));
    let inner = block.inner(chunks[2]);
    f.render_widget(block, chunks[2]);

    if app.vault.hosts.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " 还没有主机 — 按 a 新增",
                Style::default().fg(theme::dim()),
            ))),
            inner,
        );
    } else {
        let items: Vec<ListItem> = app
            .vault
            .hosts
            .iter()
            .map(|h| {
                let (mark, mark_color) = live_mark(app, &h.alias);
                let mut spans = vec![
                    Span::styled(
                        format!("{mark} "),
                        Style::default().fg(mark_color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:<14}", h.alias),
                        Style::default().fg(theme::warn()).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(h.target()),
                    Span::styled(
                        format!("  [{}]", h.auth_label()),
                        Style::default().fg(theme::muted()),
                    ),
                ];
                if let Some(j) = &h.jump {
                    spans.push(Span::styled(
                        format!(" 经 {j}"),
                        Style::default().fg(theme::alt()),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let mut ls = RtListState::default();
        ls.select(Some(app.list.selected.min(app.vault.hosts.len() - 1)));
        f.render_stateful_widget(
            List::new(items).highlight_style(select_style()),
            inner,
            &mut ls,
        );
    }

    let keys = format!(
        " ↑↓ 选择 · Enter 连接/切回 · a 新增 · e 编辑 · d 删除 · i 导入 · s 设置 · {}/{}/{} 切标签 · ? 帮助 · q 退出 ",
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

    // 顶部右端官网徽章
    draw_homepage_badge(f, f.area());

    // 删除二级确认弹窗最后渲染，盖住列表
    if let Some(alias) = app.delete_confirm.clone() {
        draw_delete_confirm(f, app.confirm_index, &alias);
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

/// 帮助页：分区块列出全部键位。内容是编译期常量，宽度不够时自动换行。
fn draw_help(f: &mut Frame, area: Rect, binds: &crate::keybinds::KeyBinds) {
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
    let list_body = "↑↓/jk 选择 · Enter 连接（这台已经连着就切回它那一格标签）· a 新增 · e 编辑 · d 删除（二次确认）· s 设置 · i 导入 ~/.ssh/config · ?/F1 帮助 · q/Ctrl-C 退出；行首 ● 已连接 / ○ 正在连接，顶部标签条可以直接点".to_string();
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
    let sections: [(&str, std::borrow::Cow<str>); 10] = [
        ("主机列表", std::borrow::Cow::Owned(list_body)),
        ("多标签会话", std::borrow::Cow::Owned(tab_body)),
        ("会话终端", std::borrow::Cow::Owned(term_body)),
        ("改键", std::borrow::Cow::Owned(format!(
            "会话与标签页按键可在「设置 → 快捷键设置」里自定义（列表页按 s、会话页按顶部【设置】）：Enter 选中某项后按下新按键即可绑定，只接受 F2–F9 或带 Ctrl/Alt 的组合键（F1 留给帮助页，F10–F12 常被终端或系统吃掉），改完即时写入 ~/.ells/settings.ini；撞到已占用的键会自动互换，【恢复默认】一键还原。跨平台：mac/Linux 终端把 Ctrl-] 这类组合发成与 Ctrl-5 同一个字节，已自动归一，默认键在三个平台都能触发；本机注意点——{}",
            crate::keybinds::platform_note(),
        ))),
        ("外观主题", std::borrow::Cow::Borrowed("设置页「界面主题」按 Enter 或 ←→ 循环四套：深色（画死黑底灰条，Windows 最贴）· 跟随终端（一处底色都不画，全用终端自己的主题，mac 终端/iTerm2/WezTerm 推荐，也是 mac 默认）· 高对比（去掉灰色小字，靠粗体与反显分层）· 浅色底（白底终端用深字）。切换即时预览，【保 存】才写入 settings.ini 的 theme=，【取消】还原。")),
        ("文件浏览器", std::borrow::Cow::Borrowed("↑↓/滚轮 选择 · Enter 进入目录或下载 · u 上传文件 · U 上传整个目录 · d 下载 · m 新建目录 · n 重命名 · D 删除（递归，先确认）· Ctrl-C 取消全部在途传输 · r 刷新 · Backspace 上级 · Esc 返回终端")),
        ("主机表单", std::borrow::Cow::Borrowed("Tab/↓ 下一个字段 · ↑ 上一个 · ←→ 切换认证方式 · Ctrl-F 选私钥 · Ctrl-J 选跳板机 · Ctrl-C 清空当前字段 · Enter 在\"私钥路径/跳板机\"上直接打开选择器，保存要点【保 存】或聚焦后回车")),
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
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        Rect {
            x: panel.x + 1,
            y: panel.y + 1,
            width: panel.width.saturating_sub(2),
            height: panel.height.saturating_sub(2),
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
    let avail = (inner.width as usize).saturating_sub(LABEL_W + 2).max(4);
    let lines: Vec<Line> = vis
        .iter()
        .map(|&i| {
            let field = &app.form.fields[i];
            let focused = app.form.footer.is_none() && i == app.form.focus;
            let full = match field.kind {
                FieldKind::Secret if !field.value.is_empty() => field
                    .value
                    .chars()
                    .map(|_| '•')
                    .collect::<String>(),
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
            Line::from(vec![
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
                " Tab/↓ 移项 · Enter 选私钥/跳板机 · ←/→ 换认证",
                Style::default().fg(theme::dim()),
            )),
            Line::from(Span::styled(
                " 鼠标点击定位 · 按钮 Enter/点击 保存 · 私钥口令仅加密私钥才填",
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
            " ↑↓/滚轮 选择 · Enter 进入/下载 · u 上传文件 · U 上传目录 · d 下载 · m 新建目录 · n 重命名 · D 删除 · Ctrl-C 取消传输 · r 刷新 · Esc 返回终端 ";
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(HEADER_ROWS), Constraint::Min(1)])
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
    // 弹窗最后渲染：内嵌终端的逐格写入会覆盖先画的浮层
    if app.settings_open {
        draw_settings_stack(f, app);
    }
    if app.transfer_popup {
        draw_transfer_popup(f, app);
    }
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

/// 内嵌终端可视区（顶部 3 行之下的全部空间）。绘制与鼠标命中测试共用。
pub fn session_emu_rect(area: Rect) -> Rect {
    Rect {
        x: area.x,
        y: area.y + HEADER_ROWS,
        width: area.width,
        height: area.height.saturating_sub(HEADER_ROWS),
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
    centered(56, 12, area)
}

/// 设置弹窗的 8 个可点击行：高亮 / 保活 / 主密码开关 / 修改主密码 / 快捷键 / 主题 / 保存 / 取消。
/// 必须与 draw_settings_overlay 的几何完全一致。
pub fn settings_hit_rects(area: Rect) -> [Rect; 8] {
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
        Rect { x: ix.saturating_add(iw / 2).saturating_sub(16), y: iy + 7, width: 14, height: 1 },
        Rect { x: ix.saturating_add(iw / 2).saturating_add(2), y: iy + 7, width: 14, height: 1 },
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
    // 行 6：操作提示
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑↓ 选择 · Enter/点击 修改 · ←→ 微调 · Esc 取消",
            Style::default().fg(theme::dim()).bg(theme::bg()),
        ))),
        Rect { x: rects[0].x, y: rects[0].y + 6, width: rects[0].width, height: 1 },
    );
    // 行 7：保存 / 取消按钮
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "【 保 存 】",
            Style::default()
                .fg(theme::on_accent())
                .bg(theme::accent_bg())
                .add_modifier(if focus == 6 { Modifier::BOLD } else { Modifier::empty() })
                .add_modifier(if focus == 6 { Modifier::UNDERLINED } else { Modifier::empty() }),
        ))),
        rects[6],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "【 取 消 】",
            Style::default()
                .fg(theme::text())
                .bg(theme::band())
                .add_modifier(if focus == 7 { Modifier::BOLD } else { Modifier::empty() })
                .add_modifier(if focus == 7 { Modifier::UNDERLINED } else { Modifier::empty() }),
        ))),
        rects[7],
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
        // 6 个可编辑项必须是连续的 6 行：加一行（主题）时鼠标命中不能错位
        for i in 1..6 {
            assert_eq!(r[i].y, r[i - 1].y + 1, "第 {i} 行和上一行不挨着");
        }
        assert_eq!(r[5].y, panel.y + 6);
        // 按钮在操作提示（第 7 行）之下的同一行
        assert_eq!(r[6].y, r[7].y);
        assert_eq!(r[6].y, panel.y + 8);
        for (i, rect) in r.iter().enumerate() {
            assert!(rect.right() <= panel.right(), "第 {i} 行超出面板右边界");
            assert!(rect.bottom() <= panel.bottom(), "第 {i} 行超出面板下边界");
        }
    }
}

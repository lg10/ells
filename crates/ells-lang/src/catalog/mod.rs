//! 中英对照目录：键是源码里的中文原句，值是英文。
//!
//! 键必须与源码**逐字一致**（含全角标点、空格）。带 `{}` 的键是运行期模板：
//! 中文侧 `format!("正在连接 {}…", alias)` 画出来是「正在连接 web…」，
//! 匹配到模板后按英文侧的 `{}` 顺序把捕获值填回去。
//!
//! 分模块只为编辑方便；[`pairs`] 把它们串成一份。

pub mod app;
pub mod cli;
pub mod clihelp;
pub mod common;
pub mod core;
pub mod form;
pub mod help;
pub mod keys;
pub mod list;
pub mod session;
pub mod ui;
pub mod unlock;
pub mod update;

static GROUPS: &[&[(&str, &str)]] = &[
    common::ENTRIES,
    unlock::ENTRIES,
    list::ENTRIES,
    form::ENTRIES,
    session::ENTRIES,
    help::ENTRIES,
    app::ENTRIES,
    ui::ENTRIES,
    keys::ENTRIES,
    cli::ENTRIES,
    clihelp::ENTRIES,
    update::ENTRIES,
    core::ENTRIES,
];

pub fn pairs() -> impl Iterator<Item = (&'static str, &'static str)> {
    GROUPS.iter().flat_map(|group| group.iter().copied())
}

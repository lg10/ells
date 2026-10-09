//! 系统文件对话框的主线程代理服务。
//!
//! macOS 的 AppKit（NSOpenPanel/NSSavePanel）只允许在进程主线程上创建对话框，
//! 而 ells 的事件循环跑在 tokio worker 线程上，直接调用 rfd 会 panic
//! "Fallback Sync Dialog Must Be Spawned On Main Thread"。
//! 因此 `main` 把主线程留作本模块的服务循环，工作线程通过 channel 投递请求。

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::OnceLock;

/// 一次对话框请求；`respond` 用于回传选中的路径（None = 用户取消）。
pub enum Job {
    Pick {
        title: String,
        directory: PathBuf,
        filters: Vec<(String, Vec<String>)>,
        respond: Sender<Option<String>>,
    },
    Save {
        title: String,
        directory: PathBuf,
        file_name: String,
        respond: Sender<Option<String>>,
    },
    PickDir {
        title: String,
        directory: PathBuf,
        respond: Sender<Option<String>>,
    },
    Shutdown,
}

static JOBS: OnceLock<Sender<Job>> = OnceLock::new();

/// 由 `main` 在启动服务循环前调用，登记投递端。
pub fn install(tx: Sender<Job>) {
    let _ = JOBS.set(tx);
}

fn ask(job: Job, respond: Receiver<Option<String>>) -> Option<String> {
    JOBS.get()?.send(job).ok()?;
    respond.recv().ok().flatten()
}

fn to_string(p: PathBuf) -> Option<String> {
    p.to_str().map(|s| s.to_string())
}

/// 选择单个文件。`filters` 为空时不加类型过滤；None = 用户取消。
pub fn pick_file(title: &str, directory: PathBuf, filters: Vec<(String, Vec<String>)>) -> Option<String> {
    let (respond, rx) = channel();
    ask(
        Job::Pick { title: title.to_string(), directory, filters, respond },
        rx,
    )
}

/// 选择保存路径（可预填文件名）；None = 用户取消。
pub fn save_file(title: &str, directory: PathBuf, file_name: &str) -> Option<String> {
    let (respond, rx) = channel();
    ask(
        Job::Save { title: title.to_string(), directory, file_name: file_name.to_string(), respond },
        rx,
    )
}

/// 选择目录（递归上传的源、目录下载的落点）；None = 用户取消。
pub fn pick_directory(title: &str, directory: PathBuf) -> Option<String> {
    let (respond, rx) = channel();
    ask(
        Job::PickDir { title: title.to_string(), directory, respond },
        rx,
    )
}

/// 通知服务循环退出（`main` 的关闭守卫会调用）。
pub fn shutdown() {
    if let Some(tx) = JOBS.get() {
        let _ = tx.send(Job::Shutdown);
    }
}

/// 主线程前台守卫：只在对话框弹出期间存在。
#[cfg(target_os = "macos")]
mod foreground {
    use objc2::rc::Retained;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use std::sync::OnceLock;

    /// macOS 上从终端启动的纯命令行进程：NSApp 处于 Prohibited 且从未 finishLaunching。
    /// rfd 在这个状态直接 runModal，会把系统共享的 openAndSavePanelService 一起锁死
    /// ——不只 ells，连访达都卡住，直到 ells 退出才恢复。
    /// 正确姿势是弹框前以 Regular 正式拉起 AppKit 并激活，收尾后调回 Prohibited，
    /// Dock 与焦点都还给终端。
    pub struct Foreground {
        app: Option<Retained<NSApplication>>,
    }

    impl Foreground {
        pub fn acquire() -> Self {
            let Some(mtm) = MainThreadMarker::new() else {
                return Self { app: None };
            };
            let app = NSApplication::sharedApplication(mtm);
            app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
            static LAUNCHED: OnceLock<()> = OnceLock::new();
            if LAUNCHED.set(()).is_ok() {
                app.finishLaunching();
            }
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
            Self { app: Some(app) }
        }
    }

    impl Drop for Foreground {
        fn drop(&mut self) {
            if let Some(app) = &self.app {
                app.deactivate();
                app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
            }
        }
    }
}

/// 非 macOS 平台 rfd 自行处理线程与事件循环，守卫是空的。
#[cfg(not(target_os = "macos"))]
mod foreground {
    pub struct Foreground;

    impl Foreground {
        pub fn acquire() -> Self {
            Foreground
        }
    }
}

/// 在主线程上运行；收到 `Shutdown` 后返回。
pub fn run_service(rx: Receiver<Job>) {
    while let Ok(job) = rx.recv() {
        if let Job::Shutdown = job {
            break;
        }
        let _fg = foreground::Foreground::acquire();
        handle(job);
    }
}

fn handle(job: Job) {
    match job {
        Job::Shutdown => {}
        Job::Pick { title, directory, filters, respond } => {
            let mut dialog = rfd::FileDialog::new().set_title(&title);
            if directory.is_dir() {
                dialog = dialog.set_directory(&directory);
            }
            for (name, exts) in &filters {
                let refs: Vec<&str> = exts.iter().map(|e| e.as_str()).collect();
                dialog = dialog.add_filter(name, &refs);
            }
            let picked = dialog.pick_file().and_then(to_string);
            let _ = respond.send(picked);
        }
        Job::PickDir { title, directory, respond } => {
            let mut dialog = rfd::FileDialog::new().set_title(&title);
            if directory.is_dir() {
                dialog = dialog.set_directory(&directory);
            }
            let picked = dialog.pick_folder().and_then(to_string);
            let _ = respond.send(picked);
        }
        Job::Save { title, directory, file_name, respond } => {
            let mut dialog = rfd::FileDialog::new().set_title(&title).set_file_name(&file_name);
            if directory.is_dir() {
                dialog = dialog.set_directory(&directory);
            }
            let picked = dialog.save_file().and_then(to_string);
            let _ = respond.send(picked);
        }
    }
}

/// 保证无论正常返回还是 panic 退栈，主线程的对话框服务都能收尾退出。
pub struct ShutdownGuard;

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        shutdown();
    }
}

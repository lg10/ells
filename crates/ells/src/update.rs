//! 自更新：查新版本 → 下载本平台裸二进制 → 校验 SHA256 → 原子替换自身。
//!
//! 三条约束解释了下面这些看起来多余的写法：
//! - **不新增序列化依赖**：`tag_name` 与 `SHA256SUMS.txt` 都用极小的手写解析，
//!   并且解析失败一律当"没有新版本"，绝不拿猜出来的版本去替换二进制。
//! - **不信任下载内容**：校验和与二进制同取自一个 Release 资产，SHA256 不过就删临时件、
//!   不替换（信任模型与 `install.sh` 一致：信 GitHub 的发布物 + TLS）。
//! - **不占 UI 线程**：`check` / `apply` 都按 `spawn_blocking` 的形态写，进度走回调。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ureq::ResponseExt as _;

/// 匿名访问 GitHub API 只有 60 次/小时，所以首选 HEAD 跟重定向拿 tag（零正文、不计限额）。
const PAGE_URL: &str = "https://github.com/lg10/ells/releases/latest";
const API_URL: &str = "https://api.github.com/repos/lg10/ells/releases/latest";
const DOWNLOAD_BASE: &str = "https://github.com/lg10/ells/releases/download";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 当前版本号，和 `ells --version` 同源。
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// 解析 `v0.1.5` / `0.1.5`。带 `-rc1` 之类后缀或段数不对都返回 None：
    /// 认不出来的版本不能参与比较，否则会拿一个猜出来的号去替换自己。
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches(['v', 'V']);
        let mut parts = text.split('.');
        let (Some(major), Some(minor), Some(patch)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major: major.parse().ok()?,
            minor: minor.parse().ok()?,
            patch: patch.parse().ok()?,
        })
    }
}

/// 远端版本严格新于本地才算"有更新"。
pub fn is_newer(latest: &str) -> bool {
    let Some(current) = Version::parse(current_version()) else {
        return false;
    };
    Version::parse(latest).is_some_and(|new| new > current)
}

/// 本平台有没有预编译包（与 `install.sh` / `install.ps1` 的资产表一致）。
pub fn asset_name() -> Option<&'static str> {
    asset_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn asset_for(os: &str, arch: &str) -> Option<&'static str> {
    match os {
        "windows" => Some("ells-windows-x86_64.exe"),
        "macos" => Some("ells-macos-universal"),
        // Linux 只出 x86_64；arm 上继续指路安装脚本，别下载一个跑不起来的二进制
        "linux" if arch == "x86_64" => Some("ells-linux-x86_64"),
        _ => None,
    }
}

/// 没有预编译包时给用户的替代路径。
pub fn unsupported_hint() -> String {
    format!(
        "{} / {} 暂无预编译包，请用安装脚本更新（见 README 的一键安装）",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

fn env_url(key: &str, default: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => default.to_string(),
    }
}

fn api_url() -> String {
    env_url("ELLS_API_URL", API_URL)
}

fn page_url() -> String {
    env_url("ELLS_RELEASE_URL", PAGE_URL)
}

fn download_base() -> String {
    env_url("ELLS_DOWNLOAD_URL", DOWNLOAD_BASE).trim_end_matches('/').to_string()
}

fn asset_url(tag: &str, name: &str) -> String {
    join_download(&download_base(), tag, name)
}

/// 镜像地址允许带或不带尾斜杠（`ELLS_DOWNLOAD_URL` 与安装脚本同一份契约）。
fn join_download(base: &str, tag: &str, name: &str) -> String {
    format!("{}/{}/{}", base.trim_end_matches('/'), tag, name)
}

/// 从 `…/releases/tag/v0.1.6` 取 tag；形状不对返回 None。
pub fn tag_from_uri(uri: &str) -> Option<String> {
    let (_, tail) = uri.split_once("/releases/tag/")?;
    let tag = tail.split(['?', '#']).next()?.trim_end_matches('/');
    normalize_tag(tag)
}

/// 从 GitHub API 响应里抠 `"tag_name": "v0.1.6"`（不为一个字段引 serde_json）。
pub fn tag_from_api(body: &str) -> Option<String> {
    let (_, rest) = body.split_once("\"tag_name\"")?;
    let (_, rest) = rest.split_once(':')?;
    let (_, rest) = rest.split_once('"')?;
    normalize_tag(rest.split('"').next()?)
}

/// tag 会被拼进下载 URL，所以字符集收到最窄：不许斜杠、不许 `..`。
fn normalize_tag(tag: &str) -> Option<String> {
    let tag = tag.trim();
    let ok = !tag.is_empty()
        && tag.len() <= 40
        && !tag.contains("..")
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    ok.then(|| tag.to_string())
}

/// 解析 `SHA256SUMS.txt`（`<hex><空白><文件名>`）取某个资产的校验和。
pub fn sha_from_sums(text: &str, asset: &str) -> Option<String> {
    for line in text.lines() {
        let Some((sum, name)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let name = name.trim().trim_start_matches('*');
        // 有的工具写带路径的文件名，只比最后一段
        let base = name.rsplit('/').next().unwrap_or(name);
        if base == asset
            && sum.len() == 64
            && sum.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Some(sum.to_ascii_lowercase());
        }
    }
    None
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 上次检查的缓存（`~/.ells/update.cache`）。启动必查，所以这里只为设置页显示"多久之前"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cache {
    pub checked_at: u64,
    pub latest: String,
}

fn cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ells").join("update.cache"))
}

pub fn read_cache() -> Option<Cache> {
    let text = std::fs::read_to_string(cache_path()?).ok()?;
    let (mut checked_at, mut latest) = (0u64, String::new());
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "checked_at" => checked_at = v.trim().parse().unwrap_or(0),
            "latest" => latest = v.trim().to_string(),
            _ => {}
        }
    }
    (checked_at > 0 && !latest.is_empty()).then_some(Cache { checked_at, latest })
}

pub fn write_cache(latest: &str) {
    let Some(path) = cache_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, format!("checked_at={}\nlatest={}\n", unix_now(), latest));
}

/// 把"上次检查"说成人话：只关心多久之前，不必拉日历进来。
pub fn age_label(checked_at: u64) -> String {
    let secs = unix_now().saturating_sub(checked_at);
    if secs < 60 {
        "刚刚".to_string()
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86400 {
        format!("{} 小时前", secs / 3600)
    } else {
        format!("{} 天前", secs / 86400)
    }
}

fn agent(recv_timeout: Duration) -> ureq::Agent {
    // 刻意不配 `timeout_total`：新版本二进制有十几 MB，整体超时只会误杀慢速网络。
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(recv_timeout))
        .user_agent(concat!("ells/", env!("CARGO_PKG_VERSION")))
        .build();
    ureq::Agent::new_with_config(config)
}

/// 把 ureq 的错误翻成用户能照着做的一句话。
fn net_msg(e: &ureq::Error) -> String {
    use ureq::Error::*;
    match e {
        StatusCode(404) => "GitHub 上没有这个发布（404）".to_string(),
        StatusCode(403) | StatusCode(429) => {
            "GitHub 拒绝了请求（访问频率受限或需要代理），稍后再试".to_string()
        }
        StatusCode(n) => format!("GitHub 返回状态码 {n}"),
        Timeout(_) => "网络超时，请检查连通性后重试".to_string(),
        HostNotFound | ConnectionFailed => "连不上 GitHub，请检查网络或代理设置".to_string(),
        BadUri(_) | InvalidProxyUrl => "下载地址不合法（检查 ELLS_API_URL / ELLS_DOWNLOAD_URL）".to_string(),
        Io(io) => format!("网络错误：{io}"),
        other => format!("网络错误：{other}"),
    }
}

/// 查一次新版本：`Ok(None)` = 已是最新，`Ok(Some(tag))` = 有更新，`Err` = 失败文案。
///
/// 顺带把结果写进缓存，失败时不写（保留上次成功的记录）。
pub fn check() -> Result<Option<String>, String> {
    let tag = probe_tag()?;
    write_cache(&tag);
    // 自动回退过的版本不再推给用户：不然就是"回退 → 又提示更新 → 又起不来 → 又回退"
    if is_rejected(&tag) {
        tracing::info!(%tag, "这个版本自动回退过，本次不再提示更新");
        return Ok(None);
    }
    Ok(is_newer(&tag).then_some(tag))
}

/// 先 HEAD 发布页跟重定向（不计 API 限额），失败再退回 API JSON。
fn probe_tag() -> Result<String, String> {
    match head_tag() {
        Ok(tag) => Ok(tag),
        Err(first) => api_tag().map_err(|second| format!("{first}；备用接口也失败：{second}")),
    }
}

fn head_tag() -> Result<String, String> {
    let url = page_url();
    let res = agent(Duration::from_secs(10))
        .head(&url)
        .call()
        .map_err(|e| net_msg(&e))?;
    // 跟完重定向后的最终 URI 就是 `…/releases/tag/vX.Y.Z`，一个字节正文都不用下
    let uri = res.get_uri().to_string();
    tag_from_uri(&uri).ok_or_else(|| "发布页重定向地址里没有版本号".to_string())
}

fn api_tag() -> Result<String, String> {
    let mut res = agent(Duration::from_secs(10))
        .get(&api_url())
        .call()
        .map_err(|e| net_msg(&e))?;
    let mut text = String::new();
    res.body_mut()
        .as_reader()
        .take(1 << 20)
        .read_to_string(&mut text)
        .map_err(|e| format!("读取版本接口响应失败：{e}"))?;
    tag_from_api(&text).ok_or_else(|| "无法从版本接口响应里解析 tag_name".to_string())
}

/// 替换发生后 Windows 上 `current_exe()` 会跟着改名后的 `.old` 走，
/// 所以把"安装位置"在改名之前记下来，重启时用这个路径。
static INSTALL_TARGET: OnceLock<PathBuf> = OnceLock::new();

fn exe_path() -> Result<PathBuf, String> {
    if let Some(path) = INSTALL_TARGET.get() {
        return Ok(path.clone());
    }
    std::env::current_exe().map_err(|e| format!("无法定位 ells 自身的可执行文件：{e}"))
}

fn old_path(exe: &Path) -> PathBuf {
    let mut name = exe
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ells".to_string());
    name.push_str(".old");
    exe.with_file_name(name)
}

/// 下载并替换自身。`cancel` 置位即中止并清掉临时件。
///
/// 成功不代表用户已经在跑新版本——正在跑的还是旧镜像，只是盘上的文件已经换好，
/// 调用方要提示重启。换好之后立刻登记"待验证"（见 `recover_at_startup`）：
/// 新二进制万一起不来，下次启动才有东西可退。
pub fn apply(
    tag: &str,
    cancel: &AtomicBool,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    let exe = exe_path()?;
    let result = apply_to(&exe, tag, cancel, progress);
    if result.is_ok() {
        heal_launchers_in_dir(&exe);
    }
    result
}

/// 更新后留下的"这份新版本还没画出一帧"记录，和 `.old` 备份放在同一个目录里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub tag: String,
    pub backup: PathBuf,
    pub tries: u32,
}

/// 一次启动该做什么：`RolledBack` 表示盘上的二进制已被退回上一版（`tag` 是被退回
/// 的那个版本），调用方要把退回后的那份重新拉起来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Startup {
    Ready,
    RolledBack { tag: String },
}

fn pending_path(exe: &Path) -> PathBuf {
    exe.parent().map(|d| d.join("ells.update.pending")).unwrap_or_else(|| PathBuf::from("ells.update.pending"))
}

/// 已经自动退回过的版本：别再把它当成"有更新"推给用户，否则就是
/// 回退 → 提示更新 → 再崩 → 再回退的死循环。
fn rejected_path(exe: &Path) -> PathBuf {
    exe.parent().map(|d| d.join("ells.update.rejected")).unwrap_or_else(|| PathBuf::from("ells.update.rejected"))
}

fn read_pending(exe: &Path) -> Option<Pending> {
    let text = std::fs::read_to_string(pending_path(exe)).ok()?;
    let mut tag = String::new();
    let mut backup = PathBuf::new();
    let mut tries = 0u32;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "tag" => tag = v.trim().to_string(),
            "backup" => backup = PathBuf::from(v.trim()),
            "tries" => tries = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    (!tag.is_empty() && backup.is_file()).then_some(Pending { tag, backup, tries })
}

fn write_pending(exe: &Path, pending: &Pending) {
    let text = format!(
        "tag={}\nbackup={}\ntries={}\n",
        pending.tag,
        pending.backup.display(),
        pending.tries
    );
    if let Err(e) = std::fs::write(pending_path(exe), text) {
        tracing::warn!("写更新待验证记录失败：{e}");
    }
}

fn clear_pending(exe: &Path) {
    let _ = std::fs::remove_file(pending_path(exe));
}

/// 这次启动要不要把二进制退回上一版。
///
/// 只在界面启动路径上调用：无头子命令跑得动不代表界面版没坏，反过来也不该让
/// `ells list` 去动盘上的二进制。
pub fn recover_at_startup() -> Startup {
    match exe_path() {
        Ok(exe) => {
            let startup = recover_from(&exe, current_version());
            if let Startup::RolledBack { .. } = startup {
                // 改名会让 Windows 的 current_exe() 跟着跑到 .broken 上，
                // 所以先把"该拉起哪份文件"记死，restart 才不会把坏的那份再 exec 一遍
                let _ = INSTALL_TARGET.set(exe);
            }
            startup
        }
        Err(_) => Startup::Ready,
    }
}

/// `recover_at_startup` 的内核，`current` 传这份二进制自己的版本号，好在临时目录里测。
fn recover_from(exe: &Path, current: &str) -> Startup {
    let Some(pending) = read_pending(exe) else {
        return Startup::Ready;
    };
    if !is_same_version(&pending.tag, current) {
        // 版本号对不上 = 这条是陈账（重装过、或后来又更新过一轮）。拿它去退体只会把
        // 一份好端端的二进制换成更早的，所以当场作废，本次照常启动。
        clear_pending(exe);
        return Startup::Ready;
    }
    if pending.tries == 0 {
        // 这是换上新版本后的第一次启动，先给它一次机会，只把次数记下来
        write_pending(exe, &Pending { tries: 1, ..pending.clone() });
        return Startup::Ready;
    }
    let tag = pending.tag.clone();
    match rollback_to_backup(exe, &pending) {
        Ok(()) => Startup::RolledBack { tag },
        Err(e) => {
            tracing::warn!("自动回退失败：{e}");
            clear_pending(exe);
            Startup::Ready
        }
    }
}

/// 把备份挪回原位，顶掉这份起不来的新版本。
///
/// Windows 不许覆盖正在运行的镜像，所以先把坏的那份改名挪走（改名是允许的），
/// 再把备份放回来；备份挪不回来必须原样挪回，否则启动名直接消失。
fn rollback_to_backup(exe: &Path, pending: &Pending) -> Result<(), String> {
    let broken = {
        let mut name = exe
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "ells".to_string());
        name.push_str(".broken");
        exe.with_file_name(name)
    };
    if cfg!(windows) {
        let _ = std::fs::remove_file(&broken);
        std::fs::rename(exe, &broken)
            .map_err(|e| format!("挪不开正在运行的 {}：{e}", exe.display()))?;
    }
    if let Err(e) = std::fs::rename(&pending.backup, exe) {
        if cfg!(windows) {
            let _ = std::fs::rename(&broken, exe);
        }
        return Err(format!("把上一版挪回 {} 失败：{e}", exe.display()));
    }
    write_rejected(exe, &pending.tag);
    clear_pending(exe);
    tracing::info!(tag = %pending.tag, backup = %pending.backup.display(), "已自动退回上一版");
    Ok(())
}

fn write_rejected(exe: &Path, tag: &str) {
    let _ = std::fs::write(rejected_path(exe), format!("{tag}\n"));
}

/// 属于"当前这份二进制"的待验证记录。
///
/// 只读不改：确认与回滚都在自己那份记录上做决定，作废陈账是启动自愈（`recover_from`）
/// 一个人的活，别让一个查询顺手删掉别人的退路。
fn pending_for(exe: &Path, current: &str) -> Option<Pending> {
    let pending = read_pending(exe)?;
    (is_same_version(&pending.tag, current)).then_some(pending)
}

/// 第一帧真的画到屏幕上之后调用：新版本被认定可用，待验证记录与备份都可以扔掉。
pub fn confirm_first_frame() {
    if let Ok(exe) = exe_path() {
        confirm_for(&exe, current_version());
    }
}

fn confirm_for(exe: &Path, current: &str) {
    let Some(pending) = pending_for(exe, current) else {
        return;
    };
    let _ = std::fs::remove_file(&pending.backup);
    clear_pending(exe);
}

/// 主循环带着错误退出时调用：这是"新版本起来了但第一帧都没画出来"的直接证据，
/// 当场退回上一版，不要等用户再试一次。返回被退回的版本号表示盘上已退回，调用方要重启。
pub fn roll_back_after_failed_start() -> Option<String> {
    let exe = exe_path().ok()?;
    let tag = roll_back_for(&exe, current_version())?;
    let _ = INSTALL_TARGET.set(exe);
    Some(tag)
}

fn roll_back_for(exe: &Path, current: &str) -> Option<String> {
    let pending = pending_for(exe, current)?;
    let tag = pending.tag.clone();
    match rollback_to_backup(exe, &pending) {
        Ok(()) => Some(tag),
        Err(e) => {
            tracing::warn!("自动回退失败：{e}");
            clear_pending(exe);
            None
        }
    }
}

/// 记录里的 tag 是发布形状（`v0.2.4`），`current_version()` 是 crate 形状（`0.2.4`），
/// 只能按解析出来的三段号比，直接比字符串会永远对不上 —— 对不上就等于永不回滚。
fn is_same_version(a: &str, b: &str) -> bool {
    Version::parse(a).is_some_and(|x| Version::parse(b) == Some(x))
}

/// 这个 tag 是不是已经被自动回退过一次：是的话不再提示更新。
pub fn is_rejected(tag: &str) -> bool {
    let Ok(exe) = exe_path() else {
        return false;
    };
    std::fs::read_to_string(rejected_path(&exe))
        .map(|text| text.trim() == tag)
        .unwrap_or(false)
}

/// 启动时把同目录里另一个启动名对齐到正在跑的这份。
///
/// 安装脚本在同一个目录里放下了不止一个启动名（Windows 上 `ells.exe` 和 `s.exe`
/// 是两份真拷贝，unix 上 `s` 是指向 `ells` 的软链），而自更新只换得动"正在跑的
/// 那一个"。不补这一步，用户下次敲另一个名字拿到的还是旧版本，它还会再提示一次
/// "有更新"；更糟的是软链可能在替换中变成一个独立副本，两份各跑各的版本。
///
/// 三种坏法都在这里修：软链断了（重新指回来）、只剩一个启动名（补上另一个）、
/// 两份内容不同（用正在跑的这份覆盖）。跳过正在跑的本身，以及被另一个 ells
/// 占用的启动名（Windows 不许覆盖运行中的镜像）—— 占用只是跳过，不算失败。
pub fn heal_launchers() {
    let Ok(exe) = exe_path() else { return };
    // 开发构建里不补：`cargo run` 跑的是 target/debug/ells，那本来就不是安装目录，
    // 照同一套规则会在开发机上凭空多拷出一个几百 MB 的 target/debug/s.exe。
    // cargo 的输出目录里一定有 deps/，真实安装目录不会有。
    if exe.parent().is_some_and(|dir| dir.join("deps").is_dir()) {
        return;
    }
    heal_launchers_in_dir(&exe);
}

fn heal_launchers_in_dir(exe: &Path) {
    let Some(dir) = exe.parent() else {
        return;
    };
    for name in LAUNCHER_NAMES {
        let path = dir.join(name);
        if path == exe {
            continue;
        }
        // 是不是同一个文件：先按解析后的真实路径比（软链因此也判成同一个），
        // 解析不了再退回比名字——宁可跳过，也不能把刚装好的二进制拷回它自己
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                if same_file(exe, &path) {
                    continue;
                }
                if meta.file_type().is_symlink() && !path.is_file() {
                    // 断链：安装脚本留下的名字还在 PATH 里，但执行只会报"没有那个文件"
                    let _ = std::fs::remove_file(&path);
                    link_launcher(exe, &path);
                    continue;
                }
                if !path.is_file() {
                    continue;
                }
                match refresh_copy(exe, &path) {
                    Refresh::Same => {}
                    Refresh::Done => {
                        tracing::info!(path = %path.display(), "另一个启动名已跟着更新")
                    }
                    Refresh::Blocked(e) => tracing::warn!(
                        path = %path.display(),
                        "另一个启动名覆盖不了（多半是别处的 ells 正在运行），本次跳过：{e}"
                    ),
                    Refresh::Failed(e) => {
                        tracing::warn!(path = %path.display(), "另一个启动名修不好：{e}")
                    }
                }
            }
            // 名字压根不在：这个目录里有正在跑的二进制，说明它是安装目录，补齐安装脚本的形状
            Err(_) => link_launcher(exe, &path),
        }
    }
}

fn same_file(exe: &Path, other: &Path) -> bool {
    match (std::fs::canonicalize(exe), std::fs::canonicalize(other)) {
        (Ok(a), Ok(b)) => a == b,
        _ => other.file_name() == exe.file_name(),
    }
}

enum Refresh {
    Same,
    Done,
    Blocked(String),
    Failed(String),
}

/// 内容一致就不写盘：省一次几十 MB 的覆盖，也不会去碰别人正在跑的镜像。
fn refresh_copy(exe: &Path, path: &Path) -> Refresh {
    let (Some(want), Some(got)) = (digest_of(exe), digest_of(path)) else {
        return Refresh::Failed("读不出内容".to_string());
    };
    if want == got {
        return Refresh::Same;
    }
    match std::fs::copy(exe, path) {
        Ok(_) => Refresh::Done,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Refresh::Blocked(e.to_string())
        }
        Err(e) => Refresh::Failed(e.to_string()),
    }
}

/// 整份文件的 SHA256；读不出来给 None。只用来判断"两份是不是同一份东西"。
fn digest_of(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Some(ctx.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect())
}

/// 按安装脚本的形状补上启动名：unix 是指向同目录主二进制的软链，Windows 只能再拷一份。
fn link_launcher(exe: &Path, path: &Path) {
    #[cfg(unix)]
    {
        let Some(target) = exe.file_name() else { return };
        match std::os::unix::fs::symlink(target, path) {
            Ok(_) => tracing::info!(path = %path.display(), "缺失的启动名已补上软链"),
            Err(e) => tracing::warn!(path = %path.display(), "启动名补不出来：{e}"),
        }
    }
    #[cfg(not(unix))]
    {
        match std::fs::copy(exe, path) {
            Ok(_) => tracing::info!(path = %path.display(), "缺失的启动名已补上副本"),
            Err(e) => tracing::warn!(path = %path.display(), "启动名补不出来：{e}"),
        }
    }
}

/// 安装脚本会创建的启动名。
#[cfg(windows)]
const LAUNCHER_NAMES: [&str; 2] = ["ells.exe", "s.exe"];
/// unix 上 `s` 是指向 `ells` 的软链（同一个文件，自愈时按"是不是同一个文件"跳过）。
#[cfg(not(windows))]
const LAUNCHER_NAMES: [&str; 2] = ["ells", "s"];

/// 把 `exe` 换成 `tag` 版本的官方资产。目标路径单独传进来，是为了能在临时目录里
/// 跑完整链路做验证，而不是拿正在运行的自己试。
fn apply_to(
    exe: &Path,
    tag: &str,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    let asset = asset_name().ok_or_else(unsupported_hint)?;
    // tag 来自上一次网络响应，这里再收一次口，防止被拼接成越界路径
    let tag = normalize_tag(tag).ok_or_else(|| "版本号格式无法识别，已放弃更新".to_string())?;
    let dir = exe.parent().ok_or("无法定位 ells 所在目录")?;
    let tmp = dir.join(format!("ells.update.{}.tmp", std::process::id()));

    let result = (|| -> Result<(), String> {
        let want = fetch_sum(&tag, asset)?;
        let got = download(&tag, asset, &tmp, cancel, &mut progress)?;
        if !got.eq_ignore_ascii_case(&want) {
            return Err(format!(
                "SHA256 校验不通过（期望 {}…，实到 {}…），已放弃替换",
                &want[..12],
                &got[..12]
            ));
        }
        mark_executable(&tmp)?;
        install_in_place(exe, &tmp)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    } else {
        let _ = INSTALL_TARGET.set(exe.to_path_buf());
        // 盘上换好了，但"能不能跑起来"只有下一帧才知道。先登记待验证，
        // 万一这份新二进制开局就崩，下次启动还有 `.old` 可以退回去。
        write_pending(
            exe,
            &Pending { tag: tag.clone(), backup: old_path(exe), tries: 0 },
        );
        tracing::info!(%tag, path = %exe.display(), "ells 已更新");
    }
    result
}

fn fetch_sum(tag: &str, asset: &str) -> Result<String, String> {
    let url = asset_url(tag, "SHA256SUMS.txt");
    let mut res = agent(Duration::from_secs(15))
        .get(&url)
        .call()
        .map_err(|e| format!("取校验文件失败：{}", net_msg(&e)))?;
    let mut text = String::new();
    res.body_mut()
        .as_reader()
        .take(1 << 20)
        .read_to_string(&mut text)
        .map_err(|e| format!("读取校验文件失败：{e}"))?;
    sha_from_sums(&text, asset).ok_or_else(|| format!("校验文件里没有 {asset} 这一项"))
}

/// 流式下载到 `tmp`，边写盘边算 SHA256，返回摘要十六进制。
fn download(
    tag: &str,
    asset: &str,
    tmp: &Path,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<String, String> {
    let url = asset_url(tag, asset);
    let mut res = agent(Duration::from_secs(30))
        .get(&url)
        .call()
        .map_err(|e| format!("下载失败：{}", net_msg(&e)))?;
    let total = res.body().content_length();
    let mut file = std::fs::File::create(tmp)
        .map_err(|e| format!("无法在 {} 创建临时文件：{e}", tmp.display()))?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut reader = res.body_mut().as_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut done = 0u64;
    // 每个 64KB 块都回一次会让界面刷个不停，攒够 256KB 再报一次（收尾必报）
    let mut since_report = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消下载".to_string());
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("下载中断：{e}"))?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
        file.write_all(&buf[..n])
            .map_err(|e| format!("写入临时文件失败：{e}"))?;
        done += n as u64;
        since_report += n as u64;
        if since_report >= 256 * 1024 {
            progress(done, total);
            since_report = 0;
        }
    }
    file.flush()
        .map_err(|e| format!("写入临时文件失败：{e}"))?;
    drop(file);
    if total.is_some_and(|expect| expect != done) {
        return Err(format!(
            "下载不完整（收到 {done}/{} 字节），已放弃替换",
            total.unwrap_or(done)
        ));
    }
    progress(done, total);
    let digest = ctx.finish();
    Ok(digest.as_ref().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
fn mark_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("无法给下载文件加执行权限：{e}"))
}

#[cfg(not(unix))]
fn mark_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// 用新二进制顶掉正在运行的自己，并把旧的留成 `.old`。
///
/// Windows 不许删除运行中的 exe，只许改名，所以必须先挪开；unix 上同样挪这一挪，
/// 是因为那是"新版换上了却根本起不来"时唯一能退回的东西（见 `recover_at_startup`）。
/// 替换失败必须原样挪回，否则用户的 `s` / `ells` 命令会直接消失。
fn install_in_place(exe: &Path, tmp: &Path) -> Result<(), String> {
    let old = old_path(exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| {
        format!("无法挪开正在运行的 {}：{e}（Windows 偶发占用，稍后重试即可）", exe.display())
    })?;
    if let Err(e) = std::fs::rename(tmp, exe) {
        let _ = std::fs::rename(old_path(exe), exe);
        return Err(replace_err(exe, &e));
    }
    Ok(())
}

fn replace_err(exe: &Path, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(5) {
        format!(
            "没有权限替换 {}：该目录需要管理员/root 权限，请用安装脚本（或 sudo）更新",
            exe.display()
        )
    } else {
        format!("替换 {} 失败：{e}", exe.display())
    }
}

/// 清掉上一次更新留下的崩溃残留（临时件与回退坏件只删一天前的，
/// 免得误杀另一个正在下载的 ells 实例、或删掉别人正要退回的备份）。
pub fn cleanup_leftovers() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // 备份的归宿是 `confirm_first_frame`：新版还没被证明能画出一帧时，
    // `.old` 是唯一的退路，这里不能碰。
    if read_pending(&exe).is_none() {
        let _ = std::fs::remove_file(old_path(&exe));
    }
    let Some(dir) = exe.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = unix_now().saturating_sub(86400);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let leftover = (name.starts_with("ells.update.") && name.ends_with(".tmp"))
            || name.ends_with(".broken");
        if !leftover {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| {
                t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) < cutoff
            })
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 用替换后的新二进制重启自己。**必须由主线程调用**，且终端已还原。
///
/// unix 走 exec：原地替换当前进程映像，pid、进程组、控制终端全部保留。
/// 之前用 spawn 拉新进程，旧进程一退出，新进程就成了"死进程组"里的孤儿——
/// tty 不再是它的前台，macOS 上任何 termios 初始化直接 EIO，新版本还没开局就报
/// "Input/output error (os error 5)"。
///
/// Windows 反过来：新进程必须**自己带一块控制台**。同控制台拉起来时新进程确实活着
/// （实测事件流、备用屏都正常），但父进程一退出，外层 shell 立刻把提示符打回同一块
/// 屏幕 —— 两个进程往同一个控制台写，用户只看见提示符压着一片残片，像闪退，实际是
/// 版本已经在后台跑着却没了屏幕。`CREATE_NEW_CONSOLE` 在 Windows Terminal 下就是
/// 新开一个标签，新版本从第一帧起就独占它那块屏幕。
pub fn restart() -> Result<(), String> {
    let exe = exe_path()?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // exec 成功则永不返回；返回即失败
        let err = cmd.exec();
        Err(format!("新版本启动失败：{err}（请手动运行 {}）", exe.display()))
    }
    #[cfg(not(unix))]
    {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NEW_CONSOLE);
        }
        match cmd.spawn() {
            Ok(_) => {
                // 旧标签得说一句人话：它回到 shell 是交接完成，不是崩了。
                // unix 上不用——exec 原地换映像，新画面紧跟着就盖过来了。
                println!("新版本已在新标签/新窗口里打开，本标签交还给 shell。");
                Ok(())
            }
            Err(e) => Err(format!("新版本启动失败：{e}（请手动运行 {}）", exe.display())),
        }
    }
}

/// Windows 控制台创建标志：给子进程另开一块控制台，不继承父进程那块。
#[cfg(windows)]
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_segments_not_strings() {
        // 字符串序会把 0.1.10 判成比 0.1.9 旧
        assert!(Version::parse("v0.1.10").unwrap() > Version::parse("0.1.9").unwrap());
        assert!(Version::parse("0.2.0").unwrap() > Version::parse("0.1.99").unwrap());
        assert!(!is_newer("v0.0.1"));
    }

    #[test]
    fn release_and_crate_version_shapes_are_the_same_version() {
        // 待验证记录里存的是发布 tag（带 v），比的是 CARGO_PKG_VERSION（不带 v）
        assert!(is_same_version("v0.2.4", "0.2.4"));
        assert!(is_same_version("0.2.4", "0.2.4"));
        assert!(!is_same_version("v0.2.4", "0.2.5"));
        // 认不出来的版本不能判成同一条记录，否则陈账会被当成新账误退
        assert!(!is_same_version("latest", "0.2.4"));
        assert!(!is_same_version("v0.2.4", ""));
    }

    #[test]
    fn unparseable_versions_are_never_treated_as_updates() {
        for text in ["", "v0.1", "0.1.5-rc1", "0.1.5.1", "latest", "v0.x.5"] {
            assert_eq!(Version::parse(text), None, "{text} 不该被认出来");
        }
        assert!(!is_newer("随便什么"));
    }

    #[test]
    fn tag_comes_from_the_redirected_uri() {
        assert_eq!(
            tag_from_uri("https://github.com/lg10/ells/releases/tag/v0.1.5").as_deref(),
            Some("v0.1.5")
        );
        assert_eq!(tag_from_uri("https://github.com/lg10/ells").as_deref(), None);
        // 带查询/锚点、结尾斜杠也要认
        assert_eq!(
            tag_from_uri("https://ghproxy/x/releases/tag/v1.2.3?sa=1/").as_deref(),
            Some("v1.2.3")
        );
    }

    #[test]
    fn tag_never_carries_path_separators_or_dotdots() {
        assert_eq!(normalize_tag("v0.1.5/../../etc"), None);
        assert_eq!(normalize_tag(""), None);
        assert_eq!(normalize_tag("v0.1.5"), Some("v0.1.5".to_string()));
    }

    #[test]
    fn api_tag_survives_without_a_json_parser() {
        let body = r#"{"url":"https://api.github.com/repos/lg10/ells/releases/1","tag_name":"v0.1.6","name":"v0.1.6"}"#;
        assert_eq!(tag_from_api(body).as_deref(), Some("v0.1.6"));
        assert_eq!(tag_from_api(r#"{"message":"Not Found"}"#), None);
    }

    #[test]
    fn sums_match_only_the_requested_asset() {
        let sha = "b".repeat(64);
        let text = format!(
            "{sha}  ells-windows-x86_64.exe\n\
             zzz  ells-linux-x86_64\n\
             {}\n",
            "c".repeat(64)
        );
        assert_eq!(
            sha_from_sums(&text, "ells-windows-x86_64.exe").as_deref(),
            Some(sha.as_str())
        );
        // 缺项 / 摘要不是 64 位十六进制都不能给出值
        assert_eq!(sha_from_sums(&text, "ells-macos-universal"), None);
        assert_eq!(sha_from_sums(&text, "ells-linux-x86_64"), None);
        // 带路径前缀、`*` 二进制标记、单个空格分隔都要认
        assert_eq!(
            sha_from_sums(&format!("{sha} *./ells-windows-x86_64.exe"), "ells-windows-x86_64.exe")
                .as_deref(),
            Some(sha.as_str())
        );
        assert_eq!(
            sha_from_sums(&format!("{} ells-windows-x86_64.exe", sha.to_uppercase()), "ells-windows-x86_64.exe")
                .as_deref(),
            Some(sha.as_str())
        );
    }

    #[test]
    fn asset_table_matches_the_install_scripts() {
        assert_eq!(asset_for("windows", "x86_64"), Some("ells-windows-x86_64.exe"));
        // mac 是 universal 包，arm/x86 同一个资产名
        assert_eq!(asset_for("macos", "aarch64"), Some("ells-macos-universal"));
        assert_eq!(asset_for("macos", "x86_64"), Some("ells-macos-universal"));
        assert_eq!(asset_for("linux", "x86_64"), Some("ells-linux-x86_64"));
        assert_eq!(asset_for("linux", "aarch64"), None);
        assert_eq!(asset_for("freebsd", "x86_64"), None);
    }

    #[test]
    fn age_label_stays_in_chinese_and_humans_scale() {
        assert_eq!(age_label(unix_now()), "刚刚");
        assert_eq!(age_label(unix_now() - 120), "2 分钟前");
        assert_eq!(age_label(unix_now() - 7200), "2 小时前");
        assert_eq!(age_label(unix_now() - 86400 * 3), "3 天前");
        // 时钟被往回调过（未来时间戳）也不能崩
        assert_eq!(age_label(unix_now() + 9999), "刚刚");
    }

    #[test]
    fn download_urls_tolerate_a_trailing_slash_on_the_mirror() {
        assert_eq!(
            join_download("https://mirror.local/ells", "v0.1.5", "ells-linux-x86_64"),
            "https://mirror.local/ells/v0.1.5/ells-linux-x86_64"
        );
        assert_eq!(
            join_download("https://mirror.local/ells//", "v0.1.5", "x"),
            "https://mirror.local/ells/v0.1.5/x"
        );
        // 默认走 GitHub，且版本号原样成段
        let url = asset_url("v0.1.5", "ells-linux-x86_64");
        assert!(url.ends_with("/v0.1.5/ells-linux-x86_64"), "{url}");
        assert!(url.starts_with("https://"), "{url}");
    }

    #[test]
    fn old_path_sits_next_to_the_executable() {
        let exe = Path::new(if cfg!(windows) { r"C:\bin\ells.exe" } else { "/usr/local/bin/ells" });
        let old = old_path(exe);
        assert_eq!(old.parent(), exe.parent());
        assert!(old.to_string_lossy().ends_with(".old"));
    }

    /// 每个自愈/回退测试各自的临时目录；名字带 pid，跑完即删。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ells-update-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_stale_launcher_copy_is_refreshed_from_the_running_one() {
        let dir = temp_dir("sibling");
        let [first, second] = LAUNCHER_NAMES;
        let exe = dir.join(second);
        let other = dir.join(first);
        std::fs::write(&exe, b"new binary").unwrap();
        std::fs::write(&other, b"stale binary").unwrap();

        heal_launchers_in_dir(&exe);

        assert_eq!(std::fs::read_to_string(&other).unwrap(), "new binary");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new binary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_launcher_name_is_created_beside_the_binary() {
        // 用户报的正是这个形状：`ells` 打得开、`s` 再也打不开。安装目录里两个启动名
        // 是安装脚本放下的，自愈不能只留一个。
        let dir = temp_dir("missing-launcher");
        let exe = dir.join(LAUNCHER_NAMES[0]);
        let other = dir.join(LAUNCHER_NAMES[1]);
        std::fs::write(&exe, b"new binary").unwrap();
        assert!(!other.exists());

        heal_launchers_in_dir(&exe);

        assert!(other.is_file(), "缺失的启动名要补回来");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "new binary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_running_binary_is_never_copied_onto_itself() {
        // 自己也在启动名清单里；拷回自己 = 边截断边读同一个 inode，会把刚装好的二进制写坏
        let dir = temp_dir("self");
        let exe = dir.join(LAUNCHER_NAMES[0]);
        std::fs::write(&exe, b"new binary").unwrap();

        heal_launchers_in_dir(&exe);

        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new binary", "自己不能被拷成零字节");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn healing_from_a_symlink_alias_does_not_split_the_launcher_in_two() {
        // 用户敲 `s` 起的其实是软链。把它当"另一个启动名"拷一份，两份就各跑各的版本了。
        let dir = temp_dir("alias");
        let main = dir.join("ells");
        let alias = dir.join("s");
        std::fs::write(&main, b"new binary").unwrap();
        std::os::unix::fs::symlink("ells", &alias).unwrap();

        heal_launchers_in_dir(&alias);

        assert!(
            std::fs::symlink_metadata(&alias).unwrap().file_type().is_symlink(),
            "软链不该被换成一份独立拷贝"
        );
        assert_eq!(std::fs::read_to_string(&main).unwrap(), "new binary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_launcher_symlink_is_relinked() {
        // 替换中途被截断时，`s` 会指着一个已经不存在的名字：命令还在 PATH 里，
        // 执行只会报"没有那个文件"，看起来就像"s 打不开、ells 能打开"。
        let dir = temp_dir("dangling");
        let exe = dir.join("ells");
        let link = dir.join("s");
        std::fs::write(&exe, b"new binary").unwrap();
        std::os::unix::fs::symlink("ells.gone", &link).unwrap();

        heal_launchers_in_dir(&exe);

        assert!(link.is_file(), "断链修好后应该能读到主二进制");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "new binary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pending_record_rolls_back_on_the_second_start() {
        // tag 一律用发布形状 `v0.2.4`、current 用 crate 形状 `0.2.4`：这就是盘上真实的
        // 两种写法，比字符串的话这条记录永远不会生效。
        let dir = temp_dir("recover");
        let exe = dir.join(LAUNCHER_NAMES[0]);
        let backup = old_path(&exe);
        std::fs::write(&exe, b"broken new").unwrap();
        std::fs::write(&backup, b"good old").unwrap();
        write_pending(&exe, &Pending { tag: "v0.2.4".into(), backup: backup.clone(), tries: 0 });

        // 换上新版本的第一次启动：先给它一次机会，只记次数
        assert_eq!(recover_from(&exe, "0.2.4"), Startup::Ready);
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "broken new");
        assert_eq!(read_pending(&exe).unwrap().tries, 1);

        // 第二次还是起不来：退回上一版，并把这一版记成"别再推给用户"
        assert_eq!(
            recover_from(&exe, "0.2.4"),
            Startup::RolledBack { tag: "v0.2.4".into() }
        );
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "good old");
        assert!(read_pending(&exe).is_none(), "退完不该留记录");
        assert_eq!(
            std::fs::read_to_string(rejected_path(&exe)).unwrap().trim(),
            "v0.2.4",
            "退回过的版本要记下来，否则会陷入 提示更新→再崩→再退回"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pending_record_from_another_version_is_dropped_not_acted_on() {
        // 记录里写着 v0.2.4、盘上跑的却是 v0.3.0（重装过、或又更新过一轮）。
        // 拿这条陈账去退体会把好端端的二进制换成更早的一份，所以当场作废。
        let dir = temp_dir("stale-pending");
        let exe = dir.join(LAUNCHER_NAMES[0]);
        let backup = old_path(&exe);
        std::fs::write(&exe, b"current").unwrap();
        std::fs::write(&backup, b"much older").unwrap();
        write_pending(&exe, &Pending { tag: "v0.2.4".into(), backup: backup.clone(), tries: 3 });

        assert_eq!(recover_from(&exe, "0.3.0"), Startup::Ready);
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "current");
        assert!(!pending_path(&exe).exists(), "陈账要当场清掉，不能留着下次误退");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_confirmed_frame_throws_the_backup_away_and_keeps_an_unrelated_one() {
        let dir = temp_dir("confirm");
        let exe = dir.join(LAUNCHER_NAMES[0]);
        let backup = old_path(&exe);
        std::fs::write(&exe, b"new").unwrap();
        std::fs::write(&backup, b"old").unwrap();
        write_pending(&exe, &Pending { tag: "v0.2.4".into(), backup: backup.clone(), tries: 1 });

        // 版本号对不上 = 这条不是给这份二进制画的帧，备份得留着
        confirm_for(&exe, "0.9.9");
        assert!(backup.is_file(), "对不上版本的确认不能删备份");

        confirm_for(&exe, "0.2.4");
        assert!(!backup.exists(), "新版被证明能出帧，退路就没用了");
        assert!(!pending_path(&exe).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_start_rolls_back_on_the_spot() {
        let dir = temp_dir("roll-back-now");
        let exe = dir.join(LAUNCHER_NAMES[0]);
        let backup = old_path(&exe);
        std::fs::write(&exe, b"broken new").unwrap();
        std::fs::write(&backup, b"good old").unwrap();
        write_pending(&exe, &Pending { tag: "v0.2.4".into(), backup: backup.clone(), tries: 1 });

        assert_eq!(roll_back_for(&exe, "0.2.4").as_deref(), Some("v0.2.4"));
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "good old");
        // 退过一次之后没有第二条记录可退，不能无限重启自己
        assert_eq!(roll_back_for(&exe, "0.2.4"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_rolls_back_when_the_backup_is_gone() {
        // 没有备份就没有退路：这时必须让当前这份跑下去，而不是把二进制挪没了
        let dir = temp_dir("no-backup");
        let exe = dir.join(LAUNCHER_NAMES[0]);
        std::fs::write(&exe, b"the only copy").unwrap();
        write_pending(
            &exe,
            &Pending { tag: "v0.2.4".into(), backup: old_path(&exe), tries: 5 },
        );

        assert_eq!(recover_from(&exe, "0.2.4"), Startup::Ready);
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "the only copy");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_moves_the_new_file_in_and_keeps_the_old_one_as_backup() {
        let dir = temp_dir("replace");
        let exe = dir.join(LAUNCHER_NAMES[0]);
        let tmp = dir.join("ells.update.1.tmp");
        std::fs::write(&exe, b"old binary").unwrap();
        std::fs::write(&tmp, b"new binary").unwrap();

        install_in_place(&exe, &tmp).expect("替换应成功");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new binary");
        assert!(!tmp.exists(), "临时件应被改名走，不该留下");
        // unix 上也要留这一挪：那是"新版换上却起不来"时唯一能退回的东西
        assert_eq!(std::fs::read_to_string(old_path(&exe)).unwrap(), "old binary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_replace_moves_the_original_back() {
        // 新二进制挪不上去（这里用"临时件压根不在盘上"造出来），旧的必须原样挪回，
        // 否则 ells / s 两个启动名会一起消失，用户连报错都看不到。
        let dir = temp_dir("restore");
        let exe = dir.join(LAUNCHER_NAMES[0]);
        std::fs::write(&exe, b"old binary").unwrap();
        let tmp = dir.join("no-such-dir").join("ells.update.1.tmp");

        assert!(install_in_place(&exe, &tmp).is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old binary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_replace_leaves_the_original_executable_in_place() {
        // 目标目录不存在 → rename 必失败；失败后原来的 exe 必须还在原位
        let dir = std::env::temp_dir().join(format!("ells-update-missing-{}", std::process::id()));
        let exe = dir.join("ells");
        let tmp = dir.join("ells.update.1.tmp");
        assert!(install_in_place(&exe, &tmp).is_err());
        assert!(!exe.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真链路冒烟（会下整个平台二进制，约 9MB）：`cargo test -p ells -- --ignored`。
    /// 目标是临时目录里的假 exe，绝不动正在运行的自己。
    #[test]
    #[ignore = "需要下载发布二进制"]
    fn installed_asset_matches_the_published_checksum() {
        let asset = asset_name().expect("CI 跑在已发布资产覆盖的平台上");
        let dir = std::env::temp_dir().join(format!("ells-apply-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(if cfg!(windows) { "ells.exe" } else { "ells" });
        std::fs::write(&exe, b"placeholder-old-ells").unwrap();

        let cancel = AtomicBool::new(false);
        let mut ticks = 0usize;
        apply_to(&exe, "v0.1.5", &cancel, &mut |_, _| ticks += 1).expect("更新应完成");

        let want = fetch_sum("v0.1.5", asset).expect("发布资产应有校验条目");
        assert_eq!(digest_of(&exe).as_deref(), Some(want.as_str()), "换上去的不是发布的那份文件");
        assert!(std::fs::metadata(&exe).unwrap().len() > 1 << 20, "不像一个真二进制");
        assert!(ticks > 1, "进度回调至少要有几拍：{ticks}");
        assert!(old_path(&exe).exists(), "旧镜像应被挪成 .old 而不是丢掉");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真网络冒烟：跑 `cargo test -- --ignored`。校验和是发布时算好的，写死在这里
    /// 才能证明"解析 + 下载 + 摘要"三件事整体没错。
    #[test]
    #[ignore = "需要联网访问 GitHub"]
    fn published_checksums_parse_against_the_real_release() {
        let asset = "ells-linux-x86_64";
        let want = "cd429e85cb1032feb4a0a35dfe035108a55a72c4fefb99c43ad67049bdac6a47";
        let got = fetch_sum("v0.1.5", asset).expect("应能取到 v0.1.5 的校验条目");
        assert_eq!(got, want);
        // 探测走的是 HEAD 重定向，不计 API 限额；不断言具体版本号（会随发版过期）
        assert!(probe_tag().unwrap().starts_with('v'));
    }
}

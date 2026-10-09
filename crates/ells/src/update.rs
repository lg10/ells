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
/// 调用方要提示重启。
pub fn apply(
    tag: &str,
    cancel: &AtomicBool,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    apply_to(&exe_path()?, tag, cancel, progress)
}

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

/// 用新二进制顶掉正在运行的自己。
///
/// Windows 不许删除运行中的 exe，但允许改名，所以先把它挪成 `ells.exe.old`
/// （下次启动由 `cleanup_leftovers` 清掉）；替换失败必须原样挪回，
/// 否则用户的 `s` / `ells` 命令会直接消失。
fn install_in_place(exe: &Path, tmp: &Path) -> Result<(), String> {
    let moved_aside = cfg!(windows);
    if moved_aside {
        let old = old_path(exe);
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).map_err(|e| {
            format!("无法挪开正在运行的 {}：{e}（Windows 偶发占用，稍后重试即可）", exe.display())
        })?;
    }
    if let Err(e) = std::fs::rename(tmp, exe) {
        if moved_aside {
            let _ = std::fs::rename(old_path(exe), exe);
        }
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

/// 清掉上一次更新留下的备份件与崩溃残留的临时件（临时件只删一天前的，
/// 免得误杀另一个正在下载的 ells 实例）。
pub fn cleanup_leftovers() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::fs::remove_file(old_path(&exe));
    let Some(dir) = exe.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = unix_now().saturating_sub(86400);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("ells.update.") && name.ends_with(".tmp")) {
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
/// "Input/output error (os error 5)"。Windows 控制台没有这套进程组语义，维持 spawn。
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
        cmd.spawn()
            .map(|_| ())
            .map_err(|e| format!("新版本启动失败：{e}（请手动运行 {}）", exe.display()))
    }
}

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

    #[test]
    fn replace_moves_the_new_file_in_and_backs_the_old_one_up() {
        let dir = std::env::temp_dir().join(format!("ells-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(if cfg!(windows) { "ells.exe" } else { "ells" });
        let tmp = dir.join("ells.update.1.tmp");
        std::fs::write(&exe, b"old binary").unwrap();
        std::fs::write(&tmp, b"new binary").unwrap();

        install_in_place(&exe, &tmp).expect("替换应成功");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new binary");
        assert!(!tmp.exists(), "临时件应被改名走，不该留下");
        if cfg!(windows) {
            // Windows 上旧镜像被挪成 .old，不能凭空消失
            assert!(old_path(&exe).exists());
        }
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
        assert_eq!(digest_of(&exe), want, "换上去的不是发布的那份文件");
        assert!(std::fs::metadata(&exe).unwrap().len() > 1 << 20, "不像一个真二进制");
        assert!(ticks > 1, "进度回调至少要有几拍：{ticks}");
        if cfg!(windows) {
            assert!(old_path(&exe).exists(), "旧镜像应被挪成 .old 而不是丢掉");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn digest_of(path: &Path) -> String {
        let mut file = std::fs::File::open(path).unwrap();
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            ctx.update(&buf[..n]);
        }
        ctx.finish()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
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

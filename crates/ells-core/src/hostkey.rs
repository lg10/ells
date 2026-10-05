//! 主机密钥信任：TOFU（第一次见到才记录）+ known_hosts 校验 + 变更告警。
//!
//! 记录写在 `~/.ells/known_hosts`（OpenSSH 兼容格式，可直接被 `ssh` 读取），
//! 并只读地参考 `~/.ssh/known_hosts`：用户已经用 OpenSSH 信任过的机器
//! 不会在 ells 里再问一次。`~/.ssh/known_hosts` 永不被 ells 修改。

use std::io::Write;
use std::path::{Path, PathBuf};

use russh::keys::{HashAlg, PublicKey};
use tokio::sync::{mpsc, oneshot};

/// 一次主机密钥决策请求，由 UI 回答（true=接受并记录）。
pub struct HostKeyPrompt {
    pub host: String,
    pub port: u16,
    /// 如 `ssh-ed25519`
    pub algorithm: String,
    /// 如 `SHA256:AbC…`（OpenSSH 同款）
    pub fingerprint: String,
    pub trust: KeyTrust,
    pub responder: oneshot::Sender<bool>,
}

// responder 不可 Debug：手工实现，保证 AppEvent 仍能 derive(Debug)
impl std::fmt::Debug for HostKeyPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostKeyPrompt")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("algorithm", &self.algorithm)
            .field("fingerprint", &self.fingerprint)
            .field("trust", &self.trust)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyTrust {
    /// 没有任何记录：首次连接（TOFU 询问）
    Unknown,
    /// 与记录一致：直接放行
    Matched,
    /// 记录里是另一把密钥：可能是服务器重装，也可能是中间人
    Changed,
}

/// 连接期共享的主机密钥策略。
#[derive(Clone)]
pub struct HostKeyPolicy {
    prompts: mpsc::UnboundedSender<HostKeyPrompt>,
    accept_new: bool,
    trust_all: bool,
}

impl HostKeyPolicy {
    pub fn new(prompts: mpsc::UnboundedSender<HostKeyPrompt>, accept_new: bool) -> Self {
        Self { prompts, accept_new, trust_all: false }
    }

    /// 全部放行：既不询问也不落盘。仅供冒烟测试连接本地 fake_sshd 使用。
    pub fn trust_all() -> Self {
        Self {
            prompts: mpsc::unbounded_channel().0,
            accept_new: false,
            trust_all: true,
        }
    }

    /// 校验一把服务器密钥；必要时向 UI 发问并等待回答。
    pub async fn verify(&self, host: &str, port: u16, key: &PublicKey) -> bool {
        if self.trust_all {
            return true;
        }
        let trust = evaluate(host, port, key);
        if trust == KeyTrust::Matched {
            return true;
        }
        if trust == KeyTrust::Unknown && self.accept_new {
            if let Err(err) = record(host, port, key) {
                tracing::warn!(%err, "写入 known_hosts 失败");
            }
            return true;
        }
        let (tx, rx) = oneshot::channel();
        let prompt = HostKeyPrompt {
            host: host.to_string(),
            port,
            algorithm: key.algorithm().as_str().to_string(),
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
            trust,
            responder: tx,
        };
        if self.prompts.send(prompt).is_err() {
            // UI 已退出：宁可断开也不盲信
            tracing::warn!("没有可用的主机密钥确认界面，拒绝连接");
            return false;
        }
        let accepted = rx.await.unwrap_or(false);
        if accepted {
            if let Err(err) = replace(host, port, key) {
                tracing::warn!(%err, "更新 known_hosts 失败");
            }
        }
        accepted
    }
}

/// ells 自己的 known_hosts 路径。
pub fn our_known_hosts() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ells").join("known_hosts"))
}

/// 查询顺序：ells 的记录优先，其次 OpenSSH 的（只读）。
fn search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(p) = our_known_hosts() {
        paths.push(p);
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".ssh").join("known_hosts"));
    }
    paths
}

/// 在 known_hosts 里查这把密钥的状态。
pub fn evaluate(host: &str, port: u16, key: &PublicKey) -> KeyTrust {
    evaluate_in(&search_paths(), host, port, key)
}

/// 只比较**同算法**的记录：一台服务器通常同时记有 ed25519/rsa/ecdsa 多把密钥，
/// 若按"任意记录不同"判定，服务器只是换了 offered 算法就会误报成中间人告警。
fn evaluate_in(paths: &[PathBuf], host: &str, port: u16, key: &PublicKey) -> KeyTrust {
    let host_port = host_prefix(host, port);
    let algorithm = key.algorithm().as_str().to_string();
    let mut changed = false;
    for path in paths {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            let Some(record) = parse_line(line) else { continue };
            if !record.matches(&host_port) || record.algorithm != algorithm {
                continue;
            }
            match &record.key {
                Some(k) if same_material(k, key) => return KeyTrust::Matched,
                // 密钥本体解析不出来（未知算法/坏 base64）：保守当作不一致，
                // 结果是弹确认框而不是盲信。
                _ => changed = true,
            }
        }
    }
    if changed { KeyTrust::Changed } else { KeyTrust::Unknown }
}

/// 只比密钥本体：`PublicKey` 的相等还包含注释字段，而注释在"服务器送来的键"
/// （空注释）与"从 known_hosts 解析的键"（可能带注释）之间天然不同，直接 `==` 会漏判。
fn same_material(a: &PublicKey, b: &PublicKey) -> bool {
    match (a.to_openssh(), b.to_openssh()) {
        (Ok(x), Ok(y)) => blob(&x) == blob(&y),
        _ => false,
    }
}

/// OpenSSH 公钥行的第二段（base64 本体）。
fn blob(line: &str) -> &str {
    line.split_whitespace().nth(1).unwrap_or("")
}

/// 一条 known_hosts 记录：主机字段（逗号分隔的多个模式）+ 算法 + 公钥本体。
struct Record<'a> {
    hosts: &'a str,
    algorithm: String,
    key: Option<PublicKey>,
}

impl Record<'_> {
    fn matches(&self, host_port: &str) -> bool {
        // 哈希主机条目（`|1|…`）无法在此比对（需要 HMAC-SHA1），按"无记录"处理：
        // 最多多问一次指纹，不会盲信。
        self.hosts.split(',').any(|h| h == host_port)
    }
}

/// 拆一行 known_hosts。选项前缀（`@cert-authority`、`no-pty` 之类）通过
/// "定位算法名 token"来跳过；认不出算法名的行返回 None（当作无法解析，忽略该行而不是整个文件）。
fn parse_line(line: &str) -> Option<Record<'_>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let pos = tokens.iter().position(|t| is_algorithm_token(t))?;
    if pos == 0 || tokens.len() <= pos + 1 {
        return None;
    }
    let algorithm = tokens[pos].to_string();
    let key = russh::keys::parse_public_key_base64(tokens[pos + 1]).ok();
    Some(Record { hosts: tokens[pos - 1], algorithm, key })
}

fn is_algorithm_token(token: &str) -> bool {
    token.starts_with("ssh-") || token.starts_with("ecdsa-") || token.starts_with("sk-")
}

/// 追加一条记录（`~/.ells/known_hosts`，权限 0600）。
pub fn record(host: &str, port: u16, key: &PublicKey) -> anyhow::Result<()> {
    let path = our_known_hosts().ok_or_else(|| anyhow::anyhow!("无法定位用户主目录"))?;
    record_at(&path, host, port, key)
}

fn record_at(path: &Path, host: &str, port: u16, key: &PublicKey) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let line = format!("{} {}\n", host_prefix(host, port), key.to_openssh()?);
    append_line(path, line.as_bytes())
}

/// 记录新密钥，并删除同一 host:port 的旧密钥行（用户确认"确实换了"）。
pub fn replace(host: &str, port: u16, key: &PublicKey) -> anyhow::Result<()> {
    let path = our_known_hosts().ok_or_else(|| anyhow::anyhow!("无法定位用户主目录"))?;
    replace_at(&path, host, port, key)
}

fn replace_at(path: &Path, host: &str, port: u16, key: &PublicKey) -> anyhow::Result<()> {
    drop_host_lines_at(path, &host_prefix(host, port), key.algorithm().as_str())?;
    record_at(path, host, port, key)
}

/// 删除该主机前缀下**同算法**的记录行；其他算法的记录（ed25519/rsa 并存）保留。
fn drop_host_lines_at(path: &Path, prefix: &str, algorithm: &str) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let kept: String = text
        .lines()
        .filter(|l| match parse_line(l) {
            Some(record) => !(record.matches(prefix) && record.algorithm == algorithm),
            // 解析不出来的行原样保留：不因为一次替换弄丢用户手写的内容
            None => true,
        })
        .map(|l| format!("{l}\n"))
        .collect();
    write_secret_file(path, kept.as_bytes())
}

/// OpenSSH 的写法：默认端口不带端口号，其余用 `[host]:port`。
fn host_prefix(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

fn append_line(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut out = std::fs::read(path).unwrap_or_default();
    if !out.is_empty() && out.last() != Some(&b'\n') {
        out.push(b'\n');
    }
    out.extend_from_slice(bytes);
    write_secret_file(path, &out)
}

fn write_secret_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::PrivateKey;

    fn sample_key(seed: u8) -> PublicKey {
        use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
        let mut seed_bytes = [7u8; 32];
        seed_bytes[0] = seed;
        let keypair = Ed25519Keypair::from_seed(&seed_bytes);
        PrivateKey::new(KeypairData::Ed25519(keypair), "test")
            .expect("ed25519 key")
            .public_key()
            .clone()
    }

    #[test]
    fn host_prefix_matches_openssh() {
        assert_eq!(host_prefix("example.com", 22), "example.com");
        assert_eq!(host_prefix("example.com", 2222), "[example.com]:2222");
    }

    #[test]
    fn evaluate_reports_unknown_matched_and_changed() {
        let dir = std::env::temp_dir().join(format!("ells-known-hosts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_hosts");
        let key = sample_key(1);
        let other = sample_key(2);

        record_at(&path, "h1", 2200, &key).unwrap();
        let paths = vec![path.clone()];
        assert_eq!(evaluate_in(&paths, "h1", 2200, &key), KeyTrust::Matched);
        assert_eq!(evaluate_in(&paths, "h1", 2200, &other), KeyTrust::Changed);
        assert_eq!(evaluate_in(&paths, "h2", 2200, &key), KeyTrust::Unknown);
        // 端口不同的同名主机是另一条记录
        assert_eq!(evaluate_in(&paths, "h1", 22, &key), KeyTrust::Unknown);

        // 确认换密钥后：旧行被替换，新密钥变为 Matched
        replace_at(&path, "h1", 2200, &other).unwrap();
        assert_eq!(evaluate_in(&paths, "h1", 2200, &other), KeyTrust::Matched);
        // 旧密钥再来就是"密钥变更"告警，而不是"首次连接"
        assert_eq!(evaluate_in(&paths, "h1", 2200, &key), KeyTrust::Changed);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1, "旧密钥应被删除: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appends_newline_when_file_lacks_one() {
        let dir = std::env::temp_dir().join(format!("ells-kh-nl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_hosts");
        std::fs::write(&path, b"old-entry ssh-ed25519 AAAA").unwrap();
        append_line(&path, b"appended\n").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一台机器同时记有别的算法（rsa）不应该变成中间人告警。
    #[test]
    fn other_algorithm_records_do_not_fake_a_change() {
        let dir = std::env::temp_dir().join(format!("ells-kh-algo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_hosts");
        std::fs::write(
            &path,
            b"h1 ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQDfakefakefake==\n",
        )
        .unwrap();
        let key = sample_key(1);
        let line = format!("h1 {} {}\n", key.algorithm().as_str(), blob(&key.to_openssh().unwrap()));
        std::fs::write(&path, format!("{}{}", std::fs::read_to_string(&path).unwrap(), line))
            .unwrap();
        let paths = vec![path.clone()];
        assert_eq!(evaluate_in(&paths, "h1", 22, &key), KeyTrust::Matched);
        assert_eq!(evaluate_in(&paths, "h9", 22, &key), KeyTrust::Unknown);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 带选项前缀的行（`no-pty`/`cert-authority` 等）也要能认出主机字段。
    #[test]
    fn parse_line_skips_option_markers() {
        let record = parse_line("@cert-authority *.example.com ssh-ed25519 AAAA").unwrap();
        assert_eq!(record.hosts, "*.example.com");
        assert_eq!(record.algorithm, "ssh-ed25519");
        assert!(record.key.is_none());
        assert!(parse_line("# 注释").is_none());
        assert!(parse_line("只有两个字段").is_none());
    }
}

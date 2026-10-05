//! OpenSSH 客户端配置（~/.ssh/config）导入。
//!
//! 只认 ells 真正用得上的键：Host / HostName / Port / User / IdentityFile /
//! ProxyJump。其余键（Ciphers、RemoteCommand、Match…）忽略而不报错——ells 不是
//! ssh(1)，静默忽略未知键比拒绝整份文件更有用。
//!
//! 两条刻意的取舍：
//! - 通配段（`Host *`）只作为全局默认值下发，本身不生成条目（它不是具体主机）；
//! - 一行 `Host a b c` 会生成 3 条主机，别名各留各的，与 ssh 的用法一致。

use crate::host::{Auth, Host};

#[derive(Debug, Default, Clone)]
struct Block {
    patterns: Vec<String>,
    hostname: Option<String>,
    port: Option<String>,
    user: Option<String>,
    identity: Vec<String>,
    jump: Option<String>,
}

/// 解析配置文本，返回可直接入库的主机列表（不含密码，密钥路径已展开 `~`）。
pub fn parse(text: &str) -> Vec<Host> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut cur = Block::default();
    for raw in text.lines() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = split_kv(&line) else {
            continue;
        };
        match key.to_ascii_lowercase().as_str() {
            "host" => {
                blocks.push(std::mem::take(&mut cur));
                cur.patterns = value.split_whitespace().map(|s| unquote(s)).collect();
            }
            "hostname" => cur.hostname = Some(unquote(value)),
            "port" => cur.port = Some(unquote(value)),
            "user" => cur.user = Some(unquote(value)),
            // IdentityFile 允许重复出现，按声明顺序收集，最后挑一个存在的
            "identityfile" => cur.identity.push(unquote(value)),
            "proxyjump" => cur.jump = Some(unquote(value)),
            _ => {}
        }
    }
    blocks.push(cur);

    // 默认值 = 首个 Host 之前的全局段 + 所有不产生具体条目的段（`Host *`、`Host *2 *`）
    let mut defaults = Block::default();
    for block in &blocks {
        if block.patterns.iter().all(|p| is_wildcard(p)) {
            merge_defaults(&mut defaults, block);
        }
    }

    let mut out = Vec::new();
    for block in &blocks {
        for pattern in &block.patterns {
            if is_wildcard(pattern) {
                continue;
            }
            let hostname = block
                .hostname
                .as_deref()
                .or(defaults.hostname.as_deref())
                .unwrap_or(pattern)
                .to_string();
            let port = block
                .port
                .as_deref()
                .or(defaults.port.as_deref())
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(22);
            let user = block
                .user
                .as_deref()
                .or(defaults.user.as_deref())
                .map(|s| s.to_string())
                .unwrap_or_else(whoami);
            let identity = pick_identity(block, &defaults);
            let auth = match &identity {
                Some(path) => Auth::PrimaryKey {
                    path: crate::ssh::expand_tilde(path),
                    passphrase: None,
                },
                None => Auth::Password,
            };
            let jump = block
                .jump
                .as_deref()
                .or(defaults.jump.as_deref())
                .filter(|j| !j.is_empty() && !is_wildcard(j))
                .map(|s| s.to_string());
            out.push(Host {
                alias: pattern.clone(),
                hostname,
                port,
                user,
                auth,
                password: None,
                jump,
                note: Some("来自 ~/.ssh/config".to_string()),
            });
        }
    }
    out
}

/// `IdentityFile ~/a ~/b` 一行可以给多个候选：取第一个真实存在的。
fn pick_identity(block: &Block, defaults: &Block) -> Option<String> {
    let candidates: Vec<String> = block
        .identity
        .iter()
        .chain(defaults.identity.iter())
        .cloned()
        .collect();
    candidates
        .iter()
        .find(|p| std::fs::metadata(crate::ssh::expand_tilde(p)).is_ok())
        .or(candidates.first())
        .cloned()
}

fn merge_defaults(defaults: &mut Block, block: &Block) {
    let pick = |dst: &mut Option<String>, src: &Option<String>| {
        if src.is_some() {
            *dst = src.clone();
        }
    };
    pick(&mut defaults.hostname, &block.hostname);
    pick(&mut defaults.port, &block.port);
    pick(&mut defaults.user, &block.user);
    pick(&mut defaults.jump, &block.jump);
    for id in &block.identity {
        if !defaults.identity.iter().any(|d| d == id) {
            defaults.identity.push(id.clone());
        }
    }
}

fn is_wildcard(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.starts_with('!')
}

/// OpenSSH 的分隔符可省：`Key value`、`Key=value`、`Key = value` 都合法，
/// 而 `Host a=b` 里的 `=` 属于值本身（模式名可以含 '='）。
fn split_kv(line: &str) -> Option<(&str, &str)> {
    let sep = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let key = line[..sep].trim();
    if key.is_empty() {
        return None;
    }
    let mut value = line[sep..].trim_start();
    if let Some(rest) = value.strip_prefix('=') {
        value = rest.trim_start();
    }
    Some((key, value))
}

fn strip_comment(line: &str) -> &str {
    // 只有行首或空白后的 # 才是注释，避免吃掉 `Host a#b` 这种合法主机名
    let bytes = line.as_bytes();
    for (i, c) in bytes.iter().enumerate() {
        if *c == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            return &line[..i];
        }
    }
    line
}

fn unquote(value: &str) -> String {
    let v = value.trim();
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return v[1..v.len() - 1].to_string();
        }
    }
    v.to_string()
}

/// User 缺省时用本机登录名（与 ssh(1) 行为一致）；拿不到就留空让用户补。
fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
}

/// 读取并解析用户自己的 OpenSSH 配置；文件不存在返回空列表。
pub fn load_user_config() -> Vec<Host> {
    let Some(path) = dirs::home_dir().map(|h| h.join(".ssh").join("config")) else {
        return Vec::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(err) => {
            tracing::debug!(%err, path = %path.display(), "没有可导入的 ssh 配置");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_block() {
        let text = r#"
Host web
    HostName 203.0.113.7
    Port 2222
    User deploy
    IdentityFile ~/.ssh/id_ed25519
    ProxyJump bastion
    Ciphers chacha20-poly1305@openssh.com
"#;
        let hosts = parse(text);
        assert_eq!(hosts.len(), 1);
        let h = &hosts[0];
        assert_eq!((h.alias.as_str(), h.hostname.as_str()), ("web", "203.0.113.7"));
        assert_eq!(h.port, 2222);
        assert_eq!(h.user, "deploy");
        assert!(matches!(&h.auth, Auth::PrimaryKey { path, .. } if path.ends_with("id_ed25519")));
        assert_eq!(h.jump.as_deref(), Some("bastion"));
    }

    #[test]
    fn multiple_aliases_and_wildcards_are_expanded() {
        let text = "Host *2 *\n  User ops\nHost a b\n  HostName h.example\n";
        let hosts = parse(text);
        let aliases: Vec<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(aliases, ["a", "b"]);
        assert!(hosts.iter().all(|h| h.user == "ops"));
        assert_eq!(hosts[0].hostname, "h.example");
    }

    #[test]
    fn missing_identity_falls_back_to_password() {
        let hosts = parse("Host plain\n  HostName 198.51.100.9\n  User root\n");
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].auth, Auth::Password);
    }

    #[test]
    fn inline_comments_and_equals_syntax_are_accepted() {
        let text = "# 全局注释\nHost c\n  HostName=10.0.0.5 # 行尾注释\nUser tan\n";
        let hosts = parse(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "c");
        assert_eq!(hosts[0].hostname, "10.0.0.5");
        assert_eq!(hosts[0].user, "tan");
    }
}

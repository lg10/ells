//! OpenSSH 客户端配置（~/.ssh/config）的双向读写。
//!
//! 只认 ells 真正用得上的键：Host / HostName / Port / User / IdentityFile /
//! ProxyJump / LocalForward / RemoteForward / DynamicForward。其余键（Ciphers、
//! RemoteCommand、Match…）忽略而不报错——ells 不是 ssh(1)，静默忽略未知键比拒绝
//! 整份文件更有用。
//!
//! 三条刻意的取舍：
//! - 通配段（`Host *`）只作为全局默认值下发，本身不生成条目（它不是具体主机）；
//! - 一行 `Host a b c` 会生成 3 条主机，别名各留各的，与 ssh 的用法一致；
//! - **导出绝不写密码**：密码只存在于加密保险库里，导出的 ssh_config 里
//!   用注释标出"该主机由 ells 托管凭据"，避免一次导出把凭据泄成明文文件。

use crate::host::{Auth, EndpointKind, Forward, Host};

#[derive(Debug, Default, Clone)]
struct Block {
    patterns: Vec<String>,
    hostname: Option<String>,
    port: Option<String>,
    user: Option<String>,
    identity: Vec<String>,
    jump: Option<String>,
    forwards: Vec<Forward>,
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
            // 转发指令可重复出现，且远程/本地/动态三种都要接住
            "localforward" => {
                if let Some(f) = Forward::parse_endpoint(EndpointKind::Local, unquote(value).as_str())
                {
                    cur.forwards.push(f);
                }
            }
            "remoteforward" => {
                if let Some(f) =
                    Forward::parse_endpoint(EndpointKind::Remote, unquote(value).as_str())
                {
                    cur.forwards.push(f);
                }
            }
            "dynamicforward" => {
                if let Some(f) = Forward::parse_dynamic(&unquote(value)) {
                    cur.forwards.push(f);
                }
            }
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
            let mut forwards = block.forwards.clone();
            for f in &defaults.forwards {
                if !forwards.iter().any(|existing| existing == f) {
                    forwards.push(f.clone());
                }
            }
            out.push(Host {
                alias: pattern.clone(),
                hostname,
                port,
                user,
                auth,
                password: None,
                jump,
                note: Some("来自 ~/.ssh/config".to_string()),
                forwards,
                ..Default::default()
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
    for f in &block.forwards {
        if !defaults.forwards.iter().any(|d| d == f) {
            defaults.forwards.push(f.clone());
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
    let Some(path) = user_config_path() else {
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

fn user_config_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".ssh").join("config"))
}

/// 界面导出的落点：`~/.ells/ssh_config.export`。
///
/// 只写到 ells 自己的目录，绝不碰 `~/.ssh/config` 本体——那份文件属于用户，
/// 追加一段就等于替别人改全局 SSH 行为（`Host` 别名撞名会静默生效）。
pub fn export_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".ells").join("ssh_config.export"))
}

/// 把 ells 托管的主机渲染成 ssh_config 片段（`ells export` / 列表页导出）。
///
/// 只写 ells 能负责的部分：认证走密钥就写 IdentityFile，走密码就留一行注释说明
/// 凭据在保险库里 —— 密码本身永不进这份文本。
pub fn to_config_text(hosts: &[Host]) -> String {
    let mut out = String::from("# 由 ells 导出。密码类主机的凭据留在 ~/.ells/vault.bin 中。\n");
    for host in hosts {
        out.push_str(&format!("Host {}\n", host.alias));
        if host.hostname != host.alias {
            out.push_str(&format!("    HostName {}\n", host.hostname));
        }
        if host.port != 22 {
            out.push_str(&format!("    Port {}\n", host.port));
        }
        if !host.user.is_empty() {
            out.push_str(&format!("    User {}\n", host.user));
        }
        match &host.auth {
            Auth::PrimaryKey { path, .. } => {
                out.push_str(&format!("    IdentityFile {path}\n"));
            }
            Auth::Password => {
                out.push_str("    # ells 托管密码（此文件不含凭据）\n");
            }
            Auth::Agent => {
                out.push_str("    # ells 通过 ssh-agent 认证\n");
            }
        }
        if let Some(jump) = &host.jump {
            out.push_str(&format!("    ProxyJump {jump}\n"));
        }
        for f in &host.forwards {
            match f {
                Forward::Local { .. } => {
                    out.push_str(&format!("    LocalForward {}\n", forward_spec(f)))
                }
                Forward::Remote { .. } => {
                    out.push_str(&format!("    RemoteForward {}\n", forward_spec(f)))
                }
                Forward::Dynamic { .. } => {
                    out.push_str(&format!("    DynamicForward {}\n", forward_spec(f)))
                }
            }
        }
        if let Some(group) = &host.group {
            out.push_str(&format!("    # ells 分组: {group}\n"));
        }
        if !host.tags.is_empty() {
            out.push_str(&format!("    # ells 标签: {}\n", host.tags.join(",")));
        }
        out.push('\n');
    }
    out
}

/// ssh_config 里不带 `-L/-R/-D` 前缀的转发值本体。
fn forward_spec(f: &Forward) -> String {
    let bind = match f.bind() {
        Some(b) if !b.is_empty() => format!("{b}:"),
        _ => String::new(),
    };
    match f {
        Forward::Local { listen_port, dest_host, dest_port, .. }
        | Forward::Remote { listen_port, dest_host, dest_port, .. } => {
            format!("{bind}{listen_port}:{dest_host}:{dest_port}")
        }
        Forward::Dynamic { listen_port, .. } => format!("{bind}{listen_port}"),
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

    /// 三种转发都要接住；`Host *` 段里的转发追加在主机自己的之后。
    #[test]
    fn forwards_are_parsed_and_inherited_from_wildcard() {
        let text = r#"
Host *
    LocalForward 1088:localhost:1088
Host web
    HostName 10.0.0.5
    User root
    LocalForward 0.0.0.0:8080:localhost:80
    RemoteForward 9000:backup.internal:22
    DynamicForward 1080
"#;
        let hosts = parse(text);
        let web = hosts.iter().find(|h| h.alias == "web").unwrap();
        assert_eq!(
            web.forwards,
            vec![
                Forward::Local {
                    bind: Some("0.0.0.0".into()),
                    listen_port: 8080,
                    dest_host: "localhost".into(),
                    dest_port: 80,
                },
                Forward::Remote {
                    bind: None,
                    listen_port: 9000,
                    dest_host: "backup.internal".into(),
                    dest_port: 22,
                },
                Forward::Dynamic { bind: None, listen_port: 1080 },
                Forward::Local {
                    bind: None,
                    listen_port: 1088,
                    dest_host: "localhost".into(),
                    dest_port: 1088,
                },
            ]
        );
        // 通配段自己不生成条目
        assert_eq!(hosts.len(), 1);
        assert_eq!(
            web.forwards.first().unwrap().bind_address(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
        );
        assert_eq!(
            web.forwards[3].bind_address(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
    }

    #[test]
    fn malformed_forward_specs_are_dropped() {
        assert!(Forward::parse_endpoint(EndpointKind::Local, "").is_none());
        assert!(Forward::parse_endpoint(EndpointKind::Local, "8080:localhost").is_none());
        assert!(Forward::parse_endpoint(EndpointKind::Local, "notaport:h:22").is_none());
        assert!(Forward::parse_dynamic("1080:extra:more").is_none());
        assert_eq!(
            Forward::parse_dynamic("127.0.0.1:1080"),
            Some(Forward::Dynamic { bind: Some("127.0.0.1".into()), listen_port: 1080 })
        );
    }

    /// IPv6 目标自带冒号，方括号内的冒号不能当分隔符。
    #[test]
    fn ipv6_destination_keeps_its_colons() {
        let f = Forward::parse_endpoint(EndpointKind::Local, "8080:[::1]:6379").unwrap();
        assert_eq!(
            f,
            Forward::Local {
                bind: None,
                listen_port: 8080,
                dest_host: "[::1]".into(),
                dest_port: 6379
            }
        );
    }

    #[test]
    fn export_never_writes_passwords() {
        let hosts = vec![
            Host {
                alias: "web".into(),
                hostname: "10.0.0.5".into(),
                port: 2222,
                user: "deploy".into(),
                auth: Auth::Password,
                password: Some("hunter2".into()),
                jump: Some("bastion".into()),
                forwards: vec![Forward::Local {
                    bind: None,
                    listen_port: 8080,
                    dest_host: "localhost".into(),
                    dest_port: 80,
                }],
                ..Default::default()
            },
            Host {
                alias: "key".into(),
                hostname: "key.example".into(),
                port: 22,
                user: "root".into(),
                auth: Auth::PrimaryKey {
                    path: "/home/dev/.ssh/id_ed25519".into(),
                    passphrase: None,
                },
                ..Default::default()
            },
            Host {
                alias: "db.internal".into(),
                hostname: "db.internal".into(),
                port: 22,
                user: "dba".into(),
                auth: Auth::Agent,
                ..Default::default()
            },
        ];
        let text = to_config_text(&hosts);
        assert!(!text.contains("hunter2"), "导出的配置里绝不能出现密码");
        assert!(text.contains("Host web"));
        assert!(text.contains("    Port 2222"));
        assert!(text.contains("    ProxyJump bastion"));
        assert!(text.contains("    LocalForward 8080:localhost:80"));
        assert!(text.contains("    IdentityFile /home/dev/.ssh/id_ed25519"));
        assert!(text.contains("    HostName 10.0.0.5"));
        assert!(text.contains("    HostName key.example"));
        // 别名与主机名相同时省略 HostName：ssh 本来就按别名解析
        assert!(!text.contains("HostName db.internal"));

        // 导出的片段能被自己的解析器读回来（转发、端口、跳板都不丢）
        let back = parse(&text);
        let web = back.iter().find(|h| h.alias == "web").unwrap();
        assert_eq!(web.port, 2222);
        assert_eq!(web.jump.as_deref(), Some("bastion"));
        assert_eq!(web.forwards.len(), 1);
        assert_eq!(web.hostname, "10.0.0.5");
    }
}

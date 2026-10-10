use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Auth {
    Password,
    PrimaryKey {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        passphrase: Option<String>,
    },
    Agent,
}

/// 一条端口转发规则，对应 `ssh -L` / `-R` / `-D`。
///
/// `bind` 为 `None` 时只监听回环（与 ssh 的默认一致）；`Some("*")` 表示所有网卡，
/// 这在内网跳板上把端口暴露给局域网时才需要，所以不能省掉这个字段。
///
/// `listen_port` 写 `0` 是"本地口交给系统分配"，与 `ssh -L 0:db:5432` 同义：
/// 两台主机都想占 8080 时，留空就不撞。已分配到的实际端口由隧道上报给界面。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Forward {
    /// 本地监听 `listen_port`（`0` = 系统分配），经服务器送到 `dest_host:dest_port`（服务器视角解析）。
    Local {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bind: Option<String>,
        listen_port: u16,
        dest_host: String,
        dest_port: u16,
    },
    /// 服务器侧监听 `listen_port`（`0` = 服务器分配），回送到本地/目标 `dest_host:dest_port`（`ssh -R`）。
    Remote {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bind: Option<String>,
        listen_port: u16,
        dest_host: String,
        dest_port: u16,
    },
    /// 本地 SOCKS5 动态代理（`ssh -D`）：目标地址由客户端在连接时给出。
    Dynamic {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bind: Option<String>,
        listen_port: u16,
    },
}

impl Forward {
    /// 本地监听端口（`Remote` 没有本地监听口，返回 `None`）。
    pub fn local_listen_port(&self) -> Option<u16> {
        match self {
            Self::Local { listen_port, .. } | Self::Dynamic { listen_port, .. } => Some(*listen_port),
            Self::Remote { .. } => None,
        }
    }

    pub fn bind(&self) -> Option<&str> {
        match self {
            Self::Local { bind, .. } | Self::Remote { bind, .. } | Self::Dynamic { bind, .. } => {
                bind.as_deref()
            }
        }
    }

    /// 本地口是否交给系统分配（`listen_port == 0`）。
    pub fn auto_port(&self) -> bool {
        match self {
            Self::Local { listen_port, .. }
            | Self::Remote { listen_port, .. }
            | Self::Dynamic { listen_port, .. } => *listen_port == 0,
        }
    }

    /// 它占住的固定本地监听口；自动分配的口（`0`）不占固定口，返回 `None`。
    pub fn fixed_local_port(&self) -> Option<u16> {
        self.local_listen_port().filter(|p| *p != 0)
    }

    /// 绑定的本地地址：`None`/`localhost` 只走回环，`*` 走所有网卡。
    pub fn bind_address(&self) -> std::net::IpAddr {
        match self.bind() {
            None | Some("") | Some("localhost") => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            Some("*") | Some("0.0.0.0") => std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            Some(other) => match other.parse() {
                Ok(ip) => ip,
                // 绑成一个主机名（`proxy.internal:8080:…`）：解析不了就退回回环，
                // 绝不能退成"所有网卡"——那是把端口意外开放出去。
                Err(_) => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            },
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Local { bind, listen_port, dest_host, dest_port } => {
                format!("-L {}{listen_port}:{dest_host}:{dest_port}", bind_prefix(bind))
            }
            Self::Remote { bind, listen_port, dest_host, dest_port } => {
                format!("-R {}{listen_port}:{dest_host}:{dest_port}", bind_prefix(bind))
            }
            Self::Dynamic { bind, listen_port } => {
                format!("-D {}{listen_port}", bind_prefix(bind))
            }
        }
    }

    /// 界面用的写法：自动口不该显示成 `-L 0:db:5432` 这种像填错的形态。
    /// 表单文本栏和 ssh_config 导出仍用 `label()`，那两种写法要能被原样解析回去。
    pub fn display(&self) -> String {
        if !self.auto_port() {
            return self.label();
        }
        let bind = match self.bind() {
            Some(b) if !b.is_empty() => format!("{b} "),
            _ => String::new(),
        };
        match self {
            Self::Local { dest_host, dest_port, .. } => {
                format!("-L {bind}自动 → {dest_host}:{dest_port}")
            }
            Self::Remote { dest_host, dest_port, .. } => {
                format!("-R {bind}自动 → {dest_host}:{dest_port}")
            }
            Self::Dynamic { .. } => format!("-D {bind}自动"),
        }
    }

    /// 解析 `LocalForward` / `RemoteForward` 的值：`[bind:]host:port` 之外的
    /// ssh 语法是 `[bind:]port:host:hostport`。
    ///
    /// 从右往左锚定端口字段：IPv6 目标写作 `[::1]:80` 时自带冒号，
    /// 从左数会把 bind 切错。
    pub fn parse_endpoint(kind: EndpointKind, spec: &str) -> Option<Self> {
        let parts: Vec<&str> = split_forward(spec.trim());
        if !(3..=4).contains(&parts.len()) {
            return None;
        }
        let (bind, port, host, hostport) = if parts.len() == 4 {
            (Some(parts[0]), parts[1], parts[2], parts[3])
        } else {
            (None, parts[0], parts[1], parts[2])
        };
        let listen_port = port.parse().ok()?;
        let dest_port = hostport.parse().ok()?;
        if host.is_empty() || is_glob(host) {
            return None;
        }
        let bind = bind.filter(|b| !b.is_empty()).map(str::to_string);
        Some(match kind {
            EndpointKind::Local => Self::Local {
                bind,
                listen_port,
                dest_host: host.to_string(),
                dest_port,
            },
            EndpointKind::Remote => Self::Remote {
                bind,
                listen_port,
                dest_host: host.to_string(),
                dest_port,
            },
        })
    }

    /// 只写目的端的简写：`db:5432` → 本地口由系统分配，等价于 `0:db:5432`。
    ///
    /// 两段都是数字的形态（`8080:5432`）不认——那更像是漏写了目的主机，
    /// 静默猜成"自动口 + 目的 5432:8080"会把流量送到错的地方。
    pub fn parse_only_dest(kind: EndpointKind, spec: &str) -> Option<Self> {
        let parts: Vec<&str> = split_forward(spec.trim());
        if parts.len() != 2 {
            return None;
        }
        let (host, hostport) = (parts[0], parts[1]);
        if host.is_empty() || is_glob(host) || host.parse::<u16>().is_ok() {
            return None;
        }
        let dest_port = hostport.parse().ok()?;
        Some(match kind {
            EndpointKind::Local => Self::Local {
                bind: None,
                listen_port: 0,
                dest_host: host.to_string(),
                dest_port,
            },
            EndpointKind::Remote => Self::Remote {
                bind: None,
                listen_port: 0,
                dest_host: host.to_string(),
                dest_port,
            },
        })
    }

    /// 解析 `DynamicForward` 的值：`[bind:]port`。
    pub fn parse_dynamic(spec: &str) -> Option<Self> {
        let parts: Vec<&str> = split_forward(spec.trim());
        match parts.len() {
            1 => Some(Self::Dynamic { bind: None, listen_port: parts[0].parse().ok()? }),
            2 => Some(Self::Dynamic {
                bind: (!parts[0].is_empty()).then(|| parts[0].to_string()),
                listen_port: parts[1].parse().ok()?,
            }),
            _ => None,
        }
    }

    /// 解析用户在表单里写的一整行：`-L 8080:localhost:80 -D 1080`。
    ///
    /// 宽松处理，因为写错的那条只该被丢掉并说明原因，不该让整台主机保存不了：
    /// 省略前缀按 `-L` 算（最常用的形态）；`-L8080:…` 粘连写法和 `-L 8080:…`
    /// 分开写法都吃；`-`/`--` 之外的负数不是转发，不在这个语法里。
    ///
    /// 本地口可以不写：`-L db:5432` 或 `-L 0:db:5432` 都表示"随便给个空闲口"，
    /// 两台主机想要同一个端口时就不会互相挤掉——实际端口由隧道上报，界面上能查。
    pub fn parse_specs(text: &str) -> (Vec<Self>, Vec<String>) {
        let mut ok = Vec::new();
        let mut bad = Vec::new();
        let tokens: Vec<&str> = text.split_whitespace().collect();
        let mut i = 0;
        while i < tokens.len() {
            let token = tokens[i];
            let (kind, spec) = match flag_of(token) {
                Some((kind, inline)) if inline.is_empty() => {
                    i += 1;
                    let Some(next) = tokens.get(i) else {
                        bad.push(token.to_string());
                        i += 1;
                        continue;
                    };
                    (kind, *next)
                }
                Some((kind, inline)) => (kind, inline),
                // 没有前缀：整条按本地转发解析，这是 `8080:db:5432` 的自然写法
                None => (SpecKind::Local, token),
            };
            i += 1;
            let parsed = match kind {
                SpecKind::Local => Self::parse_endpoint(EndpointKind::Local, spec)
                    .or_else(|| Self::parse_only_dest(EndpointKind::Local, spec)),
                SpecKind::Remote => Self::parse_endpoint(EndpointKind::Remote, spec)
                    .or_else(|| Self::parse_only_dest(EndpointKind::Remote, spec)),
                SpecKind::Dynamic => Self::parse_dynamic(spec),
            };
            match parsed {
                Some(f) => ok.push(f),
                None => bad.push(format!("{} {spec}", kind.dash()).trim().to_string()),
            }
        }
        (ok, bad)
    }
}

/// 一条撞口记录：哪个别名、它的哪条规则占着同一个口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortClash {
    pub alias: String,
    pub rule: Forward,
}

/// 两条规则是否抢同一个本地监听口。
///
/// 只比端口、不比 bind：`*:8080` 和 `127.0.0.1:8080` 在大多数系统上确实起不来第二个，
/// 而"回环口互不干扰"这种写法极少见——宁可多提示一句，也不让用户等到启动失败才反应过来。
/// 自动分配的口（`0`）永远不算撞。
pub fn ports_clash(a: &Forward, b: &Forward) -> bool {
    match (a.fixed_local_port(), b.fixed_local_port()) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// 这台主机的某条规则撞了谁的口。`skip` 是这条规则在该主机 `forwards` 里的下标，
/// 用来把自己排除掉；同一台机器上的两条同口规则照样算撞。
pub fn clashes_with(hosts: &[Host], alias: &str, skip: usize, rule: &Forward) -> Vec<PortClash> {
    let Some(port) = rule.fixed_local_port() else { return Vec::new() };
    let mut out: Vec<PortClash> = hosts
        .iter()
        .flat_map(|h| h.forwards.iter().enumerate().map(move |(i, f)| (h.alias.as_str(), i, f)))
        .filter(|(other, i, f)| {
            !(*other == alias && *i == skip) && f.fixed_local_port() == Some(port)
        })
        .map(|(other, _, f)| PortClash { alias: other.to_string(), rule: f.clone() })
        .collect();
    out.sort_by(|a, b| a.alias.cmp(&b.alias));
    out
}

/// 全库里互相撞口的分组，按端口升序：`(端口, [(别名, 规则)])`，只收录同口 ≥2 条的。
/// 隧道面板顶部的提醒条用它，这样"没点进编辑器也该知道起不来"。
pub fn all_port_clashes(hosts: &[Host]) -> Vec<(u16, Vec<PortClash>)> {
    let mut by_port: std::collections::BTreeMap<u16, Vec<PortClash>> = std::collections::BTreeMap::new();
    for h in hosts {
        for f in &h.forwards {
            if let Some(port) = f.fixed_local_port() {
                by_port.entry(port).or_default().push(PortClash {
                    alias: h.alias.clone(),
                    rule: f.clone(),
                });
            }
        }
    }
    by_port
        .into_iter()
        .filter(|(_, v)| {
            let mut seen: Vec<&str> = v.iter().map(|c| c.alias.as_str()).collect();
            seen.sort_unstable();
            seen.dedup();
            seen.len() > 1
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
enum SpecKind {
    Local,
    Remote,
    Dynamic,
}

impl SpecKind {
    fn dash(self) -> &'static str {
        match self {
            Self::Local => "-L",
            Self::Remote => "-R",
            Self::Dynamic => "-D",
        }
    }
}

/// `-L 8080:…` → `Some((Local, ""))`；`-L8080:…` → `Some((Local, "8080:…"))`；
/// 其它（不含前导 `-`）→ `None`。
fn flag_of(token: &str) -> Option<(SpecKind, &str)> {
    let rest = token.strip_prefix('-')?;
    let (letter, inline) = rest.split_at(1);
    let kind = match letter.to_ascii_lowercase().as_str() {
        "l" => SpecKind::Local,
        "r" => SpecKind::Remote,
        "d" => SpecKind::Dynamic,
        _ => return None,
    };
    Some((kind, inline.strip_prefix(':').unwrap_or(inline)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    Local,
    Remote,
}

/// `bind:` 前缀；没有 bind 时是空串。
fn bind_prefix(bind: &Option<String>) -> String {
    match bind.as_deref() {
        Some(b) if !b.is_empty() => format!("{b}:"),
        _ => String::new(),
    }
}

/// ssh_config 的 `!`/`*` 前缀表示"排除/通配"，不是具体转发目标。
fn is_glob(value: &str) -> bool {
    value.contains('*') || value.starts_with('!')
}

/// 按冒号切分，但尊重 `[...]`（IPv6 字面量）。
fn split_forward(spec: &str) -> Vec<&str> {
    let bytes = spec.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'[' => depth += 1,
            b']' => depth = depth.saturating_sub(1),
            b':' if depth == 0 => {
                out.push(&spec[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&spec[start..]);
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub alias: String,
    pub hostname: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    pub auth: Auth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Alias of a bastion/jump host to tunnel through (ProxyJump style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// 连接建立后要拉起的端口转发（`ssh -L/-R/-D`）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forwards: Vec<Forward>,
    /// 所属分组名（仅用于列表归类，不参与连接）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 多标签，列表里按 `#` 做 AND 过滤。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub favorite: bool,
    /// 最近一次连接成功的 Unix 秒；0 表示从未连过，用于"最近使用"排序。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub last_connected: u64,
}

impl Default for Host {
    fn default() -> Self {
        Self {
            alias: String::new(),
            hostname: String::new(),
            port: 22,
            user: String::new(),
            auth: Auth::Password,
            password: None,
            jump: None,
            note: None,
            forwards: Vec::new(),
            group: None,
            tags: Vec::new(),
            favorite: false,
            last_connected: 0,
        }
    }
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

fn default_port() -> u16 {
    22
}

impl Host {
    pub fn target(&self) -> String {
        format!("{}@{}:{}", self.user, self.hostname, self.port)
    }

    pub fn auth_label(&self) -> &'static str {
        match &self.auth {
            Auth::Password => "密码",
            Auth::PrimaryKey { .. } => "密钥",
            Auth::Agent => "agent",
        }
    }

    /// `密码 · 1 条转发`：列表副标题里体现"这台机器带着隧道"。
    pub fn meta_label(&self) -> String {
        let mut out = String::from(self.auth_label());
        let n = self.forwards.len();
        if n > 0 {
            out.push_str(&format!(" · {n} 条转发"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_style_spec_line() {
        let (ok, bad) = Forward::parse_specs("-L 8080:localhost:80 -D 1080 -R 9000:backup:22");
        assert!(bad.is_empty(), "不应有被丢掉的规则: {bad:?}");
        assert_eq!(
            ok,
            vec![
                Forward::Local {
                    bind: None,
                    listen_port: 8080,
                    dest_host: "localhost".into(),
                    dest_port: 80
                },
                Forward::Dynamic { bind: None, listen_port: 1080 },
                Forward::Remote {
                    bind: None,
                    listen_port: 9000,
                    dest_host: "backup".into(),
                    dest_port: 22
                },
            ]
        );
    }

    /// 省略前缀按 `-L` 算，粘连写法 `-L1080` 也吃：表单里两种都会出现。
    #[test]
    fn accepts_bare_and_glued_forms() {
        let (ok, bad) = Forward::parse_specs("8080:db.internal:5432 -D1080");
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(ok[0].label(), "-L 8080:db.internal:5432");
        assert_eq!(ok[1].label(), "-D 1080");
    }

    /// 写错的那条只丢它自己，并给出可展示的原文。
    #[test]
    fn keeps_good_rules_when_one_is_broken() {
        let (ok, bad) = Forward::parse_specs("-L 8080:localhost -D notaport -L 9000:h:22");
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].label(), "-L 9000:h:22");
        assert_eq!(bad, ["-L 8080:localhost", "-D notaport"]);
    }

    #[test]
    fn bind_prefix_and_ipv6_target_survive() {
        let (ok, bad) = Forward::parse_specs("-L *:1080:127.0.0.1:22 -L 6379:[::1]:6379");
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(ok[0].bind(), Some("*"));
        assert_eq!(ok[1].label(), "-L 6379:[::1]:6379");
    }

    #[test]
    fn empty_line_is_not_an_error() {
        let (ok, bad) = Forward::parse_specs("   ");
        assert!(ok.is_empty() && bad.is_empty());
    }

    /// 只写目的端 = 本地口交给系统分配；`label()` 要能原样解析回去，`display()` 才说人话。
    #[test]
    fn bare_dest_means_system_picked_port() {
        let (ok, bad) = Forward::parse_specs("-L db:5432 -D 0");
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(ok[0].label(), "-L 0:db:5432");
        assert_eq!(ok[0].display(), "-L 自动 → db:5432");
        assert!(ok[0].auto_port());
        assert_eq!(ok[0].fixed_local_port(), None);
        assert_eq!(ok[1], Forward::Dynamic { bind: None, listen_port: 0 });
        assert_eq!(ok[1].display(), "-D 自动");
        // 自动口的形态得能被再解析一次（表单里改完别的东西要原样回来）
        let (again, bad) = Forward::parse_specs(&ok[0].label());
        assert!(bad.is_empty() && again == vec![ok[0].clone()]);
    }

    /// 两段但首段是数字：更像漏写目的主机，静默猜会把流量送到别处，所以不认。
    #[test]
    fn two_numeric_parts_are_not_an_auto_rule() {
        let (ok, bad) = Forward::parse_specs("-L 8080:5432");
        assert!(ok.is_empty());
        assert_eq!(bad, ["-L 8080:5432"]);
        assert_eq!(Forward::parse_only_dest(EndpointKind::Local, "8080:5432"), None);
    }

    #[test]
    fn auto_ports_never_clash_but_fixed_ones_do() {
        let auto = Forward::parse_specs("-L db:5432").0.remove(0);
        let fixed = Forward::parse_specs("-L 8080:db:5432").0.remove(0);
        let other = Forward::parse_specs("-L 8080:other:5432").0.remove(0);
        assert!(!ports_clash(&auto, &fixed), "自动口不占固定口");
        assert!(ports_clash(&fixed, &other), "同口不同目的端也是撞");
        // 同口的两条规则彼此都算撞，"别看自己"是 clashes_with 用下标排除的
        assert!(ports_clash(&fixed, &fixed.clone()));
    }

    /// 撞口清单要能点名对方是谁，并把"正在被检查的这条"排除掉。
    #[test]
    fn clashes_with_names_the_other_host_and_skips_itself() {
        let rule = |spec: &str| Forward::parse_specs(spec).0.remove(0);
        let hosts = vec![
            Host {
                alias: "db-prod".into(),
                forwards: vec![rule("-L 8080:127.0.0.1:5432")],
                ..Default::default()
            },
            Host {
                alias: "web-prod".into(),
                forwards: vec![rule("-L 8080:127.0.0.1:80"), rule("-D 1080"), rule("-L redis:6379")],
                ..Default::default()
            },
        ];
        let web = &hosts[1];
        let hits = clashes_with(&hosts, "web-prod", 0, &web.forwards[0]);
        assert_eq!(
            hits.iter().map(|c| c.alias.as_str()).collect::<Vec<_>>(),
            ["db-prod"],
            "第 0 条自己不该出现"
        );
        // 自动口什么都不撞
        assert!(clashes_with(&hosts, "web-prod", 2, &web.forwards[2]).is_empty());

        // 同一台机器里的两条同口规则也要抓到（别名就是它自己）
        let dup = vec![Host {
            alias: "x".into(),
            forwards: vec![rule("-L 8080:a:1"), rule("-L 8080:b:2")],
            ..Default::default()
        }];
        let hits = clashes_with(&dup, "x", 0, &dup[0].forwards[0]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].alias, "x");
    }

    #[test]
    fn all_port_clashes_groups_by_port_across_hosts() {
        let host = |alias: &str, spec: &str| Host {
            alias: alias.into(),
            forwards: Forward::parse_specs(spec).0,
            ..Default::default()
        };
        let hosts = vec![host("a", "-L 8080:x:1 -L 9000:y:2"), host("b", "-L 8080:z:3"), host("c", "-D 1080")];
        let groups = all_port_clashes(&hosts);
        assert_eq!(groups.len(), 1, "只有 8080 是两台共用");
        assert_eq!(groups[0].0, 8080);
        assert_eq!(groups[0].1.iter().map(|c| c.alias.as_str()).collect::<Vec<_>>(), ["a", "b"]);

        // 同一台机器里重复不算"跨主机撞口"，顶部提示只喊同时起不来的那种
        let same = vec![host("a", "-L 8080:x:1 -L 8080:y:2"), host("b", "-L 9000:z:3")];
        assert!(all_port_clashes(&same).is_empty());
    }
}

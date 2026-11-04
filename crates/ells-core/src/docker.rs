//! Docker 容器的只读数据层：把 Docker Engine API 的 JSON 或
//! `docker ps --format '{{json .}}'` 的行式 JSON 收敛成同一份结构。
//!
//! 只做解析与投影，不碰网络——网络那条腿在 `ssh.rs` 里，方便单测。

use chrono::{DateTime, Utc};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortMapping {
    /// 绑定地址。空串表示"只暴露、没发布"（Dockerfile 的 EXPOSE）。
    pub ip: String,
    pub private_port: u16,
    pub public_port: Option<u16>,
    /// tcp / udp / sctp
    pub proto: String,
}

impl PortMapping {
    fn key(&self) -> (Option<u16>, u16, &str) {
        (self.public_port, self.private_port, self.proto.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerContainer {
    pub id: String,
    pub name: String,
    pub image: String,
    /// running / exited / paused / created ...
    pub state: String,
    /// Docker 给的那句人话（"Up 2 hours"）。
    pub status: String,
    pub ports: Vec<PortMapping>,
}

impl DockerContainer {
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(12)]
    }

    pub fn is_running(&self) -> bool {
        self.state.eq_ignore_ascii_case("running")
    }
}

/// 容器的一次性生命周期动作。只允许这几个动词，界面传不进来任意命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerAction {
    Start,
    Stop,
    Restart,
    Kill,
    Pause,
    Unpause,
    Remove,
}

impl ContainerAction {
    pub fn method(self) -> &'static str {
        match self {
            ContainerAction::Remove => "DELETE",
            _ => "POST",
        }
    }

    pub fn api_path(self, id: &str) -> String {
        match self {
            ContainerAction::Start => format!("/containers/{id}/start"),
            ContainerAction::Stop => format!("/containers/{id}/stop?t=10"),
            ContainerAction::Restart => format!("/containers/{id}/restart?t=10"),
            ContainerAction::Kill => format!("/containers/{id}/kill"),
            ContainerAction::Pause => format!("/containers/{id}/pause"),
            ContainerAction::Unpause => format!("/containers/{id}/unpause"),
            ContainerAction::Remove => format!("/containers/{id}"),
        }
    }

    pub fn cli_command(self, id: &str) -> String {
        match self {
            ContainerAction::Start => format!("docker start {id}"),
            ContainerAction::Stop => format!("docker stop -t 10 {id}"),
            ContainerAction::Restart => format!("docker restart -t 10 {id}"),
            ContainerAction::Kill => format!("docker kill {id}"),
            ContainerAction::Pause => format!("docker pause {id}"),
            ContainerAction::Unpause => format!("docker unpause {id}"),
            ContainerAction::Remove => format!("docker rm {id}"),
        }
    }
}

/// 容器 ID 只可能是十六进制；放宽到常见安全字符，防止把回包内容拼进远端 shell。
pub fn is_safe_container_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// 一次 `docker stats --no-stream` 的结果。缺哪项就是 `None`，由界面画 `—`。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ContainerStats {
    pub cpu_percent: Option<f64>,
    pub mem_used: Option<u64>,
    pub mem_limit: Option<u64>,
    pub mem_percent: Option<f64>,
    pub net_rx: Option<u64>,
    pub net_tx: Option<u64>,
    pub pids: Option<u64>,
    pub block_read: Option<u64>,
    pub block_write: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInfo {
    pub name: String,
    pub ip: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub source: String,
    pub destination: String,
    pub read_only: bool,
    /// bind / volume / tmpfs ...
    pub kind: String,
}

/// 详情弹窗要的那一份：`docker inspect` 的摘录，外加运行中的一次性资源占用。
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerDetail {
    pub id: String,
    pub name: String,
    pub image: String,
    pub image_id: String,
    pub state: String,
    pub health: Option<String>,
    pub created: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i64>,
    pub restart_count: u64,
    pub restart_policy: String,
    pub restart_max_retry: u64,
    pub networks: Vec<NetworkInfo>,
    pub mounts: Vec<MountInfo>,
    pub ports: Vec<PortMapping>,
    pub entrypoint: Option<String>,
    pub cmd: Option<String>,
    pub user: String,
    pub working_dir: String,
    pub oom_killed: bool,
    pub stats: Option<ContainerStats>,
    /// 资源占用读不到时留一句原因，详情本身照常显示。
    pub stats_error: Option<String>,
}

impl ContainerDetail {
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(12)]
    }

    /// 跑着（含暂停 / 重启中）才值得去问 stats。
    pub fn is_active(&self) -> bool {
        matches!(self.state.as_str(), "running" | "paused" | "restarting")
    }
}

/// `/containers/{id}/json` 或 `docker inspect <id>` 的响应。
/// CLI 给的是数组，socket 给的是对象，两种都收。
pub fn parse_inspect_json(body: &str) -> Result<ContainerDetail, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| format!("docker inspect 返回的不是 JSON：{e}"))?;
    let obj = match &value {
        Value::Array(items) => items
            .first()
            .ok_or_else(|| "docker inspect 返回了空数组".to_string())?,
        Value::Object(_) => &value,
        _ => return Err("docker inspect 返回的不是对象或数组".to_string()),
    };
    if !obj.is_object() {
        return Err("docker inspect 返回的条目不是对象".to_string());
    }
    Ok(detail_from_inspect(obj))
}

fn detail_from_inspect(v: &Value) -> ContainerDetail {
    let get = |path: &str| v.pointer(path);
    let text = |path: &str| get(path).and_then(Value::as_str).unwrap_or_default();
    let state = get("/State");
    let config = get("/Config");
    let exit_code = state
        .and_then(|s| s.get("ExitCode"))
        .and_then(|x| x.as_i64().or_else(|| x.as_u64().map(|n| n as i64)));
    let networks = get("/NetworkSettings/Networks")
        .and_then(Value::as_object)
        .map(|map| {
            let mut rows: Vec<NetworkInfo> = map
                .iter()
                .map(|(name, net)| NetworkInfo {
                    name: name.clone(),
                    ip: net
                        .get("IPAddress")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect();
            rows.sort_by(|a, b| a.name.cmp(&b.name));
            rows
        })
        .unwrap_or_default();
    let mounts = v
        .get("Mounts")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|m| MountInfo {
                    source: m
                        .get("Source")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    destination: m
                        .get("Destination")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    read_only: !m.get("RW").and_then(Value::as_bool).unwrap_or(true),
                    kind: m
                        .get("Type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    ContainerDetail {
        id: text("/Id").to_string(),
        name: text("/Name").trim_start_matches('/').to_string(),
        image: text("/Config/Image").to_string(),
        image_id: text("/Image").to_string(),
        state: text("/State/Status").to_string(),
        health: get("/State/Health/Status")
            .and_then(Value::as_str)
            .map(str::to_string),
        created: parse_rfc3339(get("/Created").and_then(Value::as_str)),
        started_at: parse_rfc3339(get("/State/StartedAt").and_then(Value::as_str)),
        finished_at: parse_rfc3339(get("/State/FinishedAt").and_then(Value::as_str)),
        exit_code,
        restart_count: v.get("RestartCount").and_then(Value::as_u64).unwrap_or(0),
        restart_policy: text("/HostConfig/RestartPolicy/Name").to_string(),
        restart_max_retry: get("/HostConfig/RestartPolicy/MaximumRetryCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        networks,
        mounts,
        ports: inspect_ports(v),
        entrypoint: join_command(config.and_then(|c| c.get("Entrypoint"))),
        cmd: join_command(config.and_then(|c| c.get("Cmd"))),
        user: text("/Config/User").to_string(),
        working_dir: text("/Config/WorkingDir").to_string(),
        oom_killed: get("/State/OOMKilled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        stats: None,
        stats_error: None,
    }
}

/// inspect 里的端口分成两张表：`NetworkSettings.Ports` 是已发布的绑定，
/// `Config.ExposedPorts` 是只声明未发布的。两边合起来去重，得到详情页的完整清单。
fn inspect_ports(v: &Value) -> Vec<PortMapping> {
    let mut out = Vec::new();
    let mut published = std::collections::HashSet::new();
    if let Some(map) = v
        .pointer("/NetworkSettings/Ports")
        .and_then(Value::as_object)
    {
        for (key, bindings) in map {
            let Some((private_port, proto)) = split_port_key(key) else {
                continue;
            };
            let mut any = false;
            if let Some(items) = bindings.as_array() {
                for binding in items {
                    out.push(PortMapping {
                        ip: binding
                            .get("HostIp")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        private_port,
                        public_port: binding
                            .get("HostPort")
                            .and_then(Value::as_str)
                            .and_then(|s| s.parse::<u16>().ok()),
                        proto: proto.clone(),
                    });
                    any = true;
                }
            }
            published.insert((private_port, proto.clone()));
            if !any {
                out.push(PortMapping {
                    ip: String::new(),
                    private_port,
                    public_port: None,
                    proto,
                });
            }
        }
    }
    if let Some(map) = v.pointer("/Config/ExposedPorts").and_then(Value::as_object) {
        for key in map.keys() {
            let Some((private_port, proto)) = split_port_key(key) else {
                continue;
            };
            if published.contains(&(private_port, proto.clone())) {
                continue;
            }
            out.push(PortMapping {
                ip: String::new(),
                private_port,
                public_port: None,
                proto,
            });
        }
    }
    out
}

fn split_port_key(key: &str) -> Option<(u16, String)> {
    let (port, proto) = key.split_once('/')?;
    Some((port.trim().parse().ok()?, proto.trim().to_string()))
}

fn join_command(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Array(items) if !items.is_empty() => {
            Some(items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
        }
        _ => None,
    }
}

/// Docker 给的时间戳是 RFC3339；零值 `0001-01-01T00:00:00Z` 表示"还没发生"。
fn parse_rfc3339(raw: Option<&str>) -> Option<DateTime<Utc>> {
    let raw = raw?;
    if raw.starts_with("0001-01-01") {
        return None;
    }
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// `/containers/{id}/stats?stream=false` 的响应。CPU 百分比按 Docker 自己的公式算：
/// 两次采样的 CPU 时间差 ÷ 系统时间差 × 在线核数。
pub fn parse_stats_engine(body: &str) -> Result<ContainerStats, String> {
    let v: Value =
        serde_json::from_str(body).map_err(|e| format!("docker stats 返回的不是 JSON：{e}"))?;
    if !v.is_object() {
        return Err("docker stats 返回的不是对象".to_string());
    }
    let mut stats = ContainerStats::default();
    let cpu = v.get("cpu_stats");
    let pre = v.get("precpu_stats");
    let total = cpu
        .and_then(|c| c.pointer("/cpu_usage/total_usage"))
        .and_then(Value::as_u64);
    let pre_total = pre
        .and_then(|c| c.pointer("/cpu_usage/total_usage"))
        .and_then(Value::as_u64);
    let system = cpu
        .and_then(|c| c.get("system_cpu_usage"))
        .and_then(Value::as_u64);
    let pre_system = pre
        .and_then(|c| c.get("system_cpu_usage"))
        .and_then(Value::as_u64);
    let online = cpu
        .and_then(|c| c.get("online_cpus"))
        .and_then(Value::as_u64)
        .or_else(|| {
            cpu.and_then(|c| c.pointer("/cpu_usage/percpu_usage"))
                .and_then(Value::as_array)
                .map(|a| a.len() as u64)
        })
        .unwrap_or(1)
        .max(1);
    if let (Some(total), Some(system)) = (total, system) {
        let cpu_delta = total.saturating_sub(pre_total.unwrap_or(0));
        let system_delta = system.saturating_sub(pre_system.unwrap_or(0));
        if system_delta > 0 {
            stats.cpu_percent =
                Some(cpu_delta as f64 / system_delta as f64 * online as f64 * 100.0);
        } else if cpu_delta == 0 {
            stats.cpu_percent = Some(0.0);
        }
    }

    let usage = v
        .pointer("/memory_stats/usage")
        .and_then(Value::as_u64);
    let limit = v
        .pointer("/memory_stats/limit")
        .and_then(Value::as_u64);
    let cache = v
        .pointer("/memory_stats/stats/cache")
        .and_then(Value::as_u64)
        .or_else(|| {
            v.pointer("/memory_stats/stats/inactive_file")
                .and_then(Value::as_u64)
        })
        .unwrap_or(0);
    if let (Some(usage), Some(limit)) = (usage, limit) {
        let used = usage.saturating_sub(cache);
        stats.mem_used = Some(used);
        stats.mem_limit = Some(limit);
        if limit > 0 {
            stats.mem_percent = Some(used as f64 / limit as f64 * 100.0);
        }
    }

    if let Some(nets) = v.get("networks").and_then(Value::as_object) {
        let mut rx = 0u64;
        let mut tx = 0u64;
        let mut seen = false;
        for net in nets.values() {
            if let Some(n) = net.get("rx_bytes").and_then(Value::as_u64) {
                rx += n;
                seen = true;
            }
            if let Some(n) = net.get("tx_bytes").and_then(Value::as_u64) {
                tx += n;
                seen = true;
            }
        }
        if seen {
            stats.net_rx = Some(rx);
            stats.net_tx = Some(tx);
        }
    }
    stats.pids = v
        .pointer("/pids_stats/current")
        .and_then(Value::as_u64);

    if let Some(items) = v
        .pointer("/blkio_stats/io_service_bytes_recursive")
        .and_then(Value::as_array)
    {
        let mut read = 0u64;
        let mut write = 0u64;
        let mut seen = false;
        for item in items {
            let value = item.get("value").and_then(Value::as_u64).unwrap_or(0);
            match item
                .get("op")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str()
            {
                "read" => {
                    read += value;
                    seen = true;
                }
                "write" => {
                    write += value;
                    seen = true;
                }
                _ => {}
            }
        }
        if seen {
            stats.block_read = Some(read);
            stats.block_write = Some(write);
        }
    }
    Ok(stats)
}

/// `docker stats --no-stream --format '{{json .}}' <id>`：一行 JSON，数值是带单位的人话。
pub fn parse_stats_cli(text: &str) -> Result<ContainerStats, String> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| "docker stats 没有输出".to_string())?;
    let v: Value =
        serde_json::from_str(line).map_err(|e| format!("docker stats 第 1 行不是 JSON：{e}"))?;
    let mut stats = ContainerStats::default();
    stats.cpu_percent = v
        .get("CPUPerc")
        .and_then(Value::as_str)
        .and_then(parse_percent);
    let (mem_used, mem_limit) = split_sizes(v.get("MemUsage").and_then(Value::as_str));
    stats.mem_used = mem_used;
    stats.mem_limit = mem_limit;
    stats.mem_percent = v
        .get("MemPerc")
        .and_then(Value::as_str)
        .and_then(parse_percent)
        .or_else(|| match (mem_used, mem_limit) {
            (Some(used), Some(limit)) if limit > 0 => {
                Some(used as f64 / limit as f64 * 100.0)
            }
            _ => None,
        });
    let (rx, tx) = split_sizes(v.get("NetIO").and_then(Value::as_str));
    stats.net_rx = rx;
    stats.net_tx = tx;
    let (read, write) = split_sizes(v.get("BlockIO").and_then(Value::as_str));
    stats.block_read = read;
    stats.block_write = write;
    stats.pids = v
        .get("PIDs")
        .and_then(Value::as_str)
        .and_then(|s| s.trim().parse().ok())
        .or_else(|| v.get("PIDs").and_then(Value::as_u64));
    Ok(stats)
}

fn parse_percent(text: &str) -> Option<f64> {
    text.trim().trim_end_matches('%').trim().parse().ok()
}

fn split_sizes(text: Option<&str>) -> (Option<u64>, Option<u64>) {
    let Some(text) = text else {
        return (None, None);
    };
    let mut parts = text.split('/');
    (
        parts.next().and_then(parse_size),
        parts.next().and_then(parse_size),
    )
}

/// `12.3MiB` / `1.2kB` / `0B` → 字节数。Docker CLI 的 KiB/MiB/GiB 按 1024 进制，
/// kB/MB/GB 按 1000 进制。
pub fn parse_size(text: &str) -> Option<u64> {
    const UNITS: &[(&str, u64)] = &[
        ("TiB", 1 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
        ("TB", 1_000_000_000_000),
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
        ("kB", 1_000),
        ("KB", 1_000),
        ("B", 1),
    ];
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("N/A") {
        return None;
    }
    for (suffix, factor) in UNITS {
        if let Some(number) = text.strip_suffix(suffix) {
            let number: f64 = number.trim().parse().ok()?;
            return Some((number * *factor as f64) as u64);
        }
    }
    text.parse().ok()
}

/// 只看状态行，不要求 2xx：停止已在停止的容器时 Docker 会回 304，
/// 对动作来说是成功的（幂等）。
pub fn parse_http_status(raw: &[u8]) -> Result<u16, String> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..end]);
    let status_line = head.lines().next().unwrap_or_default();
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("看不懂 Docker socket 的状态行：{status_line}"))
}

/// Engine API `/containers/json` 的响应：一个 JSON 数组。
pub fn parse_engine_json(body: &str) -> Result<Vec<DockerContainer>, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("Docker API 返回的不是 JSON：{e}"))?;
    let items = value.as_array().ok_or_else(|| "Docker API 返回的不是容器数组".to_string())?;
    Ok(items.iter().map(container_from_engine).collect())
}

fn container_from_engine(v: &Value) -> DockerContainer {
    let id = v.get("Id").and_then(Value::as_str).unwrap_or_default().to_string();
    let name = v
        .get("Names")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_start_matches('/')
        .to_string();
    let image = v.get("Image").and_then(Value::as_str).unwrap_or_default().to_string();
    let state = v.get("State").and_then(Value::as_str).unwrap_or_default().to_string();
    let status = v.get("Status").and_then(Value::as_str).unwrap_or_default().to_string();
    let ports = v
        .get("Ports")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(port_from_engine).collect())
        .unwrap_or_default();
    DockerContainer { id, name, image, state, status, ports }
}

fn port_from_engine(v: &Value) -> Option<PortMapping> {
    let private_port = u16::try_from(v.get("PrivatePort").and_then(Value::as_u64)?).ok()?;
    let public_port = v
        .get("PublicPort")
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok());
    let proto = v
        .get("Type")
        .and_then(Value::as_str)
        .unwrap_or("tcp")
        .to_string();
    let ip = v.get("IP").and_then(Value::as_str).unwrap_or_default().to_string();
    Some(PortMapping { ip, private_port, public_port, proto })
}

/// `docker ps --format '{{json .}}'` 的输出：每行一个 JSON 对象。
/// 空行忽略；一行解析不了就整体报错（让界面说"这台给的格式不认识"，
/// 而不是悄悄少画几个容器）。
pub fn parse_cli_json_lines(text: &str) -> Result<Vec<DockerContainer>, String> {
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)
            .map_err(|e| format!("docker ps 第 {} 行不是 JSON：{e}", idx + 1))?;
        out.push(container_from_cli(&v));
    }
    Ok(out)
}

fn container_from_cli(v: &Value) -> DockerContainer {
    let get = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or_default();
    DockerContainer {
        id: get("ID").to_string(),
        name: get("Names").to_string(),
        image: get("Image").to_string(),
        state: get("State").to_string(),
        status: get("Status").to_string(),
        ports: parse_ports_text(get("Ports")),
    }
}

/// `0.0.0.0:8080->80/tcp, :::8080->80/tcp, 9000/tcp`
pub fn parse_ports_text(text: &str) -> Vec<PortMapping> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(parse_port_text)
        .collect()
}

fn parse_port_text(item: &str) -> Option<PortMapping> {
    let (left, right) = match item.split_once("->") {
        Some((l, r)) => (Some(l.trim()), r.trim()),
        None => (None, item),
    };
    let (private_port, proto) = right.split_once('/')?;
    let private_port = private_port.trim().parse().ok()?;
    let proto = proto.trim().to_string();
    let (ip, public_port) = match left {
        Some(l) => {
            let (ip, port) = l.rsplit_once(':')?;
            (ip.trim_matches(|c| c == '[' || c == ']').to_string(), Some(port.parse().ok()?))
        }
        None => (String::new(), None),
    };
    Some(PortMapping { ip, private_port, public_port, proto })
}

/// IPv4/IPv6 通配双绑折叠：同一个端口在 `0.0.0.0` 和 `::` 上各画一遍没有信息量。
/// 只折这一对，指定了具体地址的绑定原样保留（它们确实不同）。
pub fn dedupe_ports(ports: &[PortMapping]) -> Vec<PortMapping> {
    let mut out = Vec::with_capacity(ports.len());
    for p in ports {
        let v6_dup = p.ip == "::"
            && ports.iter().any(|q| q.ip == "0.0.0.0" && q.key() == p.key());
        if !v6_dup {
            out.push(p.clone());
        }
    }
    out
}

/// 拆一个 HTTP/1.1 响应。`Connection: close` 的简单响应和 chunked 都要能收。
/// 返回响应体；非 2xx 直接报错（把状态行带回去给人看）。
pub fn parse_http_response(raw: &[u8]) -> Result<Vec<u8>, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "Docker socket 的响应缺了响应头分隔".to_string())?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("看不懂 Docker socket 的状态行：{status_line}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("Docker API 返回 {status_line}"));
    }
    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            } else if name == "content-length" {
                content_length = value.parse::<usize>().ok();
            }
        }
    }
    if chunked {
        return decode_chunked(body);
    }
    if let Some(len) = content_length {
        if body.len() < len {
            return Err("Docker socket 的响应比声明的短".to_string());
        }
        return Ok(body[..len].to_vec());
    }
    Ok(body.to_vec())
}

fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| "chunked 响应缺了块长度".to_string())?;
        let len_text = std::str::from_utf8(&body[..line_end]).unwrap_or_default();
        let len_text = len_text.split(';').next().unwrap_or("").trim();
        let len = usize::from_str_radix(len_text, 16)
            .map_err(|_| format!("chunked 块长度不认识：{len_text}"))?;
        body = &body[line_end + 2..];
        if len == 0 {
            return Ok(out);
        }
        if body.len() < len {
            return Err("chunked 响应在块中间断了".to_string());
        }
        out.extend_from_slice(&body[..len]);
        body = &body[len..];
        if body.starts_with(b"\r\n") {
            body = &body[2..];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENGINE: &str = r#"[
      {"Id":"0123456789abcdef0123","Names":["/web"],"Image":"nginx:latest",
       "State":"running","Status":"Up 2 hours",
       "Ports":[{"IP":"0.0.0.0","PrivatePort":80,"PublicPort":8080,"Type":"tcp"},
                {"IP":"::","PrivatePort":80,"PublicPort":8080,"Type":"tcp"},
                {"PrivatePort":9000,"Type":"tcp"}]},
      {"Id":"fedcba9876543210fedc","Names":["/db"],"Image":"postgres:16",
       "State":"exited","Status":"Exited (0) 3 days ago","Ports":null}
    ]"#;

    const CLI: &str = r#"
{"ID":"0123456789ab","Names":"web","Image":"nginx","State":"running","Status":"Up 2 hours","Ports":"0.0.0.0:8080->80/tcp, :::8080->80/tcp, 9000/tcp"}
{"ID":"fedcba987654","Names":"db","Image":"postgres","State":"exited","Status":"Exited (0)","Ports":""}
"#;

    #[test]
    fn engine_json_maps_fields_and_ports() {
        let list = parse_engine_json(ENGINE).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "0123456789abcdef0123");
        assert_eq!(list[0].short_id(), "0123456789ab");
        assert_eq!(list[0].name, "web");
        assert!(list[0].is_running());
        assert_eq!(list[0].ports.len(), 3);
        assert_eq!(list[0].ports[0].public_port, Some(8080));
        assert_eq!(list[0].ports[2].public_port, None);
        assert_eq!(list[1].state, "exited");
        assert!(!list[1].is_running());
        assert!(list[1].ports.is_empty());
    }

    #[test]
    fn engine_json_rejects_non_array() {
        assert!(parse_engine_json("{}").is_err());
        assert!(parse_engine_json("not json").is_err());
    }

    #[test]
    fn cli_json_lines_parses_ports_text() {
        let list = parse_cli_json_lines(CLI).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "web");
        assert_eq!(
            list[0].ports,
            vec![
                PortMapping { ip: "0.0.0.0".into(), private_port: 80, public_port: Some(8080), proto: "tcp".into() },
                PortMapping { ip: "::".into(), private_port: 80, public_port: Some(8080), proto: "tcp".into() },
                PortMapping { ip: String::new(), private_port: 9000, public_port: None, proto: "tcp".into() },
            ]
        );
        assert!(list[1].ports.is_empty());
    }

    #[test]
    fn cli_json_lines_reports_bad_line() {
        let err = parse_cli_json_lines("{\"ID\":\"a\"}\nnot-json\n").unwrap_err();
        assert!(err.contains("第 2 行"), "{err}");
    }

    #[test]
    fn ports_text_handles_bracketed_ipv6_and_udp() {
        let p = parse_ports_text("[::1]:5353->53/udp, 127.0.0.1:8080->80/tcp");
        assert_eq!(p[0].ip, "::1");
        assert_eq!(p[0].public_port, Some(5353));
        assert_eq!(p[0].proto, "udp");
        assert_eq!(p[1].ip, "127.0.0.1");
    }

    #[test]
    fn dedupe_collapses_only_wildcard_pair() {
        let ports = vec![
            PortMapping { ip: "0.0.0.0".into(), private_port: 80, public_port: Some(8080), proto: "tcp".into() },
            PortMapping { ip: "::".into(), private_port: 80, public_port: Some(8080), proto: "tcp".into() },
            PortMapping { ip: "127.0.0.1".into(), private_port: 80, public_port: Some(8080), proto: "tcp".into() },
            PortMapping { ip: "::1".into(), private_port: 53, public_port: Some(5353), proto: "udp".into() },
        ];
        let out = dedupe_ports(&ports);
        assert_eq!(out.len(), 3);
        assert!(out.iter().any(|p| p.ip == "127.0.0.1"));
        assert!(out.iter().any(|p| p.ip == "::1"));
        assert!(!out.iter().any(|p| p.ip == "::" && p.private_port == 80));
    }

    #[test]
    fn http_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]";
        assert_eq!(parse_http_response(raw).unwrap(), b"[]");
    }

    #[test]
    fn http_chunked_body() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n[{\"a\r\n3\r\n\":1\r\n2\r\n}]\r\n0\r\n\r\n";
        assert_eq!(parse_http_response(raw).unwrap(), b"[{\"a\":1}]");
    }

    #[test]
    fn http_error_status_is_reported() {
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let err = parse_http_response(raw).unwrap_err();
        assert!(err.contains("404"), "{err}");
    }

    #[test]
    fn http_status_allows_304_for_idempotent_actions() {
        let raw = b"HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(parse_http_status(raw).unwrap(), 304);
        assert_eq!(
            parse_http_status(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap(),
            204
        );
    }

    #[test]
    fn action_paths_and_cli_commands_are_fixed() {
        assert_eq!(ContainerAction::Stop.method(), "POST");
        assert_eq!(
            ContainerAction::Stop.api_path("abc"),
            "/containers/abc/stop?t=10"
        );
        assert_eq!(
            ContainerAction::Restart.api_path("abc"),
            "/containers/abc/restart?t=10"
        );
        assert_eq!(
            ContainerAction::Start.api_path("abc"),
            "/containers/abc/start"
        );
        assert_eq!(ContainerAction::Remove.method(), "DELETE");
        assert_eq!(ContainerAction::Remove.api_path("abc"), "/containers/abc");
        assert_eq!(ContainerAction::Stop.cli_command("abc"), "docker stop -t 10 abc");
        assert_eq!(ContainerAction::Remove.cli_command("abc"), "docker rm abc");
    }

    #[test]
    fn inspect_parses_health_networks_mounts_and_command() {
        let detail = parse_inspect_json(INSPECT).unwrap();
        assert_eq!(detail.name, "web");
        assert_eq!(detail.image, "nginx:latest");
        assert_eq!(detail.state, "running");
        assert_eq!(detail.health.as_deref(), Some("healthy"));
        assert_eq!(detail.restart_policy, "unless-stopped");
        assert_eq!(detail.restart_count, 2);
        assert_eq!(detail.networks.len(), 1);
        assert_eq!(detail.networks[0].name, "bridge");
        assert_eq!(detail.networks[0].ip, "172.17.0.2");
        assert_eq!(detail.mounts.len(), 2);
        assert!(!detail.mounts[0].read_only);
        assert!(detail.mounts[1].read_only);
        assert_eq!(detail.ports.len(), 2);
        assert_eq!(detail.ports[0].public_port, Some(8080));
        assert_eq!(detail.ports[1].private_port, 9000);
        assert_eq!(detail.ports[1].public_port, None);
        assert_eq!(detail.entrypoint.as_deref(), Some("/docker-entrypoint.sh"));
        assert_eq!(detail.cmd.as_deref(), Some("nginx -g daemon off;"));
        assert_eq!(detail.user, "root");
        assert_eq!(detail.working_dir, "/usr/share/nginx/html");
        assert!(!detail.oom_killed);
        assert!(detail.created.is_some());
        assert!(detail.started_at.is_some());
        assert!(detail.finished_at.is_none(), "零值时间应当被当成没有");
    }

    #[test]
    fn inspect_accepts_cli_array_and_ignores_zero_times() {
        let detail = parse_inspect_json(INSPECT).unwrap();
        assert_eq!(detail.exit_code, Some(0));
        // `docker inspect` 给的就是数组，和 socket 的对象共用一套解析
        let array = parse_inspect_json(INSPECT).unwrap();
        assert_eq!(array.id, detail.id);
    }

    #[test]
    fn stats_engine_computes_cpu_memory_and_io() {
        let stats = parse_stats_engine(STATS).unwrap();
        assert!((stats.cpu_percent.unwrap() - 100.0).abs() < 0.01);
        assert_eq!(stats.mem_used, Some(120 * 1024 * 1024));
        assert_eq!(stats.mem_limit, Some(2 * 1024 * 1024 * 1024));
        assert!((stats.mem_percent.unwrap() - 5.865).abs() < 0.1);
        assert_eq!(stats.net_rx, Some(1024 * 1024));
        assert_eq!(stats.net_tx, Some(2048));
        assert_eq!(stats.pids, Some(12));
        assert_eq!(stats.block_read, Some(4096));
        assert_eq!(stats.block_write, Some(8192));
    }

    #[test]
    fn stats_cli_json_parses_human_units() {
        let stats = parse_stats_cli(
            r#"{"CPUPerc":"12.50%","MemUsage":"12.3MiB / 2GiB","MemPerc":"0.60%","NetIO":"1.2kB / 3kB","PIDs":"7","BlockIO":"1KiB / 2KiB"}"#,
        )
        .unwrap();
        assert_eq!(stats.cpu_percent, Some(12.5));
        assert_eq!(stats.mem_used, Some(12_897_484));
        assert_eq!(stats.mem_limit, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(stats.net_rx, Some(1200));
        assert_eq!(stats.net_tx, Some(3000));
        assert_eq!(stats.pids, Some(7));
        assert_eq!(stats.block_read, Some(1024));
        assert_eq!(stats.block_write, Some(2048));
    }

    #[test]
    fn human_size_parser_handles_docker_units() {
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("1.5GiB"), Some(1_610_612_736));
        assert_eq!(parse_size("1.2kB"), Some(1200));
        assert_eq!(parse_size("N/A"), None);
        assert_eq!(parse_size(" 12 MiB "), Some(12 * 1024 * 1024));
    }

    const INSPECT: &str = r#"[{
      "Id":"0123456789abcdef0123",
      "Name":"/web",
      "Created":"2026-11-04T10:00:00.123456789Z",
      "Path":"/docker-entrypoint.sh",
      "Config":{
        "Image":"nginx:latest",
        "Entrypoint":["/docker-entrypoint.sh"],
        "Cmd":["nginx","-g","daemon off;"],
        "User":"root",
        "WorkingDir":"/usr/share/nginx/html",
        "ExposedPorts":{"80/tcp":{},"9000/tcp":{}}
      },
      "State":{
        "Status":"running",
        "Running":true,
        "ExitCode":0,
        "OOMKilled":false,
        "StartedAt":"2026-11-04T10:00:01.987654321Z",
        "FinishedAt":"0001-01-01T00:00:00Z",
        "Health":{"Status":"healthy","FailingStreak":0}
      },
      "Image":"sha256:abcdef0123456789",
      "RestartCount":2,
      "HostConfig":{"RestartPolicy":{"Name":"unless-stopped","MaximumRetryCount":0}},
      "NetworkSettings":{
        "Networks":{"bridge":{"IPAddress":"172.17.0.2"}},
        "Ports":{"80/tcp":[{"HostIp":"0.0.0.0","HostPort":"8080"}],"9000/tcp":null}
      },
      "Mounts":[
        {"Type":"bind","Source":"/srv/www","Destination":"/usr/share/nginx/html","RW":true},
        {"Type":"volume","Source":"/var/lib/docker/volumes/cache/_data","Destination":"/cache","RW":false}
      ]
    }]"#;

    const STATS: &str = r#"{
      "read":"2026-11-04T12:00:00Z",
      "precpu_stats":{"cpu_usage":{"total_usage":1000000000},"system_cpu_usage":10000000000},
      "cpu_stats":{"cpu_usage":{"total_usage":1500000000},"system_cpu_usage":12000000000,"online_cpus":4},
      "memory_stats":{"usage":134217728,"limit":2147483648,"stats":{"cache":8388608}},
      "networks":{"eth0":{"rx_bytes":1048576,"tx_bytes":2048}},
      "pids_stats":{"current":12},
      "blkio_stats":{"io_service_bytes_recursive":[
        {"op":"Read","value":4096},{"op":"Write","value":8192}
      ]}
    }"#;
}

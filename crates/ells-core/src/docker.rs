//! Docker 容器的只读数据层：把 Docker Engine API 的 JSON 或
//! `docker ps --format '{{json .}}'` 的行式 JSON 收敛成同一份结构。
//!
//! 只做解析与投影，不碰网络——网络那条腿在 `ssh.rs` 里，方便单测。

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
}

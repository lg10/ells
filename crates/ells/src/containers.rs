//! 容器视图的状态与纯投影。网络与解析在 `ells_core`，这里只管"打开着的那一屏"。

use ells_core::docker::{dedupe_ports, DockerContainer, PortMapping};

/// 底栏一句提示的存活时间：`docker ps` 打完之后提示一会儿就收，
/// 免得用户早就去干别的了，底栏还挂着"F4 看容器"。
const HINT_TTL: std::time::Duration = std::time::Duration::from_secs(12);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DockerTab {
    #[default]
    Containers,
    Ports,
}

/// 端口中心视图的一行：一条**已发布**的映射（没发布的暴露口不进这张表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortRow {
    pub name: String,
    pub ip: String,
    pub public_port: u16,
    pub private_port: u16,
    pub proto: String,
}

#[derive(Debug, Default)]
pub struct DockerView {
    pub open: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub items: Vec<DockerContainer>,
    pub selected: usize,
    /// 连 `docker ps -a`（含已停止）还是只看运行中的
    pub all: bool,
    /// 选中行的端口明细展开着没有
    pub expanded: bool,
    pub tab: DockerTab,
    /// 这一轮请求的代号：关掉、刷新、切 `all` 都 +1，旧回包因此作废
    seq: u64,
}

impl DockerView {
    /// 发起一轮查询（也用于刷新 / 切换 `all`）：旧回包从此对不上代号。
    pub fn begin_fetch(&mut self, all: bool) -> u64 {
        self.all = all;
        self.open = true;
        self.loading = true;
        self.error = None;
        self.seq += 1;
        self.seq
    }

    /// 关掉视图并作废在途回包。
    pub fn close(&mut self) {
        self.open = false;
        self.loading = false;
        self.seq += 1;
    }

    /// 断开 / 关标签：连数据一起清掉。代号继续往上走，在途的旧回包不能对上新会话。
    pub fn reset(&mut self) {
        let seq = self.seq;
        *self = DockerView::default();
        self.seq = seq;
    }

    pub fn adopt(
        &mut self,
        seq: u64,
        res: Result<Vec<DockerContainer>, String>,
    ) -> bool {
        if seq != self.seq {
            return false;
        }
        self.loading = false;
        match res {
            Ok(items) => {
                self.items = items;
                self.error = None;
            }
            Err(e) => {
                self.items.clear();
                self.error = Some(e);
            }
        }
        if self.selected >= self.rows_len() {
            self.selected = self.rows_len().saturating_sub(1);
        }
        true
    }

    pub fn rows_len(&self) -> usize {
        match self.tab {
            DockerTab::Containers => self.items.len(),
            DockerTab::Ports => self.ports().len(),
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        let len = self.rows_len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, len as isize - 1) as usize;
    }

    pub fn toggle_tab(&mut self) {
        self.tab = match self.tab {
            DockerTab::Containers => DockerTab::Ports,
            DockerTab::Ports => DockerTab::Containers,
        };
        self.selected = 0;
        self.expanded = false;
    }

    pub fn running_count(&self) -> usize {
        self.items.iter().filter(|c| c.is_running()).count()
    }

    /// 端口中心：只留发布了宿主口的映射，IPv4/IPv6 通配双绑折叠，
    /// 按宿主端口升序——找"8080 被谁占了"是第一诉求。
    pub fn ports(&self) -> Vec<PortRow> {
        let mut rows: Vec<PortRow> = self
            .items
            .iter()
            .flat_map(|c| {
                dedupe_ports(&c.ports)
                    .into_iter()
                    .filter_map(|p| p.public_port.map(|public| (c, public, p)))
                    .map(|(c, public_port, p)| PortRow {
                        name: c.name.clone(),
                        ip: p.ip,
                        public_port,
                        private_port: p.private_port,
                        proto: p.proto,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        rows.sort_by(|a, b| {
            a.public_port
                .cmp(&b.public_port)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.proto.cmp(&b.proto))
        });
        rows
    }

    /// 选中行要展开的端口明细（容器页用）。没有端口就返回空。
    pub fn selected_ports(&self) -> Vec<PortMapping> {
        self.items
            .get(self.selected)
            .map(|c| dedupe_ports(&c.ports))
            .unwrap_or_default()
    }

    /// 选中容器展开出来的端口明细行数（最多 4 行，面板给它的位置就这么多）。
    pub fn detail_rows(&self) -> usize {
        if !self.expanded || self.tab != DockerTab::Containers {
            return 0;
        }
        self.selected_ports().len().min(4)
    }

    /// 复制选中行：TSV，粘到表格软件里就是一列对一格。
    pub fn copy_text(&self) -> Option<String> {
        match self.tab {
            DockerTab::Containers => {
                let c = self.items.get(self.selected)?;
                Some(format!(
                    "{}\t{}\t{}\t{}\t{}",
                    c.name,
                    c.image,
                    c.state,
                    c.status,
                    ports_summary(&c.ports)
                ))
            }
            DockerTab::Ports => {
                let r = self.ports().get(self.selected)?.clone();
                Some(format!(
                    "{}\t{}\t{}\t{}/{}",
                    r.name, r.ip, r.public_port, r.private_port, r.proto
                ))
            }
        }
    }
}

/// 一个容器全部端口的紧凑写法：`8080→80/tcp, 9000/tcp（仅暴露）`。
pub fn ports_summary(ports: &[PortMapping]) -> String {
    let ports = dedupe_ports(ports);
    if ports.is_empty() {
        return "—".to_string();
    }
    ports
        .iter()
        .map(|p| match p.public_port {
            Some(public) => format!("{public}→{}/{}", p.private_port, p.proto),
            None => format!("{}/{}（仅暴露）", p.private_port, p.proto),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// 绑定地址的人话：通配地址意味着这台机器对外可达。
pub fn exposure(ip: &str) -> &str {
    match ip {
        "" => "仅暴露",
        "0.0.0.0" | "::" => "全网可达",
        ip => ip,
    }
}

/// 这一屏输出像不像 `docker ps` 的表头。只认**同一行**里同时出现
/// CONTAINER ID / IMAGE 和 STATUS 或 PORTS：偶然打印到这几个单词的日志不算。
pub fn looks_like_docker_ps(text: &str) -> bool {
    text.lines().any(|line| {
        line.contains("CONTAINER ID")
            && line.contains("IMAGE")
            && (line.contains("STATUS") || line.contains("PORTS") || line.contains("COMMAND"))
    })
}

/// 滚动扫描窗口：`docker ps` 的表头完全可能被拆在两个输出块里
/// （SSH 的包边界不按行切），所以留一小截尾巴再匹配。
#[derive(Debug, Default)]
pub struct DockerHint {
    tail: Vec<u8>,
}

impl DockerHint {
    const MAX: usize = 8 * 1024;

    /// 喂一段远端输出；这一次是否**新**认出表头。
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return false;
        }
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > Self::MAX {
            let cut = self.tail.len() - Self::MAX;
            self.tail.drain(..cut);
        }
        if looks_like_docker_ps(&String::from_utf8_lossy(&self.tail)) {
            // 认出来就把尾巴清掉：紧接着的表格行不该反复刷新提示。
            self.tail.clear();
            return true;
        }
        false
    }
}

#[derive(Debug, Default)]
pub struct DockerHintClock {
    hint: DockerHint,
    until: Option<std::time::Instant>,
}

impl DockerHintClock {
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.hint.feed(bytes) {
            self.until = Some(std::time::Instant::now() + HINT_TTL);
        }
    }

    pub fn active(&self) -> bool {
        self.until.is_some_and(|t| std::time::Instant::now() < t)
    }

    pub fn clear(&mut self) {
        self.until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ells_core::docker::PortMapping;

    fn c(name: &str, state: &str, ports: Vec<PortMapping>) -> DockerContainer {
        DockerContainer {
            id: format!("{name:0<12}"),
            name: name.to_string(),
            image: "img".to_string(),
            state: state.to_string(),
            status: "status".to_string(),
            ports,
        }
    }

    fn p(ip: &str, public: Option<u16>, private: u16) -> PortMapping {
        PortMapping {
            ip: ip.to_string(),
            private_port: private,
            public_port: public,
            proto: "tcp".to_string(),
        }
    }

    #[test]
    fn detects_docker_ps_header_even_with_ansi() {
        let colored = "\x1b[1mCONTAINER ID\x1b[0m   IMAGE     COMMAND   CREATED   STATUS    PORTS     NAMES\r\n";
        assert!(looks_like_docker_ps(colored));
        assert!(!looks_like_docker_ps("$ ls -la\r\ntotal 0\r\n"));
    }

    #[test]
    fn hint_survives_split_chunks_and_fires_once() {
        let mut h = DockerHint::default();
        assert!(!h.feed(b"output\r\nCONTAINER"));
        assert!(h.feed(b" ID   IMAGE   COMMAND   STATUS   PORTS\r\nabc web\r\n"));
        assert!(!h.feed(b"remaining rows\r\n"));
    }

    #[test]
    fn port_rows_are_published_only_deduped_and_sorted() {
        let view = DockerView {
            items: vec![
                c("web", "running", vec![
                    p("0.0.0.0", Some(8080), 80),
                    p("::", Some(8080), 80),
                    p("", None, 9000),
                ]),
                c("db", "running", vec![p("127.0.0.1", Some(5432), 5432)]),
            ],
            ..Default::default()
        };
        let rows = view.ports();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].public_port, 5432, "按宿主端口升序");
        assert_eq!(rows[1].name, "web");
        assert_eq!(rows[1].ip, "0.0.0.0");
    }

    #[test]
    fn reset_does_not_let_a_stale_result_back_in() {
        let mut view = DockerView::default();
        let old = view.begin_fetch(false);
        view.reset();
        let fresh = view.begin_fetch(false);
        assert_ne!(old, fresh);
        assert!(!view.adopt(old, Ok(vec![c("ghost", "running", vec![])])));
        assert!(view.items.is_empty());
    }

    #[test]
    fn stale_results_are_ignored() {
        let mut view = DockerView::default();
        let fresh = view.begin_fetch(false);
        let stale = view.begin_fetch(true);
        assert_ne!(fresh, stale);
        assert!(!view.adopt(fresh, Ok(vec![c("old", "running", vec![])])));
        assert!(view.adopt(stale, Ok(vec![c("new", "running", vec![])])));
        assert_eq!(view.items[0].name, "new");
        assert!(!view.loading);
    }

    #[test]
    fn selection_clamps_after_shrink() {
        let mut view = DockerView {
            items: vec![c("a", "running", vec![]), c("b", "running", vec![])],
            selected: 1,
            ..Default::default()
        };
        let seq = view.begin_fetch(false);
        view.adopt(seq, Ok(vec![c("a", "running", vec![])]));
        assert_eq!(view.selected, 0);
        view.move_by(-3);
        assert_eq!(view.selected, 0);
        view.move_by(99);
        assert_eq!(view.selected, 0);
    }

    #[test]
    fn copy_text_is_tab_separated() {
        let mut view = DockerView {
            items: vec![c("web", "running", vec![p("0.0.0.0", Some(8080), 80)])],
            ..Default::default()
        };
        assert_eq!(
            view.copy_text().unwrap(),
            "web\timg\trunning\tstatus\t8080→80/tcp"
        );
        view.tab = DockerTab::Ports;
        assert_eq!(
            view.copy_text().unwrap(),
            "web\t0.0.0.0\t8080\t80/tcp"
        );
    }

    #[test]
    fn summary_marks_exposed_only_ports() {
        let ports = vec![p("0.0.0.0", Some(8080), 80), p("", None, 9000)];
        assert_eq!(ports_summary(&ports), "8080→80/tcp, 9000/tcp（仅暴露）");
        assert_eq!(ports_summary(&[]), "—");
    }

    #[test]
    fn exposure_reads_as_human() {
        assert_eq!(exposure("0.0.0.0"), "全网可达");
        assert_eq!(exposure("::"), "全网可达");
        assert_eq!(exposure("127.0.0.1"), "127.0.0.1");
        assert_eq!(exposure(""), "仅暴露");
    }
}

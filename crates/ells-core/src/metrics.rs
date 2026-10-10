//! 远端主机指标：会话页底部那排 CPU / 内存 / 磁盘进度条的数据来源。
//!
//! 这里只有**数据形状**和**纯计算**，不碰网络连接——采集走
//! `crate::ssh::ProbeTarget::gather`：优先在同一条已认证的连接上用 SFTP 读
//! `/proc`、用 `statvfs@openssh.com` 问磁盘，服务器不配合才回落一条一次性 exec。
//! 把过滤伪文件系统、复算 `df` 的 Capacity 这些判断留在 Rust 侧是有意的：它们容易
//! 出错，而塞进远端 shell 的一小段 awk 就没法被单测覆盖。

/// 兜底路径的探针命令（主路径是 SFTP，见 `crate::ssh::ProbeTarget::gather`）。
///
/// 只用 POSIX sh + `sed` + `grep` + `cat` + `df`：不假设 bash，也不假设 coreutils 的
/// `timeout`（很多机器没有）。`2>/dev/null` 保证没有 /proc 的机器照样把后面的
/// `df` 跑完。第一行的 `ellsm1` 是心跳标记：它在，说明命令真的执行了、只是这台
/// 机器没给这些数据；它不在，说明连 exec 通道都不通（比如服务器只允许 forced-command）。
pub const PROBE_COMMAND: &str = "echo ellsm1; sed -n '1p' /proc/stat 2>/dev/null; \
grep -E '^Mem(Total|Available|Free|Buffers|Cached|SReclaimable):' /proc/meminfo 2>/dev/null; \
cat /proc/loadavg 2>/dev/null; df -Pk 2>/dev/null";

/// 一次探针回包解析出来的原始指标。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Probe {
    /// 是否看到心跳标记。
    pub alive: bool,
    /// CPU jiffies 快照（全机汇总）；没有 /proc/stat 时是 `None`。
    pub cpu: Option<CpuSample>,
    /// 内存（KB）；没有 /proc/meminfo 时是 `None`。
    pub mem: Option<MemSample>,
    /// 平均负载；没有 /proc/loadavg 时是 `None`。
    pub load: Option<LoadSample>,
    /// 已经筛掉伪文件系统、按使用率降序排好的磁盘。
    pub disks: Vec<DiskSample>,
}

/// `/proc/stat` 第一行的累计 jiffies。
///
/// CPU 利用率必须**两次快照做差**：单看一次只有开机以来的平均值，
/// 那根条会几乎不动，看着像坏了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuSample {
    pub total: u64,
    pub idle: u64,
}

/// `/proc/meminfo` 里能凑出可用内存的那几行（单位 KB）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemSample {
    pub total_kb: u64,
    pub avail_kb: u64,
}

/// `/proc/loadavg` 的三个滑动平均： runnable 任务的平均数（1/5/15 分钟）。
///
/// 它和 CPU% 不是重复信息：CPU% 说"这一瞬时核被占了多久"，负载说"有多少活儿在排队"。
/// 8 核机器上 50% 的 CPU% 很闲，负载 24 就是已经在挤了 —— 只看一根条会看漏。
/// 界面上写成 `0.42/0.31/0.19`，用户 `uptime` 一对照就是同一串数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadSample {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

/// `df -Pk` 的一行。
///
/// `used_percent` 直接取 Capacity 列，不自己算：`used/(used+avail)` 会因为 ext4
/// 预留块与 `df` 差几个点，而用户转头 `df` 一对照看到不一致，只会认为 ells 在编数字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskSample {
    pub device: String,
    pub mount: String,
    pub used_percent: u8,
    pub used_kb: u64,
    pub total_kb: u64,
}

impl Probe {
    /// 解析 exec 兜底路径的回包。逐行按形状认领，认不出的丢掉：远端的 shell rc
    /// 有时会往 stdout 多吐一行欢迎语，那不该让整排进度条变成空。
    pub fn parse(text: &str) -> Probe {
        Probe {
            alive: text.lines().any(|line| line.trim() == "ellsm1"),
            cpu: cpu_from_stat(text),
            mem: mem_from_meminfo(text),
            load: load_from_loadavg(text),
            disks: disks_from_df(text),
        }
    }

    /// 用另一次采集**只补缺的那几格**，已有数值一律不覆盖。
    ///
    /// 这条规则是为了混合路径：SFTP 读到了 `/proc`、却问不到磁盘（服务器没实现
    /// `statvfs@openssh.com`），补一轮 exec 就只把磁盘填进去。反过来要是让 exec
    /// 整份盖掉，CPU 就可能出现"SFTP 这一轮减 exec 上一轮"——两个时刻、两种来源
    /// 做差，画出来的是一条没有意义的斜线。
    pub fn fill_gaps(&mut self, other: &Probe) {
        self.alive |= other.alive;
        if self.cpu.is_none() {
            self.cpu = other.cpu;
        }
        if self.mem.is_none() {
            self.mem = other.mem;
        }
        if self.load.is_none() {
            self.load = other.load;
        }
        if self.disks.is_empty() {
            self.disks = other.disks.clone();
        }
    }

    /// 这几项里有没有任何一项是真数。
    pub fn has_data(&self) -> bool {
        self.cpu.is_some() || self.mem.is_some() || self.load.is_some() || !self.disks.is_empty()
    }

    /// 要显示的那块盘：真实文件系统里使用率最高的一个。
    pub fn worst_disk(&self) -> Option<&DiskSample> {
        self.disks.first()
    }
}

/// `/proc/stat`（整份文件或只有汇总行都行）→ 快照。
///
/// 只有 `cpu ` 开头那一行认领得到汇总数据：紧跟其后的 `cpu0`、`cpu1` 是每核数据，
/// 前缀不同、正好认不到，所以不必要求调用方先把文件剪到第一行。
pub fn cpu_from_stat(text: &str) -> Option<CpuSample> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("cpu ").and_then(parse_cpu_line))
}

/// `/proc/meminfo` → 可用内存。
pub fn mem_from_meminfo(text: &str) -> Option<MemSample> {
    let mut total = None;
    let mut avail = None;
    let mut free = None;
    let mut buffers = None;
    let mut cached = None;
    let mut reclaimable = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let digits = value.trim().trim_end_matches(" kB").trim();
        let Ok(v) = digits.parse::<u64>() else { continue };
        match key.trim() {
            "MemTotal" => total = Some(v),
            "MemAvailable" => avail = Some(v),
            "MemFree" => free = Some(v),
            "Buffers" => buffers = Some(v),
            "Cached" => cached = Some(v),
            "SReclaimable" => reclaimable = Some(v),
            _ => {}
        }
    }
    match (total, avail) {
        (Some(total), Some(avail)) => Some(MemSample { total_kb: total, avail_kb: avail.min(total) }),
        // 内核 3.14 以前没有 MemAvailable，按老办法凑 free + buffers + cached
        // （+ SReclaimable：现在的可回收 slab 确实顶得上可用内存）
        (Some(total), None) => {
            let avail = free.unwrap_or(0) + buffers.unwrap_or(0) + cached.unwrap_or(0)
                + reclaimable.unwrap_or(0);
            Some(MemSample { total_kb: total, avail_kb: avail.min(total) })
        }
        _ => None,
    }
}

/// `df -Pk` 的输出 → 已筛掉伪文件系统、按使用率降序的磁盘。
pub fn disks_from_df(text: &str) -> Vec<DiskSample> {
    rank_disks(text.lines().filter_map(parse_df_line).collect())
}

/// `/proc/loadavg` → 1/5/15 分钟平均负载。
///
/// 认领条件很挑：前三个字段都得是**非负有限**小数，第四个必须是 `运行中/总数`。
/// 这一行不像 `cpu ` 或 `MemTotal:` 有专属前缀可认，所以宁可少认一行 ——
/// 远端 shell rc 多吐的欢迎语、或者某个奇怪的 df 折行，都不该被当成负载画出来。
pub fn load_from_loadavg(text: &str) -> Option<LoadSample> {
    text.lines().find_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            return None;
        }
        let nums: Vec<f64> = cols[..3].iter().filter_map(|v| v.parse::<f64>().ok()).collect();
        if nums.len() != 3 || !nums.iter().all(|n| n.is_finite() && *n >= 0.0 && *n < 1000.0) {
            return None;
        }
        // 第四列 `1/234`：running/total。它在这儿才让这一行和"三个随便的小数"分得开。
        let (running, total) = cols[3].split_once('/')?;
        if running.parse::<u32>().is_err() || total.parse::<u32>().is_err() {
            return None;
        }
        Some(LoadSample { one: nums[0], five: nums[1], fifteen: nums[2] })
    })
}

impl LoadSample {
    /// 界面细节里那一串数：`0.42/0.31/0.19`，和 `uptime` 给的顺序一致。
    pub fn display(&self) -> String {
        format!("{:.2}/{:.2}/{:.2}", self.one, self.five, self.fifteen)
    }
}

/// 两条采集路径共用的整理：丢掉常年 100% 的只读盘和伪文件系统，再把最高的排前面。
pub fn rank_disks(disks: Vec<DiskSample>) -> Vec<DiskSample> {
    let mut real: Vec<DiskSample> = disks.into_iter().filter(|d| !is_pseudo(d)).collect();
    // 使用率最高的排前面：那一格要回答的是"哪块盘快满了"。同一设备被 bind
    // 到两处也只会被挑一次。
    real.sort_by_key(|d| std::cmp::Reverse(d.used_percent));
    real
}

/// `user nice system idle iowait irq softirq steal guest guest_nice` → 快照。
///
/// 空闲按内核的口径算 `idle + iowait`：等 IO 的时间 CPU 确实在闲。把它算进忙碌
/// 只会让磁盘慢的机器显得 CPU 更忙，那不是用户想看到的归因。
///
/// 只要 `cpu ` 开头那一行（全机汇总）：紧跟其后的 `cpu0`、`cpu1` 是每核数据，
/// 前缀不同，正好认领不到——摘要行才是这一格要的东西。
fn parse_cpu_line(rest: &str) -> Option<CpuSample> {
    let nums: Vec<u64> = rest
        .split_whitespace()
        .filter_map(|v| v.parse::<u64>().ok())
        .collect();
    if nums.len() < 4 {
        return None;
    }
    let total = nums.iter().take(8).sum();
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    if total == 0 {
        return None;
    }
    Some(CpuSample { total, idle: idle.min(total) })
}

/// `df -Pk` 的一行：`Filesystem 1024-blocks Used Available Capacity Mounted-on`。
///
/// 只认这一种列序，别的（长设备名折行、非 POSIX 的实现）一律丢掉，不猜列。
/// 挂载点里带空格的用后面的列拼回去，`/cygdrive/...` 之类照收。
fn parse_df_line(line: &str) -> Option<DiskSample> {
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 6 || cols[0] == "Filesystem" {
        return None;
    }
    let percent = cols[4].strip_suffix('%')?.parse::<u8>().ok()?;
    let total_kb = cols[1].parse::<u64>().ok()?;
    let used_kb = cols[2].parse::<u64>().ok()?;
    if total_kb == 0 {
        return None;
    }
    Some(DiskSample {
        device: cols[0].to_string(),
        mount: cols[5..].join(" "),
        used_percent: percent,
        used_kb,
        total_kb,
    })
}

/// `statvfs@openssh.com` 回包里算使用率要用的那几个数。
///
/// 字段名跟着扩展的线格式走，但类型是我们自己的：这样"从块数算百分比"这件事
/// 能被单测覆盖，不必连一个 SFTP 服务器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsStats {
    /// `f_bsize`：建议的 IO 块大小。
    pub block_size: u64,
    /// `f_frsize`：`blocks`/`blocks_free`/`blocks_avail` 的单位。
    pub fragment_size: u64,
    pub blocks: u64,
    pub blocks_free: u64,
    /// 非特权用户真能用到的空闲块数（ext4 的预留块不在这里）。
    pub blocks_avail: u64,
}

/// 把一次 `fs_info` 换算成和 `df -P` 同源的一行磁盘数据。
///
/// 百分比刻意复算 `df` 的式子（`used/(used+avail)` **向上取整**，分母不是总块数）：
/// ext4 预留块那 5% 用户根本写不进去，算成可用会让 ells 比 `df` 显得乐观，而用户
/// 转头敲一个 `df` 就对不上号。对得上号比"更精确"重要。
pub fn disk_from_fs_stats(device: String, mount: String, stats: &FsStats) -> Option<DiskSample> {
    // 块数一律按 f_frsize 解释；某些实现把 f_frsize 留 0，那时才退回 f_bsize。
    let unit = match (stats.fragment_size, stats.block_size) {
        (frsize, _) if frsize > 0 => frsize,
        (_, bsize) if bsize > 0 => bsize,
        _ => return None,
    };
    if stats.blocks == 0 {
        return None;
    }
    let used = stats.blocks.saturating_sub(stats.blocks_free);
    let denom = used.saturating_add(stats.blocks_avail);
    if denom == 0 {
        return None;
    }
    let percent = (used.saturating_mul(100).saturating_add(denom - 1) / denom).min(100) as u8;
    Some(DiskSample {
        device,
        mount,
        used_percent: percent,
        used_kb: to_kib(used, unit),
        total_kb: to_kib(stats.blocks, unit),
    })
}

/// 块数 → 1024-block（和 `df -Pk` 同单位），用 128 位中间量免得大盘溢出。
fn to_kib(blocks: u64, unit_bytes: u64) -> u64 {
    let bytes = blocks as u128 * unit_bytes as u128;
    (bytes / 1024).min(u64::MAX as u128) as u64
}

/// 一个值得为它发一次 `statvfs` 的挂载点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountCandidate {
    pub device: String,
    pub mount: String,
}

/// 每轮最多问几块盘：每次 `fs_info` 是一个往返，第 20 块盘不会让那一格更有用，
/// 而"哪块最快满"在排好序的前几块里就答完了。
pub const MAX_MOUNT_CANDIDATES: usize = 8;

/// 从 `/proc/mounts` 挑出值得 statvfs 的挂载点。
///
/// 网络盘（nfs/cifs/fuse/…）在这里就被筛掉，不是嫌它们不准，而是**不敢问**：
/// 远端 sftp-server 是单线程的，一次卡在失效 NFS 挂载点上的 `statvfs` 会把整条
/// SFTP 会话一起拖住，连文件传输面板都跟着停。ells 的传输面板和采集用的就是同一条
/// SFTP 会话，这个代价不能为了让磁盘那一格多一块 nfs 而付。
pub fn mount_candidates(text: &str) -> Vec<MountCandidate> {
    let mut out: Vec<MountCandidate> = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 3 {
            continue;
        }
        let (device, mount, fstype) = (cols[0], unescape_mount(cols[1]), cols[2]);
        // statvfs 要绝对路径；`/proc/self/mounts` 里也有挂载在相对位置上的怪东西。
        if !mount.starts_with('/') {
            continue;
        }
        if skip_fstype(fstype) {
            continue;
        }
        if PSEUDO_MOUNTS
            .iter()
            .any(|p| mount == *p || mount.starts_with(&format!("{p}/")))
        {
            continue;
        }
        if PSEUDO_DEVICES.iter().any(|p| device == *p || device.starts_with(p)) {
            continue;
        }
        // 同一设备 bind 到多处（或 overlay 套同一块宿主盘）只问一次。
        if out.iter().any(|c| c.device == device) {
            continue;
        }
        out.push(MountCandidate { device: device.to_string(), mount });
        if out.len() >= MAX_MOUNT_CANDIDATES {
            break;
        }
    }
    out
}

/// 伪文件系统和"问了可能把人卡住"的文件系统类型。
fn skip_fstype(fstype: &str) -> bool {
    // fuse.* / sshfs / rclone 之类都归到含 "fuse" 的那一条：用户态进程挂了以后
    // statvfs 是**真的**会一直不返回。
    if fstype.contains("fuse") || fstype.contains("nfs") {
        return true;
    }
    PSEUDO_FSTYPES
        .iter()
        .chain(NETWORK_FSTYPES.iter())
        .any(|p| fstype == *p || fstype.starts_with(p))
}

/// 内核自己造的那些盘：容量是内存或干脆是假的，算进"哪块盘快满了"只会误导。
const PSEUDO_FSTYPES: [&str; 12] = [
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "tmpfs",
    "ramfs",
    "cgroup",
    "pstore",
    "debugfs",
    "autofs",
    "squashfs",
    "iso9660",
];

const NETWORK_FSTYPES: [&str; 10] = [
    "cifs",
    "smb",
    "smbfs",
    "davfs",
    "webdav",
    "9p",
    "glusterfs",
    "lustre",
    "ocfs2",
    "gfs2",
];

/// 一条会话上磁盘那一路的缓存：跟单哪个挂载点 + statvfs 能不能问。
///
/// 为什么要缓存：一次普查是 1 次读 `/proc/mounts` 外加**每块盘一次** `statvfs`（最多
/// `MAX_MOUNT_CANDIDATES` 个往返），而 60 秒里挂载点集合几乎一动不动。有了这份缓存，
/// 中间那些轮只对"当前最满那块"发一次 `statvfs` —— 那一格 5 秒一档，代价却和
/// CPU/内存同一量级。
///
/// 住在 `Arc<Mutex<…>>` 里、由 `ProbeTarget` 持有：采集任务每轮摘一份句柄，锁只护住
/// "这一轮该问谁"这个决定，任何锁守卫都不能跨过 `await`（否则一条慢 SFTP 会把别的
/// 轮次一起钉住）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskState {
    /// 下一轮要跟单的挂载点；`None` 表示"下一轮得先普查"（还没建缓存，或上一块盘问不到了）。
    pub follow: Option<String>,
    /// 这台服务器对 `statvfs@openssh.com` 的态度：`None` 还没试过，`Some(false)` 就别再问。
    pub statvfs_supported: Option<bool>,
}

/// 这一轮磁盘该走哪条路。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskPlan {
    /// 读 `/proc/mounts` 并逐块 `statvfs`：重建候选表、重新决定谁最满。
    Survey,
    /// 只对这一个挂载点问一次 `statvfs`。
    Follow(String),
    /// 一个磁盘请求都不发，那一格沿用上次的数。
    Skip,
}

/// 这一轮磁盘怎么走。纯函数，把"到没到普查点 / 有没有得跟 / 这台问不问得动"一次说清，
/// 好让这几条分支都能被单测钉住。
///
/// 顺序是有意为之的：
/// - **问不动就 Skip**：一台没有 `statvfs@openssh.com` 的服务器，跟单轮和普查轮都得安静，
///   否则磁盘那一格就变成每 5 秒 fork 一个 shell 跑 `df`（`gather` 那边的兜底只认调用方
///   点名的那一次普查）；
/// - 到普查点、或者缓存里没得跟（第一轮之前 / 上一块盘已经不在了）→ 普查，让它自愈；
/// - 其余轮次跟单那一块。
#[must_use]
pub fn plan_disks(
    want_survey: bool,
    follow: Option<&str>,
    statvfs_supported: Option<bool>,
) -> DiskPlan {
    if statvfs_supported == Some(false) {
        return DiskPlan::Skip;
    }
    if want_survey || follow.is_none() {
        return DiskPlan::Survey;
    }
    DiskPlan::Follow(follow.expect("上面已经挡掉 None").to_string())
}

/// `/proc/mounts` 把挂载点名里的空格、制表、换行和反斜杠写成三位八进制转义。
fn unescape_mount(mount: &str) -> String {
    if !mount.contains('\\') {
        return mount.to_string();
    }
    // 按 char 走：挂载点名字里可以有中文，按字节解 would 把它拆成一堆 Latin-1 乱码。
    let cs: Vec<char> = mount.chars().collect();
    let mut out = String::with_capacity(mount.len());
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '\\'
            && i + 3 < cs.len()
            && cs[i + 1..i + 4].iter().all(|d| matches!(d, '0'..='7'))
        {
            let oct: String = cs[i + 1..i + 4].iter().collect();
            // 内核只写这三个转义，别把 \255 之类解成半截 UTF-8 序列的头。
            if let Ok(byte) = u8::from_str_radix(&oct, 8) {
                if matches!(byte, b' ' | b'\t' | b'\n' | b'\\') {
                    out.push(byte as char);
                    i += 4;
                    continue;
                }
            }
        }
        // 未知转义或尾部残字节：按原样留着，宁可挂载点丑一点，也不丢整块盘。
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// 常年 100% 的只读快照盘、伪文件系统和 EFI 分区：算进去只会让那一格永远红着，

/// 而用户根本动不了它们。
fn is_pseudo(d: &DiskSample) -> bool {
    let mount = d.mount.as_str();
    if PSEUDO_MOUNTS
        .iter()
        .any(|p| mount == *p || mount.starts_with(&format!("{p}/")))
    {
        return true;
    }
    let dev = d.device.as_str();
    PSEUDO_DEVICES
        .iter()
        .any(|p| dev == *p || dev.starts_with(p))
}

const PSEUDO_MOUNTS: [&str; 7] = [
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/snap",
    "/boot/efi",
    "/var/lib/nfs",
];

const PSEUDO_DEVICES: [&str; 11] = [
    "tmpfs",
    "devtmpfs",
    "udev",
    "shm",
    "none",
    "squashfs",
    "iso9660",
    "overlayfs",
    "proc",
    "sysfs",
    "/dev/loop",
];

/// 两次 CPU 快照之间的利用率百分比。
///
/// 回绕、时钟被调、或两次快照其实同一份（`total` 没变）都返回 `None`，让界面
/// 显示 `—`，而不是编出一个看起来很有把握的数。
pub fn cpu_percent(prev: &CpuSample, now: &CpuSample) -> Option<f64> {
    if now.total <= prev.total || now.idle < prev.idle {
        return None;
    }
    let total_delta = now.total - prev.total;
    let idle_delta = now.idle - prev.idle;
    if total_delta == 0 || idle_delta > total_delta {
        return None;
    }
    let busy = total_delta - idle_delta;
    Some((busy as f64 / total_delta as f64 * 100.0).clamp(0.0, 100.0))
}

/// 内存使用率：`(总 - 可用) / 总`。
pub fn mem_percent(mem: &MemSample) -> Option<f64> {
    if mem.total_kb == 0 {
        return None;
    }
    let used = mem.total_kb.saturating_sub(mem.avail_kb);
    Some((used as f64 / mem.total_kb as f64 * 100.0).clamp(0.0, 100.0))
}

/// 界面用的整数百分比。没有值就保持 `None`：让界面画 `—`。
///
/// 绝不把"没数"折成 0 —— 那一格看着像"这台机器很闲"，而实际是采集没成功，
/// 这类假安静比空白危险得多。
pub fn round_percent(value: Option<f64>) -> Option<u8> {
    value.map(|v| v.round().clamp(0.0, 100.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一台 Ubuntu 22.04 的真实形状：/proc/stat 先来汇总行再逐核，meminfo 是
    /// `Key:  数字 kB`，df 用 -Pk 的固定列序。
    const REAL: &str = concat!(
        "ellsm1\n",
        "cpu  100 0 50 800 20 0 0 3 0 0\n",
        "cpu0 60 0 30 410 10 0 0 2 0 0\n",
        "MemTotal:       16258304 kB\n",
        "MemFree:          120400 kB\n",
        "MemAvailable:    9452112 kB\n",
        "Buffers:          218320 kB\n",
        "Cached:          9044116 kB\n",
        "SReclaimable:      92000 kB\n",
        "0.42 0.31 0.19 1/234 5678\n",
        "Filesystem       1024-blocks      Used  Available Capacity Mounted on\n",
        "/dev/nvme0n1p2    491798416  334388960  117409456      75% /\n",
        "/dev/nvme1n1p1   1014094560  887429888  126664672      88% /data\n",
        "tmpfs                1609008       1232     1607776       1% /dev/shm\n",
        "/dev/nvme0n1p1         974764     350252      624512      36% /boot/efi\n",
        "/dev/loop0               5376       5376           0     100% /snap/core20/1403\n",
        "overlay            491798416  334388960  117409456      75% /var/lib/docker/overlay2/x/merged\n",
    );

    #[test]
    fn parses_a_real_probe_payload() {
        let probe = Probe::parse(REAL);
        assert!(probe.alive);
        // 只认全机汇总那一行，后面的 cpu0 不许把每核数据顶掉
        assert_eq!(probe.cpu, Some(CpuSample { total: 973, idle: 820 }));
        let mem = probe.mem.expect("有 MemTotal 就该有内存");
        assert_eq!((mem.total_kb, mem.avail_kb), (16258304, 9452112));
        let pct = mem_percent(&mem).expect("总不为 0");
        assert!((41.8..42.0).contains(&pct), "实际 {pct}");
        assert_eq!(probe.load.map(|l| (l.one, l.five, l.fifteen)), Some((0.42, 0.31, 0.19)));
        assert_eq!(probe.load.map(|l| l.display()).as_deref(), Some("0.42/0.31/0.19"));
    }

    /// 负载那一串和 `uptime` 得是同一串数，顺序也是 1/5/15 分钟。
    #[test]
    fn loadavg_is_claimed_by_its_shape() {
        let load = load_from_loadavg("0.00 0.01 0.05 1/158 12345\n").expect("该认出来");
        assert_eq!((load.one, load.five, load.fifteen), (0.0, 0.01, 0.05));
        assert_eq!(load.display(), "0.00/0.01/0.05");
        // 大机器上负载上百也是正常数字
        let hot = load_from_loadavg("128.50 96.25 71.00 130/4021 99112\n").expect("也该认");
        assert_eq!(hot.display(), "128.50/96.25/71.00");
        // 尾部被读断（只剩 4 列）也照样能用：那一行前四个字段已经够 distinctive
        assert!(load_from_loadavg("1.2 3.4 5.6 1/234\n").is_some());
    }

    /// 这一行没有专属前缀可认，所以认领条件要挑：欢迎语、df 的折行、负数
    /// 都不该被当成负载画出来 —— 画错了比不画更糟。
    #[test]
    fn loadavg_shape_is_picky() {
        for text in [
            "Welcome to production-server!\n",
            "Filesystem 1024-blocks Used Available\n",
            "1.2 3.4 5.6 not-a-fraction 789\n",
            "-0.5 0.31 0.19 1/234 5678\n",
            "cpu  100 0 50 800 20 0 0 3 0 0\n",
        ] {
            assert_eq!(load_from_loadavg(text), None, "不该认领：{text:?}");
        }
        // 前面有几行垃圾也能找到真正那一行
        let probe = Probe::parse("Motd line one\nMotd line two\n0.10 0.20 0.30 1/2 3\nMemTotal: 8 kB\n");
        assert_eq!(probe.load.map(|l| l.one), Some(0.10));
        assert_eq!(probe.mem.map(|m| m.total_kb), Some(8));
    }

    /// 那一格要回答"哪块盘快满了"：挑使用率最高的**真实**挂载点。
    /// tmpfs、snap 的只读盘、EFI 分区都不算，docker 的 overlay 算（它吃的是宿主 /）。
    #[test]
    fn disk_is_the_worst_real_filesystem() {
        let probe = Probe::parse(REAL);
        let mounts: Vec<&str> = probe.disks.iter().map(|d| d.mount.as_str()).collect();
        assert_eq!(mounts, vec!["/data", "/", "/var/lib/docker/overlay2/x/merged"]);
        let worst = probe.worst_disk().expect("至少有一块盘");
        assert_eq!((worst.mount.as_str(), worst.used_percent), ("/data", 88));
        assert_eq!((worst.used_kb, worst.total_kb), (887429888, 1014094560));
        assert_eq!(worst.device, "/dev/nvme1n1p1");
    }

    /// 排序必须稳定地"最高的在前"，且并列时别把顺序换来换去让人看不出规律。
    #[test]
    fn disks_are_ordered_by_usage_descending() {
        let text = concat!(
            "ellsm1\n",
            "/dev/sda1 1000 100 900 10% /a\n",
            "/dev/sdb1 1000 900 100 90% /b\n",
            "/dev/sdc1 1000 500 500 50% /c\n",
        );
        let probe = Probe::parse(text);
        assert_eq!(
            probe.disks.iter().map(|d| d.mount.as_str()).collect::<Vec<_>>(),
            vec!["/b", "/c", "/a"]
        );
    }

    #[test]
    fn cpu_percent_is_the_diff_between_two_snapshots() {
        let a = CpuSample { total: 1000, idle: 800 };
        // 这 100 个 jiffies 里只闲了 20 个 → 80% 忙
        let b = CpuSample { total: 1100, idle: 820 };
        assert_eq!(cpu_percent(&a, &b), Some(80.0));
        // 全程空闲 → 0%
        let c = CpuSample { total: 1200, idle: 920 };
        assert_eq!(cpu_percent(&b, &c), Some(0.0));
        // 连发的两次同一快照不许当成 0%（那会让条看起来"刚降下来"）
        assert_eq!(cpu_percent(&c, &c), None);
        // jiffies 回绕 / 时钟被调小
        assert_eq!(cpu_percent(&c, &a), None);
        // 空闲增量比总增量还大＝两份快照不同源（比如中间重启过）
        assert_eq!(
            cpu_percent(&CpuSample { total: 10, idle: 9 }, &CpuSample { total: 20, idle: 8 }),
            None
        );
    }

    /// 内核 < 3.14 没有 MemAvailable，只能 free + buffers + cached 凑；
    /// 这一格要是因此空着，老机器上就永远看不到内存。
    #[test]
    fn memory_falls_back_when_memavailable_is_missing() {
        let text = concat!(
            "ellsm1\n",
            "MemTotal: 1000 kB\n",
            "MemFree: 100 kB\n",
            "Buffers: 50 kB\n",
            "Cached: 300 kB\n",
            "SReclaimable: 50 kB\n",
        );
        let probe = Probe::parse(text);
        let mem = probe.mem.expect("凑得出来");
        assert_eq!((mem.total_kb, mem.avail_kb), (1000, 500));
        assert_eq!(mem_percent(&mem), Some(50.0));
    }

    /// 可用数报得比总数还大（K8s 节点上真见过）要夹回总数，
    /// 否则内存使用率会算成负数。
    #[test]
    fn available_is_never_more_than_total() {
        let probe = Probe::parse("ellsm1\nMemTotal: 1000 kB\nMemAvailable: 4000 kB\n");
        let mem = probe.mem.expect("有数");
        assert_eq!(mem.avail_kb, 1000);
        assert_eq!(mem_percent(&mem), Some(0.0));
    }

    /// 没有 /proc 的机器（FreeBSD、奇怪的容器）不该把整排打成空：
    /// 有 df 就至少磁盘那一格是真的。
    #[test]
    fn keeps_disk_when_proc_is_missing() {
        let probe = Probe::parse("ellsm1\nFilesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 1000 900 100 90% /\n");
        assert!(probe.alive);
        assert!(probe.cpu.is_none());
        assert!(probe.mem.is_none());
        assert_eq!(probe.worst_disk().map(|d| d.used_percent), Some(90));
    }

    /// ZFS 与 Cygwin 的设备名不以 `/` 开头，那是真的盘，不能因为前缀就丢掉。
    #[test]
    fn keeps_real_filesystems_whose_device_is_not_a_path() {
        let probe = Probe::parse(concat!(
            "ellsm1\n",
            "zroot/ROOT/default 1000 700 300 70% /\n",
            "C: 1000 800 200 80% /cygdrive/c\n",
        ));
        assert_eq!(probe.disks.len(), 2, "实际 {:?}", probe.disks);
        assert_eq!(probe.worst_disk().map(|d| d.mount.as_str()), Some("/cygdrive/c"));
    }

    /// exec 通道被服务器上的 forced-command 顶掉时，输出里连心跳都没有。
    /// 界面靠这个把"这台采不到"和"这轮还没到"分开。
    #[test]
    fn missing_heartbeat_means_the_probe_never_ran() {
        let probe = Probe::parse("sh: df: command not found\n");
        assert!(!probe.alive);
        assert!(probe.disks.is_empty());
        assert!(probe.cpu.is_none());
    }

    /// 远端 shell rc 会往 stdout 多吐欢迎语；认不出形状的行必须被丢掉，
    /// 而不是让解析器 panic 或者去猜列。
    #[test]
    fn ignores_banner_noise_and_the_header_line() {
        let noisy = format!("Welcome to production-server!\n{REAL}");
        let probe = Probe::parse(&noisy);
        assert_eq!(probe.disks.len(), 3, "噪声不该改变结果");
        assert!(probe.alive);
        // 只有表头、一行数据都没有的 df 不该被当成一块盘
        let only_header = Probe::parse("ellsm1\nFilesystem 1024-blocks Used Available Capacity Mounted on\n");
        assert!(only_header.disks.is_empty());
    }

    /// `df` 也会报总块数为 0 的怪东西（某些 fuse、卸载没刷掉的挂载点），拿它算
    /// 使用率是 0/0。
    #[test]
    fn zero_block_filesystems_are_not_offered() {
        let probe = Probe::parse("ellsm1\nfoo 0 0 0 100% /weird\nbar 100 50 50 50% /ok\n");
        assert_eq!(probe.disks.len(), 1);
        assert_eq!(probe.disks[0].mount, "/ok");
    }

    /// 列数不够的半截行（通道被超时掐断时真会出现）必须整行丢掉，
    /// 不能拿最后一列当挂载点。
    #[test]
    fn truncated_rows_are_dropped() {
        let probe = Probe::parse("ellsm1\n/dev/sda1 1000 500 500 50%\n/dev/sdb1 1000 500 500 50% /ok\n");
        assert_eq!(probe.disks.len(), 1);
        assert_eq!(probe.disks[0].mount, "/ok");
    }

    #[test]
    fn percent_rounding_saturates_and_keeps_none_absent() {
        assert_eq!(round_percent(Some(41.7)), Some(42));
        assert_eq!(round_percent(Some(-3.0)), Some(0));
        assert_eq!(round_percent(Some(140.0)), Some(100));
        // 没有数就是没有数：折成 0 会变成"这台很空闲"的假象
        assert_eq!(round_percent(None), None);
    }

    /// 主路径的磁盘：`statvfs` 的块数和 `df -Pk` 的 1024-block 描述的是同一块盘，
    /// 两条路径必须给出**一模一样**的数字。差一个点都会被用户抓住 —— 他敲 `df`
    /// 看到的和 ells 画的不该是两套算术。
    #[test]
    fn statvfs_and_df_agree_on_the_same_filesystem() {
        let from_df = Probe::parse("ellsm1\n/dev/sda1 1000 880 120 88% /data\n")
            .disks
            .into_iter()
            .next()
            .expect("df 那一行该认领到");
        let stats = FsStats {
            block_size: 1024,
            fragment_size: 1024,
            blocks: 1000,
            blocks_free: 120,
            blocks_avail: 120,
        };
        let from_statvfs =
            disk_from_fs_stats("/dev/sda1".to_string(), "/data".to_string(), &stats).expect("有数");
        assert_eq!(from_df, from_statvfs, "两条路径算出的是同一件事");
    }

    /// 真实盘大多是 4K 块，而 `df -P` 报的是 1024-block：换算要落在 used_kb/total_kb
    /// 上（界面那格显示 "896 MiB/1.0 GiB" 靠它），百分比则跟 df 一样只看块数比例。
    #[test]
    fn statvfs_percent_is_df_ceiling_and_kb_are_rescaled() {
        // 1 GiB 的盘，用掉 896 MiB、非特权可用 64 MiB → df: 917504/1048576 KiB, 94%
        let stats = FsStats {
            block_size: 4096,
            fragment_size: 4096,
            blocks: 262144,
            blocks_free: 32768,
            blocks_avail: 16384,
        };
        let d = disk_from_fs_stats("/dev/sdb1".to_string(), "/data".to_string(), &stats).unwrap();
        assert_eq!(d.used_percent, 94, "向上取整，和 df 同式");
        assert_eq!((d.used_kb, d.total_kb), (917504, 1048576));
    }

    /// 有些实现（以及某些 FUSE 之外的怪东西）把 f_frsize 留 0，只有 f_bsize 有值。
    #[test]
    fn statvfs_falls_back_to_block_size() {
        let stats = FsStats {
            block_size: 1024,
            fragment_size: 0,
            blocks: 1000,
            blocks_free: 100,
            blocks_avail: 100,
        };
        let d = disk_from_fs_stats("x".to_string(), "/y".to_string(), &stats).unwrap();
        assert_eq!((d.used_percent, d.used_kb, d.total_kb), (90, 900, 1000));
        // 两个尺寸都是 0 就没法换算，宁可不画这一格
        let zero = FsStats { block_size: 0, fragment_size: 0, blocks: 1000, blocks_free: 0, blocks_avail: 0 };
        assert_eq!(disk_from_fs_stats("x".to_string(), "/y".to_string(), &zero), None);
    }

    /// 总块数为 0（某些 fuse、没刷掉的卸载点）算使用率是 0/0，和 df 路径同一态度。
    #[test]
    fn statvfs_skips_empty_and_full_weirdness() {
        let empty = FsStats { block_size: 512, fragment_size: 512, blocks: 0, blocks_free: 0, blocks_avail: 0 };
        assert_eq!(disk_from_fs_stats("x".to_string(), "/y".to_string(), &empty), None);
        // 报出 used > 总块数的怪内核：百分比夹在 100，不画 130%
        let odd = FsStats { block_size: 1024, fragment_size: 1024, blocks: 100, blocks_free: 0, blocks_avail: 0 };
        assert_eq!(disk_from_fs_stats("x".to_string(), "/y".to_string(), &odd).map(|d| d.used_percent), Some(100));
    }

    /// 一台 Ubuntu 的真实 `/proc/mounts`：伪文件系统、网络盘、snap 的只读盘、
    /// bind 重复挂载和 docker 的 overlay 全在里面。
    const MOUNTS: &str = concat!(
        "proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\n",
        "sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0\n",
        "devtmpfs /dev devtmpfs rw,nosuid,relatime 0 0\n",
        "tmpfs /run tmpfs rw,nosuid,nodev,mode=755 0 0\n",
        "/dev/sda1 / ext4 rw,relatime,errors=remount-ro 0 1\n",
        "/dev/sdb1 /data xfs rw,nosuid 0 0\n",
        "/dev/sda1 /home ext4 rw,relatime,bind 0 0\n",
        "server:/export /mnt/nfs nfs4 rw,relatime 0 0\n",
        "//10.0.0.5/share /mnt/smb cifs rw 0 0\n",
        "sshfs#host:/ /mnt/remote fuse.sshfs rw 0 0\n",
        "overlay /var/lib/docker/overlay2/m/merged overlay rw 0 0\n",
        "/dev/loop0 /snap/core20/1403 squashfs ro 0 0\n",
        "/dev/sr0 /media/cdrom iso9660 ro 0 0\n",
        "ramfs /mnt/ram ramfs rw 0 0\n",
    );

    /// 挑哪些盘去 statvfs 的判断标准只有一个：**问了不会出事**。nfs/cifs/fuse 一律
    /// 不碰（一次卡死的 statvfs 会把整条 SFTP 会话拖住，连传输面板一起停），
    /// tmpfs、snap、光驱、ramfs 这些伪盘问了也只是让那一格永远 100%。
    #[test]
    fn mount_candidates_keep_only_local_real_disks() {
        let got: Vec<(String, String)> = mount_candidates(MOUNTS)
            .into_iter()
            .map(|c| (c.device, c.mount))
            .collect();
        assert_eq!(
            got,
            vec![
                ("/dev/sda1".to_string(), "/".to_string()),
                ("/dev/sdb1".to_string(), "/data".to_string()),
                ("overlay".to_string(), "/var/lib/docker/overlay2/m/merged".to_string()),
            ]
        );
    }

    /// 同一块盘 bind 到两处只问一次；挂载点必须是绝对路径，否则 statvfs 打不通
    /// （`/proc/mounts` 里偶尔能看到相对路径的怪行，某些 fuse 实现会那么写）。
    #[test]
    fn mount_candidates_dedupe_by_device_and_require_absolute_paths() {
        let text = concat!(
            "/dev/sda1 / ext4 rw 0 0\n",
            "/dev/sda1 /srv/data ext4 rw,bind 0 0\n",
            "/dev/sdd1 relative/mnt ext4 rw 0 0\n",
            "/dev/sdc1 /logs ext4 rw 0 0\n",
        );
        let mounts: Vec<String> = mount_candidates(text)
            .into_iter()
            .map(|c| c.mount)
            .collect();
        assert_eq!(mounts, vec!["/", "/logs"]);
    }

    /// 一次 statvfs 是一个往返，第 20 块盘不会让那一格更有用。
    #[test]
    fn mount_candidates_are_capped() {
        let text: String = (1..=40)
            .map(|i| format!("/dev/sd{i} /mnt/sd{i} ext4 rw 0 0\n"))
            .collect();
        assert_eq!(mount_candidates(&text).len(), MAX_MOUNT_CANDIDATES);
    }

    /// 内核把挂载点名里的空格写成 `\040`（三位八进制）。解错了就得到一个不存在的
    /// 路径，那块盘从那一格里彻底消失。
    #[test]
    fn mount_names_are_unescaped() {
        let text = concat!(
            "/dev/sda1 /data/my\\040disk ext4 rw 0 0\n",
            "/dev/sdb1 /中文/名字 ext4 rw 0 0\n",
            "/dev/sdc1 /weird\\134name ext4 rw 0 0\n",
        );
        let mounts: Vec<String> = mount_candidates(text)
            .into_iter()
            .map(|c| c.mount)
            .collect();
        assert_eq!(mounts, vec!["/data/my disk", "/中文/名字", "/weird\\name"]);
    }

    /// 混合路径的核心规则：exec 只补 SFTP 没给到的那一格。
    ///
    /// 尤其不能覆盖 CPU —— 那一格是两次快照做差，混进另一个来源、另一个时刻的
    /// 基线，画出来的就是一条没有意义的斜线。
    #[test]
    fn fill_gaps_only_fills_what_is_missing() {
        let mut sftp = Probe {
            alive: true,
            cpu: Some(CpuSample { total: 1000, idle: 800 }),
            mem: None,
            load: None,
            disks: vec![],
        };
        let exec = Probe::parse(concat!(
            "ellsm1\n",
            "cpu  9999 0 9999 9999 9999 0 0 0\n",
            "MemTotal: 1000 kB\n",
            "MemAvailable: 250 kB\n",
            "/dev/sda1 1000 880 120 88% /data\n",
        ));
        sftp.fill_gaps(&exec);
        assert_eq!(sftp.cpu, Some(CpuSample { total: 1000, idle: 800 }), "SFTP 的基线不许被盖掉");
        assert_eq!(sftp.mem.map(|m| (m.total_kb, m.avail_kb)), Some((1000, 250)));
        assert_eq!(sftp.worst_disk().map(|d| d.mount.as_str()), Some("/data"));
        assert!(sftp.has_data());
        // 全都有数的一轮不该被另一次采集改动
        let mut full = sftp.clone();
        full.fill_gaps(&Probe::parse("ellsm1\nMemTotal: 7 kB\n"));
        assert_eq!(full, sftp);
    }

    /// 拆成独立解析器之后，`/proc` 那份文本不能再被 `df` 的列序误认领
    /// （`mem` 那行有三个空格分隔的列，恰好是最容易被当成 df 的形状）。
    #[test]
    fn proc_text_does_not_become_disks() {
        let probe = Probe::parse(concat!(
            "ellsm1\n",
            "MemTotal: 16258304 kB\n",
            "MemAvailable: 9452112 kB\n",
            "HugePages_Total: 0\n",
            "cpu  100 0 50 800 20 0 0 3 0 0\n",
        ));
        assert!(probe.disks.is_empty(), "实际 {:?}", probe.disks);
        assert!(probe.cpu.is_some() && probe.mem.is_some());
    }

    /// 到普查点了：哪怕缓存里已经有跟单的盘，也要重排一次"谁最满"。
    ///
    /// 缓存那个数已经是最多 60 秒前的实测，而用户问的是"现在哪块盘最吃紧" ——
    /// 省下的那 8 次往返不能拿这个问题的答案来换。
    #[test]
    fn the_survey_point_always_re_ranks_the_mounts() {
        assert_eq!(
            plan_disks(true, Some("/data"), Some(true)),
            DiskPlan::Survey,
            "到点就该重排，不能抱着旧的那块盘不放"
        );
    }

    /// 中间那些轮只对缓存里那块问一次 —— 这就是方案 B 省下来的东西。
    #[test]
    fn a_middle_round_follows_the_cached_mount_only() {
        assert_eq!(
            plan_disks(false, Some("/data"), Some(true)),
            DiskPlan::Follow("/data".to_string()),
            "跟单轮只问最满那一块"
        );
        // 第一次普查刚建好缓存、还没试过扩展态度时也照跟（Some(true) 不是必要条件）。
        assert_eq!(
            plan_disks(false, Some("/data"), None),
            DiskPlan::Follow("/data".to_string())
        );
    }

    /// 这台没有 `statvfs@openssh.com`：跟单轮一个请求都不发。
    ///
    /// 不拦住的话，`gather` 那边会看到"要磁盘却没问到"，于是每 5 秒补一条一次性 `df` ——
    /// 每 5 秒 fork 一个 shell 正是这套 SFTP 主路径要避免的东西。
    /// 普查轮同样 `Skip`：既然知道问不动，那 8 次必败的 `fs_info` 也省了，
    /// 磁盘那一格由 60 秒一次的 `df` 兜底填。
    #[test]
    fn a_machine_without_the_extension_asks_nothing() {
        assert_eq!(
            plan_disks(false, Some("/data"), Some(false)),
            DiskPlan::Skip,
            "跟单轮绝不能退化成每 5 秒一条 df"
        );
        assert_eq!(
            plan_disks(true, None, Some(false)),
            DiskPlan::Skip,
            "明知问不动就别再撞那 8 次往返"
        );
    }

    /// 上一块盘问不到了（已经卸载 / 权限变了）：缓存交回普查，下一轮自愈。
    ///
    /// 这条是"缓存不能变成死循环"的关键 —— 如果这里返回 Skip，那一格会永远停在
    /// 一块已经不存在的挂载点上，而 60 秒的普查点又被 `disk_rounds` 数着，看着像坏了。
    #[test]
    fn a_lost_mount_falls_back_to_a_survey() {
        assert_eq!(
            plan_disks(false, None, Some(true)),
            DiskPlan::Survey,
            "没得跟就重查，下一轮重新决定跟谁"
        );
    }

    /// 连上的第一轮：既没到点（其实到了，倒计时默认 0）、也没缓存、也没试过扩展 ——
    /// 无论如何都得先普查一次，否则磁盘那一格永远空着。
    #[test]
    fn the_very_first_round_surveys() {
        assert_eq!(plan_disks(true, None, None), DiskPlan::Survey);
        assert_eq!(plan_disks(false, None, None), DiskPlan::Survey);
    }
}

use std::time::Duration;

use ells_core::host::{Auth, Host};
use ells_core::ssh::{RemoteEvent, RemoteSession};
use ells_core::{HostKeyPolicy, Vault};
use tokio::sync::mpsc::UnboundedReceiver;

fn test_host() -> Host {
    Host {
        alias: "smoke".into(),
        hostname: "127.0.0.1".into(),
        port: 2222,
        user: "tester".into(),
        auth: Auth::Password,
        password: Some("test123".into()),
        jump: None,
        note: None,
        ..Default::default()
    }
}

async fn next_data(rx: &mut UnboundedReceiver<RemoteEvent>) -> Option<Vec<u8>> {
    match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
        Ok(Some(RemoteEvent::Data(bytes))) => Some(bytes),
        _ => None,
    }
}

#[tokio::test]
#[ignore = "requires tests/fake_sshd.py running on 127.0.0.1:2222"]
async fn remote_shell_roundtrip() {
    let mut session =
        RemoteSession::connect(&test_host(), &Vault::default(), 80, 24, &HostKeyPolicy::trust_all())
            .await
            .expect("connect to fake sshd");
    let mut rx = session.take_output().expect("output rx");

    let mut banner = String::new();
    while let Some(bytes) = next_data(&mut rx).await {
        banner.push_str(&String::from_utf8_lossy(&bytes));
        if banner.contains("ready") {
            break;
        }
    }
    assert!(banner.contains("ells-test-sh ready"), "banner: {banner:?}");

    session.write_input(b"echo hi\r".to_vec());
    let mut out = String::new();
    while let Some(bytes) = next_data(&mut rx).await {
        out.push_str(&String::from_utf8_lossy(&bytes));
        if out.contains("> hi") {
            break;
        }
    }
    assert!(out.contains("> hi"), "echo output: {out:?}");

    session.write_input(b"echo COLOR\r".to_vec());
    let mut got_ansi = false;
    while let Some(bytes) = next_data(&mut rx).await {
        if String::from_utf8_lossy(&bytes).contains("\x1b[31m") {
            got_ansi = true;
            break;
        }
    }
    assert!(got_ansi, "expected raw ANSI color bytes from remote");

    session.write_input(b"exit\r".to_vec());
    session.close();
}

/// 主机指标探针的活体冒烟：交互 shell 通道正在被用着的时候，同一条已认证连接上
/// 另开一条一次性 exec 通道取回 /proc 形状，之后 shell 还得照常回显。
/// 单元测试只能验解析器；"不重做握手、不碰用户那一格 PTY"只有真服务器说得清。
#[tokio::test]
#[ignore = "requires tests/fake_sshd.py running on 127.0.0.1:2222"]
async fn metrics_probe_shares_the_live_connection() {
    let mut session =
        RemoteSession::connect(&test_host(), &Vault::default(), 80, 24, &HostKeyPolicy::trust_all())
            .await
            .expect("connect to fake sshd");
    let mut rx = session.take_output().expect("output rx");

    let mut banner = String::new();
    while let Some(bytes) = next_data(&mut rx).await {
        banner.push_str(&String::from_utf8_lossy(&bytes));
        if banner.contains("ready") {
            break;
        }
    }
    assert!(banner.contains("ells-test-sh ready"), "banner: {banner:?}");

    let target = session.probe_target().expect("已认证的连接该能摘出采集句柄");
    let text = tokio::time::timeout(
        Duration::from_secs(10),
        target.probe(ells_core::PROBE_COMMAND, Duration::from_secs(5), 1 << 20),
    )
    .await
    .expect("采集卡住也得自己收，不能把这一格钉死")
    .expect("采集应成功");

    let probe = ells_core::Probe::parse(&text);
    assert!(probe.alive, "心跳标记没回来：{text:?}");
    let cpu = probe.cpu.expect("/proc/stat 那一行该认出来");
    assert!(cpu.total > cpu.idle, "总量该大于空闲：{cpu:?}");
    assert_eq!(
        probe.worst_disk().map(|d| (d.mount.as_str(), d.used_percent)),
        Some(("/data", 88)),
        "df 的 Capacity 列就是界面上那个数"
    );
    assert!(
        ells_core::PROBE_COMMAND.contains("/proc/loadavg"),
        "兜底命令少了 loadavg，快的那条腿就会少一个数"
    );
    assert_eq!(
        probe.load.map(|l| l.display()),
        Some("1.75/0.90/0.35".to_string()),
        "exec 那一份的负载也该认出来：{text:?}"
    );

    // 探针走后交互通道照旧：用户敲的东西一个字都不该少
    session.write_input(b"echo after-probe\r".to_vec());
    let mut out = String::new();
    while let Some(bytes) = next_data(&mut rx).await {
        out.push_str(&String::from_utf8_lossy(&bytes));
        if out.contains("> after-probe") {
            break;
        }
    }
    assert!(out.contains("> after-probe"), "shell 回显：{out:?}");

    session.write_input(b"exit\r".to_vec());
    session.close();
}

/// 主路径的活体冒烟：指标应当从**这条已认证连接上已有的 SFTP 会话**读来 ——
/// `/proc` 是读文件、不是 fork shell。单元测试只能验解析，"真的能用 SFTP 读到
/// /proc、读不到时真的会回落"只有真服务器说得清。
///
/// 假 sshd 里两份数据是刻意做岔的：SFTP 那份 MemAvailable 390（→ 61%），exec 那份
/// 250（→ 75%）。看到 61 就是走了主路径，看到 75 就是悄悄回落了。
/// 磁盘这一格则正好走"服务器没有 statvfs@openssh.com 扩展 → 补一轮 exec"那条分支
/// （paramiko 的 SFTP 服务器没实现这个扩展，OpenSSH 有）。
#[tokio::test]
#[ignore = "requires tests/fake_sshd.py running on 127.0.0.1:2222"]
async fn metrics_gather_reads_proc_over_sftp() {
    let mut session =
        RemoteSession::connect(&test_host(), &Vault::default(), 80, 24, &HostKeyPolicy::trust_all())
            .await
            .expect("connect to fake sshd");
    let mut rx = session.take_output().expect("output rx");

    let mut banner = String::new();
    while let Some(bytes) = next_data(&mut rx).await {
        banner.push_str(&String::from_utf8_lossy(&bytes));
        if banner.contains("ready") {
            break;
        }
    }
    assert!(banner.contains("ells-test-sh ready"), "banner: {banner:?}");

    let target = session.probe_target().expect("已认证的连接该能摘出采集句柄");
    // 磁盘的两个开关：每一轮都想要数（want_disks），但只有到点的轮次才普查
    // （want_survey）。假服务器没有 statvfs，所以普查轮会补一条 df，跟单轮什么都不能问。
    let gather = |want_disks: bool, want_survey: bool| {
        target.gather(Duration::from_secs(5), 1 << 20, want_disks, want_survey)
    };
    let first = tokio::time::timeout(Duration::from_secs(10), gather(true, true))
        .await
        .expect("采集卡住也得自己收，不能把这一格钉死")
        .expect("采集应成功");

    let mem = first.mem.expect("/proc/meminfo 该从 SFTP 读到");
    assert_eq!(
        (mem.total_kb, mem.avail_kb),
        (1000, 390),
        "61% 那一份才是 SFTP 读来的；拿到 75% 说明悄悄回落了 exec"
    );
    assert_eq!(
        ells_core::metrics::round_percent(ells_core::metrics::mem_percent(&mem)),
        Some(61)
    );
    assert_eq!(
        first.load.map(|l| l.display()),
        Some("0.42/0.31/0.19".to_string()),
        "负载同样从 SFTP 读到；拿到 1.75 那一串说明走的是 exec"
    );
    assert_eq!(
        first.worst_disk().map(|d| (d.mount.as_str(), d.used_percent)),
        Some(("/data", 88)),
        "statvfs 不被支持时磁盘那一格由 exec 的 df 补上"
    );
    // 首轮只有基线，界面那格要空着而不是 0%
    let prev = first.cpu.expect("/proc/stat 该从 SFTP 读到");

    // 跟单的那一轮（挂载点缓存还在，只是还没到普查点）：这台服务器没有 statvfs，
    // 所以计划是 Skip —— 一个请求都不发，磁盘沿用普查那轮拿到的数。
    //
    // 这一条是"无 statvfs 的服务器不会每 5 秒跑一次 df"的运行时证据：这里的
    // disks.is_empty() 只有真没跑 df 才成立（跑了 df 就会把 /data 88% 填回来）。
    let follow = tokio::time::timeout(Duration::from_secs(10), gather(true, false))
        .await
        .expect("跟单轮也该自己收")
        .expect("采集应成功");
    assert!(
        follow.disks.is_empty(),
        "跟单轮在没有 statvfs 的服务器上不该回落成一条 df：{:#?}",
        follow.disks
    );
    assert!(follow.mem.is_some(), "跟单轮照样要读 /proc，CPU/内存不受磁盘节奏影响");

    // 背景那一档（这一轮连磁盘数都不想要，60 秒一轮的标签）：更不该有任何磁盘请求。
    let second = tokio::time::timeout(Duration::from_secs(10), gather(false, false))
        .await
        .expect("第二轮也该自己收")
        .expect("采集应成功");
    assert!(
        second.disks.is_empty(),
        "没到点的那一轮不该再去开一条 exec 问磁盘：{:#?}",
        second.disks
    );
    let now = second.cpu.expect("第二轮同样有快照");
    assert_eq!(
        ells_core::metrics::round_percent(ells_core::metrics::cpu_percent(&prev, &now)),
        Some(33),
        "假 /proc/stat 每轮多忙 40/120 jiffies：做差才是这一格要画的数"
    );
    assert!(second.cpu.unwrap().total > prev.total, "两次快照不能是同一份");

    // 到普查点的那一轮：这台服务器没有 statvfs，于是普查问不到、再由 df 把那一格填回来
    let slow = tokio::time::timeout(Duration::from_secs(10), gather(true, true))
        .await
        .expect("慢的那一轮同样要自己收")
        .expect("采集应成功");
    assert_eq!(
        slow.worst_disk().map(|d| (d.mount.as_str(), d.used_percent)),
        Some(("/data", 88)),
        "间隔只是省掉不问的轮次，不是把那一格永久问没了"
    );

    // 采集走完，用户那一格 PTY 照旧：敲的东西一个字都不该少
    session.write_input(b"echo after-gather\r".to_vec());
    let mut out = String::new();
    while let Some(bytes) = next_data(&mut rx).await {
        out.push_str(&String::from_utf8_lossy(&bytes));
        if out.contains("> after-gather") {
            break;
        }
    }
    assert!(out.contains("> after-gather"), "shell 回显：{out:?}");

    session.write_input(b"exit\r".to_vec());
    session.close();
}

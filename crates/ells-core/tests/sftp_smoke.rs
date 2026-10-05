use std::time::Duration;

use ells_core::host::{Auth, Host};
use ells_core::ssh::RemoteSession;
use ells_core::{HostKeyPolicy, Vault};
use ells_transfer::{Cancel, Progress};
use rand::{RngCore, SeedableRng};
use tokio::sync::mpsc;

fn test_host() -> Host {
    Host {
        alias: "sftp-smoke".into(),
        hostname: "127.0.0.1".into(),
        port: 2222,
        user: "tester".into(),
        auth: Auth::Password,
        password: Some("test123".into()),
        jump: None,
        note: None,
    }
}

const SIZE: usize = 256 * 1024; // 262144 bytes

#[tokio::test]
#[ignore = "requires fake_sshd with sftp"]
async fn sftp_smoke() {
    let mut session = RemoteSession::connect(
        &test_host(),
        &Vault::default(),
        80,
        24,
        &HostKeyPolicy::trust_all(),
    )
    .await
    .expect("连接 fake sshd 失败");

    // 1. SFTP 子系统必须可用
    let sftp = match session.sftp() {
        Some(s) => s,
        None => panic!("服务器未提供 SFTP 子系统：session.sftp() 返回 None"),
    };

    // 2. 根目录列表：包含 hello.txt 与 sub，且目录排在文件之前
    let entries = tokio::time::timeout(Duration::from_secs(10), ells_transfer::list(&sftp, "/"))
        .await
        .expect("list 超时")
        .expect("列出远端根目录失败");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"hello.txt"),
        "根目录应包含 hello.txt，实际条目: {names:?}"
    );
    assert!(names.contains(&"sub"), "根目录应包含 sub，实际条目: {names:?}");
    let sub_pos = entries.iter().position(|e| e.name == "sub").unwrap();
    let hello_pos = entries.iter().position(|e| e.name == "hello.txt").unwrap();
    assert!(entries[sub_pos].is_dir, "sub 应为目录");
    assert!(
        sub_pos < hello_pos,
        "目录应排在文件之前，实际顺序: {names:?}"
    );

    // 3. 上传 256KB 随机文件并收集进度事件
    let dir = tempfile::tempdir().expect("创建临时目录失败");
    let src = dir.path().join("rand_256k.bin");
    let mut data = vec![0u8; SIZE];
    rand::rngs::StdRng::from_entropy().fill_bytes(&mut data);
    std::fs::write(&src, &data).expect("写入本地随机文件失败");

    let (tx, mut rx) = mpsc::unbounded_channel::<Progress>();
    let cancel = Cancel::default();
    tokio::time::timeout(
        Duration::from_secs(30),
        ells_transfer::upload(&sftp, &src, "/upload_test.bin".into(), tx.clone(), &cancel),
    )
    .await
    .expect("upload 超时")
    .expect("上传到 /upload_test.bin 失败");
    drop(tx);

    let mut events = Vec::new();
    while let Ok(p) = rx.try_recv() {
        events.push(p);
    }
    assert!(
        !events.is_empty(),
        "上传过程中应至少收到一条 Progress 事件"
    );
    let last = events.last().expect("upload 最后一条进度事件存在");
    assert_eq!(
        last.transferred,
        SIZE as u64,
        "最后一条进度事件 transferred 应为 {SIZE}"
    );
    assert_eq!(
        last.total,
        Some(SIZE as u64),
        "最后一条进度事件 total 应为 Some({SIZE})"
    );

    // 4. 下载回来并逐字节比对
    let (tx2, mut rx2) = mpsc::unbounded_channel::<Progress>();
    let dl_dir = dir.path().join("downloaded");
    let got = tokio::time::timeout(
        Duration::from_secs(30),
        ells_transfer::download(
            &sftp,
            "/upload_test.bin".into(),
            &dl_dir,
            tx2.clone(),
            "upload_test.bin",
            &cancel,
        ),
    )
    .await
    .expect("download 超时")
    .expect("下载 /upload_test.bin 失败");
    drop(tx2);

    let downloaded = std::fs::read(&got).expect("读取下载文件失败");
    assert_eq!(downloaded.len(), data.len(), "下载文件大小应与源文件一致");
    assert_eq!(downloaded, data, "下载内容与源文件字节不一致");

    let mut dl_events = 0usize;
    while rx2.try_recv().is_ok() {
        dl_events += 1;
    }
    assert!(dl_events >= 1, "下载也应至少产生一条进度事件");

    session.close();
}

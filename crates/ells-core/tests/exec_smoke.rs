//! 无头 exec 的活体冒烟：连本地 fake_sshd，验证"退出码 + 两段输出"这套对外契约。
//!
//! exec.rs 的单元测试只覆盖命令拼接；通道消息（Data / ExtendedData / ExitStatus）
//! 分流只能靠真服务器。跑法：先 `python tests/fake_sshd.py`，再
//! `cargo test -p ells-core -- --ignored`。

use std::time::Duration;

use ells_core::host::{Auth, Host};
use ells_core::{HostKeyPolicy, exec, vault};

fn test_host() -> Host {
    Host {
        alias: "smoke".into(),
        hostname: "127.0.0.1".into(),
        port: 2222,
        user: "tester".into(),
        auth: Auth::Password,
        password: Some("test123".into()),
        ..Default::default()
    }
}

async fn run(command: &str, tty: bool, stdin: &[u8]) -> exec::ExecResult {
    // dev 库里有 fake_sshd 那台主机；没有就退回内联定义，测试不依赖 ~/.ells
    let vault = vault::load_dev_vault().unwrap_or_default();
    let host = vault.find("smoke").cloned().unwrap_or_else(test_host);
    let policy = HostKeyPolicy::trust_all();
    tokio::time::timeout(
        Duration::from_secs(15),
        exec::exec(&host, &vault, &policy, command, tty, 80, 24, stdin),
    )
    .await
    .expect("fake sshd 应在 15 秒内回应")
    .expect("exec 应成功")
}

#[tokio::test]
#[ignore = "requires tests/fake_sshd.py running on 127.0.0.1:2222"]
async fn exec_returns_stdout_and_zero() {
    let res = run("echo hi", false, &[]).await;
    assert_eq!(res.status, 0);
    assert_eq!(res.stdout, b"hi\n");
    assert!(res.stderr.is_empty());
}

#[tokio::test]
#[ignore = "requires tests/fake_sshd.py running on 127.0.0.1:2222"]
async fn exec_splits_stderr_and_passes_the_exit_code() {
    let res = run("err", false, &[]).await;
    assert_eq!(res.status, 1, "远端退出码必须原样透传");
    assert_eq!(res.stderr, b"boom\n");
    assert!(res.stdout.is_empty(), "stderr 不能混进 stdout：{}/{}", res.stdout.len(), res.stderr.len());
}

#[tokio::test]
#[ignore = "requires tests/fake_sshd.py running on 127.0.0.1:2222"]
async fn exec_pipes_stdin_to_the_remote_command() {
    let res = run("cat", false, b"piped-in\n").await;
    assert_eq!(res.status, 0);
    assert_eq!(res.stdout, b"piped-in\n");
}

#[tokio::test]
#[ignore = "requires tests/fake_sshd.py running on 127.0.0.1:2222"]
async fn exec_with_pty_still_returns_the_output() {
    let res = run("echo tty", true, &[]).await;
    assert_eq!(res.status, 0);
    assert!(
        String::from_utf8_lossy(&res.stdout).contains("tty"),
        "开了 PTY 也要拿到输出：{:?}",
        String::from_utf8_lossy(&res.stdout)
    );
}

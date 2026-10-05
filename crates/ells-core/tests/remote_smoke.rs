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

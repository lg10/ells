use anyhow::{anyhow, bail, Context, Result};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::host::{Auth, Host};

/// Events streamed back from the remote SSH channel to the UI.
#[derive(Debug)]
pub enum RemoteEvent {
    Data(Vec<u8>),
    Closed(Result<()>),
}

/// 空闲保活间隔（秒），全局可调（ells 设置弹窗），默认 30s。
static KEEPALIVE_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(30);

pub fn set_keepalive_interval(secs: u64) {
    KEEPALIVE_SECS.store(secs.max(5), std::sync::atomic::Ordering::Relaxed);
}

pub fn keepalive_interval() -> u64 {
    KEEPALIVE_SECS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Input submitted to the remote side (keystrokes, paste, emulator answers).
pub enum SessionInput {
    Bytes(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

pub struct RemoteSession {
    input_tx: mpsc::UnboundedSender<SessionInput>,
    output_rx: Option<mpsc::UnboundedReceiver<RemoteEvent>>,
    sftp: Option<Arc<russh_sftp::client::SftpSession>>,
    alive: bool,
}

#[derive(Debug)]
pub struct Handler;

impl russh::client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        // M1: blind accept. TOFU fingerprint prompt lands with M2 UI polish.
        Ok(true)
    }
}

impl RemoteSession {
    pub async fn connect(
        host: &Host,
        vault: &crate::Vault,
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        let mut visited = Vec::new();
        let handle = connect_handle(host, vault, &mut visited).await?;

        // Open the SFTP subsystem first; a server without it must not block
        // the shell session.
        let sftp = async {
            let ch = handle.channel_open_session().await.ok()?;
            ch.request_subsystem(true, "sftp").await.ok()?;
            russh_sftp::client::SftpSession::new(ch.into_stream())
                .await
                .map(Arc::new)
                .ok()
        }
        .await;
        if sftp.is_none() {
            tracing::warn!("服务器不支持 SFTP 子系统，文件传输不可用");
        }

        let channel = handle
            .channel_open_session()
            .await
            .context("打开会话通道失败")?;

        channel
            .request_pty(true, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
            .await
            .context("申请伪终端(PTY)失败")?;
        channel
            .request_shell(true)
            .await
            .context("申请 shell 失败")?;

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<SessionInput>();
        let (output_tx, output_rx) = mpsc::unbounded_channel::<RemoteEvent>();

        let writer = channel.make_writer();
        let mut writer = Box::pin(writer);
        let mut channel = channel;

        tokio::spawn(async move {
            let mut open = true;
            while open {
                tokio::select! {
                    msg = channel.wait() => {
                        match msg {
                            Some(russh::ChannelMsg::Data { data }) => {
                                if output_tx.send(RemoteEvent::Data(data.to_vec())).is_err() {
                                    open = false;
                                }
                            }
                            Some(russh::ChannelMsg::ExtendedData { data, .. }) => {
                                if output_tx.send(RemoteEvent::Data(data.to_vec())).is_err() {
                                    open = false;
                                }
                            }
                            Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                                tracing::debug!("remote shell exited with status {exit_status}");
                            }
                            Some(russh::ChannelMsg::Eof)
                            | Some(russh::ChannelMsg::Close)
                            | None => {
                                open = false;
                            }
                            Some(_) => {}
                        }
                    }
                    Some(input) = input_rx.recv() => {
                        match input {
                            SessionInput::Bytes(bytes) => {
                                if writer.write_all(&bytes).await.is_err() {
                                    open = false;
                                }
                            }
                            SessionInput::Resize { cols, rows } => {
                                let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
                            }
                            SessionInput::Close => {
                                open = false;
                            }
                        }
                    }
                }
            }
            let _ = channel.close().await;
            let _ = output_tx.send(RemoteEvent::Closed(Ok(())));
        });

        Ok(Self {
            input_tx,
            output_rx: Some(output_rx),
            sftp,
            alive: true,
        })
    }

    /// Shared SFTP session handle, or None if the server lacks the subsystem.
    pub fn sftp(&self) -> Option<Arc<russh_sftp::client::SftpSession>> {
        self.sftp.clone()
    }

    pub fn is_alive(&self) -> bool {
        self.alive
    }

    pub fn take_output(&mut self) -> Option<mpsc::UnboundedReceiver<RemoteEvent>> {
        self.output_rx.take()
    }

    pub fn write_input(&self, bytes: Vec<u8>) {
        let _ = self.input_tx.send(SessionInput::Bytes(bytes));
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.input_tx.send(SessionInput::Resize { cols, rows });
    }

    pub fn close(&mut self) {
        self.alive = false;
        let _ = self.input_tx.send(SessionInput::Close);
    }
}

/// Establish an authenticated handle to `host`, tunneling through the
/// configured jump host (recursively, ProxyJump style) when present.
fn connect_handle<'a>(
    host: &'a Host,
    vault: &'a crate::Vault,
    visited: &'a mut Vec<String>,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<russh::client::Handle<Handler>>> + Send + 'a>,
> {
    Box::pin(async move {
        if visited.iter().any(|a| a == &host.alias) {
            let chain = visited.join(" → ");
            return Err(anyhow!("跳板链存在循环: {chain} → {}", host.alias));
        }
        visited.push(host.alias.clone());
        // 空闲保活：云上 NAT/防火墙会静默掐断长时间无流量的 SSH TCP，
        // 表现为界面卡死。每 30s 发一次 keepalive@openssh.com；连续 3 次
        // 无响应则主动断开（触发 RemoteClosed，会话界面给出结束提示）。
        let config = std::sync::Arc::new(russh::client::Config {
            keepalive_interval: Some(std::time::Duration::from_secs(keepalive_interval())),
            keepalive_max: 3,
            ..Default::default()
        });
        let mut handle = match host.jump.as_deref() {
            Some(alias) => {
                let jump_host = vault
                    .find(alias)
                    .with_context(|| format!("找不到跳板机别名 `{alias}`"))?
                    .clone();
                let jump_handle = connect_handle(&jump_host, vault, visited).await?;
                let channel = jump_handle
                    .channel_open_direct_tcpip(
                        host.hostname.as_str(),
                        host.port as u32,
                        "127.0.0.1",
                        0,
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "跳板机 `{alias}` 到 {}:{} 的转发通道打开失败",
                            host.hostname, host.port
                        )
                    })?;
                russh::client::connect_stream(config, channel.into_stream(), Handler)
                    .await
                    .with_context(|| {
                        format!("无法经跳板机与 {}:{} 建立 SSH", host.hostname, host.port)
                    })?
            }
            None => russh::client::connect(config, (host.hostname.as_str(), host.port), Handler)
                .await
                .with_context(|| format!("无法连接 {}:{}", host.hostname, host.port))?,
        };
        authenticate(&mut handle, host).await?;
        Ok(handle)
    })
}

async fn authenticate(handle: &mut russh::client::Handle<Handler>, host: &Host) -> Result<()> {
    match &host.auth {
        Auth::Password => {
            let password = host
                .password
                .as_deref()
                .context("该主机没有保存密码")?;
            let result = handle
                .authenticate_password(&host.user, password)
                .await
                .map_err(|e| anyhow!("密码认证失败: {e}"))?;
            describe(result, "密码")
        }
        Auth::PrimaryKey { path, passphrase } => {
            let expanded = expand_tilde(path);
            if expanded.ends_with(".pub") {
                bail!("私钥路径指向的是公钥(.pub)，请选择私钥文件");
            }
            let raw = std::fs::read_to_string(&expanded)
                .with_context(|| format!("无法读取私钥文件 {}", expanded))?;
            let key = if raw.contains("DEK-Info: DES-EDE3-CBC") {
                let pass = passphrase.as_deref().ok_or_else(|| {
                    anyhow!("该私钥为旧式 DES-EDE3-CBC 加密格式，请在「私钥口令」中填写其密码")
                })?;
                let der = decrypt_des3_pem(&raw, pass)
                    .with_context(|| format!("解密旧式 3DES 私钥失败（口令可能不正确）: {}", expanded))?;
                russh::keys::decode_secret_key(&pkcs1_pem(&der), None)
                    .map_err(|e| anyhow!("解析已解密的 PKCS#1 私钥失败: {e}"))?
            } else {
                match russh::keys::load_secret_key(&expanded, passphrase.as_deref()) {
                    Ok(k) => k,
                    Err(e) if raw.contains("DEK-Info:") => return Err(e).with_context(|| {
                        format!("无法加载加密私钥 {expanded}（旧式加密格式暂不支持，可用 ssh-keygen -p -m PEM 转换）")
                    }),
                    Err(e) => return Err(e)
                        .with_context(|| format!("无法加载私钥 {}", expanded)),
                }
            };
            let key = russh::keys::PrivateKeyWithHashAlg::new(std::sync::Arc::new(key), None);
            let result = handle
                .authenticate_publickey(&host.user, key)
                .await
                .map_err(|e| anyhow!("密钥认证失败: {e}"))?;
            describe(result, "密钥")
        }
        Auth::Agent => Err(anyhow!(
            "该主机的「认证方式」被存成了 ssh-agent（此方式尚未实现，与跳板机无关）。\
             请返回后按 e 编辑该主机，用 ←/→ 把认证方式改为「密钥」或「密码」再保存"
        )),
    }
}

fn describe(result: russh::client::AuthResult, method: &str) -> Result<()> {
    match result {
        russh::client::AuthResult::Success => Ok(()),
        russh::client::AuthResult::Failure {
            remaining_methods,
            partial_success,
        } => Err(anyhow!(
            "服务器{method}认证{}；仍可用的认证方式: {remaining_methods:?}",
            if partial_success {
                "已通过，但需要后续步骤"
            } else {
                "被拒绝"
            }
        )),
    }
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

/// Decrypt a legacy PEM private key encrypted with `DEK-Info: DES-EDE3-CBC`
/// (OpenSSL `-des3` output) and return the inner PKCS#1 DER.
fn decrypt_des3_pem(pem: &str, passphrase: &str) -> Result<Vec<u8>> {
    use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
    use md5::Digest;

    let iv_hex = pem
        .lines()
        .find_map(|l| l.split_once("DES-EDE3-CBC,"))
        .map(|(_, rest)| rest.trim().to_string())
        .ok_or_else(|| anyhow!("缺少 DEK-Info 头"))?;
    let iv = hex_decode(&iv_hex).ok_or_else(|| anyhow!("DEK-Info IV 非法"))?;
    anyhow::ensure!(iv.len() == 8, "DEK-Info IV 长度异常");

    let b64: String = pem
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty()
                && !t.starts_with("-----")
                && !t.starts_with("Proc-Type:")
                && !t.starts_with("DEK-Info:")
        })
        .map(|l| l.trim())
        .collect();
    use base64::Engine;
    let ct = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| anyhow!("私钥 base64 解码失败: {e}"))?;

    // OpenSSL EVP_BytesToKey (MD5, no iteration, salt = IV first 8 bytes).
    let mut dk = Vec::with_capacity(24);
    let mut prev = [0u8; 16];
    let mut first = true;
    while dk.len() < 24 {
        let mut h = md5::Md5::new();
        if !first {
            h.update(prev);
        }
        h.update(passphrase.as_bytes());
        h.update(&iv[..8]);
        let cur: [u8; 16] = h.finalize().into();
        dk.extend_from_slice(&cur);
        prev = cur;
        first = false;
    }
    dk.truncate(24);

    let cipher = cbc::Decryptor::<des::TdesEde3>::new_from_slices(&dk, &iv)
        .map_err(|e| anyhow!("3DES 初始化失败: {e}"))?;
    let n = ct.len();
    anyhow::ensure!(n > 0 && n % 8 == 0, "私钥密文长度异常");
    let mut buf = ct;
    let len = cipher
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| anyhow!("3DES 解密失败（口令可能不正确）"))?
        .len();
    Ok(buf[..len].to_vec())
}

/// Wrap raw PKCS#1 DER in a plaintext PEM envelope russh can parse.
fn pkcs1_pem(der: &[u8]) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    format!("-----BEGIN RSA PRIVATE KEY-----\n{b64}\n-----END RSA PRIVATE KEY-----\n")
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let nib = |c: u8| c.is_ascii_digit().then(|| c - b'0').or_else(|| {
        c.is_ascii_hexdigit()
            .then(|| c.to_ascii_lowercase() - b'a' + 10)
    });
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks(2) {
        out.push((nib(pair[0])? << 4) | nib(pair[1])?);
    }
    Some(out)
}

#[cfg(test)]
mod legacy_key_tests {
    use super::*;

    #[test]
    fn decrypts_openssl_des3_pem() {
        let pem = include_str!("../tests/legacy_des3.pem");
        let der = decrypt_des3_pem(pem, "test123").expect("decrypt");
        let key = russh::keys::decode_secret_key(&pkcs1_pem(&der), None).expect("parse pkcs1");
        assert!(key.algorithm().is_rsa());
        // Wrong passphrase must fail cleanly, not panic.
        assert!(decrypt_des3_pem(pem, "wrong").is_err());
    }
}

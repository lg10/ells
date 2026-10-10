//! 无头执行：`ells exec <别名> -- <命令>` 背后的那条 exec 通道。
//!
//! 与交互式会话分开是因为脚本要的是"退出码 + 两段输出"，而不是一个 PTY 循环：
//! 不开 PTY 时 stdout 与 stderr 必须分流，否则 `2>` 重定向形同虚设。

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use crate::host::Host;
use crate::hostkey::HostKeyPolicy;
use crate::ssh::connect_handle_to;

/// 一次无头执行的结果。`status` 是远端的退出码；远端没给（被信号杀掉、
/// 通道直接关闭）时按 shell 的惯例算作失败。
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// 在目标主机上执行一条命令，输出全部收进内存后返回。
///
/// `tty = true` 时申请伪终端：远端程序会输出颜色与控制序列，stdout/stderr 混在
/// 一起（与 `ssh -t` 同），适合给人看；给机器读就别开。
pub async fn exec(
    host: &Host,
    vault: &crate::Vault,
    policy: &HostKeyPolicy,
    command: &str,
    tty: bool,
    cols: u16,
    rows: u16,
    stdin_bytes: &[u8],
) -> Result<ExecResult> {
    let handle = connect_handle_to(host, vault, policy)
        .await
        .context("建立连接失败")?;
    let channel = handle
        .channel_open_session()
        .await
        .context("打开会话通道失败")?;
    if tty {
        channel
            .request_pty(
                false,
                "xterm-256color",
                cols as u32,
                rows as u32,
                0,
                0,
                &[],
            )
            .await
            .context("申请伪终端(PTY)失败")?;
    }
    if !stdin_bytes.is_empty() {
        let mut writer = channel.make_writer();
        writer.write_all(stdin_bytes).await.context("送入标准输入失败")?;
        writer.shutdown().await.ok();
    }
    channel
        .exec(true, command)
        .await
        .context("执行命令失败")?;

    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut status: i32 = -1;
    let mut channel = channel;
    loop {
        match channel.wait().await {
            Some(russh::ChannelMsg::Data { data }) => out.extend_from_slice(&data),
            Some(russh::ChannelMsg::ExtendedData { ext, data }) => {
                // ssh 协议里 ext=1 才是 stderr，其它（调试输出）并到 stderr：
                // 无头模式没有第三条管道可给
                let _ = ext;
                err.extend_from_slice(&data);
            }
            Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                status = exit_status as i32;
            }
            Some(russh::ChannelMsg::Eof) | Some(russh::ChannelMsg::Close) | None => break,
            Some(_) => {}
        }
    }
    let _ = channel.close().await;
    Ok(ExecResult {
        status,
        stdout: out,
        stderr: err,
    })
}

/// 把命令参数拼成远端要执行的一行。
///
/// 只有一个参数时当作整行 shell 命令原样送出（`-- "ls | wc -l"` 的管道要用）；
/// 多个参数逐个加引号再拼（`-- rm "/a b"` 的空格不会被拆成两个参数）。
/// 这比 `ssh` 的"一律空格拼接"更贴合直觉，也更不容易误删文件。
pub fn join_command(args: &[String]) -> String {
    match args {
        [] => String::new(),
        [one] => one.clone(),
        many => many
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// 单引号包裹，内部出现的单引号按 shell 惯例换成 `'\''`。
fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "~._/:-@%+=".contains(c))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_argument_is_a_shell_line() {
        assert_eq!(
            join_command(&["ls | wc -l".to_string()]),
            "ls | wc -l"
        );
    }

    #[test]
    fn multiple_arguments_keep_their_spaces() {
        assert_eq!(
            join_command(&["rm".to_string(), "/a b".to_string()]),
            "rm '/a b'"
        );
    }

    #[test]
    fn plain_tokens_are_not_quoted() {
        assert_eq!(
            join_command(&["ls".to_string(), "-la".to_string(), "/tmp".to_string()]),
            "ls -la /tmp"
        );
    }

    #[test]
    fn embedded_single_quote_survives() {
        assert_eq!(
            join_command(&["echo".to_string(), "it's".to_string()]),
            r"echo 'it'\''s'"
        );
    }
}

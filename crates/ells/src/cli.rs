//! 无头子命令：不进界面也能用 ells（脚本、CI、将主机导出成 ssh_config）。
//!
//! 界面是 ells 的主形态，但"把主机、隧道、传输能力交给脚本"是 SSHub 与 OmnySSH
//! 都有而 ells 缺的入口。这里的原则只有一条：**任何密码、私钥内容、主密码都不会
//! 被打印出来**——`--format json` 里能出现的都只有可公开的元数据。

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ells_core::vault;
use ells_core::{
    Host, HostKeyPolicy,
    audit::{self, AuditKind},
    ssh, sshconfig,
    tunnel::TunnelEvent,
};
use ells_transfer::{self as transfer, Cancel, FileEntry, Progress};
use russh_sftp::client::SftpSession;
use tokio::sync::mpsc;

/// 以下退出码是对外契约的一部分，脚本会按它分支，改之前先想清楚谁会受影响。
pub const EX_OK: i32 = 0;
/// 一般失败（连不上、认证被拒、主机密钥被拒、远端命令返回非 0）
pub const EX_FAIL: i32 = 1;
/// 用法不安全：覆盖没给 `--yes`、删目录没给 `--recursive`、参数组合不合法
pub const EX_USAGE: i32 = 2;
/// `--timeout` 到点
pub const EX_TIMEOUT: i32 = 124;

/// 用法闸门：自己把中文原因打到 stderr，再把退出码带回 2。
/// 用 `Err` 不行——`main` 会把任何错误统一成 1，脚本就分不出"差一个 --yes"和"连不上"了。
fn usage(msg: impl Into<String>) -> Result<i32> {
    eprintln!("ells：{}", msg.into());
    Ok(EX_USAGE)
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 列出主机（表格或 JSON；绝不输出密码与私钥内容）
    List {
        /// table | json
        #[arg(long, default_value = "table")]
        format: String,
        /// 按别名/主机/用户/分组/标签过滤（子串，不区分大小写）
        #[arg(long)]
        filter: Option<String>,
    },
    /// 在远端执行一条命令就退出：ells exec web -- uname -a
    Exec {
        /// 主机别名
        alias: String,
        /// 远端命令（用 `--` 与 ells 自己的选项分开）
        #[arg(last = true)]
        command: Vec<String>,
        /// 申请伪终端：给 top/systemctl 这类要 TTY 的命令；此时 stdout 含控制序列
        #[arg(long)]
        tty: bool,
        /// 整体超时秒数（含连接与认证）；到点退出码 124
        #[arg(long)]
        timeout: Option<u64>,
        /// 把本进程的标准输入整段送进远端命令
        #[arg(long)]
        stdin: bool,
    },
    /// 不界面的文件操作：ells sftp ls web /var/log
    Sftp {
        #[command(subcommand)]
        op: SftpOp,
        /// 允许覆盖已存在的目标（不给就报错退出）
        ///
        /// `global` 不是好看：`--yes` 声明在这一层，写在 `get`/`put` 后面会被 clap
        /// 当成子命令不认识的参数，而"ells sftp get web a b --yes"才是人会打的字。
        #[arg(long, global = true)]
        yes: bool,
    },
    /// 只起端口转发并常驻：ells tunnel web（Ctrl-C 结束）
    ///
    /// 每一行状态都带着这条隧道实际绑上的本地端口，留空（自动分配）的会标出"（自动）"，
    /// 所以 `ells tunnel web` 也是查"这个 db 到底开在哪个口"的命令。
    /// 绑定失败会点名是谁占着同一个口（另一台 ells 主机，还是外部程序）。
    Tunnel {
        /// 主机别名（配合 --all 时可省略）
        alias: Option<String>,
        /// 拉起所有配了转发规则的主机
        #[arg(long)]
        all: bool,
    },
    /// 导出成 ~/.ssh/config 片段（永不含密码）
    Export {
        /// 写到文件而不是标准输出
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// 打印 shell 补全脚本：ells completions bash
    Completions {
        /// bash | zsh | fish
        shell: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum SftpOp {
    /// 列远端目录
    Ls { alias: String, path: Option<String> },
    /// 下载远端文件
    Get {
        alias: String,
        remote: String,
        /// 本地目录或文件名（默认当前目录同名）
        local: Option<PathBuf>,
    },
    /// 上传本地文件
    Put {
        alias: String,
        local: PathBuf,
        /// 远端路径（默认同名落到远端家目录）
        remote: Option<String>,
    },
    /// 删远端文件（目录要 --recursive）
    Rm {
        alias: String,
        path: String,
        #[arg(long)]
        recursive: bool,
    },
    /// 改远端权限：ells sftp chmod web 600 /home/app/.env
    Chmod {
        alias: String,
        /// 八进制，如 600 / 0644
        mode: String,
        path: String,
    },
}

/// 主入口：返回进程退出码（远端命令的退出码原样透传）。
pub async fn dispatch(cmd: Command, dev: bool, yes: bool) -> Result<i32> {
    match cmd {
        Command::List { format, filter } => list(&format, filter, dev),
        Command::Exec {
            alias,
            command,
            tty,
            timeout,
            stdin,
        } => exec_head(&alias, &command, tty, timeout, stdin, dev, yes).await,
        Command::Sftp { op, yes: overwrite } => sftp(op, overwrite, dev, yes).await,
        Command::Tunnel { alias, all } => tunnel(alias.as_deref(), all, dev, yes).await,
        Command::Export { out } => export(out, dev),
        Command::Completions { shell } => completions(&shell),
    }
}

/// 取保险库：`--dev` 读明文开发库，否则用主密码解 `vault.bin`。
/// 主密码来自 `ELLS_MASTER`、管道里的第一行，或在终端上掩码输入。
fn open_vault_head(dev: bool) -> Result<Hosts> {
    if dev {
        return Ok(Hosts(vault::load_dev_vault().context("--dev 读 ~/.ells/hosts.dev.toml 失败")?));
    }
    let master = read_master()?;
    let (vault, _key) =
        vault::unlock_vault(&master).context("保险库解锁失败（主密码不对或 vault.bin 损坏）")?;
    Ok(Hosts(vault))
}

/// 包一层，避免调用方误以为拿到了解锁密钥（无头模式不需要也不该留着它）。
struct Hosts(ells_core::Vault);

fn read_master() -> Result<String> {
    if let Ok(from_env) = std::env::var("ELLS_MASTER") {
        return Ok(from_env);
    }
    // 管道：`echo pw | ells list`。不是 tty 就绝不假装能交互，否则会吞掉用户的管道数据
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("从标准输入读主密码失败")?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            bail!("标准输入里没有主密码（或改用 ELLS_MASTER 传入）");
        }
        return Ok(trimmed.to_string());
    }
    prompt_secret("主密码：")
}

/// 终端掩码输入：回车结束，退格可改，屏幕上只留星号。
fn prompt_secret(label: &str) -> Result<String> {
    let mut out = String::new();
    let mut err = std::io::stderr();
    write!(err, "{label}")?;
    err.flush()?;
    enable_raw_mode().context("开启原始模式失败")?;
    let result = read_secret(&mut out);
    let _ = disable_raw_mode();
    let _ = write!(err, "\r\n");
    let _ = err.flush();
    result?;
    Ok(out)
}

fn read_secret(out: &mut String) -> Result<()> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
    loop {
        let Event::Key(key) = read().context("读取按键失败")? else {
            continue;
        };
        // X11 会连按键释放一起报上来，不过滤等于每个字符输两遍
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    if c == 'c' {
                        bail!("已取消");
                    }
                    continue;
                }
                out.push(c);
                let _ = write!(std::io::stderr(), "*");
                let _ = std::io::stderr().flush();
            }
            KeyCode::Backspace => {
                if out.pop().is_some() {
                    // 退格也要把星号擦掉，否则掩码长度骗人
                    let _ = write!(std::io::stderr(), "\u{8} \u{8}");
                    let _ = std::io::stderr().flush();
                }
            }
            KeyCode::Enter => return Ok(()),
            KeyCode::Esc => bail!("已取消"),
            _ => {}
        }
    }
}

fn find_host(vault: &ells_core::Vault, alias: &str) -> Result<Host> {
    let found = vault
        .find(alias)
        .cloned()
        .or_else(|| {
            let lower = alias.to_lowercase();
            vault
                .hosts
                .iter()
                .find(|h| h.alias.to_lowercase() == lower)
                .cloned()
        });
    found.with_context(|| format!("找不到别名为 `{alias}` 的主机（ells list 看全部）"))
}

fn list(format: &str, filter: Option<String>, dev: bool) -> Result<i32> {
    let Hosts(vault) = open_vault_head(dev)?;
    let needle = filter.map(|f| f.to_lowercase());
    let rows: Vec<&Host> = vault
        .hosts
        .iter()
        .filter(|h| match &needle {
            None => true,
            Some(n) => {
                let mut hay = format!("{} {} {} {:?}", h.alias, h.hostname, h.user, h.group)
                    .to_lowercase();
                for t in &h.tags {
                    hay.push(' ');
                    hay.push_str(&t.to_lowercase());
                }
                hay.contains(n)
            }
        })
        .collect();
    match format {
        "json" => {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "alias": h.alias,
                        "hostname": h.hostname,
                        "port": h.port,
                        "user": h.user,
                        "auth": h.auth_label(),
                        "jump": h.jump,
                        "group": h.group,
                        "tags": h.tags,
                        "favorite": h.favorite,
                        "last_connected": h.last_connected,
                        "forwards": h.forwards.iter().map(|f| f.label()).collect::<Vec<_>>(),
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&items).context("序列化 JSON 失败")?
            );
        }
        "table" => {
            println!(
                "{:<16} {:<28} {:<12} {:<6} {}",
                "别名", "主机", "用户", "认证", "分组/转发"
            );
            for h in rows {
                let mut extra = h.group.clone().unwrap_or_default();
                if !h.forwards.is_empty() {
                    extra.push_str(&format!(
                        " {}",
                        h.forwards
                            .iter()
                            .map(|f| f.label())
                            .collect::<Vec<_>>()
                            .join(",")
                    ));
                }
                println!(
                    "{:<16} {:<28} {:<12} {:<6} {}",
                    h.alias,
                    format!("{}:{}", h.hostname, h.port),
                    h.user,
                    h.auth_label(),
                    extra.trim(),
                );
            }
        }
        other => return usage(format!("未知 --format `{other}`（可用：table / json）")),
    }
    Ok(EX_OK)
}

async fn exec_head(
    alias: &str,
    command: &[String],
    tty: bool,
    timeout: Option<u64>,
    pipe_stdin: bool,
    dev: bool,
    yes: bool,
) -> Result<i32> {
    let Hosts(vault) = open_vault_head(dev)?;
    let host = find_host(&vault, alias)?;
    if command.is_empty() {
        return usage(format!(
            "ells exec 需要命令：ells exec {alias} -- <命令>（要交互式终端就直接 ells {alias}）"
        ));
    }
    let stdin_bytes = if pipe_stdin {
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            // 终端输入直接灌进远端会看起来像卡死，明确拒绝比静默等得好
            return usage("--stdin 要求标准输入是管道或文件（当前是终端）");
        }
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        buf
    } else {
        Vec::new()
    };
    let policy = HostKeyPolicy::headless(yes);
    let command_line = ells_core::exec::join_command(command);
    let run = async {
        ells_core::exec::exec(&host, &vault, &policy, &command_line, tty, 80, 24, &stdin_bytes).await
    };
    let result = match timeout {
        Some(secs) => match tokio::time::timeout(std::time::Duration::from_secs(secs), run).await {
            Err(_) => {
                // 124 是 timeout(1) 的约定，脚本据此区分"跑慢了"和"跑挂了"
                eprintln!("超过 {secs} 秒未完成，已放弃（退出码 {EX_TIMEOUT}）");
                return Ok(EX_TIMEOUT);
            }
            Ok(inner) => inner.with_context(|| format!("在 {} 上执行失败", host.alias))?,
        },
        None => run
            .await
            .with_context(|| format!("在 {} 上执行失败", host.alias))?,
    };
    let mut stdout = std::io::stdout();
    stdout.write_all(&result.stdout)?;
    stdout.flush()?;
    std::io::stderr().write_all(&result.stderr)?;
    let _ = audit::record(
        AuditKind::Connect,
        &host.alias,
        &format!("无头执行 {command_line} → {}", result.status),
    );
    Ok(if result.status < 0 { EX_FAIL } else { result.status })
}

async fn sftp(op: SftpOp, overwrite: bool, dev: bool, yes: bool) -> Result<i32> {
    let alias = match &op {
        SftpOp::Ls { alias, .. }
        | SftpOp::Get { alias, .. }
        | SftpOp::Put { alias, .. }
        | SftpOp::Rm { alias, .. }
        | SftpOp::Chmod { alias, .. } => alias.clone(),
    };
    let Hosts(vault) = open_vault_head(dev)?;
    let host = find_host(&vault, &alias)?;
    let policy = HostKeyPolicy::headless(yes);
    let sftp = ssh::connect_sftp(&host, &vault, &policy)
        .await
        .with_context(|| format!("无法与 {} 建立 SFTP", host.alias))?;
    // 进度事件在命令行下没人画，但通道必须有人收：收满会拖住传输
    let (tx, mut rx) = mpsc::unbounded_channel::<Progress>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let cancel = Cancel::default();

    match op {
        SftpOp::Ls { path, .. } => {
            // 远端家目录可能正是 "/"（sftp 根、或禁了 realpath 的网关），普通拼接会得出 "//sub"
            let dir = match path {
                Some(p) if p.starts_with('/') => p,
                Some(p) => transfer::remote_join(&remote_home(&sftp).await?, &p),
                None => remote_home(&sftp).await?,
            };
            let entries = transfer::list(&sftp, &dir).await?;
            print_entries(&dir, &entries);
        }
        SftpOp::Get { remote, local, .. } => {
            let name = remote
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("download")
                .to_string();
            let dest = local.unwrap_or_else(|| PathBuf::from(&name));
            let (dir, filename) = if dest.is_dir() {
                (dest.clone(), name.clone())
            } else {
                (
                    dest.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")),
                    dest.file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or(name.clone()),
                )
            };
            let target = dir.join(&filename);
            if target.exists() && !overwrite {
                return usage(format!(
                    "本地已存在 {}，加 --yes 才覆盖",
                    target.display()
                ));
            }
            transfer::download(&sftp, remote.clone(), &dir, tx, &filename, &cancel)
                .await?;
            println!("{}", target.display());
            let _ = audit::record(AuditKind::Transfer, &host.alias, &format!("下载 {remote}"));
        }
        SftpOp::Put { local, remote, .. } => {
            if !local.exists() {
                return usage(format!("本地文件不存在：{}", local.display()));
            }
            let name = local
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .context("本地路径没有文件名")?;
            let target = match remote {
                Some(r) => r,
                // 家目录是 "/" 时普通拼接会得出 "//up.txt"，远端把它当成另一个路径
                None => transfer::remote_join(&remote_home(&sftp).await?, &name),
            };
            if transfer::remote_meta(&sftp, &target).await?.is_some() && !overwrite {
                return usage(format!("远端已存在 {target}，加 --yes 才覆盖"));
            }
            transfer::upload(&sftp, &local, target.clone(), tx, &cancel).await?;
            println!("{target}");
            let _ = audit::record(AuditKind::Transfer, &host.alias, &format!("上传 {target}"));
        }
        SftpOp::Rm { path, recursive, .. } => {
            let meta = transfer::remote_meta(&sftp, &path)
                .await?
                .with_context(|| format!("远端不存在 {path}"))?;
            if meta.is_dir {
                if !recursive {
                    return usage(format!("{path} 是目录，删整棵树要加 --recursive"));
                }
                let n = transfer::remove_tree(&sftp, &path, &cancel).await?;
                println!("已删除 {n} 项：{path}");
            } else {
                transfer::remove_one(&sftp, &path, false).await?;
                println!("已删除：{path}");
            }
            let _ = audit::record(AuditKind::Transfer, &host.alias, &format!("删除 {path}"));
        }
        SftpOp::Chmod { mode, path, .. } => {
            let Some(bits) = transfer::parse_mode(&mode) else {
                return usage(format!("权限要八进制，如 600 / 0644（收到的是 {mode}）"));
            };
            transfer::chmod(&sftp, &path, bits).await?;
            println!("{path} → {mode}");
            let _ = audit::record(
                AuditKind::Transfer,
                &host.alias,
                &format!("chmod {mode} {path}"),
            );
        }
    }
    Ok(EX_OK)
}

fn print_entries(dir: &str, entries: &[FileEntry]) {
    println!("# {dir}");
    for e in entries {
        if e.name == "." || e.name == ".." {
            continue;
        }
        let kind = if e.is_dir { "d" } else { "-" };
        println!("{kind} {:>10} {}", e.size, e.name);
    }
}

/// 远端家目录：SFTP 相对路径的基准，`canonicalize(".")` 是标准问法。
async fn remote_home(sftp: &SftpSession) -> Result<String> {
    Ok(sftp
        .canonicalize(".")
        .await
        .context("问不到远端家目录")?)
}

async fn tunnel(alias: Option<&str>, all: bool, dev: bool, yes: bool) -> Result<i32> {
    let Hosts(vault) = open_vault_head(dev)?;
    let targets: Vec<Host> = match (alias, all) {
        (Some(a), _) => vec![find_host(&vault, a)?],
        (None, true) => vault
            .hosts
            .iter()
            .filter(|h| !h.forwards.is_empty())
            .cloned()
            .collect(),
        (None, false) => {
            return usage("ells tunnel 需要主机别名，或加 --all 起全部有规则的机器");
        }
    };
    let targets: Vec<Host> = targets
        .into_iter()
        .filter(|h| {
            if h.forwards.is_empty() {
                eprintln!("跳过 {}：没有转发规则", h.alias);
                return false;
            }
            true
        })
        .collect();
    if targets.is_empty() {
        return usage("这些主机都没配转发规则（编辑主机，在「转发」栏写 -L/-D）");
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<TunnelEvent>();
    let policy = HostKeyPolicy::headless(yes);
    let mut manager = ells_core::TunnelManager::new(tx, policy);
    let vault = Arc::new(vault);
    for host in &targets {
        // 用 display()：自动口写成 `-L 0:db:5432` 会让人以为真去连 0 端口
        println!(
            "启动 {}：{}",
            host.alias,
            host.forwards
                .iter()
                .map(|f| f.display())
                .collect::<Vec<_>>()
                .join(" ")
        );
        manager.start(host, Arc::clone(&vault));
    }
    // 状态事件就是这条命令的全部输出；通道关了说明所有隧道都已终局（全部失败），
    // 否则用户会以为还在跑
    while let Some(event) = rx.recv().await {
        println!("{}", event.label());
    }
    manager.stop_all();
    Ok(EX_OK)
}

fn export(out: Option<PathBuf>, dev: bool) -> Result<i32> {
    let Hosts(vault) = open_vault_head(dev)?;
    let text = sshconfig::to_config_text(&vault.hosts);
    match out {
        Some(path) => {
            // 原子写：中途断电不该留下一份半截配置，那种文件粘进 ~/.ssh/config 最坑
            ells_core::write_atomic(&path, text.as_bytes())?;
            println!("已写入 {}（不含任何密码）", path.display());
            let _ = audit::record(
                AuditKind::Export,
                "ssh-config",
                &format!("导出 {} 台 → {}", vault.hosts.len(), path.display()),
            );
        }
        None => print!("{text}"),
    }
    Ok(EX_OK)
}

/// 补全脚本手写：不值得为它再拉一个 clap_complete 依赖。
fn completions(shell: &str) -> Result<i32> {
    const CMDS: &str = "list exec sftp tunnel export completions";
    let text = match shell {
        "bash" => format!(
            "_ells_completer() {{\n  local cur=\"${{COMP_WORDS[COMP_CWORD]}}\"\n  COMPREPLY=( $(compgen -W \"list exec sftp tunnel export completions\" -- \"$cur\") )\n}}\ncomplete -F _ells_completer ells\ncomplete -F _ells_completer s\n"
        ),
        "zsh" => format!(
            "#compdef ells s\n_ells() {{\n  local -a commands\n  commands=({CMDS})\n  _describe 'ells 子命令' commands\n}}\ncompdef _ells ells\ncompdef _ells s\n"
        ),
        "fish" => format!(
            "complete -c ells -f -a \"{CMDS}\"\ncomplete -c s -f -a \"{CMDS}\"\n"
        ),
        other => return usage(format!("未知 shell：{other}（可用：bash / zsh / fish）")),
    };
    print!("{text}");
    Ok(EX_OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_search_is_case_insensitive_by_alias() {
        let mut vault = ells_core::Vault::default();
        vault.upsert(Host {
            alias: "Web-Prod".into(),
            hostname: "10.0.0.1".into(),
            port: 22,
            user: "root".into(),
            auth: ells_core::Auth::Agent,
            ..Default::default()
        });
        assert_eq!(find_host(&vault, "web-prod").unwrap().alias, "Web-Prod");
        assert!(find_host(&vault, "nope").is_err());
    }

    /// 无头输出的红线：密码字段一次都不能出现。
    #[test]
    fn json_export_has_no_secrets() {
        let mut vault = ells_core::Vault::default();
        vault.upsert(Host {
            alias: "db".into(),
            hostname: "10.0.0.2".into(),
            port: 22,
            user: "postgres".into(),
            auth: ells_core::Auth::Password,
            password: Some("hunter2-super-secret".into()),
            note: Some("备注".into()),
            ..Default::default()
        });
        let item = serde_json::json!({
            "alias": vault.hosts[0].alias,
            "hostname": vault.hosts[0].hostname,
            "user": vault.hosts[0].user,
            "auth": vault.hosts[0].auth_label(),
        });
        let text = serde_json::to_string(&item).unwrap();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("备注"), "{text}");
    }
}

# 安全政策 / Security Policy

中文 | [English below](#reporting-a-vulnerability-english)

## 报告漏洞（中文）

请不要在公开 Issue 里报告可利用的安全问题 —— ells 是公开的，一条 Issue 就等于
一份攻击说明。

- **优先**：[GitHub 私密安全通告](https://github.com/lg10/ells/security/advisories/new)
  （Private Vulnerability Report）。只有你和仓库维护者能看到。
- **不确定算不算漏洞**：同样走私密通告，我们来判断，不会因为"报错了"而追究。
- 我们尽力在 **3 个自然日内**回复初步评估。修复后会发布新版本，并在 GitHub
  Advisory 里公开致谢（如果你希望匿名请注明）。

### 报告里请附上

ells 版本（`ells --version`）、操作系统与终端（Windows Terminal / iTerm2 /
GNOME Terminal 等）、触发路径、以及能复现的最小步骤。

### 绝对不要贴出来的东西

这些一旦公开就等同于交出凭据，删掉也可能已被缓存：

- 主密码，或 `~/.ells/vault.bin` 的内容（含片段、十六进制、Base64）
- `~/.ells/.master`（关闭主密码保护后的自动解锁凭据，里面就是明文主密码）
- `~/.ells/known_hosts`、`~/.ssh/` 下的任何文件
- 私钥文件、私钥口令、服务器真实地址与账号

需要让我们看到配置时，请新造一个测试用的保险库（或把主机名、IP、用户名全部
改成假值）再上传。

## 支持版本

0.x 阶段只维护最新版：安全修复会直接发新版本，不会回填已公开的旧 tag。
请尽快升级到最新版——启动时的徽标点一下就行（见 README 的「应用内更新」），
或者重跑 `install.sh` / `install.ps1`。

## ells 的安全边界（请知情使用）

1. **主密码不可找回。** 没有后门、没有服务器端备份、没有重置流程。忘记主密码
   只能删除 `~/.ells/vault.bin` 重建，所有主机凭据作废。
2. **关闭主密码保护 = 降级为文件权限级安全。** 设置里关掉保护后，明文主密码会
   写入 `~/.ells/.master` 以实现开机自动解锁；能读到这个文件的人就能解开保险库。
   ells 会把 `~/.ells` 收紧到仅当前用户（Unix 0700，Windows 用 `icacls` 去掉继承），
   但同机的 root/管理员仍可读取。共享机器或多用户环境请保持保护开启。
3. **凭据只在本机加解密**，ells 不含任何遥测，也不会把主密码发往任何服务器。
4. **主机密钥是 TOFU（首次信任）**，记录在 `~/.ells/known_hosts`。首次连接没有
   第三方验证 —— 如果第一次就连上了恶意中继，ells 无法察觉；密钥后续变化会阻断
   并告警。对高风险主机请人工核对指纹。
5. **`sz`/`rz` 劫持会把远端文件写进你选择的本地路径**，覆盖前会二次确认。
6. **应用内更新信任的是"GitHub 发布资产 + TLS"**，和 `install.sh` 同一套模型：二进制没有
   独立签名，靠发布里的 `SHA256SUMS.txt` 与传输层 TLS 保证没被换过，校验不过就绝不替换。
   需要更强保证时请继续用安装脚本手工更新，并在设置里关掉「自动更新」。
7. **开着「自动更新」时，每次启动会向 GitHub 发一个 HEAD 请求**（User-Agent 里带 ells
   版本号），这是 ells 唯一的外连，不含主机、地址或用户名。想让进程完全不主动出网就把
   `auto_update` 设为 `false`，更新改由 `ells --check-update` 手工触发。

## Reporting a vulnerability (English)

Do not open a public issue for exploitable security problems — this repository is
public, so an issue is effectively a disclosure.

- Preferred: [private vulnerability report](https://github.com/lg10/ells/security/advisories/new).
- Unsure whether it counts? Report it privately anyway; we triage.
- We aim to respond with an initial assessment within 3 calendar days. Fixes ship
  as a new release, followed by a public GitHub Advisory with credit to you unless
  you ask to stay anonymous.

Include: `ells --version`, OS and terminal (Windows Terminal / iTerm2 / GNOME
Terminal), the trigger path, and minimal reproduction steps.

Never attach: your master password, `~/.ells/vault.bin` (or any fragment of it),
`~/.ells/.master` (that is the plaintext master password when auto-unlock is on),
`~/.ells/known_hosts`, anything under `~/.ssh/`, private keys, key passphrases, or
real hostnames, IPs and usernames. If you need to share config, create a throwaway
vault first or replace every identifying value with a fake one.

ells only supports the latest 0.x release: security fixes ship as a new version and
are never backported to published tags.

In-app updates trust the same model as the installers: the binary is fetched from the
GitHub release and verified against `SHA256SUMS.txt` over TLS — there is no separate
code signature, and a checksum mismatch aborts the replacement. With 「自动更新」 left
on, each start sends one HEAD request to GitHub carrying only ells's version in the
User-Agent; disable it if you would rather nothing leave the machine on its own.

use anyhow::{anyhow, bail, Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::host::Host;

const MAGIC: &[u8; 9] = b"ELLSVAULT";
const FORMAT_VERSION: u8 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Vault {
    #[serde(default)]
    pub hosts: Vec<Host>,
}

impl Vault {
    pub fn find(&self, alias: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.alias == alias)
    }

    pub fn upsert(&mut self, host: Host) {
        if let Some(pos) = self.hosts.iter().position(|h| h.alias == host.alias) {
            self.hosts[pos] = host;
        } else {
            self.hosts.push(host);
        }
    }

    pub fn remove(&mut self, alias: &str) -> bool {
        let before = self.hosts.len();
        self.hosts.retain(|h| h.alias != alias);
        self.hosts.len() != before
    }
}

pub fn vault_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot resolve home directory")?;
    Ok(home.join(".ells").join("vault.bin"))
}

/// `~/.ells`：保险库、known_hosts、设置、自动解锁凭据都放这里。
pub fn config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ells"))
}

/// 把配置目录的访问权限收到"仅当前用户"。
///
/// 保险库里躺着所有主机凭据，目录默认权限（0755 / Windows 继承的 ACL）在同机
/// 其他账号看来是可读的。best-effort：任何一步失败都只是少一层加固，
/// 绝不能因此让程序起不来。
#[cfg(unix)]
pub fn harden_config_dir() {
    use std::os::unix::fs::PermissionsExt;
    let Some(dir) = config_dir() else { return };
    let _ = fs::create_dir_all(&dir);
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
}

/// Windows 的 unix 权限位形同虚设，只能走 ACL：`icacls` 去掉继承项、只留给
/// 当前用户（和 ssh-keygen 收紧私钥的写法一致）。
#[cfg(windows)]
pub fn harden_config_dir() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Some(dir) = config_dir() else { return };
    let _ = fs::create_dir_all(&dir);

    let mut cmd = std::process::Command::new("whoami");
    cmd.creation_flags(CREATE_NO_WINDOW);
    let Ok(out) = cmd.output() else { return };
    if !out.status.success() {
        return;
    }
    let user = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if user.is_empty() {
        return;
    }
    // (OI)(CI) = 对象与子容器继承，F = 完全控制：目录里以后的文件都跟着收紧
    let mut icacls = std::process::Command::new("icacls");
    icacls
        .arg(&dir)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{user}:(OI)(CI)F"))
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match icacls.status() {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(code = ?s.code(), "icacls 收紧 ~/.ells 权限未生效"),
        Err(err) => tracing::warn!(%err, "icacls 无法执行，~/.ells 权限未收紧"),
    }
}

pub fn vault_exists() -> bool {
    vault_path().map(|p| p.exists()).unwrap_or(false)
}

fn derive_key(master: &str, salt: &[u8]) -> Result<[u8; KEY_LEN]> {
    let params = Params::new(64 * 1024, 3, 1, Some(KEY_LEN))
        .map_err(|err| anyhow!("argon2 params rejected: {err:?}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; KEY_LEN];
    if let Err(err) = argon.hash_password_into(master.as_bytes(), salt, &mut out) {
        // 派生失败也可能已经写过 out：不留着一半密钥在栈上
        out.zeroize_local();
        return Err(anyhow!("argon2 KDF failed: {err:?}"));
    }
    Ok(out)
}

/// Derived master key, cached in memory so saves don't re-run the slow KDF.
/// Zeroized on drop.
pub struct VaultKey {
    salt: [u8; SALT_LEN],
    key: [u8; KEY_LEN],
}

impl Drop for VaultKey {
    fn drop(&mut self) {
        self.key.zeroize_local();
    }
}

/// Redacted: never let key material reach a log line.
impl std::fmt::Debug for VaultKey {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("VaultKey").finish_non_exhaustive()
    }
}

fn check_header(bytes: &[u8]) -> Result<([u8; SALT_LEN], &[u8], &[u8])> {
    let min_len = MAGIC.len() + 1 + SALT_LEN + NONCE_LEN + 16;
    if bytes.len() < min_len {
        bail!("保险库文件不完整或已损坏");
    }
    if &bytes[..MAGIC.len()] != MAGIC {
        bail!("不是 ells 保险库文件");
    }
    if bytes[MAGIC.len()] != FORMAT_VERSION {
        bail!("不支持的保险库格式版本");
    }
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&bytes[MAGIC.len() + 1..MAGIC.len() + 1 + SALT_LEN]);
    let nonce = &bytes[MAGIC.len() + 1 + SALT_LEN..MAGIC.len() + 1 + SALT_LEN + NONCE_LEN];
    let ct = &bytes[MAGIC.len() + 1 + SALT_LEN + NONCE_LEN..];
    Ok((salt, nonce, ct))
}

fn decode_with_key(all: &[u8], key: &[u8; KEY_LEN]) -> Result<Vault> {
    let (_salt, nonce_bytes, ct) = check_header(all)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = XNonce::from_slice(nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ct)
        .map_err(|_| anyhow!("主密码错误或保险库已损坏"))?;
    let text = String::from_utf8(plaintext).context("保险库内容不是有效文本")?;
    let vault: Vault = toml::from_str(&text).context("保险库内容格式错误")?;
    Ok(vault)
}

/// Derive a fresh key (random salt) for a brand-new vault.
pub fn create_vault_key(master: &str) -> Result<VaultKey> {
    if master.is_empty() {
        bail!("主密码不能为空");
    }
    let mut salt = [0u8; SALT_LEN];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let key = derive_key(master, &salt)?;
    Ok(VaultKey { salt, key })
}

/// Encrypt the vault under an already-derived key (reuses the key's salt).
pub fn store_vault_key(vault: &Vault, path: &Path, vk: &VaultKey) -> Result<()> {
    let plaintext = toml::to_string_pretty(vault).context("序列化保险库失败")?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&vk.key));
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| anyhow!("保险库加密失败: {e}"))?;
    let mut blob = Vec::with_capacity(MAGIC.len() + 1 + SALT_LEN + NONCE_LEN + ct.len());
    blob.extend_from_slice(MAGIC);
    blob.push(FORMAT_VERSION);
    blob.extend_from_slice(&vk.salt);
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ct);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).ok();
    }
    write_secret_file(path, &blob)
}

/// Save the open vault using the cached key (fast: no KDF).
pub fn save_vault_key(vault: &Vault, vk: &VaultKey) -> Result<()> {
    store_vault_key(vault, &vault_path()?, vk)
}

/// Unlock and return the vault together with its cached key.
pub fn unlock_vault(master: &str) -> Result<(Vault, VaultKey)> {
    let path = vault_path()?;
    let bytes =
        fs::read(&path).with_context(|| format!("无法读取保险库 {}", path.display()))?;
    let (salt, _, _) = check_header(&bytes)?;
    let mut key = derive_key(master, &salt)?;
    // 解不开（主密码错/文件损坏）也要把派生密钥抹掉：它和主密码等价
    let vault = match decode_with_key(&bytes, &key) {
        Ok(v) => v,
        Err(err) => {
            key.zeroize_local();
            return Err(err);
        }
    };
    Ok((vault, VaultKey { salt, key }))
}

pub fn create_vault(master: &str, vault: &Vault) -> Result<()> {
    let path = vault_path()?;
    if path.exists() {
        bail!("保险库已存在于 {}", path.display());
    }
    let vk = create_vault_key(master)?;
    store_vault_key(vault, &path, &vk)
}

pub fn open_vault(master: &str) -> Result<Vault> {
    unlock_vault(master).map(|(v, _)| v)
}

/// Rotate the master password in place (re-encrypt).
pub fn store_vault(master: &str, path: &Path, vault: &Vault) -> Result<()> {
    let vk = create_vault_key(master)?;
    store_vault_key(vault, path, &vk)
}

pub fn decode_vault(master: &str, bytes: &[u8]) -> Result<Vault> {
    let (salt, _, _) = check_header(bytes)?;
    let mut key = derive_key(master, &salt)?;
    let vault = match decode_with_key(bytes, &key) {
        Ok(v) => v,
        Err(err) => {
            key.zeroize_local();
            return Err(err);
        }
    };
    Ok(vault)
}

fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::secure_fs::write_atomic(path, bytes)
}

/// Zeroize helper on arrays (zeroize crate's impl covers common types).
trait ZeroizeLocal {
    fn zeroize_local(&mut self);
}
impl ZeroizeLocal for [u8; KEY_LEN] {
    fn zeroize_local(&mut self) {
        for b in self.iter_mut() {
            *b = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Auth, Forward};

    /// 从指定路径读一份保险库（测试用：不碰 `~/.ells`）。
    fn open_at(master: &str, path: &Path) -> Result<Vault> {
        decode_vault(master, &fs::read(path)?)
    }

    #[test]
    fn vault_roundtrip() {
        let mut v = Vault::default();
        v.upsert(Host {
            alias: "test".into(),
            hostname: "example.com".into(),
            port: 2222,
            user: "root".into(),
            auth: Auth::Password,
            password: Some("s3cret".into()),
            jump: None,
            note: None,
            ..Default::default()
        });
        let blob_master = "hunter2";
        let tmp = std::env::temp_dir().join("ells-test-vault.bin");
        let _ = fs::remove_file(&tmp);
        store_vault(blob_master, &tmp, &v).unwrap();
        let bytes = fs::read(&tmp).unwrap();
        let back = decode_vault(blob_master, &bytes).unwrap();
        assert_eq!(back.hosts.len(), 1);
        assert_eq!(back.hosts[0].password.as_deref(), Some("s3cret"));
        assert!(decode_vault("wrong", &bytes).is_err());
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn dev_hosts_toml_parses() {
        let text = r#"
[[hosts]]
alias = "smoke"
hostname = "127.0.0.1"
port = 2222
user = "tester"
auth = { type = "password" }
password = "test123"
"#;
        let vault: Vault = toml::from_str(text).unwrap();
        assert_eq!(vault.hosts.len(), 1);
        assert_eq!(vault.hosts[0].port, 2222);
        assert_eq!(vault.hosts[0].auth, Auth::Password);
    }

    #[test]
    fn toml_roundtrip_key_and_jump() {
        let mut v = Vault::default();
        v.upsert(Host {
            alias: "prod".into(),
            hostname: "203.0.113.7".into(),
            port: 22,
            user: "root".into(),
            auth: Auth::PrimaryKey {
                path: "C:\\Users\\dev\\.ssh\\id_ed25519".into(),
                passphrase: Some("pp".into()),
            },
            password: None,
            jump: Some("bastion".into()),
            note: None,
            ..Default::default()
        });
        let text = toml::to_string_pretty(&v).unwrap();
        let back: Vault = toml::from_str(&text).unwrap();
        assert_eq!(back.hosts[0].auth, v.hosts[0].auth);
        assert_eq!(back.hosts[0].jump.as_deref(), Some("bastion"));
        assert_eq!(back.hosts[0].hostname, "203.0.113.7");
    }

    /// 转发规则、分组、标签、收藏都要能进出保险库——旧格式的 vault.bin 没有这些
    /// 字段也必须解得开（全部 default）。
    #[test]
    fn forwards_tags_and_group_survive_the_vault() {
        let mut v = Vault::default();
        v.upsert(Host {
            alias: "db".into(),
            hostname: "10.0.0.9".into(),
            port: 22,
            user: "root".into(),
            auth: Auth::Agent,
            forwards: vec![
                Forward::Local {
                    bind: None,
                    listen_port: 5432,
                    dest_host: "localhost".into(),
                    dest_port: 5432,
                },
                Forward::Dynamic { bind: Some("127.0.0.1".into()), listen_port: 1080 },
            ],
            group: Some("生产".into()),
            tags: vec!["pg".into(), "重要".into()],
            favorite: true,
            last_connected: 1_700_000_000,
            ..Default::default()
        });
        let text = toml::to_string_pretty(&v).unwrap();
        let back: Vault = toml::from_str(&text).unwrap();
        assert_eq!(back.hosts[0].forwards, v.hosts[0].forwards);
        assert_eq!(back.hosts[0].group.as_deref(), Some("生产"));
        assert_eq!(back.hosts[0].tags, ["pg", "重要"]);
        assert!(back.hosts[0].favorite);
        assert_eq!(back.hosts[0].last_connected, 1_700_000_000);

        let legacy = " [[hosts]]\nalias = \"old\"\nhostname = \"h\"\nport = 22\nuser = \"u\"\nauth = { type = \"password\" }\n";
        let old: Vault = toml::from_str(legacy).unwrap();
        assert!(old.hosts[0].forwards.is_empty());
        assert!(!old.hosts[0].favorite);
    }

    /// 保险库整体加密后写盘，中途失败不能把上一份好数据毁掉。
    #[test]
    fn a_failed_save_keeps_the_previous_vault_readable() {
        let dir = std::env::temp_dir().join(format!(
            "ells-vault-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vault.bin");
        let master = "hunter2";

        let mut v1 = Vault::default();
        v1.upsert(Host {
            alias: "first".into(),
            ..Default::default()
        });
        store_vault_key(&v1, &path, &create_vault_key(master).unwrap()).unwrap();

        // 目标路径的父目录是个普通文件：临时文件都开不出来，rename 更无从谈起，
        // 而已经存在的 vault.bin 必须还完好可读
        let broken = dir.join("vault.bin").join("nested.bin");
        let mut v2 = Vault::default();
        v2.upsert(Host {
            alias: "second".into(),
            ..Default::default()
        });
        let vk = create_vault_key(master).unwrap();
        assert!(store_vault_key(&v2, &broken, &vk).is_err());
        assert_eq!(open_at(master, &path).unwrap().hosts[0].alias, "first");

        // 正常保存覆盖后，旧内容不再可见
        store_vault_key(&v2, &path, &vk).unwrap();
        assert_eq!(open_at(master, &path).unwrap().hosts[0].alias, "second");
        std::fs::remove_dir_all(&dir).ok();
    }
}

pub fn save(master: &str, vault: &Vault) -> Result<()> {
    store_vault(master, &vault_path()?, vault)
}

/// Dev-only plaintext hosts file at `~/.ells/hosts.dev.toml`
/// (`hosts = [ { alias = "...", hostname = "...", ... } ]`), used with
/// `ells --dev` so iterating on UI does not require typing the master
/// password each run.
pub fn load_dev_vault() -> Result<Vault> {
    let home = dirs::home_dir().context("cannot resolve home directory")?;
    let path = home.join(".ells").join("hosts.dev.toml");
    let text = fs::read_to_string(&path)
        .with_context(|| format!("cannot read dev hosts at {}", path.display()))?;
    let vault: Vault = toml::from_str(&text).context("malformed dev hosts file")?;
    Ok(vault)
}

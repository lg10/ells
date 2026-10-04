use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Auth {
    Password,
    PrimaryKey {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        passphrase: Option<String>,
    },
    Agent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub alias: String,
    pub hostname: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    pub auth: Auth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Alias of a bastion/jump host to tunnel through (ProxyJump style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn default_port() -> u16 {
    22
}

impl Host {
    pub fn target(&self) -> String {
        format!("{}@{}:{}", self.user, self.hostname, self.port)
    }

    pub fn auth_label(&self) -> &'static str {
        match &self.auth {
            Auth::Password => "密码",
            Auth::PrimaryKey { .. } => "密钥",
            Auth::Agent => "agent",
        }
    }
}

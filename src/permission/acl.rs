use crate::config::get_config_path;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AclConfig {
    pub rules: Vec<AclRule>,
    pub acl: AclSettings,
}

impl Default for AclConfig {
    fn default() -> Self {
        // Note: The programmatic default here is minimal.
        // The file template (DEFAULT_ACL_TOML) contains the full default configuration.
        Self {
            rules: vec![],
            acl: AclSettings::default(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AclRule {
    pub name: String,
    pub server_group_ids: Vec<u32>,
    pub allowed_skills: Vec<String>,
    pub can_target_admins: bool,
    pub rate_limit_override: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AclSettings {
    pub protected_group_ids: Vec<u32>,
}

impl Default for AclSettings {
    fn default() -> Self {
        Self {
            protected_group_ids: vec![6, 8, 9],
        }
    }
}

pub const DEFAULT_ACL_TOML: &str = r#"# 权限规则从上到下评估；第一个匹配的生效。
# server_group_ids: TeamSpeak 服务器组 ID（整数）
# allowed_skills: 技能名称列表，或者 ["*"] 表示全部
# can_target_admins: 此角色是否可以对管理员组成员执行操作
# rate_limit_override: 可选的每角色每分钟请求数（覆盖全局设置）

[[rules]]
name = "superadmin"
server_group_ids = [6]
allowed_skills = ["*"]
can_target_admins = true
rate_limit_override = 60

[[rules]]
name = "default_user"
server_group_ids = [8]
allowed_skills = [
  "poke_client",
  "send_private_msg",
  "send_channel_msg",
  "get_client_info",
  "list_clients",
  "get_server_info",
  "music_control"
]
can_target_admins = false
rate_limit_override = 20

[[rules]]
name = "default"
server_group_ids = []          # 空数组 = 捕获所有剩余情况
allowed_skills = ["music_control"]
can_target_admins = false

# 被视为“受管理员保护”的组 ID（can_target_admins = false 不能对这些组执行操作）
[acl]
protected_group_ids = [6, 8, 9]
"#;

impl AclConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = get_config_path(path)?;
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, DEFAULT_ACL_TOML)?;
            println!("Created default AclConfig at {:?}", path);
        }

        let content = std::fs::read_to_string(&path)?;
        let config: AclConfig = toml::from_str(&content)?;
        Ok(config)
    }
}

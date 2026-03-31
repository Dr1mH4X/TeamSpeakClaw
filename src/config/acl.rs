use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AclConfig {
    pub rules: Vec<AclRule>,
    pub acl: AclSettings,
}

impl Default for AclConfig {
    fn default() -> Self {
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
    pub channel_group_ids: Vec<u32>,
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

impl AclConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).context(format!(
            "ACL config file not found: {}. Please copy examples/config/acl.toml to config/",
            path.display()
        ))?;
        let config: AclConfig = toml::from_str(&content)?;
        Ok(config)
    }
}

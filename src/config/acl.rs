use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct AclConfig {
    pub rules: Vec<AclRule>,
    pub acl: AclSettings,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AclRule {
    pub server_group_ids: Vec<u32>,
    pub channel_group_ids: Vec<u32>,
    /// 工具白名单；配置键是 `allowed_tools`，旧的 `allowed_skills` 仍可解析
    #[serde(rename = "allowed_tools", alias = "allowed_skills")]
    pub allowed_tools: Vec<String>,
    pub can_target_admins: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AclSettings {
    pub protected_group_ids: Vec<u32>,
}

impl Default for AclSettings {
    fn default() -> Self {
        Self {
            protected_group_ids: vec![6, 7, 8],
        }
    }
}

impl AclConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let config: AclConfig = crate::config::load_toml(
            path,
            &format!(
                "ACL config file not found: {}. Please copy examples/config/acl.toml to config/",
                path.display()
            ),
        )?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_skills_is_accepted_as_the_legacy_key_for_allowed_tools() {
        let rule: AclRule = toml::from_str(
            r#"
            server_group_ids = [1]
            channel_group_ids = [2]
            allowed_skills = ["poke_client"]
            can_target_admins = false
            "#,
        )
        .unwrap();
        assert_eq!(rule.allowed_tools, vec!["poke_client"]);
    }

    #[test]
    fn example_acl_config_parses_with_the_shipped_key_names() {
        let raw = include_str!("../../examples/config/acl.toml");
        let config: AclConfig = toml::from_str(raw).unwrap();
        assert!(!config.rules.is_empty());
        assert!(config
            .rules
            .iter()
            .any(|rule| rule.allowed_tools.iter().any(|tool| tool == "*")));
    }
}

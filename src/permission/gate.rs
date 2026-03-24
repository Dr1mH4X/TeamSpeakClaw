use crate::config::AclConfig;
use anyhow::Result;
use tracing::debug;

pub struct PermissionGate {
    config: AclConfig,
}

impl PermissionGate {
    pub fn new(config: AclConfig) -> Self {
        Self { config }
    }
    pub fn check(&self, caller_groups: &[u32], skill_name: &str) -> Result<()> {
        // 从上到下遍历规则
        for rule in &self.config.rules {
            // 检查规则是否适用于调用者
            let match_group = if rule.server_group_ids.is_empty() {
                true // 空组列表匹配所有人（默认规则）
            } else {
                rule.server_group_ids
                    .iter()
                    .any(|gid| caller_groups.contains(gid))
            };

            if match_group {
                // 检查技能是否被允许
                let allowed = rule.allowed_skills.contains(&"*".to_string())
                    || rule.allowed_skills.iter().any(|s| s == skill_name);

                if allowed {
                    debug!(
                        "Access granted: Rule '{}' allows skill '{}'",
                        rule.name, skill_name
                    );
                    return Ok(());
                } else {
                    debug!(
                        "Access denied: Rule '{}' does not allow skill '{}'",
                        rule.name, skill_name
                    );
                    return Err(anyhow::anyhow!(
                        "Rule '{}' does not allow this skill",
                        rule.name
                    ));
                }
            }
        }

        // 拒绝
        Err(anyhow::anyhow!("No matching permission rule found"))
    }

    pub fn get_allowed_skills(&self, caller_groups: &[u32]) -> Vec<String> {
        let mut skills = Vec::new();
        for rule in &self.config.rules {
            let match_group = if rule.server_group_ids.is_empty() {
                true
            } else {
                rule.server_group_ids
                    .iter()
                    .any(|gid| caller_groups.contains(gid))
            };

            if match_group {
                if rule.allowed_skills.contains(&"*".to_string()) {
                    return vec!["*".to_string()];
                }
                skills.extend(rule.allowed_skills.clone());
            }
        }
        skills.sort();
        skills.dedup();
        skills
    }

    pub fn can_target(&self, caller_groups: &[u32], target_groups: &[u32]) -> bool {
        let is_protected = target_groups
            .iter()
            .any(|gid| self.config.acl.protected_group_ids.contains(gid));
        if !is_protected {
            return true;
        }

        for rule in &self.config.rules {
            let match_group = if rule.server_group_ids.is_empty() {
                true
            } else {
                rule.server_group_ids
                    .iter()
                    .any(|gid| caller_groups.contains(gid))
            };

            if match_group {
                return rule.can_target_admins;
            }
        }

        false
    }
}

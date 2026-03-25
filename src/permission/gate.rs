use crate::config::AclConfig;

pub struct PermissionGate {
    config: AclConfig,
}

impl PermissionGate {
    pub fn new(config: AclConfig) -> Self {
        Self { config }
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

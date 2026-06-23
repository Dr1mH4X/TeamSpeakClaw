use crate::config::AclConfig;
use crate::config::acl::AclRule;

pub struct PermissionGate {
    config: AclConfig,
}

impl PermissionGate {
    pub fn new(config: AclConfig) -> Self {
        Self { config }
    }

    fn matches_rule(&self, rule: &AclRule, caller_groups: &[u32], caller_channel_group_id: u32) -> bool {
        let match_server_group = if rule.server_group_ids.is_empty() {
            true
        } else {
            rule.server_group_ids
                .iter()
                .any(|gid| caller_groups.contains(gid))
        };

        let match_channel_group = if rule.channel_group_ids.is_empty() {
            true
        } else {
            rule.channel_group_ids.contains(&caller_channel_group_id)
        };

        match_server_group && match_channel_group
    }

    pub fn get_allowed_skills(
        &self,
        caller_groups: &[u32],
        caller_channel_group_id: u32,
    ) -> Vec<String> {
        let mut skills = Vec::new();
        for rule in &self.config.rules {
            if self.matches_rule(rule, caller_groups, caller_channel_group_id) {
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

    pub fn can_target(
        &self,
        caller_groups: &[u32],
        caller_channel_group_id: u32,
        target_groups: &[u32],
    ) -> bool {
        let is_protected = target_groups
            .iter()
            .any(|gid| self.config.acl.protected_group_ids.contains(gid));
        if !is_protected {
            return true;
        }

        for rule in &self.config.rules {
            if self.matches_rule(rule, caller_groups, caller_channel_group_id) {
                return rule.can_target_admins;
            }
        }

        false
    }
}

use crate::config::acl::AclRule;
use crate::config::AclConfig;

pub struct PermissionGate {
    config: AclConfig,
}

impl PermissionGate {
    pub fn new(config: AclConfig) -> Self {
        Self { config }
    }

    fn matches_rule(
        &self,
        rule: &AclRule,
        caller_groups: &[u32],
        caller_channel_group_id: u32,
    ) -> bool {
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
                if rule.allowed_skills.iter().any(|s| s == "*") {
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

        self.config.rules.iter().any(|rule| {
            rule.can_target_admins
                && self.matches_rule(rule, caller_groups, caller_channel_group_id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::acl::AclSettings;

    fn rule(name: &str, server_group_ids: Vec<u32>, can_target_admins: bool) -> AclRule {
        AclRule {
            name: name.to_string(),
            server_group_ids,
            channel_group_ids: Vec::new(),
            allowed_skills: Vec::new(),
            can_target_admins,
        }
    }

    #[test]
    fn target_permission_combines_all_matching_rules() {
        let gate = PermissionGate::new(AclConfig {
            rules: vec![
                rule("default", Vec::new(), false),
                rule("admin", vec![6], true),
            ],
            acl: AclSettings {
                protected_group_ids: vec![6],
            },
        });

        assert!(gate.can_target(&[6], 0, &[6]));
    }

    #[test]
    fn protected_target_is_denied_without_an_explicit_grant() {
        let gate = PermissionGate::new(AclConfig {
            rules: vec![rule("default", Vec::new(), false)],
            acl: AclSettings {
                protected_group_ids: vec![6],
            },
        });

        assert!(!gate.can_target(&[8], 0, &[6]));
    }

    #[test]
    fn nonzero_channel_group_controls_skills_and_protected_targets() {
        let gate = PermissionGate::new(AclConfig {
            rules: vec![AclRule {
                name: "channel-admin".to_string(),
                server_group_ids: Vec::new(),
                channel_group_ids: vec![42],
                allowed_skills: vec!["move_client".to_string()],
                can_target_admins: true,
            }],
            acl: AclSettings {
                protected_group_ids: vec![6],
            },
        });

        assert_eq!(
            gate.get_allowed_skills(&[], 42),
            vec!["move_client".to_string()]
        );
        assert!(gate.get_allowed_skills(&[], 0).is_empty());
        assert!(gate.can_target(&[], 42, &[6]));
        assert!(!gate.can_target(&[], 0, &[6]));
    }
}

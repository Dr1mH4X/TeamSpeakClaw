use crate::config::AclConfig;
use crate::error::{AppError, Result};
use tracing::debug;

pub struct PermissionGate {
    config: AclConfig,
}

impl PermissionGate {
    pub fn new(config: AclConfig) -> Self {
        Self { config }
    }

    /// Check if a user (with given server groups) can execute a skill.
    #[allow(dead_code)]
#[allow(dead_code)]
#[allow(dead_code)]
#[allow(dead_code)]
#[allow(dead_code)]
#[allow(dead_code)]
#[allow(dead_code)]
#[allow(dead_code)]
#[allow(dead_code)]
    pub fn check(&self, caller_groups: &[u32], skill_name: &str) -> Result<()> {
        // Iterate rules top-to-bottom
        for rule in &self.config.rules {
            // Check if rule applies to caller
            let match_group = if rule.server_group_ids.is_empty() {
                true // Empty group list matches everyone (default rule)
            } else {
                rule.server_group_ids
                    .iter()
                    .any(|gid| caller_groups.contains(gid))
            };

            if match_group {
                // Check if skill is allowed
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
                    return Err(AppError::PermissionDenied {
                        reason: format!("Rule '{}' does not allow this skill", rule.name),
                    });
                }
            }
        }

        // No rule matched? Deny by default.
        Err(AppError::PermissionDenied {
            reason: "No matching permission rule found".into(),
        })
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
        // Deduplicate
        skills.sort();
        skills.dedup();
        skills
    }

    #[allow(dead_code)]
#[allow(dead_code)]
#[allow(dead_code)]
#[allow(dead_code)]
#[allow(dead_code)]
#[allow(dead_code)]
#[allow(dead_code)]
#[allow(dead_code)]
#[allow(dead_code)]
    pub fn can_target(&self, caller_groups: &[u32], target_groups: &[u32]) -> bool {
        // Check if target is protected
        let is_protected = target_groups
            .iter()
            .any(|gid| self.config.acl.protected_group_ids.contains(gid));
        if !is_protected {
            return true;
        }

        // Target is protected. Check if caller can target admins.
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

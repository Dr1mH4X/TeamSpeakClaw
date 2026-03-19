use crate::config::AuditConfig;
use anyhow::Result;

pub struct AuditLog {
    enabled: bool,
}

impl AuditLog {
    pub fn new(config: &AuditConfig) -> Result<Self> {
        Ok(Self {
            enabled: config.enabled,
        })
    }
}

use crate::config::AppConfig;
use anyhow::Result;
use arc_swap::ArcSwap;
use std::sync::Arc;

pub struct TsAdapter {
    config: Arc<ArcSwap<AppConfig>>,
}

impl TsAdapter {
    pub async fn connect(config: Arc<ArcSwap<AppConfig>>) -> Result<Self> {
        Ok(Self { config })
    }

    pub async fn set_nickname(&self, name: &str) -> Result<()> {
        // TODO: Implement nickname setting
        let _ = name;
        Ok(())
    }
}

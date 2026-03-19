use crate::config::AppConfig;
use arc_swap::ArcSwap;
use std::sync::Arc;

pub struct LlmEngine {
    config: Arc<ArcSwap<AppConfig>>,
}

impl LlmEngine {
    pub fn new(config: Arc<ArcSwap<AppConfig>>) -> Self {
        Self { config }
    }
}

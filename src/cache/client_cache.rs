use crate::adapter::TsAdapter;
use crate::config::AppConfig;
use arc_swap::ArcSwap;
use std::sync::Arc;

pub struct ClientCache {
    config: Arc<ArcSwap<AppConfig>>,
}

impl ClientCache {
    pub fn new(config: Arc<ArcSwap<AppConfig>>) -> Self {
        Self { config }
    }

    pub async fn run_refresh_loop(&self, adapter: Arc<TsAdapter>) {
        // TODO: Implement loop
        let _ = adapter;
    }
}

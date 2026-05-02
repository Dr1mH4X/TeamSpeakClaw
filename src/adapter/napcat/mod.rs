pub mod api;
pub mod event;
pub mod types;
pub mod ws;

pub use ws::NapCatAdapter;

use crate::config::AppConfig;
use anyhow::Result;
use std::sync::Arc;

pub async fn connect_if_enabled(config: Arc<AppConfig>) -> Result<Option<Arc<NapCatAdapter>>> {
    if config.napcat.enabled {
        let nc = NapCatAdapter::connect(config.napcat.clone()).await?;
        Ok(Some(nc))
    } else {
        Ok(None)
    }
}

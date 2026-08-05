pub mod api;
pub mod event;
pub mod types;
pub mod ws;

pub(crate) use ws::ConnectionExit;
pub use ws::NapCatAdapter;

use crate::config::AppConfig;
use anyhow::Result;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub async fn connect_if_enabled(
    config: Arc<AppConfig>,
    shutdown: CancellationToken,
) -> Result<Option<Arc<NapCatAdapter>>> {
    if config.napcat.enabled {
        let nc = NapCatAdapter::connect(config.napcat.clone(), shutdown).await?;
        Ok(Some(nc))
    } else {
        Ok(None)
    }
}

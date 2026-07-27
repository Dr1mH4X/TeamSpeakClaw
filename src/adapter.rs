pub(crate) mod reconnect;

pub mod headless;
pub mod napcat;

// Re-export for backward compatibility
pub use headless::{TextMessageEvent, TextMessageTarget, TsAdapter, TsEvent};

use std::sync::Arc;

use anyhow::Result;
use tracing::{error, warn};

use crate::adapter::reconnect::{reconnect_delay_for_attempt, MAX_RECONNECT_ATTEMPTS};
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::router::EventRouter;
use crate::skills::SkillRegistry;

pub async fn run(
    config: Arc<AppConfig>,
    prompts: Arc<PromptsConfig>,
    gate: Arc<PermissionGate>,
    registry: Arc<SkillRegistry>,
    llm: Arc<LlmEngine>,
) -> Result<()> {
    for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
        match run_once(
            config.clone(),
            prompts.clone(),
            gate.clone(),
            registry.clone(),
            llm.clone(),
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                if attempt == MAX_RECONNECT_ATTEMPTS {
                    error!(
                        "All {MAX_RECONNECT_ATTEMPTS} reconnect attempts exhausted. Last error: {e}"
                    );
                    return Err(e);
                }
                let delay = reconnect_delay_for_attempt(attempt);
                warn!(
                    "Reconnect attempt {}/{} failed, retrying after {:.0?}",
                    attempt, MAX_RECONNECT_ATTEMPTS, delay
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
    unreachable!()
}

async fn run_once(
    config: Arc<AppConfig>,
    prompts: Arc<PromptsConfig>,
    gate: Arc<PermissionGate>,
    registry: Arc<SkillRegistry>,
    llm: Arc<LlmEngine>,
) -> Result<()> {
    let adapter = TsAdapter::connect(config.clone()).await?;

    let nc_adapter = napcat::connect_if_enabled(config.clone()).await?;

    let ts_router = EventRouter::new_with_clients(
        config.clone(),
        prompts.clone(),
        adapter.clone(),
        gate.clone(),
        llm.clone(),
        registry.clone(),
        nc_adapter.clone(),
    );

    let headless_runtime = headless::Runtime::start(
        config.clone(),
        prompts.clone(),
        gate.clone(),
        llm.clone(),
        registry.clone(),
        adapter.clone(),
    );

    let result = crate::router::run_routers(
        config,
        prompts,
        gate,
        llm,
        registry,
        adapter.clone(),
        ts_router,
        nc_adapter,
    )
    .await;

    headless_runtime.shutdown().await;

    if let Err(e) = adapter.quit().await {
        error!("Failed to send quit command: {}", e);
    }

    result
}

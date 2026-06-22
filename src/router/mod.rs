mod voice_router;
mod nc_router;
mod ts_router;
mod unified;

pub use voice_router::VoiceRouter;
pub use nc_router::NcRouter;
pub use ts_router::{ClientInfo, EventRouter};
pub use unified::{ReplyPolicy, UnifiedInboundEvent};

use std::sync::Arc;

use anyhow::Result;
use tracing::{error, info, warn};

use crate::adapter::napcat::NapCatAdapter;
use crate::adapter::TsAdapter;
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::skills::SkillRegistry;

pub async fn run_routers(
    config: Arc<AppConfig>,
    prompts: Arc<PromptsConfig>,
    gate: Arc<PermissionGate>,
    llm: Arc<LlmEngine>,
    registry: Arc<SkillRegistry>,
    adapter: Arc<TsAdapter>,
    ts_router: EventRouter,
    nc_adapter: Option<Arc<NapCatAdapter>>,
) -> Result<()> {
    let sigterm = wait_for_sigterm();

    if let Some(nc_adapter) = nc_adapter {
        let bot_clid = adapter.get_bot_clid();
        let nc_router = NcRouter::new_with_ts(
            config,
            prompts,
            nc_adapter,
            gate,
            llm,
            registry,
            Some(adapter),
            Some(ts_router.clients.clone()),
        );
        let nc_future = tokio::spawn(async move { nc_router.run().await });

        info!("Bot ready (clid={}). Listening for TS + NapCat events.", bot_clid);

        tokio::select! {
            res = ts_router.run() => map_ts_router_result(res),
            res = nc_future => map_nc_router_result(res),
            _ = tokio::signal::ctrl_c() => {
                Ok(())
            }
            _ = sigterm => {
                Ok(())
            }
        }
    } else {
        info!("NapCat adapter disabled, running in TeamSpeak-only mode");
        info!("Bot ready (clid={}). Listening for TeamSpeak events.", adapter.get_bot_clid());

        tokio::select! {
            res = ts_router.run() => map_ts_router_result(res),
            _ = tokio::signal::ctrl_c() => {
                Ok(())
            }
            _ = sigterm => {
                Ok(())
            }
        }
    }
}

fn map_ts_router_result(res: Result<()>) -> Result<()> {
    match res {
        Ok(()) => {
            warn!("TS Event router exited unexpectedly");
            Err(anyhow::anyhow!("TS Event router exited unexpectedly"))
        }
        Err(e) => {
            error!("TS Event router exited with error: {}", e);
            Err(e)
        }
    }
}

fn map_nc_router_result(res: Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    match res {
        Ok(Ok(())) => {
            warn!("NC router exited unexpectedly");
            Err(anyhow::anyhow!("NC router exited unexpectedly"))
        }
        Ok(Err(e)) => {
            error!("NC router error: {e}");
            Err(e)
        }
        Err(e) => {
            error!("NC router task panicked: {e}");
            Err(anyhow::anyhow!("NC router panicked"))
        }
    }
}

#[cfg(unix)]
async fn wait_for_sigterm() {
    use tokio::signal::unix::SignalKind;

    let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())
        .expect("Failed to register SIGTERM handler");
    sigterm.recv().await;
}

#[cfg(not(unix))]
async fn wait_for_sigterm() {
    // Windows doesn't have SIGTERM; this future never resolves.
    // Docker on Windows uses different termination mechanisms
    // and ctrl_c() is sufficient.
    std::future::pending::<()>().await;
}

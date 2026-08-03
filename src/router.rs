mod nc_router;
mod trigger;
mod ts_router;
mod unified;
mod voice_router;

pub use nc_router::NcRouter;
pub use trigger::strip_trigger_prefix;
pub use ts_router::EventRouter;
pub use unified::{ReplyPolicy, UnifiedInboundEvent};
pub use voice_router::VoiceRouter;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use self::ts_router::TsRouterExit;
use crate::adapter::napcat::NapCatAdapter;
use crate::adapter::TsAdapter;
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::skills::SkillRegistry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouterExit {
    Shutdown,
    TeamSpeakDisconnected,
}

#[derive(Clone)]
pub(crate) struct RouterContext {
    pub(crate) config: Arc<AppConfig>,
    pub(crate) prompts: Arc<PromptsConfig>,
    pub(crate) gate: Arc<PermissionGate>,
    pub(crate) llm: Arc<LlmEngine>,
    pub(crate) registry: Arc<SkillRegistry>,
}

impl RouterContext {
    pub(crate) fn new(
        config: Arc<AppConfig>,
        prompts: Arc<PromptsConfig>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
    ) -> Self {
        Self {
            config,
            prompts,
            gate,
            llm,
            registry,
        }
    }
}

pub(crate) async fn run_routers(
    context: RouterContext,
    adapter: Arc<TsAdapter>,
    ts_router: EventRouter,
    nc_adapter: Option<Arc<NapCatAdapter>>,
    shutdown: CancellationToken,
) -> Result<RouterExit> {
    let RouterContext {
        config,
        prompts,
        gate,
        llm,
        registry,
    } = context;
    let napcat_enabled = nc_adapter.is_some();
    if !napcat_enabled {
        info!("NapCat adapter disabled, running in TeamSpeak-only mode");
    }

    let ts_router_run = ts_router.run();
    tokio::pin!(ts_router_run);
    let bot_clid = adapter.get_bot_clid();
    let bot_ctx = match wait_for_ready_or_router(
        describe_bot(&adapter, bot_clid, napcat_enabled),
        ts_router_run.as_mut(),
        &shutdown,
    )
    .await
    {
        ReadyWait::Ready(context) => context,
        ReadyWait::Router(result) => return map_ts_router_result(result),
        ReadyWait::Shutdown => return Ok(RouterExit::Shutdown),
    };
    info!("{bot_ctx}");

    if let Some(nc_adapter) = nc_adapter {
        let nc_router = NcRouter::new_with_ts(
            config,
            prompts,
            nc_adapter,
            gate,
            llm,
            registry,
            Some(adapter),
        );

        tokio::select! {
            biased;
            _ = shutdown.cancelled() => Ok(RouterExit::Shutdown),
            res = &mut ts_router_run => map_ts_router_result(res),
            res = nc_router.run() => map_nc_router_result(res),
        }
    } else {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => Ok(RouterExit::Shutdown),
            res = &mut ts_router_run => map_ts_router_result(res),
        }
    }
}

enum ReadyWait<T, R> {
    Ready(T),
    Router(R),
    Shutdown,
}

async fn wait_for_ready_or_router<ReadyFuture, RouterFuture>(
    ready: ReadyFuture,
    mut router: Pin<&mut RouterFuture>,
    shutdown: &CancellationToken,
) -> ReadyWait<ReadyFuture::Output, RouterFuture::Output>
where
    ReadyFuture: Future,
    RouterFuture: Future,
{
    tokio::pin!(ready);
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => ReadyWait::Shutdown,
        result = router.as_mut() => ReadyWait::Router(result),
        result = &mut ready => ReadyWait::Ready(result),
    }
}

async fn describe_bot(adapter: &TsAdapter, bot_clid: u32, napcat_enabled: bool) -> String {
    let event_sources = if napcat_enabled {
        "TS + NapCat"
    } else {
        "TeamSpeak"
    };

    match adapter.list_clients().await {
        Ok(clients) => {
            if let Some(bot) = clients.iter().find(|client| client.id as u32 == bot_clid) {
                format!(
                    "Bot ready: {}({})[{}]. Listening for {event_sources} events.",
                    bot.nickname, bot.id, bot.channel_id
                )
            } else {
                format!("Bot ready (clid={bot_clid}). Listening for {event_sources} events.")
            }
        }
        Err(_) => format!("Bot ready (clid={bot_clid}). Listening for {event_sources} events."),
    }
}

fn map_ts_router_result(res: Result<TsRouterExit>) -> Result<RouterExit> {
    match res {
        Ok(TsRouterExit::Disconnected) => {
            warn!("TeamSpeak connection lost");
            Ok(RouterExit::TeamSpeakDisconnected)
        }
        Err(e) => {
            error!("TS Event router exited with error: {}", e);
            Err(e)
        }
    }
}

fn map_nc_router_result(res: Result<()>) -> Result<RouterExit> {
    match res {
        Ok(()) => {
            warn!("NC router exited unexpectedly");
            Err(anyhow::anyhow!("NC router exited unexpectedly"))
        }
        Err(e) => {
            error!("NC router error: {e}");
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        map_ts_router_result, wait_for_ready_or_router, ReadyWait, RouterExit, TsRouterExit,
    };
    use std::future::pending;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn disconnected_router_has_explicit_exit_reason() {
        assert_eq!(
            map_ts_router_result(Ok(TsRouterExit::Disconnected)).unwrap(),
            RouterExit::TeamSpeakDisconnected
        );
    }

    #[tokio::test]
    async fn router_exit_interrupts_stalled_ready_query() {
        let mut router = Box::pin(async { TsRouterExit::Disconnected });

        let outcome =
            wait_for_ready_or_router(pending::<()>(), router.as_mut(), &CancellationToken::new())
                .await;

        assert!(matches!(
            outcome,
            ReadyWait::Router(TsRouterExit::Disconnected)
        ));
    }

    #[tokio::test]
    async fn shutdown_interrupts_stalled_ready_query() {
        let mut router = Box::pin(pending::<TsRouterExit>());
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let outcome = wait_for_ready_or_router(pending::<()>(), router.as_mut(), &shutdown).await;

        assert!(matches!(outcome, ReadyWait::Shutdown));
    }
}

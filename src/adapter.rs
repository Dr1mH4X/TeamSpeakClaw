pub(crate) mod reconnect;

pub mod headless;
pub mod napcat;

// Re-export for backward compatibility
pub use headless::{TextMessageEvent, TextMessageTarget, TsAdapter, TsEvent};

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::adapter::reconnect::{
    wait_for_retry, ReconnectState, RetryDecision, MAX_RECONNECT_ATTEMPTS,
};
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::router::{EventRouter, RouterContext, RouterExit};
use crate::skills::SkillRegistry;

const COMPONENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run(
    config: Arc<AppConfig>,
    prompts: Arc<PromptsConfig>,
    gate: Arc<PermissionGate>,
    registry: Arc<SkillRegistry>,
    llm: Arc<LlmEngine>,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut reconnect = ReconnectState::default();
    let context = RouterContext::new(config, prompts, gate, llm, registry);

    loop {
        let connection = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(()),
            result = TsAdapter::connect(context.config.clone()) => result,
        };

        let (adapter, event_rx, disconnect_rx) = match connection {
            Ok(adapter) => {
                let (event_rx, disconnect_rx) = adapter.take_main_subscriptions()?;
                (adapter, event_rx, disconnect_rx)
            }
            Err(error) => {
                let had_running_session = reconnect.has_started_session();
                match reconnect.record_failure() {
                    RetryDecision::Exhausted => {
                        error!(
                            "All {MAX_RECONNECT_ATTEMPTS} initial connection attempts exhausted. Last error: {error}"
                        );
                        return Err(error);
                    }
                    RetryDecision::Retry { attempt, delay } => {
                        if had_running_session {
                            warn!(
                                "TeamSpeak reconnect attempt {attempt} failed: {error}; retrying after {:.0?}",
                                delay
                            );
                        } else {
                            warn!(
                                "Initial TeamSpeak connection attempt {attempt}/{MAX_RECONNECT_ATTEMPTS} failed: {error}; retrying after {:.0?}",
                                delay
                            );
                        }
                        if !wait_for_retry(delay, &shutdown).await {
                            return Ok(());
                        }
                        continue;
                    }
                }
            }
        };

        let session = run_connected_session(
            context.clone(),
            adapter,
            event_rx,
            disconnect_rx,
            shutdown.clone(),
        )
        .await;

        let entered_running = session.entered_running;
        if entered_running {
            reconnect.record_session_started();
        }

        let failure = match session.result {
            Ok(RouterExit::Shutdown) => return Ok(()),
            Ok(RouterExit::TeamSpeakDisconnected) => {
                warn!("TeamSpeak session disconnected; reconnecting");
                anyhow::anyhow!("TeamSpeak session disconnected")
            }
            Err(error) => {
                if entered_running {
                    warn!("Running session failed: {error}; reconnecting");
                } else {
                    warn!("Session initialization failed: {error}; retrying");
                }
                error
            }
        };

        let had_running_session = reconnect.has_started_session();
        let (attempt, delay) = match reconnect.record_failure() {
            RetryDecision::Retry { attempt, delay } => (attempt, delay),
            RetryDecision::Exhausted => {
                error!(
                    "All {MAX_RECONNECT_ATTEMPTS} initial startup attempts exhausted. Last error: {failure}"
                );
                return Err(failure);
            }
        };
        if had_running_session {
            warn!(
                "TeamSpeak reconnect attempt {attempt} scheduled after {:.0?}",
                delay
            );
        } else {
            warn!(
                "Initial startup attempt {attempt}/{MAX_RECONNECT_ATTEMPTS} failed: {failure}; retrying after {:.0?}",
                delay
            );
        }
        if !wait_for_retry(delay, &shutdown).await {
            return Ok(());
        }
    }
}

async fn run_connected_session(
    context: RouterContext,
    adapter: Arc<TsAdapter>,
    event_rx: broadcast::Receiver<TsEvent>,
    mut disconnect_rx: watch::Receiver<bool>,
    shutdown: CancellationToken,
) -> SessionCompletion {
    let napcat_shutdown = shutdown.child_token();
    let nc_adapter = match wait_for_initialization(
        napcat::connect_if_enabled(context.config.clone(), napcat_shutdown.clone()),
        &mut disconnect_rx,
        &shutdown,
    )
    .await
    {
        Ok(InitializationWait::Completed(Ok(adapter))) => adapter,
        Ok(InitializationWait::Completed(Err(error))) => {
            napcat_shutdown.cancel();
            disconnect_adapter(&adapter).await;
            return SessionCompletion::initialization(Err(error));
        }
        Ok(InitializationWait::Shutdown) => {
            napcat_shutdown.cancel();
            disconnect_adapter(&adapter).await;
            return SessionCompletion::initialization(Ok(RouterExit::Shutdown));
        }
        Ok(InitializationWait::Disconnected) => {
            napcat_shutdown.cancel();
            disconnect_adapter(&adapter).await;
            return SessionCompletion::initialization(Ok(RouterExit::TeamSpeakDisconnected));
        }
        Err(error) => {
            napcat_shutdown.cancel();
            disconnect_adapter(&adapter).await;
            return SessionCompletion::initialization(Err(error));
        }
    };

    let voice_bridge_state = headless::VoiceBridgeState::default();
    let ts_router = EventRouter::new_with_clients(
        context.clone(),
        adapter.clone(),
        event_rx,
        disconnect_rx,
        nc_adapter.clone(),
        voice_bridge_state.clone(),
    );

    let headless_runtime = match headless::Runtime::start(
        context.config.clone(),
        context.prompts.clone(),
        context.gate.clone(),
        context.llm.clone(),
        context.registry.clone(),
        adapter.clone(),
        voice_bridge_state,
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(error) => {
            error!("Headless runtime failed to start: {error}");
            napcat_shutdown.cancel();
            disconnect_adapter(&adapter).await;
            return SessionCompletion::initialization(Err(error));
        }
    };

    // 路由期间同时观察 NapCat supervisor 与 headless 组件的异常退出
    let nc_supervisor_status = nc_adapter
        .as_ref()
        .map(|adapter| adapter.supervisor_status());
    let routers = crate::router::run_routers(
        context,
        adapter.clone(),
        ts_router,
        nc_adapter.clone(),
        shutdown,
    );
    tokio::pin!(routers);

    let result = tokio::select! {
        biased;
        result = &mut routers => result,
        nc_exit = wait_nc_supervisor_failure(nc_supervisor_status) => {
            warn!("NapCat supervisor exited unexpectedly ({nc_exit:?}); ending session to trigger reconnection");
            Ok(RouterExit::TeamSpeakDisconnected)
        }
        component = wait_headless_failure(headless_runtime.failure_receiver()) => {
            error!("Headless component {component} failed; ending session to trigger reconnection");
            Err(anyhow::anyhow!("headless component {component} failed"))
        }
    };

    shutdown_napcat(nc_adapter.as_deref(), &napcat_shutdown).await;
    headless_runtime.shutdown().await;
    disconnect_adapter(&adapter).await;

    SessionCompletion::running(result)
}

/// 等待 NapCat supervisor 异常退出信号；NapCat 禁用时永不触发
async fn wait_nc_supervisor_failure(
    status: Option<watch::Receiver<Option<napcat::ConnectionExit>>>,
) -> napcat::ConnectionExit {
    let Some(mut status) = status else {
        return std::future::pending::<napcat::ConnectionExit>().await;
    };
    loop {
        if let Some(exit) = *status.borrow() {
            return exit;
        }
        match status.changed().await {
            Ok(()) => {}
            // 通道关闭（sender 随 adapter 销毁）视为异常退出，fail-closed
            Err(_) => return napcat::ConnectionExit::Disconnected,
        }
    }
}

/// 等待 headless 组件失败信号；正常状态或通道关闭视为无失败
async fn wait_headless_failure(mut status: watch::Receiver<Option<&'static str>>) -> &'static str {
    loop {
        if let Some(component) = *status.borrow() {
            return component;
        }
        match status.changed().await {
            Ok(()) => {}
            Err(_) => return "headless runtime",
        }
    }
}

struct SessionCompletion {
    entered_running: bool,
    result: Result<RouterExit>,
}

impl SessionCompletion {
    fn initialization(result: Result<RouterExit>) -> Self {
        Self {
            entered_running: false,
            result,
        }
    }

    fn running(result: Result<RouterExit>) -> Self {
        Self {
            entered_running: true,
            result,
        }
    }
}

enum InitializationWait<T> {
    Completed(T),
    Shutdown,
    Disconnected,
}

async fn wait_for_initialization<F, T>(
    future: F,
    disconnect_rx: &mut watch::Receiver<bool>,
    shutdown: &CancellationToken,
) -> Result<InitializationWait<T>>
where
    F: Future<Output = T>,
{
    tokio::pin!(future);

    loop {
        if *disconnect_rx.borrow() {
            return Ok(InitializationWait::Disconnected);
        }

        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(InitializationWait::Shutdown),
            changed = disconnect_rx.changed() => {
                changed.map_err(|_| anyhow::anyhow!("TS connection state stream closed during initialization"))?;
                if *disconnect_rx.borrow_and_update() {
                    return Ok(InitializationWait::Disconnected);
                }
            }
            output = &mut future => return Ok(InitializationWait::Completed(output)),
        }
    }
}

async fn shutdown_napcat(adapter: Option<&napcat::NapCatAdapter>, shutdown: &CancellationToken) {
    shutdown.cancel();
    let Some(adapter) = adapter else {
        return;
    };

    if matches!(
        wait_with_timeout(adapter.shutdown(), COMPONENT_SHUTDOWN_TIMEOUT).await,
        TimedWait::TimedOut
    ) {
        warn!(
            timeout_secs = COMPONENT_SHUTDOWN_TIMEOUT.as_secs(),
            "Timed out while shutting down NapCat adapter"
        );
    }
}

async fn disconnect_adapter(adapter: &TsAdapter) {
    match wait_with_timeout(adapter.quit(), COMPONENT_SHUTDOWN_TIMEOUT).await {
        TimedWait::Completed(Ok(())) => {}
        TimedWait::Completed(Err(error)) => {
            error!("Failed to send quit command: {error}");
        }
        TimedWait::TimedOut => {
            warn!(
                timeout_secs = COMPONENT_SHUTDOWN_TIMEOUT.as_secs(),
                "Timed out while disconnecting TeamSpeak adapter"
            );
        }
    }
}

enum TimedWait<T> {
    Completed(T),
    TimedOut,
}

async fn wait_with_timeout<F, T>(future: F, timeout: Duration) -> TimedWait<T>
where
    F: Future<Output = T>,
{
    match tokio::time::timeout(timeout, future).await {
        Ok(output) => TimedWait::Completed(output),
        Err(_) => TimedWait::TimedOut,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        wait_for_initialization, wait_headless_failure, wait_nc_supervisor_failure,
        wait_with_timeout, InitializationWait, SessionCompletion, TimedWait,
    };
    use crate::adapter::napcat::ConnectionExit;
    use crate::router::RouterExit;
    use std::future::pending;
    use std::time::Duration;
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn initialization_observes_disconnect_before_first_poll() {
        let (disconnect_tx, mut disconnect_rx) = watch::channel(false);
        disconnect_tx.send_replace(true);

        let outcome = wait_for_initialization(
            pending::<()>(),
            &mut disconnect_rx,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(matches!(outcome, InitializationWait::Disconnected));
    }

    #[tokio::test]
    async fn initialization_observes_root_shutdown() {
        let (_disconnect_tx, mut disconnect_rx) = watch::channel(false);
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let outcome = wait_for_initialization(pending::<()>(), &mut disconnect_rx, &shutdown)
            .await
            .unwrap();

        assert!(matches!(outcome, InitializationWait::Shutdown));
    }

    #[test]
    fn only_running_completion_marks_session_started() {
        let initialization = SessionCompletion::initialization(Err(anyhow::anyhow!("failed")));
        let running = SessionCompletion::running(Ok(RouterExit::TeamSpeakDisconnected));

        assert!(!initialization.entered_running);
        assert!(running.entered_running);
    }

    #[tokio::test]
    async fn timed_wait_stops_a_stalled_quit() {
        let outcome = wait_with_timeout(pending::<()>(), Duration::from_millis(1)).await;

        assert!(matches!(outcome, TimedWait::TimedOut));
    }

    #[tokio::test]
    async fn nc_supervisor_abnormal_signal_wakes_observation() {
        let (tx, rx) = watch::channel(None);
        tx.send(Some(ConnectionExit::Disconnected)).unwrap();

        let exit = wait_nc_supervisor_failure(Some(rx)).await;

        assert_eq!(exit, ConnectionExit::Disconnected);
    }

    #[tokio::test]
    async fn nc_supervisor_clean_shutdown_never_wakes_observation() {
        let (tx, rx) = watch::channel(None);
        let observation = tokio::spawn(wait_nc_supervisor_failure(Some(rx)));

        tx.send(None).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(!observation.is_finished());
        observation.abort();
    }

    #[tokio::test]
    async fn nc_supervisor_observation_is_inert_when_napcat_disabled() {
        let observation = tokio::spawn(wait_nc_supervisor_failure(None));

        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(!observation.is_finished());
        observation.abort();
    }

    #[tokio::test]
    async fn headless_component_failure_wakes_observation() {
        let (tx, rx) = watch::channel(None);
        tx.send(Some("headless service")).unwrap();

        let component = wait_headless_failure(rx).await;

        assert_eq!(component, "headless service");
    }
}

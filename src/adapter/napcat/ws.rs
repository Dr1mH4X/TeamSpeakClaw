use super::{
    api::action_get_login_info,
    event::{parse_event, NcEvent},
    types::{NcAction, NcApiResponse, Segment},
};
use crate::{
    adapter::reconnect::{wait_for_retry, ReconnectState, RetryDecision, MAX_RECONNECT_ATTEMPTS},
    config::NapCatConfig,
};
use anyhow::{anyhow, Context as _, Result};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::error::Error as StdError;
use std::future::Future;
use std::sync::{
    atomic::{AtomicI64, AtomicU64, Ordering},
    Arc, Weak,
};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{
        client::IntoClientRequest,
        http::header::{HeaderValue, AUTHORIZATION},
        Message,
    },
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

type WsConnection =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsSink = futures_util::stream::SplitSink<WsConnection, Message>;
type WsStream = futures_util::stream::SplitStream<WsConnection>;
const WS_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionExit {
    Cancelled,
    Disconnected,
}

/// 将 connection_loop 的失败标记映射为 supervisor 退出信号：正常退出为 None，异常退出为 Some
fn supervisor_exit_signal(failed: bool) -> Option<ConnectionExit> {
    failed.then_some(ConnectionExit::Disconnected)
}

fn parse_ws_url(value: &str) -> Result<url::Url> {
    url::Url::parse(value).map_err(|_| anyhow!("Invalid NapCat WebSocket URL"))
}

fn close_pending_requests(pending: &DashMap<String, oneshot::Sender<NcApiResponse>>) {
    pending.clear();
}

fn claim_disconnect_signal(last_generation: &AtomicU64, generation: u64) -> bool {
    last_generation.fetch_max(generation, Ordering::AcqRel) < generation
}

async fn wait_for_ws_write<F, E>(
    shutdown: &CancellationToken,
    timeout: Duration,
    write: F,
    operation: &'static str,
) -> Result<()>
where
    F: Future<Output = std::result::Result<(), E>>,
    E: StdError + Send + Sync + 'static,
{
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => Err(anyhow!("NapCat connection cancelled")),
        result = tokio::time::timeout(timeout, write) => {
            match result {
                Ok(result) => result.context(operation),
                Err(_) => Err(anyhow!(
                    "{operation} timed out after {} seconds",
                    timeout.as_secs()
                )),
            }
        }
    }
}

pub struct NapCatAdapter {
    writer: Mutex<Option<WsSink>>,
    event_tx: broadcast::Sender<NcEvent>,
    pending: DashMap<String, oneshot::Sender<NcApiResponse>>,
    self_id: AtomicI64,
    reconnect_tx: mpsc::UnboundedSender<u64>,
    config: NapCatConfig,
    shutdown: CancellationToken,
    active_generation: AtomicU64,
    last_signaled_generation: AtomicU64,
    runtime_task: Mutex<Option<JoinHandle<()>>>,
    /// supervisor 退出状态通道：None 表示正常（运行中或正常关闭），Some 表示异常退出
    supervisor_tx: watch::Sender<Option<ConnectionExit>>,
}

impl NapCatAdapter {
    pub async fn connect(config: NapCatConfig, shutdown: CancellationToken) -> Result<Arc<Self>> {
        let shutdown = shutdown.child_token();
        let mut retry = ReconnectState::default();

        loop {
            let result = tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    return Err(anyhow!("NapCat connection cancelled"));
                }
                result = Self::try_connect(config.clone(), shutdown.clone()) => result,
            };

            match result {
                Ok(adapter) => return Ok(adapter),
                Err(error) => match retry.record_failure() {
                    RetryDecision::Retry { attempt, delay } => {
                        warn!(
                            "[{attempt}/{MAX_RECONNECT_ATTEMPTS}] NapCat connect failed: {error}; retrying after {:.0?}",
                            delay
                        );
                        if !wait_for_retry(delay, &shutdown).await {
                            return Err(anyhow!("NapCat connection cancelled"));
                        }
                    }
                    RetryDecision::Exhausted => {
                        return Err(error.context(format!(
                            "NapCat: max reconnect attempts reached ({MAX_RECONNECT_ATTEMPTS})"
                        )));
                    }
                },
            }
        }
    }

    async fn try_connect(config: NapCatConfig, shutdown: CancellationToken) -> Result<Arc<Self>> {
        let ws_stream = Self::handshake(&config, &shutdown).await?;
        let (sink, stream) = ws_stream.split();
        let (event_tx, _) = broadcast::channel::<NcEvent>(256);
        let (reconnect_tx, reconnect_rx) = mpsc::unbounded_channel::<u64>();
        let (supervisor_tx, _) = watch::channel(None);

        let adapter = Arc::new(Self {
            writer: Mutex::new(Some(sink)),
            event_tx,
            pending: DashMap::new(),
            self_id: AtomicI64::new(0),
            reconnect_tx,
            config,
            shutdown: shutdown.clone(),
            active_generation: AtomicU64::new(1),
            last_signaled_generation: AtomicU64::new(0),
            runtime_task: Mutex::new(None),
            supervisor_tx,
        });

        let weak = Arc::downgrade(&adapter);
        let runtime_task = tokio::spawn(Self::connection_loop(
            weak,
            stream,
            reconnect_rx,
            1,
            shutdown,
        ));
        *adapter.runtime_task.lock().await = Some(runtime_task);

        Self::fetch_self_id(&adapter).await;
        Ok(adapter)
    }

    async fn connection_loop(
        weak: Weak<NapCatAdapter>,
        mut stream: WsStream,
        mut reconnect_rx: mpsc::UnboundedReceiver<u64>,
        mut generation: u64,
        shutdown: CancellationToken,
    ) {
        let mut retry = ReconnectState::default();
        retry.record_session_started();
        // 标记 supervisor 是否异常退出（重连耗尽/状态失败），用于向上层上报
        let mut failed = false;

        'runtime: loop {
            match Self::reader_loop(&weak, &mut stream, &mut reconnect_rx, generation, &shutdown)
                .await
            {
                ConnectionExit::Cancelled => break,
                ConnectionExit::Disconnected => {}
            }
            drop(stream);

            let Some(adapter) = weak.upgrade() else {
                return;
            };
            adapter.mark_disconnected(generation).await;
            drop(adapter);

            info!("NapCat reconnecting...");
            loop {
                let Some(adapter) = weak.upgrade() else {
                    return;
                };
                let config = adapter.config.clone();
                drop(adapter);

                match Self::handshake(&config, &shutdown).await {
                    Ok(ws_stream) => {
                        let (sink, next_stream) = ws_stream.split();
                        let Some(adapter) = weak.upgrade() else {
                            return;
                        };
                        generation = match adapter.install_connection(sink).await {
                            Ok(generation) => generation,
                            Err(error) => {
                                error!("NapCat reconnect state failed: {error}");
                                failed = true;
                                break 'runtime;
                            }
                        };
                        drop(adapter);
                        stream = next_stream;
                        retry.record_session_started();
                        info!("NapCat reconnected");
                        break;
                    }
                    Err(_) if shutdown.is_cancelled() => break 'runtime,
                    Err(error) => match retry.record_failure() {
                        RetryDecision::Retry { attempt, delay } => {
                            warn!(
                                "NapCat reconnect attempt {attempt} failed: {error}; retrying after {:.0?}",
                                delay
                            );
                            if !wait_for_retry(delay, &shutdown).await {
                                break 'runtime;
                            }
                        }
                        RetryDecision::Exhausted => {
                            error!("NapCat runtime reconnect state exhausted unexpectedly");
                            failed = true;
                            break 'runtime;
                        }
                    },
                }
            }
        }

        if let Some(adapter) = weak.upgrade() {
            adapter.mark_disconnected(generation).await;
            adapter.report_supervisor_exit(failed);
        }
    }

    async fn reader_loop(
        weak: &Weak<NapCatAdapter>,
        stream: &mut WsStream,
        reconnect_rx: &mut mpsc::UnboundedReceiver<u64>,
        generation: u64,
        shutdown: &CancellationToken,
    ) -> ConnectionExit {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => return ConnectionExit::Cancelled,
                signal = reconnect_rx.recv() => {
                    match signal {
                        Some(signaled_generation) if signaled_generation == generation => {
                            return ConnectionExit::Disconnected;
                        }
                        Some(_) => continue,
                        None => return ConnectionExit::Cancelled,
                    }
                }
                message = stream.next() => {
                    match message {
                        Some(Ok(Message::Text(text))) => {
                            let Some(adapter) = weak.upgrade() else {
                                return ConnectionExit::Cancelled;
                            };
                            adapter.handle_text_message(text.as_ref());
                        }
                        Some(Ok(Message::Close(_))) => {
                            error!("NC connection closed by remote");
                            return ConnectionExit::Disconnected;
                        }
                        Some(Ok(Message::Ping(data))) => {
                            let Some(adapter) = weak.upgrade() else {
                                return ConnectionExit::Cancelled;
                            };
                            if let Err(error) = adapter
                                .send_control_message(generation, Message::Pong(data))
                                .await
                            {
                                if shutdown.is_cancelled() {
                                    return ConnectionExit::Cancelled;
                                }
                                error!("NC pong write failed: {error}");
                                return ConnectionExit::Disconnected;
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(error)) => {
                            error!("NC read error: {error}");
                            return ConnectionExit::Disconnected;
                        }
                        None => {
                            error!("NC reader stream ended");
                            return ConnectionExit::Disconnected;
                        }
                    }
                }
            }
        }
    }

    fn handle_text_message(&self, text: &str) {
        debug!("NC << {text}");
        let val: Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(error) => {
                warn!("NC parse error: {error}");
                return;
            }
        };

        if val.get("retcode").is_some() || val.get("status").is_some() {
            let echo = val["echo"].as_str().unwrap_or("").to_string();
            if !echo.is_empty() {
                if let Some((_, tx)) = self.pending.remove(&echo) {
                    let response = serde_json::from_value(val).unwrap_or_else(|_| NcApiResponse {
                        status: "failed".into(),
                        retcode: -1,
                        data: Value::Null,
                        message: Some("parse error".into()),
                    });
                    let _ = tx.send(response);
                }
                return;
            }
        }

        let event = parse_event(val);
        if let Err(error) = self.event_tx.send(event) {
            debug!("No NC event subscribers: {error}");
        }
    }

    async fn send_control_message(&self, generation: u64, message: Message) -> Result<()> {
        let result = {
            let mut writer = self.writer.lock().await;
            if self.active_generation.load(Ordering::Acquire) != generation {
                return Err(anyhow!("NapCat connection generation changed"));
            }
            let sink = writer
                .as_mut()
                .ok_or_else(|| anyhow!("NapCat WebSocket not connected"))?;

            let result = wait_for_ws_write(
                &self.shutdown,
                WS_WRITE_TIMEOUT,
                sink.send(message),
                "NapCat WS control write failed",
            )
            .await;
            if result.is_err() {
                *writer = None;
            }
            result
        };

        if result.is_err() {
            close_pending_requests(&self.pending);
        }
        result
    }

    async fn install_connection(&self, sink: WsSink) -> Result<u64> {
        let mut writer = self.writer.lock().await;
        let generation = self
            .active_generation
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or_else(|| anyhow!("NapCat connection generation overflowed"))?;
        self.active_generation.store(generation, Ordering::Release);
        *writer = Some(sink);
        Ok(generation)
    }

    async fn mark_disconnected(&self, generation: u64) {
        let disconnected = {
            let mut writer = self.writer.lock().await;
            if self.active_generation.load(Ordering::Acquire) != generation {
                false
            } else {
                *writer = None;
                true
            }
        };
        if disconnected {
            close_pending_requests(&self.pending);
        }
    }

    fn signal_disconnect(&self, generation: u64) {
        if claim_disconnect_signal(&self.last_signaled_generation, generation) {
            let _ = self.reconnect_tx.send(generation);
        }
    }

    async fn send_action(&self, payload: String) -> Result<()> {
        let (result, generation) = {
            let mut writer = self.writer.lock().await;
            let generation = self.active_generation.load(Ordering::Acquire);
            let sink = writer
                .as_mut()
                .ok_or_else(|| anyhow!("NapCat WebSocket not connected"))?;
            let result = wait_for_ws_write(
                &self.shutdown,
                WS_WRITE_TIMEOUT,
                sink.send(Message::Text(payload.into())),
                "NapCat WS write failed",
            )
            .await;
            if result.is_err() {
                *writer = None;
            }
            (result, generation)
        };

        if result.is_err() {
            close_pending_requests(&self.pending);
            if !self.shutdown.is_cancelled() {
                self.signal_disconnect(generation);
            }
        }
        result
    }

    async fn fetch_self_id(adapter: &NapCatAdapter) {
        match adapter.call(action_get_login_info()).await {
            Ok(response) if response.is_ok() => {
                let user_id = response.data["user_id"].as_i64().unwrap_or(0);
                adapter.self_id.store(user_id, Ordering::Relaxed);
                info!("NapCat connected. Bot QQ: {user_id}");
            }
            Ok(response) => {
                warn!("get_login_info non-ok: {:?}", response.message);
            }
            Err(error) => {
                warn!("get_login_info failed: {error}");
            }
        }
    }

    /// 构建 WebSocket 握手请求，同时使用 Authorization header 和 query param 认证
    async fn handshake(
        config: &NapCatConfig,
        shutdown: &CancellationToken,
    ) -> Result<WsConnection> {
        let mut url = parse_ws_url(&config.ws_url)?;
        info!("Connecting to NapCat WebSocket");

        if !config.access_token.is_empty() {
            url.query_pairs_mut()
                .append_pair("access_token", &config.access_token);
        }
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| anyhow!("Failed to build NapCat WebSocket request"))?;

        if !config.access_token.is_empty() {
            let bearer = format!("Bearer {}", config.access_token);
            request.headers_mut().insert(
                AUTHORIZATION,
                HeaderValue::from_str(&bearer)
                    .map_err(|error| anyhow!("Invalid access_token: {error}"))?,
            );
        }

        tokio::select! {
            biased;
            _ = shutdown.cancelled() => Err(anyhow!("NapCat connection cancelled")),
            _ = tokio::time::sleep(WS_HANDSHAKE_TIMEOUT) => {
                Err(anyhow!("NapCat WebSocket handshake timed out after {}s", WS_HANDSHAKE_TIMEOUT.as_secs()))
            }
            result = connect_async_tls_with_config(request, None, false, None) => {
                let (ws_stream, _) = result
                    .map_err(|_| anyhow!("NapCat WebSocket handshake failed"))?;
                Ok(ws_stream)
            }
        }
    }

    pub async fn call(&self, action: NcAction) -> Result<NcApiResponse> {
        let payload = serde_json::to_string(&action)?;
        let echo = action.echo.clone();
        let (tx, rx) = oneshot::channel::<NcApiResponse>();
        self.pending.insert(echo.clone(), tx);

        debug!("NC >> {payload}");
        if let Err(error) = self.send_action(payload).await {
            self.pending.remove(&echo);
            return Err(error);
        }

        let response = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => {
                self.pending.remove(&echo);
                return Err(anyhow!("NapCat connection cancelled"));
            }
            result = tokio::time::timeout(Duration::from_secs(10), rx) => result,
        }
        .map_err(|_| {
            self.pending.remove(&echo);
            anyhow!("NC API timeout: '{}'", action.action)
        })?
        .map_err(|_| anyhow!("NC API response channel closed"))?;

        Ok(response)
    }

    pub async fn send_private(&self, user_id: i64, message: &[Segment]) -> Result<()> {
        let action = super::api::action_send_private_msg(user_id, message);
        let response = self.call(action).await?;
        if !response.is_ok() {
            return Err(anyhow!(
                "send_private_msg failed: retcode={}, msg={:?}",
                response.retcode,
                response.message
            ));
        }
        Ok(())
    }

    pub async fn send_group(&self, group_id: i64, message: &[Segment]) -> Result<()> {
        let action = super::api::action_send_group_msg(group_id, message);
        let response = self.call(action).await?;
        if !response.is_ok() {
            return Err(anyhow!(
                "send_group_msg failed: retcode={}, msg={:?}",
                response.retcode,
                response.message
            ));
        }
        Ok(())
    }

    pub fn get_self_id(&self) -> i64 {
        self.self_id.load(Ordering::Relaxed)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NcEvent> {
        self.event_tx.subscribe()
    }

    /// 上报 supervisor 退出状态：异常退出置位失败信号，正常关闭复位
    fn report_supervisor_exit(&self, failed: bool) {
        let _ = self.supervisor_tx.send(supervisor_exit_signal(failed));
    }

    /// 获取 supervisor 退出状态观察通道，供上层会话观察异常退出
    pub(crate) fn supervisor_status(&self) -> watch::Receiver<Option<ConnectionExit>> {
        self.supervisor_tx.subscribe()
    }

    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        let generation = self.active_generation.load(Ordering::Acquire);
        self.mark_disconnected(generation).await;

        let runtime_task = self.runtime_task.lock().await.take();
        if let Some(runtime_task) = runtime_task {
            if let Err(error) = runtime_task.await {
                error!("NapCat runtime task failed: {error}");
            }
        }
    }
}

impl Drop for NapCatAdapter {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_retry_stops_at_configured_limit() {
        let mut retry = ReconnectState::default();

        for attempt in 1..MAX_RECONNECT_ATTEMPTS {
            assert!(matches!(
                retry.record_failure(),
                RetryDecision::Retry {
                    attempt: actual,
                    ..
                } if actual == attempt
            ));
        }

        assert_eq!(retry.record_failure(), RetryDecision::Exhausted);
    }

    #[test]
    fn runtime_retry_continues_past_initial_limit_with_capped_delay() {
        let mut retry = ReconnectState::default();
        retry.record_session_started();

        for attempt in 1..=MAX_RECONNECT_ATTEMPTS + 3 {
            let RetryDecision::Retry {
                attempt: actual,
                delay,
            } = retry.record_failure()
            else {
                panic!("runtime retry unexpectedly exhausted");
            };
            assert_eq!(actual, attempt);
            if attempt >= MAX_RECONNECT_ATTEMPTS {
                assert_eq!(
                    delay,
                    crate::adapter::reconnect::reconnect_delay_for_attempt(MAX_RECONNECT_ATTEMPTS)
                );
            }
        }
    }

    #[tokio::test]
    async fn pending_write_times_out() {
        let shutdown = CancellationToken::new();
        let error = tokio::time::timeout(Duration::from_millis(100), async move {
            wait_for_ws_write(
                &shutdown,
                Duration::from_millis(1),
                std::future::pending::<std::io::Result<()>>(),
                "test write",
            )
            .await
        })
        .await
        .expect("pending write must finish at its timeout")
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn retry_wait_stops_when_cancelled() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let completed = tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_retry(Duration::from_secs(60), &shutdown),
        )
        .await
        .expect("cancelled retry wait must complete promptly");
        assert!(!completed);
    }

    #[tokio::test]
    async fn clearing_pending_requests_closes_waiters() {
        let pending = DashMap::new();
        let (tx, rx) = oneshot::channel();
        pending.insert("echo".to_string(), tx);

        close_pending_requests(&pending);

        assert!(rx.await.is_err());
        assert!(pending.is_empty());
    }

    #[test]
    fn disconnect_signal_is_emitted_once_per_generation() {
        let last_generation = AtomicU64::new(0);

        assert!(claim_disconnect_signal(&last_generation, 1));
        assert!(!claim_disconnect_signal(&last_generation, 1));
        assert!(claim_disconnect_signal(&last_generation, 2));
        assert!(!claim_disconnect_signal(&last_generation, 1));
    }

    #[test]
    fn invalid_ws_url_error_does_not_echo_credentials() {
        let secret = "secret-token";
        let error =
            parse_ws_url(&format!("://user:{secret}@host/path?access_token={secret}")).unwrap_err();

        assert!(!error.to_string().contains(secret));
    }

    #[test]
    fn supervisor_exit_signal_maps_normal_and_abnormal_exit() {
        assert_eq!(supervisor_exit_signal(false), None);
        assert_eq!(
            supervisor_exit_signal(true),
            Some(ConnectionExit::Disconnected)
        );
    }

    #[tokio::test]
    async fn supervisor_status_channel_observes_abnormal_exit() {
        let (tx, mut rx) = watch::channel(None::<ConnectionExit>);

        tx.send(Some(ConnectionExit::Disconnected)).unwrap();

        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow_and_update(), Some(ConnectionExit::Disconnected));
    }

    #[tokio::test]
    async fn supervisor_status_resets_to_normal_on_clean_shutdown() {
        let (tx, mut rx) = watch::channel(Some(ConnectionExit::Disconnected));

        tx.send(None).unwrap();

        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow_and_update(), None);
    }
}

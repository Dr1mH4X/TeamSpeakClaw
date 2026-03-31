use super::{
    api::action_get_login_info,
    event::{parse_event, NcEvent},
    types::{NcAction, NcApiResponse, Segment},
};
use crate::config::NapCatConfig;
use anyhow::{Context as _, Result};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{
        client::IntoClientRequest,
        http::header::{HeaderValue, AUTHORIZATION},
        Message,
    },
};
use tracing::{debug, error, info, warn};

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

type WsStream = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

pub struct NapCatAdapter {
    writer: Mutex<Option<WsSink>>,
    event_tx: broadcast::Sender<NcEvent>,
    pending: Arc<DashMap<String, oneshot::Sender<NcApiResponse>>>,
    self_id: AtomicI64,
    reconnect_tx: mpsc::Sender<()>,
    config: NapCatConfig,
}

impl NapCatAdapter {
    pub async fn connect(config: NapCatConfig) -> Result<Arc<Self>> {
        const MAX_RETRIES: u32 = 10;
        const BASE_DELAY_MS: u64 = 1000;

        let mut delay = Duration::from_millis(BASE_DELAY_MS);
        let mut result = None;
        for attempt in 0..MAX_RETRIES {
            match Self::try_connect(config.clone()).await {
                Ok(r) => {
                    result = Some(r);
                    break;
                }
                Err(e) => {
                    warn!(
                        "[{}/{}] NapCat connect failed: {}",
                        attempt + 1,
                        MAX_RETRIES,
                        e
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(60));
                }
            }
        }
        let (adapter, reconnect_rx) = result.ok_or_else(|| {
            anyhow::anyhow!("NapCat: max reconnect attempts reached ({MAX_RETRIES})")
        })?;

        let weak = Arc::downgrade(&adapter);
        tokio::spawn(Self::reconnect_loop(weak, reconnect_rx));

        Ok(adapter)
    }

    async fn reconnect_loop(weak: std::sync::Weak<NapCatAdapter>, mut rx: mpsc::Receiver<()>) {
        const MAX_RETRIES: u32 = 10;
        while rx.recv().await.is_some() {
            let Some(adapter) = weak.upgrade() else { break };
            info!("NapCat reconnecting...");
            *adapter.writer.lock().await = None;

            let mut delay = Duration::from_secs(1);
            for attempt in 0..MAX_RETRIES {
                match Self::do_reconnect(&adapter).await {
                    Ok(()) => {
                        info!("NapCat reconnected");
                        break;
                    }
                    Err(e) => {
                        warn!(
                            "[{}/{}] NapCat reconnect failed: {}",
                            attempt + 1,
                            MAX_RETRIES,
                            e
                        );
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(Duration::from_secs(60));
                    }
                }
            }
        }
    }

    async fn try_connect(config: NapCatConfig) -> Result<(Arc<Self>, mpsc::Receiver<()>)> {
        let (ws_stream, event_tx, pending) = Self::handshake(&config).await?;
        let (sink, stream) = ws_stream.split();

        let (reconnect_tx, reconnect_rx) = mpsc::channel::<()>(1);

        let adapter = Arc::new(Self {
            writer: Mutex::new(Some(sink)),
            event_tx,
            pending: pending.clone(),
            self_id: AtomicI64::new(0),
            reconnect_tx,
            config,
        });

        Self::spawn_reader(adapter.clone(), stream);
        Self::fetch_self_id(&adapter).await;

        Ok((adapter, reconnect_rx))
    }

    async fn do_reconnect(adapter: &Arc<NapCatAdapter>) -> Result<()> {
        let (ws_stream, _, _) = Self::handshake(&adapter.config).await?;
        let (sink, stream) = ws_stream.split();

        *adapter.writer.lock().await = Some(sink);
        Self::spawn_reader(adapter.clone(), stream);
        Self::fetch_self_id(adapter).await;

        Ok(())
    }

    fn spawn_reader(adapter: Arc<NapCatAdapter>, stream: WsStream) {
        tokio::spawn(async move {
            adapter.reader_loop(stream).await;
        });
    }

    async fn fetch_self_id(adapter: &NapCatAdapter) {
        match adapter.call(action_get_login_info()).await {
            Ok(resp) if resp.is_ok() => {
                let uid = resp.data["user_id"].as_i64().unwrap_or(0);
                adapter.self_id.store(uid, Ordering::Relaxed);
                info!("NapCat connected. Bot QQ: {uid}");
            }
            Ok(resp) => {
                warn!("get_login_info non-ok: {:?}", resp.message);
            }
            Err(e) => {
                warn!("get_login_info failed: {e}");
            }
        }
    }

    /// 构建 WebSocket 握手请求，同时使用 Authorization header 和 query param 认证
    async fn handshake(
        config: &NapCatConfig,
    ) -> Result<(
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        broadcast::Sender<NcEvent>,
        Arc<DashMap<String, oneshot::Sender<NcApiResponse>>>,
    )> {
        let url = &config.ws_url;
        info!("Connecting to NapCat at {url}");

        let mut req = url
            .clone()
            .into_client_request()
            .map_err(|e| anyhow::anyhow!("Invalid WS URL '{}': {}", url, e))?;

        if !config.access_token.is_empty() {
            // Header 认证
            let bearer = format!("Bearer {}", config.access_token);
            req.headers_mut().insert(
                AUTHORIZATION,
                HeaderValue::from_str(&bearer)
                    .map_err(|e| anyhow::anyhow!("Invalid access_token: {e}"))?,
            );

            // Query param 兼容 OneBot 11 标准
            let uri = req.uri();
            let sep = if uri.query().is_some() { "&" } else { "?" };
            let new_uri = format!("{}{}access_token={}", uri, sep, &config.access_token);
            *req.uri_mut() = new_uri
                .parse()
                .map_err(|e| anyhow::anyhow!("Failed to build URI: {e}"))?;
        }

        let (ws_stream, _) = connect_async_tls_with_config(req, None, false, None)
            .await
            .map_err(|e| anyhow::anyhow!("NapCat WS handshake failed: {e}"))?;

        let (tx, _) = broadcast::channel::<NcEvent>(256);
        let pending: Arc<DashMap<String, oneshot::Sender<NcApiResponse>>> =
            Arc::new(DashMap::new());

        Ok((ws_stream, tx, pending))
    }

    async fn reader_loop(&self, mut stream: WsStream) {
        while let Some(msg_result) = stream.next().await {
            match msg_result {
                Ok(Message::Text(text)) => {
                    debug!("NC << {text}");
                    let val: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("NC parse error: {e}");
                            continue;
                        }
                    };

                    if val.get("retcode").is_some() || val.get("status").is_some() {
                        let echo = val["echo"].as_str().unwrap_or("").to_string();
                        if !echo.is_empty() {
                            if let Some((_, tx)) = self.pending.remove(&echo) {
                                let resp: NcApiResponse = serde_json::from_value(val)
                                    .unwrap_or_else(|_| NcApiResponse {
                                        status: "failed".into(),
                                        retcode: -1,
                                        data: Value::Null,
                                        message: Some("parse error".into()),
                                    });
                                let _ = tx.send(resp);
                            }
                            continue;
                        }
                    }

                    let event = parse_event(val);
                    if let Err(e) = self.event_tx.send(event) {
                        debug!("No NC event subscribers: {e}");
                    }
                }
                Ok(Message::Close(_)) => {
                    error!("NC connection closed by remote");
                    break;
                }
                Ok(Message::Ping(data)) => {
                    let mut guard = self.writer.lock().await;
                    if let Some(ref mut w) = *guard {
                        let _ = w.send(Message::Pong(data)).await;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    error!("NC read error: {e}");
                    break;
                }
            }
        }
        error!("NC reader loop exited");
        let _ = self.reconnect_tx.try_send(());
    }

    pub async fn call(&self, action: NcAction) -> Result<NcApiResponse> {
        let echo = action.echo.clone();
        let (tx, rx) = oneshot::channel::<NcApiResponse>();
        self.pending.insert(echo.clone(), tx);

        let payload = serde_json::to_string(&action)?;
        debug!("NC >> {payload}");

        {
            let mut guard = self.writer.lock().await;
            let w = guard
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("NapCat WebSocket not connected"));
            let w = match w {
                Ok(w) => w,
                Err(e) => {
                    // Remove pending entry if not connected
                    self.pending.remove(&echo);
                    return Err(e);
                }
            };
            if let Err(e) = w
                .send(Message::Text(payload.into()))
                .await
                .context("NapCat WS write failed")
            {
                // Remove pending entry if write fails
                self.pending.remove(&echo);
                return Err(e);
            }
        }

        let resp = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .map_err(|_| {
                self.pending.remove(&echo);
                anyhow::anyhow!("NC API timeout: '{}'", action.action)
            })?
            .map_err(|_| anyhow::anyhow!("NC API response channel closed"))?;

        Ok(resp)
    }

    pub async fn send_private(&self, user_id: i64, message: &[Segment]) -> Result<()> {
        let action = super::api::action_send_private_msg(user_id, message);
        let resp = self.call(action).await?;
        if !resp.is_ok() {
            return Err(anyhow::anyhow!(
                "send_private_msg failed: retcode={}, msg={:?}",
                resp.retcode,
                resp.message
            ));
        }
        Ok(())
    }

    pub async fn send_group(&self, group_id: i64, message: &[Segment]) -> Result<()> {
        let action = super::api::action_send_group_msg(group_id, message);
        let resp = self.call(action).await?;
        if !resp.is_ok() {
            return Err(anyhow::anyhow!(
                "send_group_msg failed: retcode={}, msg={:?}",
                resp.retcode,
                resp.message
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
}

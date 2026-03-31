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
use tokio::sync::{broadcast, oneshot, Mutex};
use tokio_tungstenite::{connect_async_tls_with_config, tungstenite::Message};
use tracing::{debug, error, info, warn};

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

pub struct NapCatAdapter {
    writer: Mutex<WsSink>,
    event_tx: broadcast::Sender<NcEvent>,
    pending: Arc<DashMap<String, oneshot::Sender<NcApiResponse>>>,
    self_id: AtomicI64,
}

impl NapCatAdapter {
    /// 建立连接（带指数退避重连）
    pub async fn connect(config: NapCatConfig) -> Result<Arc<Self>> {
        const MAX_RETRIES: u32 = 10;
        const BASE_DELAY_MS: u64 = 1000;

        let mut delay = Duration::from_millis(BASE_DELAY_MS);
        for attempt in 0..MAX_RETRIES {
            match Self::try_connect(config.clone()).await {
                Ok(adapter) => return Ok(adapter),
                Err(e) => {
                    warn!(
                        "NapCat connect attempt {} failed: {}. Retrying in {:?}",
                        attempt, e, delay
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(60));
                }
            }
        }
        Err(anyhow::anyhow!(
            "NapCat: max reconnect attempts reached ({MAX_RETRIES})"
        ))
    }

    async fn try_connect(config: NapCatConfig) -> Result<Arc<Self>> {
        let url = &config.ws_url;
        info!("Connecting to NapCat at {url}");

        // 构建请求，附加 access_token（如已配置）
        let req = if config.access_token.is_empty() {
            url.clone()
        } else {
            format!("{url}?access_token={}", config.access_token)
        };

        let (ws_stream, _) = connect_async_tls_with_config(&req, None, false, None)
            .await
            .with_context(|| format!("Failed to connect WebSocket to {url}"))?;

        let (sink, stream) = ws_stream.split();
        let (tx, _) = broadcast::channel::<NcEvent>(256);
        let pending: Arc<DashMap<String, oneshot::Sender<NcApiResponse>>> =
            Arc::new(DashMap::new());

        let adapter = Arc::new(Self {
            writer: Mutex::new(sink),
            event_tx: tx,
            pending: pending.clone(),
            self_id: AtomicI64::new(0),
        });

        // 启动读取循环
        let adapter_clone = adapter.clone();
        tokio::spawn(async move {
            adapter_clone.reader_loop(stream).await;
        });

        // 获取自身 QQ 号
        match adapter.call(action_get_login_info()).await {
            Ok(resp) if resp.is_ok() => {
                let uid = resp.data["user_id"].as_i64().unwrap_or(0);
                adapter.self_id.store(uid, Ordering::Relaxed);
                info!("NapCat connected. Bot QQ: {uid}");
            }
            Ok(resp) => {
                warn!("get_login_info returned non-ok: {:?}", resp.message);
            }
            Err(e) => {
                warn!("get_login_info failed: {e}");
            }
        }

        Ok(adapter)
    }

    /// 读取循环：分发事件和 API 响应
    async fn reader_loop(
        &self,
        mut stream: futures_util::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
    ) {
        while let Some(msg_result) = stream.next().await {
            match msg_result {
                Ok(Message::Text(text)) => {
                    debug!("<< {text}");
                    let val: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("Failed to parse NapCat message: {e}");
                            continue;
                        }
                    };

                    // 判断是否为 API 响应（含 retcode 字段）
                    if val.get("retcode").is_some() || val.get("status").is_some() {
                        let echo = val["echo"].as_str().unwrap_or("").to_string();
                        if !echo.is_empty() {
                            if let Some((_, tx)) = self.pending.remove(&echo) {
                                let resp: NcApiResponse = serde_json::from_value(val)
                                    .unwrap_or_else(|_| NcApiResponse {
                                        status: "failed".into(),
                                        retcode: -1,
                                        data: Value::Null,
                                        message: Some("parse error".to_string()),
                                    });
                                let _ = tx.send(resp);
                            }
                            continue;
                        }
                    }

                    // 否则作为事件处理
                    let event = parse_event(val);
                    if let Err(e) = self.event_tx.send(event) {
                        debug!("No NapCat event subscribers: {e}");
                    }
                }
                Ok(Message::Close(_)) => {
                    error!("NapCat WebSocket connection closed by remote");
                    break;
                }
                Ok(Message::Ping(data)) => {
                    // tungstenite 自动处理 Pong，但有些实现需要手动
                    let mut w = self.writer.lock().await;
                    let _ = w.send(Message::Pong(data)).await;
                }
                Ok(_) => {} // Binary / Pong 忽略
                Err(e) => {
                    error!("NapCat WebSocket read error: {e}");
                    break;
                }
            }
        }
        error!("NapCat reader_loop exited");
    }

    /// 发送 API action 并等待对应响应
    pub async fn call(&self, action: NcAction) -> Result<NcApiResponse> {
        let echo = action.echo.clone();
        let (tx, rx) = oneshot::channel::<NcApiResponse>();
        self.pending.insert(echo.clone(), tx);

        let payload = serde_json::to_string(&action)?;
        debug!(">> {payload}");

        {
            let mut w = self.writer.lock().await;
            w.send(Message::Text(payload.into()))
                .await
                .context("NapCat WebSocket write failed")?;
        }

        let resp = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .map_err(|_| {
                self.pending.remove(&echo);
                anyhow::anyhow!("NapCat API timeout for action '{}'", action.action)
            })?
            .map_err(|_| anyhow::anyhow!("NapCat API response channel closed"))?;

        Ok(resp)
    }

    /// 发送私聊消息
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

    /// 发送群消息
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

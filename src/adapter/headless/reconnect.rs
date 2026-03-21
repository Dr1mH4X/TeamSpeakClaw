//! 自动重连模块
//!
//! 提供连接断开后的自动重连功能

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tracing::{info, warn};

use super::{
    connection::{Connection, ConnectionConfig, ConnectionState},
    error::{HeadlessError, Result},
};

/// 重连配置
#[derive(Debug, Clone)]
pub struct ReconnectConfig {
    /// 最大重试次数
    pub max_retries: u32,
    /// 初始重试延迟 (ms)
    pub initial_delay_ms: u64,
    /// 最大重试延迟 (ms)
    pub max_delay_ms: u64,
    /// 退避乘数
    pub backoff_multiplier: f64,
    /// 是否启用自动重连
    pub enabled: bool,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            max_retries: 10,
            initial_delay_ms: 1000,
            max_delay_ms: 30000,
            backoff_multiplier: 2.0,
            enabled: true,
        }
    }
}

/// 重连管理器
pub struct ReconnectManager {
    /// 连接配置
    connection_config: ConnectionConfig,
    /// 重连配置
    reconnect_config: ReconnectConfig,
    /// 当前连接
    connection: Arc<RwLock<Option<Arc<Connection>>>>,
    /// 重连状态
    is_reconnecting: Arc<RwLock<bool>>,
    /// 事件发送器
    event_tx: mpsc::Sender<ReconnectEvent>,
    /// 关闭信号
    shutdown: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    /// 监控任务句柄（用于取消旧任务）
    monitor_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

/// 重连事件
#[derive(Debug, Clone)]
pub enum ReconnectEvent {
    /// 连接成功
    Connected,
    /// 断开连接
    Disconnected(Option<String>),
    /// 重连尝试
    ReconnectAttempt,
    /// 重连成功
    Reconnected,
    /// 重连失败
    ReconnectFailed(String),
    /// 达到最大重试次数
    MaxRetriesReached,
}

impl ReconnectManager {
    /// 创建新的重连管理器
    pub fn new(
        connection_config: ConnectionConfig,
        reconnect_config: ReconnectConfig,
    ) -> (Self, mpsc::Receiver<ReconnectEvent>) {
        let (event_tx, event_rx) = mpsc::channel(1024);

        let manager = Self {
            connection_config,
            reconnect_config,
            connection: Arc::new(RwLock::new(None)),
            is_reconnecting: Arc::new(RwLock::new(false)),
            event_tx,
            shutdown: Arc::new(Mutex::new(None)),
            monitor_handle: Arc::new(Mutex::new(None)),
        };

        (manager, event_rx)
    }

    /// 启动连接
    pub async fn start(&self) -> Result<Arc<Connection>> {
        let connection = self.create_connection().await?;

        // 连接到服务器
        connection
            .connect()
            .await
            .map_err(|e| HeadlessError::ConnectionError(format!("Connect failed: {e}")))?;

        // 保存连接
        *self.connection.write().await = Some(connection.clone());

        // 发送连接成功事件
        let _ = self.event_tx.send(ReconnectEvent::Connected).await;

        // 启动监控任务
        if self.reconnect_config.enabled {
            self.start_monitoring().await;
        }

        Ok(connection)
    }

    /// 创建新连接
    async fn create_connection(&self) -> Result<Arc<Connection>> {
        let (connection, _event_rx) = Connection::new(self.connection_config.clone())
            .await
            .map_err(|e| {
                HeadlessError::ConnectionError(format!("Create connection failed: {e}"))
            })?;

        Ok(Arc::new(connection))
    }

    /// 启动监控任务
    async fn start_monitoring(&self) {
        // 取消之前的监控任务
        if let Some(handle) = self.monitor_handle.lock().await.take() {
            handle.abort();
        }

        let connection = self.connection.clone();
        let is_reconnecting = self.is_reconnecting.clone();
        let event_tx = self.event_tx.clone();
        let connection_config = self.connection_config.clone();
        let reconnect_config = self.reconnect_config.clone();

        let handle = tokio::spawn(async move {
            loop {
                // 等待连接断开
                sleep(Duration::from_secs(1)).await;

                let conn_guard = connection.read().await;
                let should_reconnect = if let Some(conn) = conn_guard.as_ref() {
                    let state = conn.state().await;
                    state == ConnectionState::Disconnected
                } else {
                    false
                };

                if should_reconnect {
                    // 释放读锁
                    drop(conn_guard);

                    // 检查是否已在重连
                    if *is_reconnecting.read().await {
                        continue;
                    }

                    // 开始重连
                    *is_reconnecting.write().await = true;
                    let _ = event_tx.send(ReconnectEvent::Disconnected(None)).await;

                    // 执行重连
                    let result = Self::do_reconnect(
                        &connection,
                        &is_reconnecting,
                        &event_tx,
                        &connection_config,
                        &reconnect_config,
                    )
                    .await;

                    match result {
                        Ok(_) => {
                            let _ = event_tx.send(ReconnectEvent::Reconnected).await;
                        }
                        Err(e) => {
                            let _ = event_tx
                                .send(ReconnectEvent::ReconnectFailed(e.to_string()))
                                .await;
                        }
                    }
                }
            }
        });

        *self.monitor_handle.lock().await = Some(handle);
    }

    /// 执行重连
    async fn do_reconnect(
        connection: &Arc<RwLock<Option<Arc<Connection>>>>,
        is_reconnecting: &Arc<RwLock<bool>>,
        event_tx: &mpsc::Sender<ReconnectEvent>,
        connection_config: &ConnectionConfig,
        reconnect_config: &ReconnectConfig,
    ) -> Result<()> {
        let mut delay = Duration::from_millis(reconnect_config.initial_delay_ms);
        let mut attempt = 0;

        loop {
            attempt += 1;

            if attempt > reconnect_config.max_retries {
                let _ = event_tx.send(ReconnectEvent::MaxRetriesReached).await;
                *is_reconnecting.write().await = false;
                return Err(HeadlessError::ConnectionError("Max retries reached".into()));
            }

            let _ = event_tx.send(ReconnectEvent::ReconnectAttempt).await;
            info!("Reconnect attempt {} (delay: {:?})", attempt, delay);

            sleep(delay).await;

            // 创建新连接
            match Self::try_connect(connection_config).await {
                Ok(new_conn) => {
                    *connection.write().await = Some(new_conn);
                    *is_reconnecting.write().await = false;
                    info!("Reconnected successfully");
                    return Ok(());
                }
                Err(e) => {
                    warn!("Reconnect attempt {} failed: {}", attempt, e);

                    // 计算下一次延迟
                    delay = Duration::from_millis(
                        (delay.as_millis() as f64 * reconnect_config.backoff_multiplier) as u64,
                    )
                    .min(Duration::from_millis(reconnect_config.max_delay_ms));
                }
            }
        }
    }

    /// 尝试连接
    async fn try_connect(config: &ConnectionConfig) -> Result<Arc<Connection>> {
        let (connection, _event_rx) = Connection::new(config.clone())
            .await
            .map_err(|e| HeadlessError::ConnectionError(format!("Create failed: {e}")))?;

        let connection = Arc::new(connection);

        // 设置连接超时
        match timeout(config.connect_timeout, connection.connect()).await {
            Ok(Ok(())) => Ok(connection),
            Ok(Err(e)) => Err(HeadlessError::ConnectionError(format!(
                "Connect failed: {e}"
            ))),
            Err(_) => Err(HeadlessError::Timeout),
        }
    }

    /// 获取当前连接
    pub async fn connection(&self) -> Option<Arc<Connection>> {
        self.connection.read().await.clone()
    }

    /// 关闭重连管理器
    pub async fn shutdown(&self) {
        if let Some(tx) = self.shutdown.lock().await.take() {
            let _ = tx.send(());
        }

        // 断开当前连接
        if let Some(conn) = self.connection.write().await.take() {
            let _ = conn.disconnect().await;
        }
    }
}

/// 自动重连连接包装器
pub struct AutoReconnectConnection {
    manager: Arc<ReconnectManager>,
}

impl AutoReconnectConnection {
    /// 创建新的自动重连连接
    pub async fn new(
        connection_config: ConnectionConfig,
        reconnect_config: ReconnectConfig,
    ) -> Result<(Self, mpsc::Receiver<ReconnectEvent>)> {
        let (manager, event_rx) = ReconnectManager::new(connection_config, reconnect_config);

        let wrapper = Self {
            manager: Arc::new(manager),
        };

        Ok((wrapper, event_rx))
    }

    /// 连接到服务器
    pub async fn connect(&self) -> Result<Arc<Connection>> {
        self.manager.start().await
    }

    /// 获取当前连接
    pub async fn connection(&self) -> Option<Arc<Connection>> {
        self.manager.connection().await
    }

    /// 关闭
    pub async fn shutdown(&self) {
        self.manager.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reconnect_config_default() {
        let config = ReconnectConfig::default();
        assert_eq!(config.max_retries, 10);
        assert_eq!(config.initial_delay_ms, 1000);
        assert!(config.enabled);
    }
}

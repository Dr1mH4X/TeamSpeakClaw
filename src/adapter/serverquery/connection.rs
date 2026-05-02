use crate::{
    adapter::serverquery::{
        command::{check_ts_error, cmd_clientupdate_nick, cmd_login, cmd_register_event, cmd_use},
        event::{parse_events, ClientEnterEvent, TsEvent},
    },
    config::{AppConfig, SqConfig},
};
use anyhow::{Context as _, Result};

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::task::{Context, Poll};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::{broadcast, mpsc, oneshot, Mutex},
    time::sleep,
};
use tracing::{debug, error, info, warn};

/// 支持的连接方法枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsMethod {
    Tcp,
    Ssh,
}

impl TryFrom<&str> for TsMethod {
    type Error = anyhow::Error;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s.to_lowercase().as_str() {
            "ssh" => Ok(TsMethod::Ssh),
            "tcp" => Ok(TsMethod::Tcp),
            _ => Err(anyhow::anyhow!(
                "Unsupported connection method: {}. Only 'tcp' or 'ssh' are allowed.",
                s
            )),
        }
    }
}

pub enum TsStream {
    Tcp(tokio::net::TcpStream),
    Ssh(russh::ChannelStream<russh::client::Msg>),
}

impl AsyncRead for TsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            TsStream::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            TsStream::Ssh(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for TsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut *self {
            TsStream::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            TsStream::Ssh(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            TsStream::Tcp(s) => Pin::new(s).poll_flush(cx),
            TsStream::Ssh(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            TsStream::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            TsStream::Ssh(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

struct SshHandler {
    host: String,
    port: u16,
}

impl russh::client::Handler for SshHandler {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        let key_dir = crate::config::config_dir();
        if !key_dir.exists() {
            if let Err(e) = tokio::fs::create_dir_all(&key_dir).await {
                error!("Failed to create config directory: {}", e);
                return Ok(false);
            }
        }

        let key_path = key_dir.join(format!("{}_{}.pub", self.host, self.port));

        let current_key = match server_public_key.to_openssh() {
            Ok(key) => key,
            Err(e) => {
                error!("Failed to serialize server public key: {}", e);
                return Ok(false);
            }
        };

        if key_path.exists() {
            match tokio::fs::read_to_string(&key_path).await {
                Ok(saved_key) => {
                    if saved_key.trim() == current_key.trim() {
                        Ok(true)
                    } else {
                        error!(
                            "SSH Host key mismatch for {}:{}! Potential MITM attack.",
                            self.host, self.port
                        );
                        Ok(false)
                    }
                }
                Err(e) => {
                    error!("Failed to read known hosts key: {}", e);
                    Ok(false)
                }
            }
        } else {
            info!(
                "Trusting new SSH host key for {}:{} and saving to {:?}",
                self.host, self.port, key_path
            );

            match tokio::fs::write(&key_path, current_key.trim()).await {
                Ok(_) => Ok(true),
                Err(e) => {
                    error!("Failed to save SSH public key: {}", e);
                    Ok(false)
                }
            }
        }
    }
}

pub struct TsAdapter {
    writer: Mutex<tokio::io::WriteHalf<TsStream>>,
    event_tx: broadcast::Sender<TsEvent>,
    bot_clid: AtomicU32,
    query_tx: Mutex<Option<oneshot::Sender<String>>>,
    query_active: AtomicBool,
    include_event_lines_active: AtomicBool,
    query_lock: Mutex<()>,
    reconnect_tx: mpsc::Sender<()>,
    config: Arc<AppConfig>,
    generation: AtomicU32,
}

impl TsAdapter {
    pub async fn connect(config: Arc<AppConfig>) -> Result<Arc<Self>> {
        let cfg = &config.serverquery;
        let method = TsMethod::try_from(cfg.method.as_str())
            .context("Invalid connection method in config")?;

        let addr = match method {
            TsMethod::Ssh => format!("{}:{}", cfg.host, cfg.ssh_port),
            TsMethod::Tcp => format!("{}:{}", cfg.host, cfg.port),
        };
        info!(
            "Connecting to TeamSpeak ServerQuery ({:?}) at {addr}",
            method
        );

        let stream = Self::connect_with_retry(cfg, method).await?;
        let (reader, writer) = tokio::io::split(stream);
        let (tx, _) = broadcast::channel::<TsEvent>(256);
        let (reconnect_tx, reconnect_rx) = mpsc::channel::<()>(1);

        let adapter = Arc::new(Self {
            writer: Mutex::new(writer),
            event_tx: tx,
            bot_clid: AtomicU32::new(0),
            query_tx: Mutex::new(None),
            query_active: AtomicBool::new(false),
            include_event_lines_active: AtomicBool::new(false),
            query_lock: Mutex::new(()),
            reconnect_tx,
            config: config.clone(),
            generation: AtomicU32::new(0),
        });

        Self::spawn_loops(adapter.clone(), reader);

        if let Err(e) = adapter.init(cfg).await {
            error!("Failed to initialize TeamSpeak session: {e}");
            return Err(e);
        }

        let weak = Arc::downgrade(&adapter);
        tokio::spawn(Self::reconnect_loop(weak, reconnect_rx));

        Ok(adapter)
    }

    fn spawn_loops(adapter: Arc<TsAdapter>, reader: tokio::io::ReadHalf<TsStream>) {
        let gen = adapter.generation.fetch_add(1, Ordering::Relaxed) + 1;

        let adapter_clone = adapter.clone();
        tokio::spawn(async move {
            adapter_clone.reader_loop(gen, BufReader::new(reader)).await;
        });

        let adapter_clone = adapter.clone();
        tokio::spawn(async move {
            adapter_clone.keepalive_loop(gen).await;
        });
    }

    async fn connect_with_retry(cfg: &SqConfig, method: TsMethod) -> Result<TsStream> {
        const MAX_RETRIES: u32 = 10;
        const BASE_DELAY_MS: u64 = 1000;

        let mut delay = Duration::from_millis(BASE_DELAY_MS);
        for attempt in 0..MAX_RETRIES {
            let res = match method {
                TsMethod::Ssh => Self::connect_ssh(cfg).await,
                TsMethod::Tcp => Self::connect_tcp(cfg).await,
            };

            match res {
                Ok(s) => return Ok(s),
                Err(e) => {
                    warn!("Connect attempt {attempt} failed: {e}. Retrying in {delay:?}");
                    sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(60));
                }
            }
        }
        Err(anyhow::anyhow!("Max reconnect attempts reached (code 999)"))
    }

    async fn connect_tcp(cfg: &SqConfig) -> Result<TsStream> {
        let addr = format!("{}:{}", cfg.host, cfg.port);
        let stream = TcpStream::connect(&addr).await?;
        Ok(TsStream::Tcp(stream))
    }

    async fn connect_ssh(cfg: &SqConfig) -> Result<TsStream> {
        let config = Arc::new(russh::client::Config::default());
        let addr = format!("{}:{}", cfg.host, cfg.ssh_port);

        let handler = SshHandler {
            host: cfg.host.clone(),
            port: cfg.ssh_port,
        };

        let mut session = russh::client::connect(config, addr, handler)
            .await
            .context("Failed to establish SSH connection")?;

        let auth_res = session
            .authenticate_password(&cfg.login_name, &cfg.login_pass)
            .await?;

        if !matches!(auth_res, russh::client::AuthResult::Success) {
            return Err(anyhow::anyhow!("SSH Authentication failed"));
        }

        let channel = session.channel_open_session().await?;
        channel.request_shell(true).await?;

        let stream = channel.into_stream();
        Ok(TsStream::Ssh(stream))
    }

    async fn reconnect_loop(weak: std::sync::Weak<TsAdapter>, mut rx: mpsc::Receiver<()>) {
        const MAX_RETRIES: u32 = 10;
        while rx.recv().await.is_some() {
            let Some(adapter) = weak.upgrade() else { break };
            info!("ServerQuery reconnecting...");

            let mut delay = Duration::from_secs(1);
            let mut success = false;
            for attempt in 0..MAX_RETRIES {
                match Self::do_reconnect(&adapter).await {
                    Ok(()) => {
                        info!("ServerQuery reconnected");
                        success = true;
                        break;
                    }
                    Err(e) => {
                        warn!(
                            "[{}/{}] ServerQuery reconnect failed: {}",
                            attempt + 1,
                            MAX_RETRIES,
                            e
                        );
                        sleep(delay).await;
                        delay = (delay * 2).min(Duration::from_secs(60));
                    }
                }
            }
            if !success {
                error!(
                    "ServerQuery reconnect exhausted all {} attempts, giving up",
                    MAX_RETRIES
                );
            }
        }
    }

    async fn do_reconnect(adapter: &Arc<TsAdapter>) -> Result<()> {
        let cfg = &adapter.config.serverquery;
        let method = TsMethod::try_from(cfg.method.as_str())
            .context("Invalid connection method in config")?;

        let stream = Self::connect_with_retry(cfg, method).await?;
        let (reader, writer) = tokio::io::split(stream);

        *adapter.writer.lock().await = writer;
        Self::spawn_loops(adapter.clone(), reader);

        adapter.init(cfg).await?;
        Ok(())
    }

    async fn init(&self, cfg: &SqConfig) -> Result<()> {
        sleep(Duration::from_millis(500)).await;

        self.send_raw(&cmd_login(&cfg.login_name, &cfg.login_pass))
            .await?;
        self.send_raw(&cmd_use(cfg.server_id)).await?;
        self.send_raw(&cmd_register_event("textprivate")).await?;
        self.send_raw(&cmd_register_event("textchannel")).await?;
        self.send_raw(&cmd_register_event("textserver")).await?;
        self.send_raw(&cmd_register_event("server")).await?;

        // 获取自身 ID
        self.send_raw("whoami").await?;

        // 等待一下以确保 bot_clid 被更新
        sleep(Duration::from_millis(200)).await;

        info!("ServerQuery session initialized");
        Ok(())
    }

    pub fn get_bot_clid(&self) -> u32 {
        self.bot_clid.load(Ordering::Relaxed)
    }

    pub async fn set_nickname(&self, nick: &str) -> Result<()> {
        let suffix = rand::random::<u16>();
        let nickname = format!("{}_{}", nick, suffix);
        info!("Setting nickname to {}", nickname);
        self.send_raw(&cmd_clientupdate_nick(&nickname)).await
    }

    pub async fn quit(&self) -> Result<()> {
        info!("Sending quit command to TeamSpeak server");
        self.send_raw("quit").await
    }

    pub async fn send_raw(&self, cmd: &str) -> Result<()> {
        if cmd.starts_with("login ") {
            debug!(">> login [REDACTED]");
        } else {
            debug!(">> {cmd}");
        }
        let mut w = self.writer.lock().await;
        w.write_all(format!("{cmd}\n").as_bytes()).await?;
        w.flush().await?;
        Ok(())
    }

    /// 发送命令并等待服务器响应（数据行 + error 行）。
    pub async fn send_query(&self, cmd: &str) -> Result<String> {
        self.send_query_internal(cmd, false).await
    }

    /// 拉取当前在线客户端快照（用于启动阶段预热路由缓存）。
    /// 通过订阅事件流而非依赖 query 返回字符串，避免数据行延迟到达的问题。
    pub async fn fetch_client_snapshot(&self) -> Result<Vec<ClientEnterEvent>> {
        debug!("fetch_client_snapshot: sending clientlist command");

        // 订阅事件流，用于接收 ClientEnterView 事件
        let mut rx = self.subscribe();

        // 发送命令（不通过 send_query_internal，避免 query_active 状态问题）
        self.send_raw("clientlist -uid -groups").await?;

        let mut clients = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);

        loop {
            if tokio::time::Instant::now() > deadline {
                break;
            }

            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(event)) => match event {
                    TsEvent::ClientEnterView(client) if client.clid != 0 => {
                        debug!(
                            "fetch_client_snapshot: got client clid={}, nickname={}",
                            client.clid, client.client_nickname
                        );
                        clients.push(client);
                    }
                    _ => {}
                },
                Ok(Err(_)) => {
                    // channel closed
                    break;
                }
                Err(_) => {
                    // deadline reached
                    break;
                }
            }
        }

        info!("fetch_client_snapshot: returning {} clients", clients.len());

        Ok(clients)
    }

    async fn send_query_internal(&self, cmd: &str, include_event_lines: bool) -> Result<String> {
        let _guard = self.query_lock.lock().await;

        if cmd.starts_with("login ") {
            debug!(">> login [REDACTED]");
        } else {
            debug!(">> {cmd}");
        }

        let (tx, rx) = oneshot::channel::<String>();
        self.include_event_lines_active
            .store(include_event_lines, Ordering::Relaxed);
        self.query_active.store(true, Ordering::Relaxed);
        {
            let mut q = self.query_tx.lock().await;
            *q = Some(tx);
        }

        {
            let mut w = self.writer.lock().await;
            w.write_all(format!("{cmd}\n").as_bytes()).await?;
            w.flush().await?;
        }

        let response = rx
            .await
            .map_err(|_| anyhow::anyhow!("Query response channel closed"))?;
        check_ts_error(&response)?;
        Ok(response)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TsEvent> {
        self.event_tx.subscribe()
    }

    async fn reader_loop(&self, _gen: u32, mut reader: BufReader<tokio::io::ReadHalf<TsStream>>) {
        let mut line = String::new();
        let mut result_lines: Vec<String> = Vec::new();
        let mut waiting_for_data = false; // 收到 error 行后，等待可能延迟的数据行

        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    error!("ServerQuery connection closed by remote");
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    debug!("<< {trimmed}");

                    // 解析 whoami 响应以获取我们自己的 client_id
                    if trimmed.contains("client_id=") && trimmed.contains("virtualserver_status=") {
                        if let Some(part) = trimmed
                            .split_whitespace()
                            .find(|s| s.starts_with("client_id="))
                        {
                            if let Ok(clid) = part[10..].parse::<u32>() {
                                self.bot_clid.store(clid, Ordering::Relaxed);
                                debug!("Updated bot_clid to {}", clid);
                            }
                        }
                    }

                    // 广播事件通知
                    let is_event = trimmed.starts_with("notify") || trimmed.starts_with("clid=");
                    if is_event {
                        if trimmed.starts_with("clid=") && trimmed.contains('|') {
                            for (i, client) in trimmed.split('|').enumerate() {
                                info!("reader_loop: clientlist[{}]: {}", i, client.trim());
                            }
                        } else {
                            debug!("reader_loop: event: {}", trimmed);
                        }
                    }
                    for event in parse_events(trimmed) {
                        if let Err(e) = self.event_tx.send(event) {
                            debug!("No active subscribers for event: {e}");
                        }
                    }

                    // 查询活跃时，收集数据行
                    let query_active = self.query_active.load(Ordering::Relaxed);
                    let include_event_lines =
                        self.include_event_lines_active.load(Ordering::Relaxed);
                    if query_active && (!is_event || include_event_lines) {
                        result_lines.push(trimmed.to_string());
                    }

                    // error 行处理
                    if trimmed.starts_with("error id=") {
                        if trimmed.contains("id=0") {
                            debug!("<< {trimmed}");
                        } else {
                            error!("TS3 Error: {trimmed}");
                        }

                        if query_active {
                            // 如果 result_lines 为空且 include_event_lines=true，等待可能延迟到达的数据行
                            if result_lines.is_empty() && include_event_lines {
                                waiting_for_data = true;
                                // 不立即返回，继续循环等待数据行
                                continue;
                            }
                            // 否则正常返回
                            self.query_active.store(false, Ordering::Relaxed);
                            let mut q = self.query_tx.lock().await;
                            if let Some(tx) = q.take() {
                                let response = result_lines.join("\n");
                                let _ = tx.send(response);
                                result_lines.clear();
                            }
                        }
                    }

                    // 如果正在等待数据行，检查当前行是否是数据行
                    if waiting_for_data {
                        if is_event && trimmed.starts_with("clid=") {
                            // 收到延迟的数据行，收集它并返回
                            result_lines.push(trimmed.to_string());
                            self.query_active.store(false, Ordering::Relaxed);
                            let mut q = self.query_tx.lock().await;
                            if let Some(tx) = q.take() {
                                let response = result_lines.join("\n");
                                let _ = tx.send(response);
                                result_lines.clear();
                            }
                            waiting_for_data = false;
                        } else {
                            // 收到了其他行（可能是另一个命令的响应），返回当前结果（可能为空）
                            self.query_active.store(false, Ordering::Relaxed);
                            let mut q = self.query_tx.lock().await;
                            if let Some(tx) = q.take() {
                                let response = result_lines.join("\n");
                                let _ = tx.send(response);
                                result_lines.clear();
                            }
                            waiting_for_data = false;
                        }
                    }
                }
                Err(e) => {
                    error!("Error reading from TS3: {e}");
                    break;
                }
            }
        }
        error!("ServerQuery reader loop exited");
        let _ = self.reconnect_tx.try_send(());
    }

    async fn keepalive_loop(&self, gen: u32) {
        const KEEPALIVE_INTERVAL_SECS: u64 = 180;
        loop {
            sleep(Duration::from_secs(KEEPALIVE_INTERVAL_SECS)).await;
            if self.generation.load(Ordering::Relaxed) != gen {
                debug!("Keepalive loop exiting (generation mismatch)");
                break;
            }
            if let Err(e) = self.send_raw("whoami").await {
                error!("Keepalive failed: {e}");
                let _ = self.reconnect_tx.try_send(());
                break;
            }
        }
    }
}

use crate::{
    adapter::{
        command::{cmd_clientupdate_nick, cmd_login, cmd_register_event, cmd_use},
        event::{parse_events, TsEvent},
    },
    config::{AppConfig, TsConfig},
};
use anyhow::{Context as _, Result};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::{broadcast, Mutex},
    time::sleep,
};
use tracing::{debug, error, info, warn};

/// 支持的连接方法枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsMethod {
    Tcp,
    Ssh,
}

impl From<&str> for TsMethod {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "ssh" => TsMethod::Ssh,
            _ => TsMethod::Tcp,
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
        let key_dir = PathBuf::from("config");
        if !key_dir.exists() {
            let _ = std::fs::create_dir_all(&key_dir);
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
            match std::fs::read_to_string(&key_path) {
                Ok(saved_key) => {
                    if saved_key.trim() == current_key.trim() {
                        Ok(true)
                    } else {
                        error!("SSH Host key mismatch for {}:{}", self.host, self.port);
                        Ok(false)
                    }
                }
                Err(e) => {
                    error!("Failed to read known hosts key: {}", e);
                    Ok(false)
                }
            }
        } else {
            match std::fs::write(&key_path, &current_key) {
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
}

impl TsAdapter {
    pub async fn connect(config: Arc<AppConfig>) -> Result<Arc<Self>> {
        let cfg = &config.teamspeak;
        let method = TsMethod::from(cfg.method.as_str());

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

        let adapter = Arc::new(Self {
            writer: Mutex::new(writer),
            event_tx: tx,
            bot_clid: AtomicU32::new(0),
        });

        let adapter_clone = adapter.clone();
        tokio::spawn(async move {
            adapter_clone.reader_loop(BufReader::new(reader)).await;
        });

        if let Err(e) = adapter.init(cfg).await {
            error!("Failed to initialize TeamSpeak session: {e}");
            return Err(e);
        }

        let adapter_clone = adapter.clone();
        tokio::spawn(async move {
            adapter_clone.keepalive_loop().await;
        });

        Ok(adapter)
    }

    async fn connect_with_retry(cfg: &TsConfig, method: TsMethod) -> Result<TsStream> {
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

    async fn connect_tcp(cfg: &TsConfig) -> Result<TsStream> {
        let addr = format!("{}:{}", cfg.host, cfg.port);
        let stream = TcpStream::connect(&addr).await?;
        Ok(TsStream::Tcp(stream))
    }

    async fn connect_ssh(cfg: &TsConfig) -> Result<TsStream> {
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

    async fn init(&self, cfg: &TsConfig) -> Result<()> {
        // 等待一小段时间，让欢迎横幅先被处理
        sleep(Duration::from_millis(500)).await;

        self.send_raw(&cmd_login(&cfg.login_name, &cfg.login_pass))
            .await?;
        self.send_raw(&cmd_use(cfg.server_id)).await?;
        self.send_raw(&cmd_register_event("textprivate")).await?;
        self.send_raw(&cmd_register_event("textchannel")).await?;
        self.send_raw(&cmd_register_event("textserver")).await?;
        self.send_raw(&cmd_register_event("server")).await?;

        // 拉取初始客户端列表
        self.send_raw("clientlist -uid -groups").await?;

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
            info!(">> login [REDACTED]");
        } else {
            info!(">> {cmd}");
        }
        let mut w = self.writer.lock().await;
        w.write_all(format!("{cmd}\n").as_bytes()).await?;
        w.flush().await?;
        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TsEvent> {
        self.event_tx.subscribe()
    }

    async fn reader_loop(&self, mut reader: BufReader<tokio::io::ReadHalf<TsStream>>) {
        let mut line = String::new();
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
                    info!("<< {trimmed}");

                    if trimmed.starts_with("error id=") && !trimmed.contains("id=0") {
                        error!("TS3 Error: {trimmed}");
                    }

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

                    for event in parse_events(trimmed) {
                        if let Err(e) = self.event_tx.send(event) {
                            debug!("No active subscribers for event: {e}");
                        }
                    }
                }
                Err(e) => {
                    error!("Error reading from TS3: {e}");
                    break;
                }
            }
        }
    }

    async fn keepalive_loop(&self) {
        const KEEPALIVE_INTERVAL_SECS: u64 = 180;
        loop {
            sleep(Duration::from_secs(KEEPALIVE_INTERVAL_SECS)).await;
            if let Err(e) = self.send_raw("whoami").await {
                error!("Keepalive failed: {e}");
            }
        }
    }
}

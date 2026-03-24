use crate::{
    adapter::{
        command::{cmd_clientupdate_nick, cmd_login, cmd_register_event, cmd_use},
        event::{parse_events, TsEvent},
    },
    config::{AppConfig, TsConfig},
};
use anyhow::Result;
use arc_swap::ArcSwap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::{broadcast, Mutex},
    time::sleep,
};
use tracing::{debug, error, info, warn};

pub struct TsAdapter {
    writer: Mutex<tokio::io::WriteHalf<TcpStream>>,
    event_tx: broadcast::Sender<TsEvent>,
    config: Arc<ArcSwap<AppConfig>>,
    bot_clid: AtomicU32,
}

impl TsAdapter {
    pub async fn connect(config: Arc<ArcSwap<AppConfig>>) -> Result<Arc<Self>> {
        let cfg = config.load();
        let addr = format!("{}:{}", cfg.teamspeak.host, cfg.teamspeak.port);
        info!("Connecting to TeamSpeak ServerQuery at {addr}");

        let stream = Self::connect_with_retry(&cfg.teamspeak).await?;
        let (reader, writer) = tokio::io::split(stream);
        let (tx, _) = broadcast::channel::<TsEvent>(256);

        let adapter = Arc::new(Self {
            writer: Mutex::new(writer),
            event_tx: tx,
            config: config.clone(),
            bot_clid: AtomicU32::new(0),
        });

        // 启动读取任务
        let adapter_clone = adapter.clone();
        tokio::spawn(async move {
            adapter_clone.reader_loop(BufReader::new(reader)).await;
        });

        // 初始化：登录、选择虚拟服务器、注册事件
        if let Err(e) = adapter.init(&cfg.teamspeak).await {
            error!("Failed to initialize TeamSpeak session: {e}");
            return Err(e);
        }

        // 启动保活任务
        let adapter_clone = adapter.clone();
        tokio::spawn(async move {
            adapter_clone.keepalive_loop().await;
        });

        Ok(adapter)
    }

    async fn connect_with_retry(cfg: &TsConfig) -> Result<TcpStream> {
        let addr = format!("{}:{}", cfg.host, cfg.port);
        let mut delay = Duration::from_millis(cfg.reconnect_base_delay_ms);
        for attempt in 0..cfg.reconnect_max_retries {
            match TcpStream::connect(&addr).await {
                Ok(s) => {
                    // 跳过 TS 欢迎横幅（2 行）
                    // 交由读取循环处理
                    return Ok(s);
                }
                Err(e) => {
                    warn!("Connect attempt {attempt} failed: {e}. Retrying in {delay:?}");
                    sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(60));
                }
            }
        }
        Err(anyhow::anyhow!("Max reconnect attempts reached (code 999)"))
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
        
        // 等待一下以确保 bot_clid 被更新 (虽然不是强一致性，但足够)
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

    pub fn subscribe(&self) -> broadcast::Receiver<TsEvent> {
        self.event_tx.subscribe()
    }

    async fn reader_loop(&self, mut reader: BufReader<tokio::io::ReadHalf<TcpStream>>) {
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
                    debug!("<< {trimmed}");

                    if trimmed.starts_with("error id=") && !trimmed.contains("id=0") {
                        error!("TS3 Error: {trimmed}");
                    }

                    // 解析 whoami 响应以获取我们自己的 client_id
                    if trimmed.contains("client_id=") && trimmed.contains("virtualserver_status=") {
                        if let Some(part) = trimmed.split_whitespace().find(|s| s.starts_with("client_id=")) {
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
        loop {
            let interval = self.config.load().teamspeak.keepalive_interval_secs;
            sleep(Duration::from_secs(interval)).await;
            if let Err(e) = self.send_raw("whoami").await {
                error!("Keepalive failed: {e}");
            }
        }
    }
}

use crate::{
    adapter::{
        command::{cmd_login, cmd_register_event, cmd_use, cmd_clientupdate_nick},
        event::{parse_events, TsEvent},
    },
    config::{AppConfig, TsConfig},
    error::{AppError, Result},
};
use arc_swap::ArcSwap;
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
        });

        // Spawn reader task
        let adapter_clone = adapter.clone();
        tokio::spawn(async move {
            adapter_clone.reader_loop(BufReader::new(reader)).await;
        });

        // Init: login, use, register events
        if let Err(e) = adapter.init(&cfg.teamspeak).await {
            error!("Failed to initialize TeamSpeak session: {e}");
            return Err(e);
        }

        // Spawn keepalive task
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
                    // Skip TS welcome banner (2 lines)
                    // We let the reader loop handle it
                    return Ok(s);
                }
                Err(e) => {
                    warn!("Connect attempt {attempt} failed: {e}. Retrying in {delay:?}");
                    sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(60));
                }
            }
        }
        Err(AppError::TsError {
            code: 999,
            message: "Max reconnect attempts reached".into(),
        })
    }

    async fn init(&self, cfg: &TsConfig) -> Result<()> {
        // Wait a bit for banner to be processed
        sleep(Duration::from_millis(500)).await;
        
        self.send_raw(&cmd_login(&cfg.login_name, &cfg.login_pass)).await?;
        self.send_raw(&cmd_use(cfg.server_id)).await?;
        self.send_raw(&cmd_register_event("textprivate")).await?;
        self.send_raw(&cmd_register_event("textchannel")).await?;
        self.send_raw(&cmd_register_event("textserver")).await?;
        self.send_raw(&cmd_register_event("server")).await?;
        
        // Populate initial client list
        self.send_raw("clientlist -uid -groups").await?;

        info!("ServerQuery session initialized");
        Ok(())
    }

    pub async fn set_nickname(&self, nick: &str) -> Result<()> {
        self.send_raw(&cmd_clientupdate_nick(nick)).await
    }

    pub async fn send_raw(&self, cmd: &str) -> Result<()> {
        debug!(">> {cmd}");
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
                    if trimmed.is_empty() { continue; }
                    debug!("<< {trimmed}");
                    
                    if trimmed.starts_with("error id=") && !trimmed.contains("id=0") {
                        error!("TS3 Error: {trimmed}");
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

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use tsproto_packets::packets::{OutCommand, OutPacket};

use crate::config::{AppConfig, PromptsConfig};
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::router::ClientInfo;
use crate::skills::SkillRegistry;

pub mod tsbot {
    pub mod voice {
        pub mod v1 {
            tonic::include_proto!("tsbot.voice.v1");
        }
    }
}

use tsbot::voice::v1 as voicev1;
use voicev1::voice_service_server::VoiceServiceServer;

mod actor;
mod playback;
mod service;
pub mod speech;
mod types;

pub use types::{PersistedVoiceState, SharedStatus};

pub const INTERNAL_GRPC_ADDR: &str = "127.0.0.1:50051";

#[derive(Clone)]
pub struct HeadlessRuntimeConfig {
    pub ts3_host: String,
    pub ts3_port: u16,
    pub nickname: String,
    pub server_password: String,
    pub channel_password: String,
    pub channel_path: String,
    pub channel_id: String,
    pub bot_respond_to_private: bool,
    pub bot_default_reply_mode: String,
    pub bot_trigger_prefixes: Vec<String>,
}

pub async fn run(config: HeadlessRuntimeConfig, shutdown: CancellationToken) -> Result<()> {
    let addr = INTERNAL_GRPC_ADDR.to_string();

    let (ts3_audio_tx, ts3_audio_rx) = mpsc::channel::<OutPacket>(200);
    let (ts3_notice_tx, ts3_notice_rx) = mpsc::channel::<(i32, u32, String)>(50);
    let (ts3_cmd_tx, ts3_cmd_rx) = mpsc::channel::<OutCommand>(50);

    let (events_tx, _events_rx) = broadcast::channel::<voicev1::Event>(512);

    let ts3_config = config.clone();
    let events_tx_clone = events_tx.clone();
    let ts3_shutdown = shutdown.clone();
    let ts3_task = tokio::spawn(async move {
        if let Err(e) = actor::ts3_actor(
            ts3_audio_rx,
            ts3_notice_rx,
            ts3_cmd_rx,
            events_tx_clone,
            ts3_shutdown,
            ts3_config,
        )
        .await
        {
            error!(%e, "ts3 actor exited");
        }
    });

    let persist_file = resolve_repo_relative("voice_state.json");

    let mut init_status = SharedStatus {
        state: 1,
        now_playing_title: String::new(),
        now_playing_source_url: String::new(),
        volume_percent: 100,
        fx_pan: 0.0,
        fx_width: 1.0,
        fx_swap_lr: false,
        fx_bass_db: 0.0,
        fx_reverb_mix: 0.0,
    };

    if let Some(ps) = types::load_persisted_voice_state(&persist_file) {
        init_status.volume_percent = ps.volume_percent.clamp(0, 200);
        init_status.fx_pan = ps.fx_pan.clamp(-1.0, 1.0);
        init_status.fx_width = ps.fx_width.clamp(0.0, 3.0);
        init_status.fx_swap_lr = ps.fx_swap_lr;
        init_status.fx_bass_db = ps.fx_bass_db.clamp(0.0, 18.0);
        init_status.fx_reverb_mix = ps.fx_reverb_mix.clamp(0.0, 1.0);
    }

    let (persist_tx, mut persist_rx) = mpsc::channel::<PersistedVoiceState>(32);
    {
        let persist_file = persist_file.clone();
        tokio::spawn(async move {
            let mut pending: Option<PersistedVoiceState> = None;
            let mut debounce: Option<Pin<Box<tokio::time::Sleep>>> = None;

            loop {
                tokio::select! {
                    r = persist_rx.recv() => {
                        match r {
                            Some(st) => {
                                pending = Some(st);
                                debounce = Some(Box::pin(tokio::time::sleep(std::time::Duration::from_millis(200))));
                            }
                            None => break,
                        }
                    }
                    _ = async {
                        if let Some(t) = debounce.as_mut() {
                            t.as_mut().await;
                        } else {
                            futures::future::pending::<()>().await;
                        }
                    } => {
                        if let Some(st) = pending.take() {
                            debounce = None;
                            if let Some(parent) = persist_file.parent() {
                                let _ = tokio::fs::create_dir_all(parent).await;
                            }
                            if let Ok(s) = serde_json::to_string_pretty(&st) {
                                let _ = tokio::fs::write(&persist_file, s).await;
                            }
                        }
                    }
                }
            }

            if let Some(st) = pending.take() {
                if let Some(parent) = persist_file.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                if let Ok(s) = serde_json::to_string_pretty(&st) {
                    let _ = tokio::fs::write(&persist_file, s).await;
                }
            }
        });
    }

    let svc = service::VoiceServiceImpl::new(
        Arc::new(Mutex::new(init_status)),
        ts3_audio_tx,
        ts3_notice_tx,
        ts3_cmd_tx,
        events_tx,
        persist_tx,
        config.bot_respond_to_private,
        config.bot_default_reply_mode.clone(),
        config.bot_trigger_prefixes.clone(),
    );

    let addr: std::net::SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(e) => return Err(anyhow!("invalid grpc address {addr}: {e}")),
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow!("grpc listen failed on {addr}: {e}"))?;

    info!("voice-service listening on {}", listener.local_addr()?);

    let server = tonic::transport::Server::builder()
        .add_service(VoiceServiceServer::new(svc))
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown.cancelled());

    tokio::select! {
        result = server => {
            if let Err(e) = result {
                error!("gRPC server failed: {e:?}");
            }
        }
        _ = shutdown.cancelled() => {
            info!("Voice service shutting down...");
        }
    }

    if let Err(e) = ts3_task.await {
        error!("Failed to wait for TS3 task: {e}");
    }

    info!("Voice service shutdown complete");
    Ok(())
}

pub struct Runtime {
    shutdown: CancellationToken,
    service_handle: Option<JoinHandle<()>>,
    bridge_handle: Option<JoinHandle<()>>,
}

impl Runtime {
    fn disabled() -> Self {
        Self {
            shutdown: CancellationToken::new(),
            service_handle: None,
            bridge_handle: None,
        }
    }

    pub fn start_if_enabled(
        config: Arc<AppConfig>,
        prompts: Arc<PromptsConfig>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
        ts_adapter: Arc<crate::adapter::TsAdapter>,
        ts_clients: Arc<DashMap<u32, ClientInfo>>,
    ) -> Self {
        if !config.headless.enabled {
            return Self::disabled();
        }

        let shutdown = CancellationToken::new();
        let hl_runtime = HeadlessRuntimeConfig {
            ts3_host: config.headless.ts3_host.clone(),
            ts3_port: config.headless.ts3_port,
            nickname: config.bot.nickname.clone(),
            server_password: config.headless.server_password.clone(),
            channel_password: config.headless.channel_password.clone(),
            channel_path: config.headless.channel_path.clone(),
            channel_id: config.headless.channel_id.clone(),
            bot_respond_to_private: config.bot.respond_to_private,
            bot_default_reply_mode: config.bot.default_reply_mode.clone(),
            bot_trigger_prefixes: config.bot.trigger_prefixes.clone(),
        };

        let shutdown_for_service = shutdown.clone();
        let service_handle = Some(tokio::spawn(async move {
            if let Err(e) = run(hl_runtime, shutdown_for_service).await {
                error!("headless service failed: {}", e);
            }
        }));

        info!("Headless voice service enabled");

        let bridge_config = config.clone();
        let bridge_prompts = prompts.clone();
        let bridge_gate = gate.clone();
        let bridge_llm = llm.clone();
        let bridge_registry = registry.clone();
        let bridge_ts_adapter = ts_adapter.clone();
        let bridge_ts_clients = ts_clients.clone();
        let shutdown_for_bridge = shutdown.clone();
        let bridge_handle = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_for_bridge.cancelled() => {
                        break;
                    }
                    run_result = crate::router::HeadlessLlmBridge::new(
                        bridge_config.clone(),
                        bridge_prompts.clone(),
                        bridge_gate.clone(),
                        bridge_llm.clone(),
                        bridge_registry.clone(),
                        bridge_ts_adapter.clone(),
                        bridge_ts_clients.clone(),
                    ).run() => {
                        if let Err(e) = run_result {
                            error!("headless LLM bridge failed: {}", e);
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            continue;
                        }
                        break;
                    }
                }
            }
        }));

        Self {
            shutdown,
            service_handle,
            bridge_handle,
        }
    }

    pub async fn shutdown(self) {
        self.shutdown.cancel();

        if let Some(handle) = self.bridge_handle {
            info!("Shutting down headless LLM bridge...");
            let _ = handle.await;
        }

        if let Some(handle) = self.service_handle {
            info!("Shutting down headless voice service...");
            let _ = handle.await;
        }
    }
}

fn resolve_repo_relative(path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    crate::config::config_dir().join(path)
}

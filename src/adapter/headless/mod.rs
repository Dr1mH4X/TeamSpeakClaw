use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::config::{AppConfig, PromptsConfig};
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
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
mod event;
pub mod speech;
mod types;
mod voice_service;

pub use self::event::{TextMessageEvent, TextMessageTarget, TsAdapter, TsEvent};

pub const INTERNAL_GRPC_ADDR: &str = "127.0.0.1:50051";

#[derive(Clone)]
pub struct HeadlessRuntimeConfig {
    pub bot_respond_to_private: bool,
    pub bot_default_reply_mode: String,
    pub bot_trigger_prefixes: Vec<String>,
}

pub async fn run(
    client: Arc<tsclient_rs::Client>,
    config: HeadlessRuntimeConfig,
    shutdown: CancellationToken,
) -> Result<()> {
    let addr = INTERNAL_GRPC_ADDR.to_string();

    let (ts3_audio_tx, ts3_audio_rx) = mpsc::channel::<(Vec<u8>, i32)>(200);
    let (ts3_notice_tx, ts3_notice_rx) = mpsc::channel::<(i32, u32, String)>(50);

    let (events_tx, _events_rx) = broadcast::channel::<voicev1::Event>(512);

    let ts3_client = client.clone();
    let events_tx_clone = events_tx.clone();
    let ts3_shutdown = shutdown.clone();
    let respond_private = config.bot_respond_to_private;
    let trigger_prefixes = config.bot_trigger_prefixes.clone();
    let default_reply = config.bot_default_reply_mode.clone();
    let ts3_task = tokio::spawn(async move {
        if let Err(e) = actor::ts3_actor(
            ts3_client,
            ts3_audio_rx,
            ts3_notice_rx,
            events_tx_clone,
            ts3_shutdown,
            respond_private,
            trigger_prefixes,
            default_reply,
        )
        .await
        {
            error!(%e, "ts3 actor exited");
        }
    });

    let svc = voice_service::VoiceServiceImpl::new(
        ts3_audio_tx,
        ts3_notice_tx,
        events_tx,
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

    info!(
        "Headless started, voice-service on {}",
        listener.local_addr()?
    );

    let server = tonic::transport::Server::builder()
        .add_service(VoiceServiceServer::new(svc))
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown.cancelled());

    tokio::select! {
        result = server => {
            if let Err(e) = result {
                error!("gRPC server failed: {e:?}");
            }
        }
        _ = shutdown.cancelled() => {}
    }

    if let Err(e) = ts3_task.await {
        error!("Failed to wait for TS3 task: {e}");
    }

    Ok(())
}

pub struct Runtime {
    shutdown: CancellationToken,
    service_handle: Option<JoinHandle<()>>,
    bridge_handle: Option<JoinHandle<()>>,
}

impl Runtime {
    pub fn start(
        config: Arc<AppConfig>,
        prompts: Arc<PromptsConfig>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
        ts_adapter: Arc<crate::adapter::TsAdapter>,
    ) -> Self {
        let voice_enabled = config.headless.stt.enabled || config.headless.tts.enabled;
        if !voice_enabled {
            info!("headless: voice disabled (stt/tts not enabled), management-only mode");
            return Self {
                shutdown: CancellationToken::new(),
                service_handle: None,
                bridge_handle: None,
            };
        }

        let shutdown = CancellationToken::new();
        let hl_runtime = HeadlessRuntimeConfig {
            bot_respond_to_private: config.bot.respond_to_private,
            bot_default_reply_mode: config.bot.default_reply_mode.clone(),
            bot_trigger_prefixes: config.bot.trigger_prefixes.clone(),
        };

        let shutdown_for_service = shutdown.clone();
        let ts_client = ts_adapter.get_client().clone();
        let service_handle = Some(tokio::spawn(async move {
            if let Err(e) = run(ts_client, hl_runtime, shutdown_for_service).await {
                error!("headless service failed: {}", e);
            }
        }));

        let bridge_config = config.clone();
        let bridge_prompts = prompts.clone();
        let bridge_gate = gate.clone();
        let bridge_llm = llm.clone();
        let bridge_registry = registry.clone();
        let bridge_ts_adapter = ts_adapter.clone();
        let shutdown_for_bridge = shutdown.clone();
        let bridge_handle = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_for_bridge.cancelled() => {
                        break;
                    }
                    run_result = crate::router::VoiceRouter::new(
                        bridge_config.clone(),
                        bridge_prompts.clone(),
                        bridge_gate.clone(),
                        bridge_llm.clone(),
                        bridge_registry.clone(),
                        bridge_ts_adapter.clone(),
                    ).run() => {
                        if let Err(e) = run_result {
                            error!("voice router failed: {}", e);
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
        info!("headless: shutting down");
        self.shutdown.cancel();

        if let Some(handle) = self.bridge_handle {
            let _ = handle.await;
        }

        if let Some(handle) = self.service_handle {
            let _ = handle.await;
        }
    }
}

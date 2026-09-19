use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::{JoinError, JoinHandle};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::adapter::reconnect::join_or_abort;
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::skills::SkillRegistry;

pub mod tsbot {
    pub mod voice {
        // tonic 生成代码：Status 变体过大来自库签名，手写层无法改根因
        #[allow(clippy::result_large_err)]
        pub mod v1 {
            tonic::include_proto!("tsbot.voice.v1");
        }
    }
}

use tsbot::voice::v1 as voicev1;
use voicev1::voice_service_server::VoiceServiceServer;

mod actor;
pub mod audio_codec;
pub mod audio_output;
mod event;
pub mod speaker_ring;
pub mod speech;
pub(crate) mod text_util;
mod voice_service;

use crate::skills::voice_audio::{VoiceAudioHandles, VoiceAudioRuntime};
use audio_output::{AudioBus, AudioOutput, PcmClipPayload};
use speaker_ring::ReplayFilter;

pub(crate) use self::event::{parse_server_groups, MainSubscriptions};
pub use self::event::{TextMessageEvent, TextMessageTarget, TsAdapter, TsEvent};
pub use self::speaker_ring::SpeakerRings;

pub const INTERNAL_GRPC_ADDR: &str = "127.0.0.1:50051";
const TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct VoiceBridgeStateInner {
    service_running: AtomicBool,
    stream_ready: AtomicBool,
    actor_ready: AtomicBool,
    connected_since_retry: AtomicBool,
}

#[derive(Clone, Default)]
pub struct VoiceBridgeState {
    inner: Arc<VoiceBridgeStateInner>,
}

impl VoiceBridgeState {
    pub fn is_ready(&self) -> bool {
        self.inner.service_running.load(Ordering::Acquire)
            && self.inner.stream_ready.load(Ordering::Acquire)
            && self.inner.actor_ready.load(Ordering::Acquire)
    }

    pub(crate) fn set_service_running(&self, running: bool) {
        self.inner.service_running.store(running, Ordering::Release);
    }

    /// actor 事件 handler 注册完成后置位
    pub(crate) fn set_actor_ready(&self, ready: bool) {
        self.inner.actor_ready.store(ready, Ordering::Release);
    }

    pub(crate) fn set_stream_ready(&self, ready: bool) {
        self.inner.stream_ready.store(ready, Ordering::Release);
        if ready {
            self.inner
                .connected_since_retry
                .store(true, Ordering::Release);
        }
    }

    fn take_connected_since_retry(&self) -> bool {
        self.inner
            .connected_since_retry
            .swap(false, Ordering::AcqRel)
    }
}

pub fn voice_features_enabled(config: &AppConfig) -> bool {
    config.headless.stt.enabled
        || config.headless.tts.enabled
        || config.llm.omni_model
        || config.voice_replay.enabled
}

/// bot 麦克风/扬声器开关：与 STT/TTS/omni 配置对齐
pub struct VoiceMuteFlags {
    pub input_muted: bool,
    pub input_hardware_on: bool,
    pub output_muted: bool,
    pub output_hardware_on: bool,
}

pub fn voice_mute_flags(config: &AppConfig) -> VoiceMuteFlags {
    let speaker_on = voice_features_enabled(config);
    let mic_on = config.headless.tts.enabled;
    VoiceMuteFlags {
        input_muted: !mic_on,
        input_hardware_on: mic_on,
        output_muted: !speaker_on,
        output_hardware_on: speaker_on,
    }
}

pub fn should_route_text_through_bridge(voice_configured: bool, bridge_ready: bool) -> bool {
    voice_configured && bridge_ready
}

/// actor 旁路：录制入环钩子（bot/音乐 bot 在此过滤）；仅 voice_replay.enabled 时装配
#[derive(Clone)]
pub struct SpeakerRecordHook {
    pub rings: Arc<SpeakerRings>,
    pub bot_clid: u32,
    pub musicbot_name: String,
}

/// headless 运行时音频：出站总线 + 录制环（window 来自 voice_replay.window_secs）
pub struct HeadlessVoiceRuntime {
    pub audio: AudioBus,
    pub speaker_rings: Arc<SpeakerRings>,
}

/// 阶段 A 装配自检：写入路径 warmup + 录制窗快照日志，确保 API 在运行时可达
async fn warm_audio_surface(output: &AudioOutput, rings: &SpeakerRings) -> Result<()> {
    let handle = output
        .enqueue_pcm_clip(PcmClipPayload {
            samples: Vec::new(),
            sample_rate: 48_000,
            channels: 2,
        })
        .context("audio output enqueue warmup failed")?;
    handle.cancel();
    handle
        .wait()
        .await
        .context("audio output warmup clip wait failed")?;

    // 外部源入队 + wait 路径预热：非法 codec 由消费者跳过
    output
        .play_encoded_media(vec![0u8; 2], "warmup-probe")
        .await
        .context("audio output encoded warmup failed")?;
    output
        .play_encoded_media_wait(vec![0u8; 2], "warmup-probe-wait")
        .await
        .context("audio output encoded wait warmup failed")?;

    let status = output.status();
    if let Some(info) = &status.current {
        info!(
            source = ?info.source,
            kind = ?info.kind,
            started_at = ?info.started_at,
            "audio output current job during warmup"
        );
    }
    let _ = status
        .current
        .as_ref()
        .map(|info| info.started_at.elapsed());
    info!(
        queued_jobs = status.queued_jobs,
        last_error = ?status.last_error,
        "audio output surface warmed"
    );

    let stats = rings.stats();
    let snapshot = rings.snapshot(ReplayFilter::All, Some(1));
    info!(
        speakers = snapshot.speakers.len(),
        buffered_ms = snapshot.buffered_ms,
        sample_rate = snapshot.sample_rate,
        channels = snapshot.channels,
        samples = snapshot.samples.len(),
        "speaker rings surface ready"
    );
    if let Some(first) = stats.first() {
        let single = rings.snapshot(ReplayFilter::Speaker { clid: first.clid }, Some(1));
        info!(
            clid = first.clid,
            name = %first.name,
            active_ms = first.active_ms,
            single_samples = single.samples.len(),
            "speaker ring track"
        );
    }
    match rings.resolve_name("") {
        speaker_ring::NameResolve::Unique(clid) => {
            info!(clid, "speaker name resolve unique");
        }
        speaker_ring::NameResolve::Ambiguous(names) => {
            info!(candidates = names.len(), "speaker name resolve ambiguous");
        }
        speaker_ring::NameResolve::None => {}
    }
    Ok(())
}

struct ServiceRunningGuard {
    bridge_state: VoiceBridgeState,
}

impl ServiceRunningGuard {
    fn new(bridge_state: VoiceBridgeState) -> Self {
        bridge_state.set_service_running(true);
        Self { bridge_state }
    }
}

impl Drop for ServiceRunningGuard {
    fn drop(&mut self) {
        self.bridge_state.set_service_running(false);
    }
}

fn component_result(
    result: std::result::Result<Result<()>, JoinError>,
    component: &str,
) -> Result<()> {
    result
        .with_context(|| format!("failed to join {component}"))?
        .with_context(|| format!("{component} failed"))
}

#[derive(Clone, Copy)]
enum HeadlessComponent {
    Actor,
    Server,
}

impl HeadlessComponent {
    fn name(self) -> &'static str {
        match self {
            Self::Actor => "TS3 actor",
            Self::Server => "gRPC server",
        }
    }
}

/// 解析并绑定内部 gRPC 监听地址；bind 失败直接返回 Err，避免"Bot ready 但语音全挂"
async fn bind_grpc_listener() -> Result<tokio::net::TcpListener> {
    let addr = INTERNAL_GRPC_ADDR.to_string();
    let addr: std::net::SocketAddr = addr
        .parse()
        .map_err(|error| anyhow!("invalid grpc address {addr}: {error}"))?;
    tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|error| anyhow!("grpc listen failed on {addr}: {error}"))
}

pub async fn run(
    client: Arc<tsclient_rs::Client>,
    listener: tokio::net::TcpListener,
    config: Arc<AppConfig>,
    shutdown: CancellationToken,
    bridge_state: VoiceBridgeState,
    bot_clid: u32,
    voice_runtime: HeadlessVoiceRuntime,
) -> Result<()> {
    let (ts3_audio_tx, ts3_audio_rx) = mpsc::channel::<(Vec<u8>, i32)>(200);

    // 统一出站：消费者是 ts3_audio_tx 的唯一写端
    let HeadlessVoiceRuntime {
        audio,
        speaker_rings,
    } = voice_runtime;
    let AudioBus {
        output: audio_output,
        consumer: audio_consumer,
    } = audio;
    let consumer_shutdown = shutdown.clone();
    let consumer_task = tokio::spawn(async move {
        audio_consumer.run(ts3_audio_tx).await;
        let _ = consumer_shutdown;
    });

    warm_audio_surface(&audio_output, &speaker_rings).await?;
    let record_hook = if config.voice_replay.enabled {
        Some(SpeakerRecordHook {
            rings: speaker_rings,
            bot_clid,
            musicbot_name: config
                .music_backend
                .as_ref()
                .map(|mc| mc.musicbot_name.clone())
                .unwrap_or_default(),
        })
    } else {
        None
    };

    // 控制事件（chat/log）与音频事件分离广播：音频洪峰不能挤掉聊天
    let (control_tx, _) = broadcast::channel::<voicev1::Event>(256);
    let (audio_tx, _) = broadcast::channel::<voicev1::Event>(1024);

    let actor_bridge_state = bridge_state.clone();
    let mut actor_task = tokio::spawn(actor::ts3_actor(
        client.clone(),
        ts3_audio_rx,
        actor::ActorEventChannels {
            control_tx: control_tx.clone(),
            audio_tx: audio_tx.clone(),
        },
        shutdown.clone(),
        actor_bridge_state,
        record_hook,
    ));

    let svc = voice_service::VoiceServiceImpl::new(
        audio_output.clone(),
        client,
        control_tx,
        audio_tx,
        config.bot.default_reply_mode.clone(),
    );

    info!(
        "Headless started, voice-service on {}",
        listener.local_addr()?
    );

    let server_shutdown = shutdown.clone();
    let mut server_task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(VoiceServiceServer::new(svc))
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                server_shutdown.cancelled(),
            )
            .await
            .context("gRPC server failed")
    });
    let service_guard = ServiceRunningGuard::new(bridge_state);

    let (first_component, first_result) = tokio::select! {
        result = &mut actor_task => (HeadlessComponent::Actor, result),
        result = &mut server_task => (HeadlessComponent::Server, result),
    };
    let shutdown_requested = shutdown.is_cancelled();
    drop(service_guard);
    shutdown.cancel();
    let _stopped_clips = audio_output.stop_clips();

    let other_result = match first_component {
        HeadlessComponent::Actor => {
            join_or_abort(&mut server_task, "gRPC server", TASK_SHUTDOWN_TIMEOUT)
                .await
                .and_then(|result| result)
        }
        HeadlessComponent::Server => {
            join_or_abort(&mut actor_task, "TS3 actor", TASK_SHUTDOWN_TIMEOUT)
                .await
                .and_then(|result| result)
        }
    };
    consumer_task.abort();
    let _ = consumer_task.await;
    let first_result = component_result(first_result, first_component.name());

    if let Err(error) = first_result {
        if let Err(other_error) = other_result {
            error!("Headless companion task also failed: {other_error}");
        }
        return Err(error);
    }
    other_result?;

    if shutdown_requested {
        Ok(())
    } else {
        Err(anyhow!("{} stopped unexpectedly", first_component.name()))
    }
}

pub struct Runtime {
    shutdown: CancellationToken,
    service_handle: Option<JoinHandle<()>>,
    bridge_handle: Option<JoinHandle<()>>,
    /// 组件失败观察通道：None 表示正常，Some(组件名) 表示组件异常退出
    failed_tx: watch::Sender<Option<&'static str>>,
}

/// 将服务任务退出结果映射为失败信号：Ok 视为正常（None），Err 视为失败（Some）
fn service_exit_signal(result: &Result<()>) -> Option<&'static str> {
    result.as_ref().err().map(|_| "headless service")
}

/// headless Runtime::start 入参（避免参数列表过长）
pub struct HeadlessStartHandles {
    pub ts_adapter: Arc<TsAdapter>,
    pub bridge_state: VoiceBridgeState,
    pub voice_audio: VoiceAudioHandles,
}

impl Runtime {
    pub async fn start(
        config: Arc<AppConfig>,
        prompts: Arc<PromptsConfig>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
        handles: HeadlessStartHandles,
    ) -> Result<Self> {
        let HeadlessStartHandles {
            ts_adapter,
            bridge_state,
            voice_audio,
        } = handles;
        let voice_enabled = voice_features_enabled(&config);
        if !voice_enabled {
            bridge_state.set_service_running(false);
            bridge_state.set_stream_ready(false);
            info!("headless: voice disabled (stt/tts/omni/voice_replay not enabled), management-only mode");
            let (failed_tx, _) = watch::channel::<Option<&'static str>>(None);
            return Ok(Self {
                shutdown: CancellationToken::new(),
                service_handle: None,
                bridge_handle: None,
                failed_tx,
            });
        }

        // 先完成 gRPC bind，失败明确返回 Err，避免后台任务只打日志导致语音静默全挂
        let listener = bind_grpc_listener().await?;

        let shutdown = CancellationToken::new();
        let (failed_tx, _failed_rx) = watch::channel::<Option<&'static str>>(None);

        // 出站/录制在 Runtime 装配：window 来自 voice_replay.window_secs；句柄供技能读取
        let window_secs = config.voice_replay.window_secs;
        let speaker_rings = Arc::new(SpeakerRings::new(Duration::from_secs(u64::from(
            window_secs,
        ))));
        let audio_bus = AudioBus::new();
        voice_audio.install(VoiceAudioRuntime {
            audio_output: audio_bus.output.clone(),
            speaker_rings: speaker_rings.clone(),
            window_secs,
        });
        let service_voice_runtime = HeadlessVoiceRuntime {
            audio: AudioBus {
                output: audio_bus.output.clone(),
                consumer: audio_bus.consumer,
            },
            speaker_rings,
        };
        let router_audio_output = audio_bus.output.clone();

        let shutdown_for_service = shutdown.clone();
        let service_bridge_state = bridge_state.clone();
        let failed_tx_for_service = failed_tx.clone();
        let ts_client = ts_adapter.get_client().clone();
        let bot_clid = ts_adapter.get_bot_clid();
        let config_for_service = config.clone();
        let service_handle = Some(tokio::spawn(async move {
            let result = run(
                ts_client,
                listener,
                config_for_service,
                shutdown_for_service.clone(),
                service_bridge_state.clone(),
                bot_clid,
                service_voice_runtime,
            )
            .await;
            service_bridge_state.set_service_running(false);
            service_bridge_state.set_stream_ready(false);
            if let Err(error) = &result {
                error!("headless service failed: {error}");
            }
            // 正常退出（Ok）置回正常值，异常退出（Err）置失败信号
            let _ = failed_tx_for_service.send(service_exit_signal(&result));
            shutdown_for_service.cancel();
        }));

        let bridge_config = config.clone();
        let bridge_prompts = prompts.clone();
        let bridge_gate = gate.clone();
        let bridge_llm = llm.clone();
        let bridge_registry = registry.clone();
        let bridge_ts_adapter = ts_adapter.clone();
        let shutdown_for_bridge = shutdown.clone();
        let bridge_state_for_router = bridge_state.clone();
        let bridge_audio_output = router_audio_output.clone();
        let bridge_voice_audio = voice_audio;
        let bridge_task = tokio::spawn(async move {
            let mut attempt = 1u32;
            let _ = bridge_state_for_router.take_connected_since_retry();
            loop {
                bridge_state_for_router.set_stream_ready(false);
                let run_result = tokio::select! {
                    biased;
                    _ = shutdown_for_bridge.cancelled() => break,
                    result = crate::router::VoiceRouter::new(
                        crate::router::VoiceRouterHandles {
                            config: bridge_config.clone(),
                            prompts: bridge_prompts.clone(),
                            gate: bridge_gate.clone(),
                            llm: bridge_llm.clone(),
                            registry: bridge_registry.clone(),
                            ts_adapter: bridge_ts_adapter.clone(),
                            bridge_state: bridge_state_for_router.clone(),
                            audio_output: bridge_audio_output.clone(),
                            voice_audio: bridge_voice_audio.clone(),
                        },
                    ).run(shutdown_for_bridge.clone()) => result,
                };
                bridge_state_for_router.set_stream_ready(false);
                if shutdown_for_bridge.is_cancelled() {
                    break;
                }

                if bridge_state_for_router.take_connected_since_retry() {
                    attempt = 1;
                }
                match run_result {
                    Ok(()) => error!("voice router stopped unexpectedly"),
                    Err(error) => error!("voice router failed: {error}"),
                }

                let delay = crate::adapter::reconnect::reconnect_delay_for_attempt(attempt);
                warn!(
                    attempt,
                    delay_secs = delay.as_secs(),
                    "voice router unavailable; TeamSpeak text fallback is active"
                );
                if !crate::adapter::reconnect::wait_for_retry(delay, &shutdown_for_bridge).await {
                    break;
                }
                attempt = attempt.saturating_add(1);
            }
            bridge_state_for_router.set_stream_ready(false);
        });
        // 监控 bridge 任务 panic：panic 不可自愈，置失败信号让上层重建会话
        let failed_tx_for_bridge = failed_tx.clone();
        let bridge_state_watcher = bridge_state;
        let bridge_handle = Some(tokio::spawn(async move {
            let result = bridge_task.await;
            if let Err(error) = result {
                if error.is_panic() {
                    bridge_state_watcher.set_stream_ready(false);
                    let _ = failed_tx_for_bridge.send(Some("voice router"));
                    error!("voice router task panicked; signaling session restart");
                }
            }
        }));

        Ok(Self {
            shutdown,
            service_handle,
            bridge_handle,
            failed_tx,
        })
    }

    /// 获取组件失败观察通道（None 表示正常）
    pub(crate) fn failure_receiver(&self) -> watch::Receiver<Option<&'static str>> {
        self.failed_tx.subscribe()
    }

    pub async fn shutdown(self) {
        info!("headless: shutting down");
        // 先置正常标志，防止任务退出被误判为组件失败
        let _ = self.failed_tx.send(None);
        self.shutdown.cancel();

        if let Some(mut handle) = self.bridge_handle {
            if let Err(error) =
                join_or_abort(&mut handle, "voice router", TASK_SHUTDOWN_TIMEOUT).await
            {
                warn!("Failed to stop voice router: {error}");
            }
        }

        if let Some(mut handle) = self.service_handle {
            if let Err(error) =
                join_or_abort(&mut handle, "headless service", TASK_SHUTDOWN_TIMEOUT).await
            {
                warn!("Failed to stop headless service: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_routing_truth_table_requires_configuration_and_ready_bridge() {
        assert!(!should_route_text_through_bridge(false, false));
        assert!(!should_route_text_through_bridge(false, true));
        assert!(!should_route_text_through_bridge(true, false));
        assert!(should_route_text_through_bridge(true, true));
    }

    #[test]
    fn bridge_state_requires_service_stream_and_actor() {
        let state = VoiceBridgeState::default();
        assert!(!state.is_ready());

        state.set_actor_ready(true);
        assert!(!state.is_ready());
        state.set_stream_ready(true);
        assert!(!state.is_ready());
        state.set_service_running(true);
        assert!(state.is_ready());

        state.set_service_running(false);
        assert!(!state.is_ready());
        state.set_stream_ready(false);
        assert!(!state.is_ready());
        state.set_actor_ready(false);
        assert!(!state.is_ready());
    }

    #[test]
    fn bridge_state_is_unready_before_actor_handler_registration() {
        let state = VoiceBridgeState::default();
        state.set_service_running(true);
        state.set_stream_ready(true);

        // actor 未就绪时即使服务与订阅流都正常也不就绪
        assert!(!state.is_ready());

        state.set_actor_ready(true);
        assert!(state.is_ready());
    }

    #[test]
    fn service_guard_marks_unready_when_dropped() {
        let state = VoiceBridgeState::default();
        state.set_stream_ready(true);
        state.set_actor_ready(true);

        {
            let _guard = ServiceRunningGuard::new(state.clone());
            assert!(state.is_ready());
        }

        assert!(!state.is_ready());
    }

    #[test]
    fn service_exit_signal_maps_failure_to_component_name() {
        assert_eq!(service_exit_signal(&Ok(())), None);
        assert_eq!(
            service_exit_signal(&Err(anyhow::anyhow!("boom"))),
            Some("headless service")
        );
    }

    #[test]
    fn voice_mute_flags_follow_stt_tts_omni() {
        let mut config = AppConfig::default();
        let flags = voice_mute_flags(&config);
        assert!(flags.input_muted);
        assert!(!flags.input_hardware_on);
        assert!(flags.output_muted);
        assert!(!flags.output_hardware_on);

        config.headless.stt.enabled = true;
        let flags = voice_mute_flags(&config);
        assert!(flags.input_muted);
        assert!(!flags.output_muted);
        assert!(flags.output_hardware_on);

        config.headless.tts.enabled = true;
        let flags = voice_mute_flags(&config);
        assert!(!flags.input_muted);
        assert!(flags.input_hardware_on);
        assert!(!flags.output_muted);
    }

    #[test]
    fn voice_features_include_voice_replay_flag() {
        let mut config = AppConfig::default();
        assert!(!voice_features_enabled(&config));
        config.voice_replay.enabled = true;
        assert!(voice_features_enabled(&config));
        let flags = voice_mute_flags(&config);
        assert!(flags.output_hardware_on);
        assert!(flags.input_muted);
    }

    #[tokio::test]
    async fn failure_channel_propagates_abnormal_exit_and_resets_on_shutdown() {
        let (tx, mut rx) = watch::channel::<Option<&'static str>>(None);

        // 组件异常退出 → 观察者收到失败信号
        tx.send(Some("headless service")).unwrap();
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow_and_update(), Some("headless service"));

        // 正常 shutdown 置回正常值 → 观察者收到复位
        tx.send(None).unwrap();
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow_and_update(), None);
    }

    #[tokio::test]
    async fn grpc_bind_fails_when_port_already_occupied() {
        let _listener = bind_grpc_listener().await.expect("首次 bind 必须成功");

        let second = bind_grpc_listener().await;

        assert!(second.is_err());
    }

    #[test]
    fn bridge_connection_latch_is_consumed_once() {
        let state = VoiceBridgeState::default();
        state.set_stream_ready(true);

        assert!(state.take_connected_since_retry());
        assert!(!state.take_connected_since_retry());
    }

    #[tokio::test]
    async fn stalled_task_is_aborted_after_shutdown_timeout() {
        let mut task = tokio::spawn(std::future::pending::<()>());

        let result = join_or_abort(&mut task, "test task", Duration::from_millis(1)).await;

        assert!(result.is_err());
        assert!(task.is_finished());
    }
}

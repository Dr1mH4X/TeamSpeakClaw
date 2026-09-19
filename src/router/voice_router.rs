use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures_util::StreamExt;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use crate::adapter::headless::audio_output::{AudioOutput, TtsSession};
use crate::adapter::headless::speech::{
    detect_audio_format, is_speakable, pcm16_mono_to_wav_bytes, preprocess_stt_text,
    preprocess_text_message, OpenAiSpeechProvider, OpusSttPipeline, SpeechChunk,
};
use crate::adapter::headless::tsbot::voice::v1 as voicev1;
use crate::adapter::headless::{
    parse_server_groups, TsAdapter, VoiceBridgeState, INTERNAL_GRPC_ADDR,
};
use crate::adapter::reconnect::{
    abort_managed_tasks, now_unix_ms, wait_for_retry, ReconnectState, RetryDecision,
};
use crate::config::{reply_target_mode, AppConfig, PromptsConfig};
use crate::llm::{LlmEngine, SessionSource, StreamCallbacks};
use crate::permission::PermissionGate;
use crate::router::{resolve_ts_inbound, run_llm_turn, LLM_ERROR_REPLY};

use crate::skills::{SkillRegistry, TsCaller, UnifiedExecutionContext};
use tokio_util::sync::CancellationToken;
use voicev1::voice_service_client::VoiceServiceClient;

const AUDIO_MAX_IN_FLIGHT: usize = 8;

struct CallerContext {
    caller_id: u32,
    caller_uid: String,
    caller_name: String,
    groups: Vec<u32>,
    channel_group_id: u32,
    channel_id: u64,
    reply_target_mode: i32,
    reply_target_client_id: u32,
}

/// finish_reason 产出端：`src/llm/provider.rs` 解析 SSE `choice.finish_reason`
/// 非空时发出 `LlmStreamEvent::Done { finish_reason, tool_calls }`。
/// tool 环校验（`src/llm/tool_loop.rs`）：有 tool 调用时 finish_reason 必须为字面量
/// `"tool_calls"`。
///
/// 本函数恒返回 true：任意 finish_reason（含 `"tool_calls"`）都先 finish 当前 TTS 会话，
/// 再执行 tool / 最终回复。保留函数作为文档锚点与测试入口，防止改回旧语义
/// （旧：`finish_reason == "tool_calls"` 时不关会话）。
fn should_close_tts_turn(finish_reason: &str) -> bool {
    let is_tool_calls = finish_reason == "tool_calls";
    let _ = is_tool_calls;
    true
}

/// 每轮 TTS 运行时：句段通道 + AudioOutput 会话；finish 关句段并让 synth 收尾会话
type TtsSentenceSender = Arc<std::sync::Mutex<Option<mpsc::Sender<(usize, String)>>>>;
type SharedTtsSession = Arc<tokio::sync::Mutex<Option<TtsSession>>>;

struct TtsTurnRuntime {
    callbacks: StreamCallbacks,
    shared_tx: TtsSentenceSender,
    shared_session: SharedTtsSession,
    synth_task: tokio::task::JoinHandle<()>,
    trace_id: String,
}

impl TtsTurnRuntime {
    const TTS_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);

    fn callbacks(&self) -> Option<&StreamCallbacks> {
        Some(&self.callbacks)
    }

    /// 关闭句段通道；synth 冲刷后 finish 会话（接收语义，不等待播完）
    async fn finish(self) {
        *self.shared_tx.lock().expect("tts tx poisoned") = None;
        match tokio::time::timeout(Self::TTS_TEARDOWN_TIMEOUT, self.synth_task).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                error!(
                    trace_id = %self.trace_id,
                    error = %error,
                    "tts synth task failed"
                );
            }
            Err(_) => {
                error!(
                    trace_id = %self.trace_id,
                    "tts synth task teardown timed out"
                );
                if let Some(session) = self.shared_session.lock().await.take() {
                    drop(session);
                }
            }
        }
    }

    /// 取消：abort 合成任务并 drop 未 finish 的会话（消费者 abort 该 job）
    async fn abort(self) {
        self.synth_task.abort();
        let _ = self.synth_task.await;
        if let Some(session) = self.shared_session.lock().await.take() {
            drop(session);
        }
    }
}

struct VoiceBridgeReadyGuard {
    bridge_state: VoiceBridgeState,
}

impl VoiceBridgeReadyGuard {
    fn new(bridge_state: VoiceBridgeState) -> Self {
        bridge_state.set_stream_ready(true);
        Self { bridge_state }
    }
}

impl Drop for VoiceBridgeReadyGuard {
    fn drop(&mut self) {
        self.bridge_state.set_stream_ready(false);
    }
}

pub struct VoiceRouter {
    config: Arc<AppConfig>,
    prompts: Arc<PromptsConfig>,
    gate: Arc<PermissionGate>,
    llm: Arc<LlmEngine>,
    registry: Arc<SkillRegistry>,
    ts_adapter: Arc<TsAdapter>,
    audio_pipeline: Mutex<Option<OpusSttPipeline>>,
    speech_provider: Option<Arc<OpenAiSpeechProvider>>,
    bridge_state: VoiceBridgeState,
    /// 统一出站：TTS/clip/外部流 FIFO；并发由 TurnCoordinator + 本层队列承担
    audio_output: AudioOutput,
    /// 直呼/技能共享的录制与出站句柄
    voice_audio: crate::skills::VoiceAudioHandles,
}

/// VoiceRouter 装配句柄（避免构造参数列表过长）
pub struct VoiceRouterHandles {
    pub config: Arc<AppConfig>,
    pub prompts: Arc<PromptsConfig>,
    pub gate: Arc<PermissionGate>,
    pub llm: Arc<LlmEngine>,
    pub registry: Arc<SkillRegistry>,
    pub ts_adapter: Arc<TsAdapter>,
    pub bridge_state: VoiceBridgeState,
    pub audio_output: AudioOutput,
    pub voice_audio: crate::skills::VoiceAudioHandles,
}

impl VoiceRouter {
    const STREAM_TTS_MIN_CHARS: usize = 4;
    const STREAM_TTS_WEAK_PUNCT_MIN_CHARS: usize = 8;
    const STREAM_TTS_MAX_CHARS: usize = 28;

    pub fn new(handles: VoiceRouterHandles) -> Self {
        let VoiceRouterHandles {
            config,
            prompts,
            gate,
            llm,
            registry,
            ts_adapter,
            bridge_state,
            audio_output,
            voice_audio,
        } = handles;
        let speech_provider =
            OpenAiSpeechProvider::new(config.clone(), prompts.tts.style_prompt.clone())
                .ok()
                .map(Arc::new);
        let need_audio_pipeline = config.headless.stt.enabled || config.llm.omni_model;
        Self {
            audio_pipeline: Mutex::new(need_audio_pipeline.then(OpusSttPipeline::new)),
            config,
            prompts,
            gate,
            llm,
            registry,
            ts_adapter,
            speech_provider,
            bridge_state,
            audio_output,
            voice_audio,
        }
    }

    fn is_tts_effectively_enabled(&self) -> bool {
        self.config.headless.tts.enabled && self.speech_provider.is_some()
    }

    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        self.bridge_state.set_stream_ready(false);
        let endpoint = format!("http://{}", INTERNAL_GRPC_ADDR);
        let channel = Channel::from_shared(endpoint.clone())?.connect().await?;
        let mut client = VoiceServiceClient::new(channel);

        let req = tonic::Request::new(voicev1::SubscribeRequest {
            include_chat: true,
            include_audio: self.config.headless.stt.enabled || self.config.llm.omni_model,
        });
        let mut stream = client.subscribe_events(req).await?.into_inner();
        let ready_guard = VoiceBridgeReadyGuard::new(self.bridge_state.clone());
        let router = Arc::new(self);
        let mut tasks = JoinSet::new();

        // 完整 utterance 的独立有界队列：满时丢弃最新完整语音段，聊天不受影响
        let (audio_chunk_tx, mut audio_chunk_rx) = tokio::sync::mpsc::channel::<(
            voicev1::AudioFrameEvent,
            SpeechChunk,
        )>(AUDIO_MAX_IN_FLIGHT);

        let drain_router = router.clone();
        tasks.spawn(async move {
            let mut client = None;
            let mut reconnect_state = ReconnectState::default();
            loop {
                let Some((audio, chunk)) = audio_chunk_rx.recv().await else {
                    break;
                };
                // 连接失败时重试，不永久退出；重试期间队列照常消费（不阻塞聊天）
                if client.is_none() {
                    let connect_result: anyhow::Result<Channel> =
                        match Channel::from_shared(format!("http://{INTERNAL_GRPC_ADDR}")) {
                            Ok(channel) => channel
                                .connect()
                                .await
                                .map_err(|error| anyhow::anyhow!("connect failed: {error}")),
                            Err(error) => Err(anyhow::anyhow!("invalid endpoint: {error}")),
                        };
                    match connect_result {
                        Ok(channel) => {
                            client = Some(VoiceServiceClient::new(channel));
                            reconnect_state.record_session_started();
                        }
                        Err(error) => {
                            // 重试策略委托 reconnect 工具：失败计数与退避等待不在此处自行实现
                            let RetryDecision::Retry { attempt, delay } =
                                reconnect_state.record_failure()
                            else {
                                warn!(error = %error, "voice audio worker reconnect attempts exhausted; giving up");
                                break;
                            };
                            warn!(
                                attempt = attempt,
                                error = %error,
                                "voice audio worker connect failed; retrying"
                            );
                            // 退避等待感知取消令牌：shutdown 触发时立即退出
                            if !wait_for_retry(delay, &shutdown).await {
                                break;
                            }
                            continue;
                        }
                    }
                }

                let mut handle_error = None;
                if let Some(client_ref) = client.as_mut() {
                    if let Err(error) = drain_router
                        .handle_audio_chunk(client_ref, audio, chunk)
                        .await
                    {
                        handle_error = Some(error);
                    }
                }
                if let Some(error) = handle_error {
                    // gRPC 调用失败可能是连接失效：丢弃客户端触发重连
                    client = None;
                    error!("Voice router audio handling failed: {error}");
                }
            }
        });

        let mut drain_tick = tokio::time::interval(Duration::from_millis(100));
        drain_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let result = loop {
            tokio::select! {
                item = stream.next() => {
                    let Some(item) = item else {
                        break Err(anyhow::anyhow!("voice event stream ended"));
                    };
                    let ev = match item {
                        Ok(ev) => ev,
                        Err(error) => {
                            break Err(anyhow::anyhow!("voice event stream error: {error}"));
                        }
                    };
                    let Some(payload) = ev.payload else {
                        continue;
                    };
                    match payload {
                        voicev1::event::Payload::Chat(chat) => {
                            let router = router.clone();
                            let mut client = client.clone();
                            tasks.spawn(async move {
                                if let Err(error) = router.handle_chat_event(&mut client, chat).await {
                                    error!("Voice router chat handling failed: {error}");
                                }
                            });
                        }
                        voicev1::event::Payload::Audio(audio) => {
                            match router.process_audio_frame(&audio).await {
                                Ok(Some(chunk)) => {
                                    if let Err(error) = audio_chunk_tx.try_send((audio, chunk)) {
                                        warn!(
                                            dropped = 1,
                                            error = %error,
                                            "audio worker queue full; dropping latest utterance"
                                        );
                                    }
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    error!("Voice router audio decoding failed: {error}");
                                }
                            }
                        }
                    }
                }
                _ = drain_tick.tick() => {
                    let chunks = {
                        let mut guard = router.audio_pipeline.lock().await;
                        guard
                            .as_mut()
                            .map(|pipeline| pipeline.drain_inactive(std::time::Instant::now()))
                            .unwrap_or_default()
                    };
                    for chunk in chunks {
                        // drain 产物无原始 audio 事件，用 chunk 自身信息构造事件
                        let audio = voicev1::AudioFrameEvent {
                            from_client_id: chunk.speaker_client_id,
                            from_client_name: chunk.speaker_name.clone(),
                            codec: 4,
                            frame: Vec::new(),
                        };
                        if let Err(error) = audio_chunk_tx.try_send((audio, chunk)) {
                            warn!(
                                dropped = 1,
                                error = %error,
                                "audio worker queue full; dropping drained utterance"
                            );
                        }
                    }
                }
                task = tasks.join_next(), if !tasks.is_empty() => {
                    match task {
                        Some(Ok(())) => {}
                        Some(Err(error)) => {
                            break Err(anyhow::anyhow!("voice router task failed: {error}"));
                        }
                        None => {
                            break Err(anyhow::anyhow!("voice router task set closed"));
                        }
                    }
                }
            }
        };

        drop(ready_guard);
        abort_managed_tasks(&mut tasks, "Voice router").await;
        result
    }

    async fn resolve_caller_from_chat(
        &self,
        chat: &voicev1::ChatEvent,
        reply_target_mode: i32,
        reply_target_client_id: u32,
    ) -> Result<CallerContext> {
        let caller_uid = if chat.invoker_unique_id.is_empty() {
            format!("clid:{}", chat.invoker_client_id)
        } else {
            chat.invoker_unique_id.clone()
        };
        let clients = self
            .ts_adapter
            .list_clients()
            .await
            .map_err(|error| anyhow::anyhow!("list chat caller failed: {error}"))?;
        let caller = clients
            .iter()
            .find(|client| u32::try_from(client.id).ok() == Some(chat.invoker_client_id))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "chat caller {} not found in online clients",
                    chat.invoker_client_id
                )
            })?;
        let channel_group_id = self
            .ts_adapter
            .get_client_channel_group_id(chat.invoker_client_id)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "resolve chat caller {} channel group failed: {error}",
                    chat.invoker_client_id
                )
            })?;
        let groups = parse_server_groups(&caller.server_groups);
        debug!(
            "Resolved chat caller '{}' from online list: clid={}",
            chat.invoker_name, caller.id
        );
        Ok(CallerContext {
            caller_id: chat.invoker_client_id,
            caller_uid,
            caller_name: chat.invoker_name.clone(),
            groups,
            channel_group_id,
            channel_id: caller.channel_id,
            reply_target_mode,
            reply_target_client_id,
        })
    }

    async fn resolve_caller_from_audio(
        &self,
        audio: &voicev1::AudioFrameEvent,
    ) -> Result<CallerContext> {
        let reply_target_mode = reply_target_mode(self.config.bot.default_reply_mode.as_str());
        let reply_target_client_id = if reply_target_mode == 1 {
            audio.from_client_id
        } else {
            0
        };
        let clients = self
            .ts_adapter
            .list_clients()
            .await
            .map_err(|error| anyhow::anyhow!("list audio caller failed: {error}"))?;
        let caller = clients
            .iter()
            .find(|client| u32::try_from(client.id).ok() == Some(audio.from_client_id))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "audio caller {} not found in online clients",
                    audio.from_client_id
                )
            })?;
        let channel_group_id = self
            .ts_adapter
            .get_client_channel_group_id(audio.from_client_id)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "resolve audio caller {} channel group failed: {error}",
                    audio.from_client_id
                )
            })?;
        let groups = parse_server_groups(&caller.server_groups);
        debug!(
            "Resolved audio caller '{}' from online list: clid={}",
            audio.from_client_name, caller.id
        );
        Ok(CallerContext {
            caller_id: audio.from_client_id,
            caller_uid: caller.uid.clone(),
            caller_name: caller.nickname.clone(),
            groups,
            channel_group_id,
            channel_id: caller.channel_id,
            reply_target_mode,
            reply_target_client_id,
        })
    }

    fn should_ignore_chat(&self, chat: &voicev1::ChatEvent, caller_id: u32) -> bool {
        if chat.invoker_name == self.config.bot.nickname
            || self.config.is_music_bot_name(&chat.invoker_name)
        {
            return true;
        }
        let bot_clid = self.ts_adapter.get_bot_clid();
        bot_clid != 0 && caller_id == bot_clid
    }

    async fn resolve_audio_chunk_caller(
        &self,
        audio: &voicev1::AudioFrameEvent,
        speaker_client_id: u32,
        speaker_name: &str,
    ) -> Result<CallerContext> {
        let mut ctx = self.resolve_caller_from_audio(audio).await?;
        if ctx.caller_id != speaker_client_id {
            anyhow::bail!(
                "audio caller changed from {} to {} while resolving ACL",
                ctx.caller_id,
                speaker_client_id
            );
        }
        if !speaker_name.is_empty() {
            ctx.caller_name = speaker_name.to_string();
        }
        Ok(ctx)
    }

    async fn handle_chat_event(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        chat: voicev1::ChatEvent,
    ) -> Result<()> {
        let Some(decision) = resolve_ts_inbound(
            &chat.message,
            chat.target_mode as u8,
            chat.invoker_client_id,
            &self.config.bot,
        ) else {
            return Ok(());
        };
        if !decision.should_trigger_llm {
            return Ok(());
        }
        let ctx = self
            .resolve_caller_from_chat(
                &chat,
                i32::from(decision.reply_target_mode),
                decision.reply_target,
            )
            .await?;
        if self.should_ignore_chat(&chat, ctx.caller_id) {
            return Ok(());
        }
        let Some(clean_text) = preprocess_text_message(&decision.text) else {
            return Ok(());
        };
        if self
            .try_handle_direct_replay(client, &ctx, &clean_text)
            .await?
        {
            return Ok(());
        }
        self.handle_user_input(client, ctx, clean_text).await
    }

    /// 直呼：与技能同一 ACL（voice_replay）；成功处理返回 true
    async fn try_handle_direct_replay(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        ctx: &CallerContext,
        text: &str,
    ) -> Result<bool> {
        if !self.config.voice_replay.enabled {
            return Ok(false);
        }
        let Some(command) = crate::skills::voice_replay::parse_direct_command(
            text,
            &self.config.voice_replay.direct_commands,
        ) else {
            return Ok(false);
        };
        if !crate::skills::voice_replay::direct_command_allowed(
            &self.gate,
            &ctx.groups,
            ctx.channel_group_id,
        ) {
            self.send_reply(client, ctx, "voice_replay denied by ACL")
                .await?;
            return Ok(true);
        }
        let Some(runtime) = self.voice_audio.get() else {
            self.send_reply(client, ctx, "voice replay runtime not ready")
                .await?;
            return Ok(true);
        };
        match crate::skills::voice_replay::execute_direct_command(command, &runtime) {
            Ok(value) => {
                let ack = value.get("status").and_then(|s| s.as_str()).unwrap_or("ok");
                let speakers = value
                    .get("speakers")
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "[]".into());
                let buffered = value
                    .get("buffered_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let msg =
                    format!("voice_replay {ack}; buffered_ms={buffered}; speakers={speakers}");
                self.send_reply(client, ctx, &msg).await?;
            }
            Err(error) => {
                self.send_reply(client, ctx, &format!("voice_replay failed: {error}"))
                    .await?;
            }
        }
        Ok(true)
    }

    async fn process_audio_frame(
        &self,
        audio: &voicev1::AudioFrameEvent,
    ) -> Result<Option<SpeechChunk>> {
        let bot_clid = self.ts_adapter.get_bot_clid();
        if bot_clid != 0 && audio.from_client_id == bot_clid {
            return Ok(None);
        }
        if self.config.is_music_bot_name(&audio.from_client_name) {
            return Ok(None);
        }

        let mut guard = self.audio_pipeline.lock().await;
        let Some(pipeline) = guard.as_mut() else {
            return Ok(None);
        };
        pipeline.process_audio_frame(audio)
    }

    async fn handle_audio_chunk(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        audio: voicev1::AudioFrameEvent,
        chunk: SpeechChunk,
    ) -> Result<()> {
        let ctx = self
            .resolve_audio_chunk_caller(&audio, chunk.speaker_client_id, &chunk.speaker_name)
            .await?;
        if self.config.is_music_bot_name(&ctx.caller_name) {
            return Ok(());
        }

        if self.config.llm.omni_model {
            return self.handle_omni_audio_chunk(client, ctx, chunk).await;
        }

        let Some(speech_provider) = self.speech_provider.as_ref() else {
            return Ok(());
        };
        let wav = pcm16_mono_to_wav_bytes(&chunk.pcm16_mono_16k, 16_000);
        let raw_text = match speech_provider.transcribe_wav(wav).await {
            Ok(t) => t,
            Err(e) => {
                warn!("stt failed for {}: {}", chunk.speaker_name, e);
                return Ok(());
            }
        };
        let Some(text) = preprocess_stt_text(&raw_text, &self.config.headless.stt) else {
            return Ok(());
        };

        self.handle_user_input(client, ctx, text).await
    }

    async fn handle_omni_audio_chunk(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        ctx: CallerContext,
        chunk: SpeechChunk,
    ) -> Result<()> {
        let wav_bytes = pcm16_mono_to_wav_bytes(&chunk.pcm16_mono_16k, 16_000);
        let audio_base64 = BASE64.encode(&wav_bytes);
        let audio_data = format!("data:audio/wav;base64,{}", audio_base64);

        // 并发门禁：TurnCoordinator 管 LLM 轮；AudioOutput FIFO 管出站，不再使用 tts_lock
        let (system_prompt, user_ctx, allowed_skills, session_source) =
            self.build_llm_request(&ctx).await;
        let Ok(_capacity) = self.llm.try_reserve_turn_capacity() else {
            warn!(
                caller_uid = %ctx.caller_uid,
                "Voice LLM turn queue full; dropping audio chunk"
            );
            return Ok(());
        };
        let _session = self.llm.acquire_turn_session(&session_source).await;
        let tts_runtime = if self.is_tts_effectively_enabled() {
            Some(self.build_tts_callbacks().await?)
        } else {
            None
        };

        match run_llm_turn(
            &self.llm,
            &self.registry,
            |llm| {
                let content =
                    vec![json!({ "type": "input_audio", "input_audio": { "data": audio_data } })];
                llm.build_omni_messages(&session_source, &system_prompt, &user_ctx, content)
            },
            &allowed_skills,
            tts_runtime.as_ref().and_then(TtsTurnRuntime::callbacks),
            || {
                UnifiedExecutionContext::for_ts(
                    TsCaller {
                        adapter: self.ts_adapter.clone(),
                        caller_id: ctx.caller_id,
                        caller_name: ctx.caller_name.clone(),
                        caller_groups: ctx.groups.clone(),
                        caller_channel_group_id: ctx.channel_group_id,
                        nc_adapter: None,
                    },
                    self.gate.clone(),
                    self.config.clone(),
                )
            },
        )
        .await
        {
            Ok(result) => {
                if !result.content.is_empty() {
                    info!(
                        event = "voice.llm.reply",
                        caller_uid = %ctx.caller_uid,
                        reply_chars = result.content.chars().count(),
                        "Voice LLM reply generated"
                    );
                    self.send_reply(client, &ctx, &result.content).await?;
                    self.llm
                        .save_turn(&session_source, "[Audio message]".into(), result.content);
                }
            }
            Err(e) => {
                if let Some(runtime) = tts_runtime {
                    runtime.abort().await;
                }
                self.send_reply(client, &ctx, LLM_ERROR_REPLY).await?;
                return Err(e.into());
            }
        };
        if let Some(runtime) = tts_runtime {
            runtime.finish().await;
        }
        Ok(())
    }

    async fn handle_user_input(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        ctx: CallerContext,
        user_msg: String,
    ) -> Result<()> {
        info!(
            event = "voice.user_message",
            caller_uid = %ctx.caller_uid,
            message_chars = user_msg.chars().count(),
            "Voice user message received"
        );
        // 并发门禁：TurnCoordinator 管 LLM 轮；AudioOutput FIFO 管出站，不再使用 tts_lock
        if let Err(error) = self.llm.check_user_text_bounds(&user_msg) {
            warn!(error = %error, caller_uid = %ctx.caller_uid, "voice message dropped for exceeding size limit");
            return Ok(());
        }
        let (system_prompt, user_ctx, allowed_skills, session_source) =
            self.build_llm_request(&ctx).await;
        let Ok(_capacity) = self.llm.try_reserve_turn_capacity() else {
            warn!(
                caller_uid = %ctx.caller_uid,
                "Voice LLM turn queue full; dropping message"
            );
            return Ok(());
        };
        let _session = self.llm.acquire_turn_session(&session_source).await;

        let tts_runtime = if self.is_tts_effectively_enabled() {
            Some(self.build_tts_callbacks().await?)
        } else {
            None
        };

        let result = match run_llm_turn(
            &self.llm,
            &self.registry,
            |llm| llm.build_messages(&session_source, &system_prompt, &user_ctx, &user_msg),
            &allowed_skills,
            tts_runtime.as_ref().and_then(TtsTurnRuntime::callbacks),
            || {
                UnifiedExecutionContext::for_ts(
                    TsCaller {
                        adapter: self.ts_adapter.clone(),
                        caller_id: ctx.caller_id,
                        caller_name: ctx.caller_name.clone(),
                        caller_groups: ctx.groups.clone(),
                        caller_channel_group_id: ctx.channel_group_id,
                        nc_adapter: None,
                    },
                    self.gate.clone(),
                    self.config.clone(),
                )
            },
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                if let Some(runtime) = tts_runtime {
                    runtime.abort().await;
                }
                self.send_reply(client, &ctx, LLM_ERROR_REPLY).await?;
                return Err(e.into());
            }
        };

        if !result.content.is_empty() {
            info!(
                event = "voice.llm.reply",
                caller_uid = %ctx.caller_uid,
                reply_chars = result.content.chars().count(),
                "Voice LLM reply generated"
            );
            self.send_reply(client, &ctx, &result.content).await?;
            self.llm
                .save_turn(&session_source, user_msg, result.content);
        }
        if let Some(runtime) = tts_runtime {
            runtime.finish().await;
        }
        Ok(())
    }

    /// 每轮 TTS：open_tts_session 占 FIFO 槽；句段合成后 push_encoded；收尾 finish（接收语义）
    async fn build_tts_callbacks(&self) -> Result<TtsTurnRuntime> {
        let speech_provider = self
            .speech_provider
            .clone()
            .ok_or_else(|| anyhow::anyhow!("TTS provider missing"))?;
        let session = self.audio_output.open_tts_session().await?;
        let shared_session: SharedTtsSession = Arc::new(tokio::sync::Mutex::new(Some(session)));
        let (sentence_tx, sentence_rx) = mpsc::channel::<(usize, String)>(128);
        let trace_id = format!("tts-{}", now_unix_ms());

        let synth_session = shared_session.clone();
        let synth_trace = trace_id.clone();
        let synth_task = tokio::spawn(async move {
            let mut rx = sentence_rx;
            let mut segment_index = 0usize;
            while let Some((_index, sentence)) = rx.recv().await {
                segment_index += 1;
                if !is_speakable(&sentence) {
                    debug!(
                        trace_id = %synth_trace,
                        segment = segment_index,
                        "skipping unspeakable tts segment"
                    );
                    continue;
                }
                match speech_provider.synthesize(&sentence).await {
                    Ok(audio) => {
                        let codec = detect_audio_format(&audio);
                        let mut guard = synth_session.lock().await;
                        let pushed = match guard.as_mut() {
                            Some(session) => session.push_encoded(audio, codec).await,
                            None => Err(anyhow::anyhow!("tts session already finished")),
                        };
                        drop(guard);
                        if let Err(error) = pushed {
                            warn!(
                                trace_id = %synth_trace,
                                segment = segment_index,
                                error = %error,
                                "tts push_encoded failed"
                            );
                            break;
                        }
                    }
                    Err(e) => warn!(
                        trace_id = %synth_trace,
                        segment = segment_index,
                        error = %e,
                        "tts synthesis failed"
                    ),
                }
            }
            let session = synth_session.lock().await.take();
            if let Some(session) = session {
                if let Err(error) = session.finish().await {
                    warn!(
                        trace_id = %synth_trace,
                        error = %error,
                        "tts session finish failed"
                    );
                }
            }
        });

        let chunker = Arc::new(std::sync::Mutex::new(StreamingSentenceChunker::new(
            Self::STREAM_TTS_MIN_CHARS,
            Self::STREAM_TTS_WEAK_PUNCT_MIN_CHARS,
            Self::STREAM_TTS_MAX_CHARS,
        )));
        let shared_tx: TtsSentenceSender = Arc::new(std::sync::Mutex::new(Some(sentence_tx)));

        let on_text_token_shared = shared_tx.clone();
        let on_text_token_chunker = chunker.clone();
        let on_text_token = move |token: &str| {
            let token = token.to_string();
            let chunker = on_text_token_chunker.clone();
            let shared = on_text_token_shared.clone();
            Box::pin(async move {
                let segments: Vec<(usize, String)> = {
                    let mut chunker_guard = chunker.lock().expect("chunker poisoned");
                    chunker_guard
                        .push_token(&token)
                        .into_iter()
                        .map(|segment| (0, segment))
                        .collect()
                };
                let tx = shared.lock().expect("tts tx poisoned").as_ref().cloned();
                if let Some(tx) = tx {
                    for (index, segment) in segments {
                        if tx.send((index, segment)).await.is_err() {
                            break;
                        }
                    }
                }
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        };

        let on_turn_end_shared = shared_tx.clone();
        let on_turn_end_chunker = chunker.clone();
        let on_turn_end = move |finish_reason: &str| {
            let finish_reason = finish_reason.to_string();
            let chunker = on_turn_end_chunker.clone();
            let shared = on_turn_end_shared.clone();
            Box::pin(async move {
                if !should_close_tts_turn(&finish_reason) {
                    return;
                }
                let segments: Vec<(usize, String)> = {
                    let mut chunker_guard = chunker.lock().expect("chunker poisoned");
                    chunker_guard
                        .finish()
                        .into_iter()
                        .map(|segment| (0, segment))
                        .collect()
                };
                let tx = shared.lock().expect("tts tx poisoned").as_ref().cloned();
                if let Some(tx) = tx {
                    for (index, segment) in segments {
                        if tx.send((index, segment)).await.is_err() {
                            break;
                        }
                    }
                }
                // 含 tool_calls：先关句段（synth finish 会话）再执行 tool
                *shared.lock().expect("tts tx poisoned") = None;
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        };

        Ok(TtsTurnRuntime {
            callbacks: StreamCallbacks {
                on_text_token: Some(Box::new(on_text_token)),
                on_turn_end: Some(Box::new(on_turn_end)),
            },
            shared_tx,
            shared_session,
            synth_task,
            trace_id,
        })
    }

    async fn build_llm_base_context(&self, ctx: &CallerContext) -> (String, String, Vec<String>) {
        let system_prompt = self.prompts.system.content.clone();

        let (online_clients, _) = self.ts_adapter.list_clients_json(0).await;

        let user_ctx = format!(
            r#"invoker: {{"name":"{}","clid":{},"channel_id":{}}}
Online: {}"#,
            ctx.caller_name, ctx.caller_id, ctx.channel_id, online_clients
        );
        let allowed_skills = self
            .gate
            .get_allowed_skills(&ctx.groups, ctx.channel_group_id);
        (system_prompt, user_ctx, allowed_skills)
    }

    async fn build_llm_request(
        &self,
        ctx: &CallerContext,
    ) -> (String, String, Vec<String>, SessionSource) {
        let (system_prompt, user_ctx, allowed_skills) = self.build_llm_base_context(ctx).await;
        let source = SessionSource::TeamSpeak {
            uid: ctx.caller_uid.clone(),
        };
        (system_prompt, user_ctx, allowed_skills, source)
    }

    async fn send_reply(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        ctx: &CallerContext,
        text: &str,
    ) -> Result<()> {
        let req = voicev1::NoticeRequest {
            message: text.to_string(),
            target_mode: ctx.reply_target_mode,
            target_client_id: ctx.reply_target_client_id,
        };
        let response = client
            .send_notice(tonic::Request::new(req))
            .await?
            .into_inner();
        if !response.ok {
            anyhow::bail!("voice notice rejected: {}", response.message);
        }
        Ok(())
    }
}

struct StreamingSentenceChunker {
    buffer: String,
    min_chars: usize,
    weak_punct_min_chars: usize,
    max_chars: usize,
}

impl StreamingSentenceChunker {
    fn new(min_chars: usize, weak_punct_min_chars: usize, max_chars: usize) -> Self {
        Self {
            buffer: String::new(),
            min_chars,
            weak_punct_min_chars,
            max_chars,
        }
    }

    fn push_token(&mut self, token: &str) -> Vec<String> {
        let mut out = Vec::new();
        for ch in token.chars() {
            self.buffer.push(ch);
            let len = self.buffer.chars().count();
            let strong_punct = matches!(ch, '。' | '！' | '？' | '.' | '!' | '?' | ';' | '；');
            let weak_punct = matches!(ch, '，' | ',' | '：' | ':');
            let flush = strong_punct
                || (weak_punct && len >= self.weak_punct_min_chars)
                || len >= self.max_chars;
            if flush {
                if let Some(seg) = self.take_buffer(len >= self.min_chars || len >= self.max_chars)
                {
                    out.push(seg);
                }
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<String> {
        self.take_buffer(true).into_iter().collect()
    }

    fn take_buffer(&mut self, force: bool) -> Option<String> {
        let text = self.buffer.trim();
        if text.is_empty() {
            self.buffer.clear();
            return None;
        }
        if !force && text.chars().count() < self.min_chars {
            return None;
        }
        let out = text.to_string();
        self.buffer.clear();
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct DropMarker(Arc<AtomicBool>);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn session_locks_serialize_only_the_same_uid() {
        let engine =
            crate::llm::LlmEngine::new(std::sync::Arc::new(crate::config::AppConfig::default()))
                .unwrap();
        let source = SessionSource::TeamSpeak {
            uid: "uid-1".to_string(),
        };
        let _capacity = engine.try_reserve_turn_capacity().unwrap();
        let first = engine.acquire_turn_session(&source).await;

        let engine = std::sync::Arc::new(engine);
        let waiting_source = source.clone();
        let waiting = {
            let engine = engine.clone();
            async move { engine.acquire_turn_session(&waiting_source).await }
        };
        assert!(tokio::time::timeout(Duration::from_millis(20), waiting)
            .await
            .is_err());

        drop(first);
    }

    #[tokio::test]
    async fn managed_tasks_are_cancelled_on_router_exit() {
        let dropped = Arc::new(AtomicBool::new(false));
        let marker = DropMarker(dropped.clone());
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let _marker = marker;
            std::future::pending::<()>().await
        });
        tokio::task::yield_now().await;

        abort_managed_tasks(&mut tasks, "Voice router").await;

        assert!(dropped.load(Ordering::SeqCst));
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn audio_worker_queue_drops_latest_when_full() {
        let (tx, mut rx) = mpsc::channel::<(voicev1::AudioFrameEvent, SpeechChunk)>(1);
        let audio = voicev1::AudioFrameEvent::default();
        let chunk = SpeechChunk {
            speaker_client_id: 1,
            speaker_name: "a".to_string(),
            pcm16_mono_16k: Vec::new(),
        };
        tx.try_send((audio.clone(), chunk)).unwrap();

        // 队列满：最新 utterance 被拒绝而非等待
        assert!(tx
            .try_send((
                audio,
                SpeechChunk {
                    speaker_client_id: 2,
                    speaker_name: "b".to_string(),
                    pcm16_mono_16k: Vec::new(),
                }
            ))
            .is_err());

        let (_, first) = rx.recv().await.unwrap();
        assert_eq!(first.speaker_client_id, 1);
    }

    #[test]
    fn tts_closes_on_tool_calls_and_all_other_finish_reasons() {
        // tool 轮 finish_reason 为 "tool_calls"，也必须先 finish 再执行 tool
        assert!(should_close_tts_turn("tool_calls"));
        for finish_reason in ["stop", "length", "content_filter", "function_call", ""] {
            assert!(should_close_tts_turn(finish_reason));
        }
    }

    #[tokio::test]
    async fn tts_sentence_channel_backpressures_sender_when_full() {
        let (tx, mut rx) = mpsc::channel::<(usize, String)>(1);
        tx.send((0, "first".to_string())).await.unwrap();

        // 满通道：send 挂起而非丢帧
        let sender = tx.clone();
        let pending =
            tokio::spawn(async move { sender.send((0, "second".to_string())).await.is_ok() });
        tokio::task::yield_now().await;
        assert!(!pending.is_finished());

        let _ = rx.recv().await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(200), pending)
            .await
            .expect("backpressured send must complete after space frees")
            .unwrap());
    }

    #[tokio::test]
    async fn tts_turn_runtime_finish_drains_synth_task() {
        let (sentence_tx, sentence_rx) = mpsc::channel::<(usize, String)>(8);
        let audio_bus = crate::adapter::headless::audio_output::AudioBus::new();
        let audio_output = audio_bus.output;
        let consumer = audio_bus.consumer;
        let (ts3_tx, mut ts3_rx) = mpsc::channel::<(Vec<u8>, i32)>(16);
        tokio::spawn(consumer.run(ts3_tx));
        tokio::spawn(async move { while ts3_rx.recv().await.is_some() {} });
        let session = audio_output.open_tts_session().await.unwrap();
        let shared_session: SharedTtsSession = Arc::new(tokio::sync::Mutex::new(Some(session)));

        let synth_task = tokio::spawn(async move {
            let mut rx = sentence_rx;
            while let Some((_, _)) = rx.recv().await {}
            let session = shared_session.lock().await.take();
            if let Some(session) = session {
                let _ = session.finish().await;
            }
        });

        let runtime = TtsTurnRuntime {
            callbacks: StreamCallbacks::default(),
            shared_tx: Arc::new(std::sync::Mutex::new(Some(sentence_tx))),
            shared_session: Arc::new(tokio::sync::Mutex::new(None)),
            synth_task,
            trace_id: "test-trace".to_string(),
        };

        runtime.finish().await;
    }

    #[tokio::test]
    async fn tts_turn_runtime_abort_cancels_synth_and_session() {
        let (sentence_tx, sentence_rx) = mpsc::channel::<(usize, String)>(8);
        let _rx_keepalive = sentence_rx;
        let audio_bus = crate::adapter::headless::audio_output::AudioBus::new();
        let audio_output = audio_bus.output;
        let consumer = audio_bus.consumer;
        let (ts3_tx, _ts3_rx) = mpsc::channel::<(Vec<u8>, i32)>(8);
        tokio::spawn(async move {
            let _ = consumer.run(ts3_tx).await;
        });
        let session = audio_output.open_tts_session().await.unwrap();
        let shared_session: SharedTtsSession = Arc::new(tokio::sync::Mutex::new(Some(session)));

        let synth_task = tokio::spawn(async {
            std::future::pending::<()>().await;
        });

        let runtime = TtsTurnRuntime {
            callbacks: StreamCallbacks::default(),
            shared_tx: Arc::new(std::sync::Mutex::new(Some(sentence_tx))),
            shared_session: shared_session.clone(),
            synth_task,
            trace_id: "test-trace".to_string(),
        };

        runtime.abort().await;
        assert!(shared_session.lock().await.is_none());
    }
}

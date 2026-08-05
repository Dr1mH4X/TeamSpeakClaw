use anyhow::Result;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures_util::StreamExt;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use crate::adapter::headless::speech::{
    detect_audio_format, is_speakable, pcm16_mono_to_wav_bytes, preprocess_stt_text,
    preprocess_text_message, OpenAiSpeechProvider, OpusSttPipeline, SpeechChunk,
};
use crate::adapter::headless::tsbot::voice::v1 as voicev1;
use crate::adapter::headless::{VoiceBridgeState, INTERNAL_GRPC_ADDR};
use crate::adapter::TsAdapter;
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::{LlmEngine, SessionSource, StreamCallbacks, ToolCall, ToolExecutor};
use crate::permission::PermissionGate;
use crate::skills::{ExecutionContext, SkillRegistry};
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

#[derive(Default)]
struct SessionLocks {
    locks: StdMutex<HashMap<String, Weak<Mutex<()>>>>,
}

impl SessionLocks {
    fn for_uid(&self, uid: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().expect("session lock map poisoned");
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(uid).and_then(Weak::upgrade) {
            return lock;
        }

        let lock = Arc::new(Mutex::new(()));
        locks.insert(uid.to_string(), Arc::downgrade(&lock));
        lock
    }
}

async fn abort_managed_tasks(tasks: &mut JoinSet<()>) {
    tasks.abort_all();
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            if !error.is_cancelled() {
                error!("Voice router task failed during shutdown: {error}");
            }
        }
    }
}

fn should_close_tts_turn(finish_reason: &str) -> bool {
    finish_reason != "tool_calls"
}

/// 每轮 TTS 运行时：持有合成与上传任务，正常结束 join，取消时 abort
/// 句段发送端共享句柄：None 表示该轮已关闭
type TtsSentenceSender = Arc<std::sync::Mutex<Option<mpsc::Sender<(usize, String)>>>>;

struct TtsTurnRuntime {
    callbacks: StreamCallbacks,
    shared_tx: TtsSentenceSender,
    synth_task: tokio::task::JoinHandle<()>,
    upload_task: tokio::task::JoinHandle<()>,
    trace_id: String,
}

impl TtsTurnRuntime {
    /// TTS 收尾超时：上游停滞（如 TS 发送背压）时防止 finish 无限挂起
    const TTS_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);

    fn callbacks(&self) -> Option<&StreamCallbacks> {
        Some(&self.callbacks)
    }

    /// 正常结束：关闭句段通道使合成任务发 EOS 后退出，再 join 上传任务
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
                return;
            }
        }
        match tokio::time::timeout(Self::TTS_TEARDOWN_TIMEOUT, self.upload_task).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                error!(
                    trace_id = %self.trace_id,
                    error = %error,
                    "tts upload task failed"
                );
            }
            Err(_) => {
                error!(
                    trace_id = %self.trace_id,
                    "tts upload task teardown timed out"
                );
            }
        }
    }

    /// 取消：abort 两个任务并收割，不产生悬挂句柄
    async fn abort(self) {
        self.synth_task.abort();
        self.upload_task.abort();
        let _ = self.synth_task.await;
        let _ = self.upload_task.await;
    }
}

struct SkillExecutor<'a> {
    router: &'a VoiceRouter,
    ctx: &'a CallerContext,
    allowed_skills: &'a [String],
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

#[async_trait]
impl ToolExecutor for SkillExecutor<'_> {
    async fn execute(&self, call: &ToolCall) -> String {
        self.router
            .execute_skill(call, self.ctx, self.allowed_skills)
            .await
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
    session_locks: SessionLocks,
    speech_provider: Option<Arc<OpenAiSpeechProvider>>,
    bridge_state: VoiceBridgeState,
}

impl VoiceRouter {
    const STREAM_TTS_MIN_CHARS: usize = 4;
    const STREAM_TTS_WEAK_PUNCT_MIN_CHARS: usize = 8;
    const STREAM_TTS_MAX_CHARS: usize = 28;

    pub fn new(
        config: Arc<AppConfig>,
        prompts: Arc<PromptsConfig>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
        ts_adapter: Arc<TsAdapter>,
        bridge_state: VoiceBridgeState,
    ) -> Self {
        let speech_provider =
            OpenAiSpeechProvider::new(config.clone(), prompts.tts.style_prompt.clone())
                .ok()
                .map(Arc::new);
        let need_audio_pipeline = config.headless.stt.enabled || config.llm.omni_model;
        Self {
            audio_pipeline: Mutex::new(need_audio_pipeline.then(OpusSttPipeline::new)),
            session_locks: SessionLocks::default(),
            config,
            prompts,
            gate,
            llm,
            registry,
            ts_adapter,
            speech_provider,
            bridge_state,
        }
    }

    fn is_tts_effectively_enabled(&self) -> bool {
        self.config.headless.tts.enabled && self.speech_provider.is_some()
    }

    pub async fn run(self) -> Result<()> {
        self.bridge_state.set_stream_ready(false);
        let endpoint = format!("http://{}", INTERNAL_GRPC_ADDR);
        let channel = Channel::from_shared(endpoint.clone())?.connect().await?;
        let mut client = VoiceServiceClient::new(channel);

        let req = tonic::Request::new(voicev1::SubscribeRequest {
            include_chat: true,
            include_log: true,
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
            let mut connect_attempt = 1u32;
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
                            connect_attempt = 1;
                        }
                        Err(error) => {
                            warn!(
                                attempt = connect_attempt,
                                error = %error,
                                "voice audio worker connect failed; retrying"
                            );
                            let delay = crate::adapter::reconnect::reconnect_delay_for_attempt(
                                connect_attempt,
                            );
                            tokio::time::sleep(delay).await;
                            connect_attempt = connect_attempt.saturating_add(1);
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
                        _ => {}
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
                            is_whisper: false,
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
        abort_managed_tasks(&mut tasks).await;
        result
    }

    async fn resolve_caller_from_chat(&self, chat: &voicev1::ChatEvent) -> Result<CallerContext> {
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
        let groups: Vec<u32> = caller
            .server_groups
            .iter()
            .filter_map(|group| group.parse().ok())
            .collect();
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
            reply_target_mode: chat.reply_target_mode,
            reply_target_client_id: chat.reply_target_client_id,
        })
    }

    async fn resolve_caller_from_audio(
        &self,
        audio: &voicev1::AudioFrameEvent,
    ) -> Result<CallerContext> {
        let reply_target_mode = match self.config.bot.default_reply_mode.as_str() {
            "channel" => 2,
            "server" => 3,
            _ => 1,
        };
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
        let groups: Vec<u32> = caller
            .server_groups
            .iter()
            .filter_map(|group| group.parse().ok())
            .collect();
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
            || self.is_music_bot_name(&chat.invoker_name)
        {
            return true;
        }
        let bot_clid = self.ts_adapter.get_bot_clid();
        bot_clid != 0 && caller_id == bot_clid
    }

    fn is_music_bot_name(&self, name: &str) -> bool {
        self.config.music_backend.as_ref().is_some_and(|config| {
            !config.musicbot_name.is_empty()
                && name
                    .to_ascii_lowercase()
                    .contains(&config.musicbot_name.to_ascii_lowercase())
        })
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

    async fn execute_skill(
        &self,
        call: &ToolCall,
        ctx: &CallerContext,
        allowed_skills: &[String],
    ) -> String {
        let exec_ctx = ExecutionContext {
            adapter: self.ts_adapter.clone(),
            caller_id: ctx.caller_id,
            caller_name: ctx.caller_name.clone(),
            caller_groups: ctx.groups.clone(),
            caller_channel_group_id: ctx.channel_group_id,
            gate: self.gate.clone(),
            config: self.config.clone(),
        };
        self.registry
            .execute_skill(call, exec_ctx, allowed_skills, None)
            .await
    }

    async fn handle_chat_event(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        chat: voicev1::ChatEvent,
    ) -> Result<()> {
        if !chat.should_trigger_llm {
            return Ok(());
        }
        let ctx = self.resolve_caller_from_chat(&chat).await?;
        if self.should_ignore_chat(&chat, ctx.caller_id) {
            return Ok(());
        }
        let Some(clean_text) = preprocess_text_message(&chat.message) else {
            return Ok(());
        };
        let session_lock = self.session_locks.for_uid(&ctx.caller_uid);
        let _session_guard = session_lock.lock().await;
        self.handle_user_input(client, ctx, clean_text).await
    }

    async fn process_audio_frame(
        &self,
        audio: &voicev1::AudioFrameEvent,
    ) -> Result<Option<SpeechChunk>> {
        let bot_clid = self.ts_adapter.get_bot_clid();
        if bot_clid != 0 && audio.from_client_id == bot_clid {
            return Ok(None);
        }
        if self.is_music_bot_name(&audio.from_client_name) {
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
        if self.is_music_bot_name(&ctx.caller_name) {
            return Ok(());
        }
        let session_lock = self.session_locks.for_uid(&ctx.caller_uid);
        let _session_guard = session_lock.lock().await;

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

        let (mut messages, tools, allowed_skills, session_source) =
            self.build_omni_llm_request(&ctx, audio_data).await;
        let executor = SkillExecutor {
            router: self,
            ctx: &ctx,
            allowed_skills: &allowed_skills,
        };
        let tts_runtime = if self.is_tts_effectively_enabled() {
            Some(self.build_tts_callbacks().await?)
        } else {
            None
        };

        match self
            .llm
            .run_tool_loop(
                &mut messages,
                &tools,
                &executor,
                tts_runtime.as_ref().and_then(TtsTurnRuntime::callbacks),
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
                self.send_reply(
                    client,
                    &ctx,
                    "AI backend unavailable. Please try again later.",
                )
                .await?;
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
        if let Err(error) = self.llm.check_user_text_bounds(&user_msg) {
            warn!(error = %error, caller_uid = %ctx.caller_uid, "voice message dropped for exceeding size limit");
            return Ok(());
        }
        let (mut messages, tools, allowed_skills, session_source) =
            self.build_llm_request(&ctx, user_msg.clone()).await;
        let executor = SkillExecutor {
            router: self,
            ctx: &ctx,
            allowed_skills: &allowed_skills,
        };

        let tts_runtime = if self.is_tts_effectively_enabled() {
            Some(self.build_tts_callbacks().await?)
        } else {
            None
        };

        let result = match self
            .llm
            .run_tool_loop(
                &mut messages,
                &tools,
                &executor,
                tts_runtime.as_ref().and_then(TtsTurnRuntime::callbacks),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                if let Some(runtime) = tts_runtime {
                    runtime.abort().await;
                }
                self.send_reply(
                    client,
                    &ctx,
                    "AI backend unavailable. Please try again later.",
                )
                .await?;
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

    /// 每轮 TTS 运行时：持有合成与上传任务，正常结束 join，取消时 abort
    async fn build_tts_callbacks(&self) -> Result<TtsTurnRuntime> {
        let speech_provider = self
            .speech_provider
            .clone()
            .ok_or_else(|| anyhow::anyhow!("TTS provider missing"))?;
        let endpoint = format!("http://{}", INTERNAL_GRPC_ADDR);
        let channel = Channel::from_shared(endpoint)?.connect().await?;
        let (sentence_tx, sentence_rx) = mpsc::channel::<(usize, String)>(128);
        let (audio_tx, audio_rx) = mpsc::channel::<voicev1::TtsAudioChunk>(8);
        let trace_id = format!(
            "tts-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );

        let synth_audio_tx = audio_tx.clone();
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
                        if let Err(error) = synth_audio_tx
                            .send(voicev1::TtsAudioChunk {
                                payload: audio,
                                codec: codec.to_string(),
                                end_of_stream: false,
                                trace_id: synth_trace.clone(),
                            })
                            .await
                        {
                            warn!(
                                trace_id = %synth_trace,
                                segment = segment_index,
                                error = %error,
                                "tts audio channel closed; aborting synthesis"
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
            // 合成结束发 EOS：工具多轮时只在最终轮触发
            let _ = synth_audio_tx
                .send(voicev1::TtsAudioChunk {
                    payload: vec![],
                    codec: "mp3".to_string(),
                    end_of_stream: true,
                    trace_id: synth_trace,
                })
                .await;
        });

        let upload_trace = trace_id.clone();
        let upload_task = tokio::spawn(async move {
            if let Err(e) = VoiceServiceClient::new(channel)
                .stream_tts_audio(tonic::Request::new(ReceiverStream::new(audio_rx)))
                .await
            {
                warn!(trace_id = %upload_trace, error = %e, "stream_tts_audio failed");
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
                // 取出发送端副本，避免 std MutexGuard 跨 await 存活导致 future 非 Send
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
                *shared.lock().expect("tts tx poisoned") = None;
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        };

        Ok(TtsTurnRuntime {
            callbacks: StreamCallbacks {
                on_text_token: Some(Box::new(on_text_token)),
                on_turn_end: Some(Box::new(on_turn_end)),
            },
            shared_tx,
            synth_task,
            upload_task,
            trace_id,
        })
    }

    async fn build_llm_base_context(
        &self,
        ctx: &CallerContext,
    ) -> (String, String, Vec<serde_json::Value>, Vec<String>) {
        let system_prompt = self.prompts.system.content.clone();

        let online_clients = match self.ts_adapter.list_clients().await {
            Ok(clients) => {
                let arr: Vec<serde_json::Value> = clients
                    .iter()
                    .map(|c| json!({"name": c.nickname, "clid": c.id, "channel_id": c.channel_id}))
                    .collect();
                debug!("Fetched {} online clients for LLM context", clients.len());
                serde_json::to_string(&arr).unwrap_or_default()
            }
            Err(e) => {
                warn!("Failed to fetch online clients: {e}");
                String::new()
            }
        };

        let user_ctx = format!(
            r#"invoker: {{"name":"{}","clid":{},"channel_id":{}}}
Online: {}"#,
            ctx.caller_name, ctx.caller_id, ctx.channel_id, online_clients
        );
        let allowed_skills = self
            .gate
            .get_allowed_skills(&ctx.groups, ctx.channel_group_id);
        let tools = self.registry.to_tool_schemas(&allowed_skills);
        (system_prompt, user_ctx, tools, allowed_skills)
    }

    async fn build_llm_request(
        &self,
        ctx: &CallerContext,
        user_msg: String,
    ) -> (
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
        Vec<String>,
        SessionSource,
    ) {
        let (system_prompt, user_ctx, tools, allowed_skills) =
            self.build_llm_base_context(ctx).await;
        let source = SessionSource::Headless {
            uid: ctx.caller_uid.clone(),
        };
        let messages = self
            .llm
            .build_messages(&source, &system_prompt, &user_ctx, &user_msg);
        (messages, tools, allowed_skills, source)
    }

    async fn build_omni_llm_request(
        &self,
        ctx: &CallerContext,
        audio_data: String,
    ) -> (
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
        Vec<String>,
        SessionSource,
    ) {
        let (system_prompt, user_ctx, tools, allowed_skills) =
            self.build_llm_base_context(ctx).await;
        let source = SessionSource::Headless {
            uid: ctx.caller_uid.clone(),
        };
        let content = vec![json!({ "type": "input_audio", "input_audio": { "data": audio_data } })];
        let messages = self
            .llm
            .build_omni_messages(&source, &system_prompt, &user_ctx, content);
        (messages, tools, allowed_skills, source)
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
        let locks = SessionLocks::default();
        let first = locks.for_uid("uid-1");
        let first_guard = first.lock().await;
        let same = locks.for_uid("uid-1");
        let other = locks.for_uid("uid-2");

        assert!(same.try_lock().is_err());
        assert!(other.try_lock().is_ok());

        drop(first_guard);
        assert!(same.try_lock().is_ok());
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

        abort_managed_tasks(&mut tasks).await;

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
    fn tts_closes_for_every_non_tool_call_finish_reason() {
        assert!(!should_close_tts_turn("tool_calls"));
        for finish_reason in ["stop", "length", "content_filter", "function_call", ""] {
            assert!(should_close_tts_turn(finish_reason));
        }
    }

    #[tokio::test]
    async fn tts_sentence_channel_backpressures_sender_when_full() {
        let (tx, mut rx) = mpsc::channel::<(usize, String)>(1);
        tx.send((1, "first".to_string())).await.unwrap();

        // 满通道：send 挂起而非丢帧
        let sender = tx.clone();
        let pending =
            tokio::spawn(async move { sender.send((2, "second".to_string())).await.is_ok() });
        tokio::task::yield_now().await;
        assert!(!pending.is_finished());

        let _ = rx.recv().await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(200), pending)
            .await
            .expect("backpressured send must complete after space frees")
            .unwrap());
    }

    #[tokio::test]
    async fn tts_turn_runtime_finish_drains_tasks() {
        let (sentence_tx, sentence_rx) = mpsc::channel::<(usize, String)>(8);
        let (audio_tx, _audio_rx) = mpsc::channel::<voicev1::TtsAudioChunk>(8);

        let synth_task = tokio::spawn(async move {
            let mut rx = sentence_rx;
            while let Some((_, _)) = rx.recv().await {}
            let _ = audio_tx
                .send(voicev1::TtsAudioChunk {
                    payload: vec![],
                    codec: "mp3".to_string(),
                    end_of_stream: true,
                    trace_id: "test".to_string(),
                })
                .await;
        });
        let upload_task = tokio::spawn(async {});

        let runtime = TtsTurnRuntime {
            callbacks: StreamCallbacks::default(),
            shared_tx: Arc::new(std::sync::Mutex::new(Some(sentence_tx))),
            synth_task,
            upload_task,
            trace_id: "test-trace".to_string(),
        };

        runtime.finish().await;
    }

    #[tokio::test]
    async fn tts_turn_runtime_abort_cancels_tasks() {
        let (sentence_tx, sentence_rx) = mpsc::channel::<(usize, String)>(8);
        let _rx_keepalive = sentence_rx;
        let (audio_tx, _audio_rx) = mpsc::channel::<voicev1::TtsAudioChunk>(8);
        drop(audio_tx);

        let synth_task = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        let upload_task = tokio::spawn(async {
            std::future::pending::<()>().await;
        });

        let runtime = TtsTurnRuntime {
            callbacks: StreamCallbacks::default(),
            shared_tx: Arc::new(std::sync::Mutex::new(Some(sentence_tx))),
            synth_task,
            upload_task,
            trace_id: "test-trace".to_string(),
        };

        runtime.abort().await;
    }
}

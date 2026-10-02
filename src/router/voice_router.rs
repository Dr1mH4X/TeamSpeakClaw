use anyhow::Result;
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
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
use crate::adapter::headless::wakeword::{WakewordGate, WakewordModels};
use crate::adapter::headless::{
    parse_server_groups, TsAdapter, VoiceBridgeState, INTERNAL_GRPC_ADDR,
};
use crate::adapter::reconnect::{abort_managed_tasks, now_unix_ms};
use crate::config::{reply_target_mode, AppConfig, PromptsConfig};
use crate::llm::tool_loop::ToolLoopError;
use crate::llm::{LlmEngine, SessionSource, StreamCallbacks};
use crate::permission::PermissionGate;
use crate::router::voice_turns::{
    decide_wakeword_action, ActiveTurn, TurnRegistry, WakewordAction,
};
use crate::router::{resolve_ts_inbound, run_llm_turn, LLM_ERROR_REPLY};

use crate::skills::{SkillRegistry, TsCaller, UnifiedExecutionContext};
use tokio_util::sync::CancellationToken;
use voicev1::voice_service_client::VoiceServiceClient;

const AUDIO_MAX_IN_FLIGHT: usize = 8;

/// 出站 gRPC 通道持有者：`Channel` clone 共享同一条连接，准入路径与各回合任务共用。
/// 传输失败时后台重连替换通道，避免单个回合的失败拖住后续通知。
#[derive(Clone)]
struct VoiceChannel {
    channel: Arc<Mutex<Channel>>,
    reconnecting: Arc<AtomicBool>,
}

impl VoiceChannel {
    fn new(channel: Channel) -> Self {
        Self {
            channel: Arc::new(Mutex::new(channel)),
            reconnecting: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn client(&self) -> VoiceServiceClient<Channel> {
        VoiceServiceClient::new(self.channel.lock().await.clone())
    }

    /// 后台重连一次并替换通道；同一时刻只允许一次重连在飞
    fn refresh_in_background(&self) {
        if self.reconnecting.swap(true, Ordering::SeqCst) {
            return;
        }
        let holder = self.clone();
        tokio::spawn(async move {
            match connect_voice_channel().await {
                Ok(channel) => {
                    *holder.channel.lock().await = channel;
                }
                Err(error) => warn!(error = %error, "voice channel reconnect failed"),
            }
            holder.reconnecting.store(false, Ordering::SeqCst);
        });
    }
}

async fn connect_voice_channel() -> Result<Channel> {
    let endpoint = format!("http://{}", INTERNAL_GRPC_ADDR);
    Ok(Channel::from_shared(endpoint)?.connect().await?)
}

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

/// 每轮 TTS 运行时：句段通道 + AudioOutput 会话；会话归 synth 任务所有，句段通道关闭后由任务 finish
type TtsSentenceSender = Arc<std::sync::Mutex<Option<mpsc::Sender<(usize, String)>>>>;
type SharedTtsSession = Arc<tokio::sync::Mutex<Option<TtsSession>>>;

struct TtsTurnRuntime {
    callbacks: StreamCallbacks,
    shared_tx: TtsSentenceSender,
    shared_session: SharedTtsSession,
    synth_task: tokio::task::JoinHandle<()>,
    /// 本轮音频 job 的取消句柄：abort 时先置位，保证已交给消费者的音频也停
    playback_cancel: Arc<AtomicBool>,
}

impl TtsTurnRuntime {
    fn callbacks(&self) -> Option<&StreamCallbacks> {
        Some(&self.callbacks)
    }

    /// 关闭句段通道并分离 synth 任务；会话由任务自行 finish（不等待、不取消）
    ///
    /// synth 的 `push_encoded` 受播放实时性背压，等待任务结束等于等待剩余音频播完；
    /// 任务在句段通道关闭后 finish 会话（不置 cancel），消费者播完已入队音频后自然收尾。
    fn finish(self) {
        *self.shared_tx.lock().expect("tts tx poisoned") = None;
        drop(self.synth_task);
    }

    /// 取消：先置播放取消位，再 abort 合成任务并 drop 未 finish 的会话
    async fn abort(self) {
        self.playback_cancel.store(true, Ordering::SeqCst);
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
    /// 唤醒门：`[headless.wakeword]` 启用时 Some；per-clid 会话状态仅 audio drain 任务访问
    wakeword: Option<Mutex<WakewordGate>>,
    /// per-clid 活跃回合：准入判断「忙」与插话取消目标
    turns: TurnRegistry,
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
    /// 已加载的唤醒词模型；`[headless.wakeword]` 未启用时为 None
    pub wakeword: Option<Arc<WakewordModels>>,
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
            wakeword,
        } = handles;
        let speech_provider =
            OpenAiSpeechProvider::new(config.clone(), prompts.tts.style_prompt.clone())
                .ok()
                .map(Arc::new);
        let need_audio_pipeline = config.headless.stt.enabled || config.llm.omni_model;
        let wakeword = wakeword.map(|models| {
            Mutex::new(WakewordGate::new(
                models,
                config.headless.wakeword.window_secs,
            ))
        });
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
            wakeword,
            turns: TurnRegistry::default(),
        }
    }

    fn is_tts_effectively_enabled(&self) -> bool {
        self.config.headless.tts.enabled && self.speech_provider.is_some()
    }

    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        self.bridge_state.set_stream_ready(false);
        let channel = connect_voice_channel().await?;
        let mut client = VoiceServiceClient::new(channel.clone());
        let channel = VoiceChannel::new(channel);

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

        // 准入与执行分离：本任务只喂唤醒门并裁决，回合在执行任务里跑，
        // 所以机器人播报期间仍能即时裁决后续语音（含第二次唤醒词的插话）
        let drain_router = router.clone();
        let drain_channel = channel.clone();
        tasks.spawn(async move {
            let mut turn_tasks: JoinSet<()> = JoinSet::new();
            loop {
                let next = tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => None,
                    next = audio_chunk_rx.recv() => next,
                };
                let Some((audio, chunk)) = next else {
                    break;
                };
                if let Err(error) = drain_router
                    .admit_audio_chunk(&drain_channel, audio, chunk, &mut turn_tasks)
                    .await
                {
                    error!("Voice router audio handling failed: {error}");
                }
                while let Some(result) = turn_tasks.try_join_next() {
                    if let Err(error) = result {
                        error!("voice turn task failed: {error}");
                    }
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
                            let channel = channel.clone();
                            tasks.spawn(async move {
                                if let Err(error) = router.handle_chat_event(&channel, chat).await {
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
        channel: &VoiceChannel,
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
            .try_handle_direct_replay(channel, &ctx, &clean_text)
            .await?
        {
            return Ok(());
        }
        // 文本触发的回复同样经 TTS 出声：与语音回合共用产出登记与取消令牌。
        // 文本不经唤醒门，不夺取话语权：正在播报的语音回合继续播完，被替换的旧条目
        // 由它自己的任务收敛，不在这里取消
        let active = Arc::new(ActiveTurn::new());
        self.turns.begin_turn(ctx.caller_id, &active);
        self.handle_user_input(channel, ctx, clean_text, &active)
            .await
    }

    /// 直呼：与技能同一 ACL（voice_replay）；成功处理返回 true
    async fn try_handle_direct_replay(
        &self,
        channel: &VoiceChannel,
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
            self.send_reply(channel, ctx, "voice_replay denied by ACL")
                .await?;
            return Ok(true);
        }
        let Some(runtime) = self.voice_audio.get() else {
            self.send_reply(channel, ctx, "voice replay runtime not ready")
                .await?;
            return Ok(true);
        };
        match crate::skills::voice_replay::execute_direct_command(command, &runtime) {
            Ok(value) => {
                let msg = match value.get("status").and_then(|s| s.as_str()) {
                    Some("empty") => value
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("no speakers in the recording window")
                        .to_string(),
                    Some(status) => {
                        let speakers = value
                            .get("speakers")
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "[]".into());
                        let buffered = value
                            .get("buffered_ms")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        format!(
                            "voice_replay {status}; buffered_ms={buffered}; speakers={speakers}"
                        )
                    }
                    None => "voice_replay ok".to_string(),
                };
                self.send_reply(channel, ctx, &msg).await?;
            }
            Err(error) => {
                self.send_reply(channel, ctx, &format!("voice_replay failed: {error}"))
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

    /// 准入：喂唤醒门 → 状态表裁决 → 丢弃 / 插话 / 注册并派发回合任务。
    /// 这里不做 gRPC 解析与 STT，保证机器人播报期间仍能即时裁决后续语音。
    async fn admit_audio_chunk(
        self: &Arc<Self>,
        channel: &VoiceChannel,
        audio: voicev1::AudioFrameEvent,
        chunk: SpeechChunk,
        turn_tasks: &mut JoinSet<()>,
    ) -> Result<()> {
        let clid = chunk.speaker_client_id;
        // 唤醒门先于 gRPC 解析：关门丢弃不产生解析/查询开销。
        // 门按整段喂入、内部仍按 80ms 块推理，所以命中块偏移与原始分数序列都能打点出来，
        // 供 Phase 0.5 标定唤醒词末端（裁切点）用
        if let Some(gate) = &self.wakeword {
            let event = {
                let mut gate = gate.lock().await;
                gate.feed(clid, &chunk.pcm16_mono_16k, Instant::now())
            };
            let verdict = event.verdict;
            if verdict.detected {
                info!(
                    event = "voice.wakeword",
                    clid,
                    speaker = %chunk.speaker_name,
                    probability = verdict.probability,
                    fire_sample = event.fire_offset.unwrap_or(0),
                    input_samples = chunk.pcm16_mono_16k.len(),
                    window_open_before_feed = event.window_open_before_feed,
                    "wakeword detected; opening gate"
                );
                debug!(
                    event = "voice.wakeword.calibration",
                    clid,
                    fire_sample = event.fire_offset.unwrap_or(0),
                    input_samples = chunk.pcm16_mono_16k.len(),
                    first_chunk_index = event.calibration.first_chunk_index,
                    peak_chunk_index = event.calibration.peak_chunk_index().unwrap_or(0),
                    peak_score = event.calibration.peak_score().unwrap_or(0.0),
                    raw_scores = %event.calibration.scores_compact(),
                    "wakeword raw score history for boundary calibration"
                );
            }
            // 「忙」与插话取消目标取自同一次快照：出站音频只有一路，产出中的回合就是通道持有者。
            // 分两次查询（先判忙再取目标）会在两次加锁之间漂移，出现「判定为忙却取不到目标」
            // 或反过来该插话却判成空闲
            let producing = self.turns.producing_turn();
            let busy = producing.is_some();
            match decide_wakeword_action(verdict.open, verdict.detected, busy) {
                WakewordAction::Talk => {}
                WakewordAction::Drop => {
                    debug!(
                        event = "voice.wakeword.drop",
                        clid,
                        speaker = %chunk.speaker_name,
                        open = verdict.open,
                        detected = verdict.detected,
                        busy,
                        "dropping utterance at wakeword gate"
                    );
                    return Ok(());
                }
                // BargeIn 由 busy 推出，而 busy 就是本快照的 Some/None，目标必然存在
                WakewordAction::BargeIn => {
                    let (owner_clid, target) =
                        producing.expect("barge-in implies a producing turn");
                    info!(
                        event = "voice.wakeword.barge_in",
                        barge_in_by = clid,
                        cancelled_owner = owner_clid,
                        speaker = %chunk.speaker_name,
                        turn_age_ms = target.age_ms(),
                        "wakeword barge-in; cancelling the turn that owns the output"
                    );
                    // 取消幂等：令牌与播放取消位各自可重复置位。摘除只做一次，
                    // 条目即时失效让后续语音立刻回到空闲准入，不必等旧回合任务收敛；
                    // 若条目已被新回合顶替，remove_turn 按身份匹配不动新条目
                    target.cancel.cancel();
                    if let Some(playback) = target.playback_cancel() {
                        playback.store(true, Ordering::SeqCst);
                    }
                    self.turns.remove_turn(&target);
                }
            }
        }

        let active = Arc::new(ActiveTurn::new());
        // 第二道闸：同说话人可能还有一条尚未进入产出（正在解析/STT）的旧回合。它不在「忙」的
        // 视野里，第一道闸看不到它，却已持有回合状态与播放取消句柄；begin_turn 把它挤下槽位，
        // 这里顺手取消——否则两条并发回合里会有一条的取消句柄再也不可达
        if let Some(displaced) = self.turns.begin_turn(clid, &active) {
            displaced.cancel.cancel();
            if let Some(playback) = displaced.playback_cancel() {
                playback.store(true, Ordering::SeqCst);
            }
        }
        let router = self.clone();
        let channel = channel.clone();
        turn_tasks.spawn(async move {
            if let Err(error) = router
                .run_admitted_utterance(channel, active, audio, chunk)
                .await
            {
                error!("Voice turn failed: {error}");
            }
        });
        Ok(())
    }

    /// 执行一条已准入的 utterance：解析 caller → STT / omni → LLM 回合
    async fn run_admitted_utterance(
        self: Arc<Self>,
        channel: VoiceChannel,
        active: Arc<ActiveTurn>,
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
            return self
                .handle_omni_audio_chunk(&channel, ctx, chunk, &active)
                .await;
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
        let Some(text) = preprocess_stt_text(&raw_text, self.config.headless.wakeword.enabled)
        else {
            return Ok(());
        };

        self.handle_user_input(&channel, ctx, text, &active).await
    }

    async fn handle_omni_audio_chunk(
        &self,
        channel: &VoiceChannel,
        ctx: CallerContext,
        chunk: SpeechChunk,
        active: &Arc<ActiveTurn>,
    ) -> Result<()> {
        let wav_bytes = pcm16_mono_to_wav_bytes(&chunk.pcm16_mono_16k, 16_000);
        // 请求与上下文各持一份：音频轮随后的消息按 input_audio 回放历史
        let request_wav = wav_bytes.clone();

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
            Some(self.build_tts_callbacks(active).await?)
        } else {
            None
        };
        active.mark_producing();

        match run_llm_turn(
            &self.llm,
            &self.registry,
            |llm| llm.build_omni_messages(&session_source, &system_prompt, &user_ctx, &request_wav),
            &allowed_skills,
            tts_runtime.as_ref().and_then(TtsTurnRuntime::callbacks),
            &active.cancel,
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
                    self.send_reply(channel, &ctx, &result.content).await?;
                    self.llm
                        .save_omni_turn(&session_source, wav_bytes, result.content);
                }
            }
            Err(ToolLoopError::Cancelled) => {
                // 插话取消：不回错误文案、不落上下文；播放取消句柄已由插话方置位
                if let Some(runtime) = tts_runtime {
                    runtime.abort().await;
                }
                debug!(
                    caller_uid = %ctx.caller_uid,
                    "voice omni turn cancelled by barge-in"
                );
                return Ok(());
            }
            Err(e) => {
                if let Some(runtime) = tts_runtime {
                    runtime.abort().await;
                }
                self.send_reply(channel, &ctx, LLM_ERROR_REPLY).await?;
                return Err(e.into());
            }
        };
        if let Some(runtime) = tts_runtime {
            runtime.finish();
        }
        Ok(())
    }

    async fn handle_user_input(
        &self,
        channel: &VoiceChannel,
        ctx: CallerContext,
        user_msg: String,
        active: &Arc<ActiveTurn>,
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
            Some(self.build_tts_callbacks(active).await?)
        } else {
            None
        };
        active.mark_producing();

        let result = match run_llm_turn(
            &self.llm,
            &self.registry,
            |llm| llm.build_messages(&session_source, &system_prompt, &user_ctx, &user_msg),
            &allowed_skills,
            tts_runtime.as_ref().and_then(TtsTurnRuntime::callbacks),
            &active.cancel,
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
            Err(ToolLoopError::Cancelled) => {
                // 插话取消：不回错误文案、不落上下文；播放取消句柄已由插话方置位
                if let Some(runtime) = tts_runtime {
                    runtime.abort().await;
                }
                debug!(
                    caller_uid = %ctx.caller_uid,
                    "voice turn cancelled by barge-in"
                );
                return Ok(());
            }
            Err(e) => {
                if let Some(runtime) = tts_runtime {
                    runtime.abort().await;
                }
                self.send_reply(channel, &ctx, LLM_ERROR_REPLY).await?;
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
            self.send_reply(channel, &ctx, &result.content).await?;
            self.llm
                .save_turn(&session_source, user_msg, result.content);
        }
        if let Some(runtime) = tts_runtime {
            runtime.finish();
        }
        Ok(())
    }

    /// 每轮 TTS：open_tts_session 占 FIFO 槽；句段合成后 push_encoded；收尾只关句段通道，不等播放。
    /// `keepalive` 是准入注册的活跃回合：synth 任务持一份直到本轮音频播放收尾，
    /// 让「机器人正在产出」覆盖播放尾，插话在 LLM 流结束后仍能取消播放。
    async fn build_tts_callbacks(&self, keepalive: &Arc<ActiveTurn>) -> Result<TtsTurnRuntime> {
        let speech_provider = self
            .speech_provider
            .clone()
            .ok_or_else(|| anyhow::anyhow!("TTS provider missing"))?;
        let session = self.audio_output.open_tts_session().await?;
        let playback_cancel = session.cancel_handle();
        keepalive.set_playback_cancel(playback_cancel.clone());
        let keepalive = keepalive.clone();
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
                // 等本轮音频真正播完再放掉 active 句柄；被插话时 job 已取消，立即返回
                if let Err(error) = session.finish_drained().await {
                    warn!(
                        trace_id = %synth_trace,
                        error = %error,
                        "tts session drain failed"
                    );
                }
            }
            drop(keepalive);
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
            playback_cancel,
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
        channel: &VoiceChannel,
        ctx: &CallerContext,
        text: &str,
    ) -> Result<()> {
        let req = voicev1::NoticeRequest {
            message: text.to_string(),
            target_mode: ctx.reply_target_mode,
            target_client_id: ctx.reply_target_client_id,
        };
        let mut client = channel.client().await;
        let response = match client.send_notice(tonic::Request::new(req)).await {
            Ok(response) => response.into_inner(),
            Err(status) => {
                // 传输失败：后台重连替换通道，本次通知不重试（避免重复发送）
                channel.refresh_in_background();
                return Err(anyhow::anyhow!("voice notice failed: {status}"));
            }
        };
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
    async fn tts_turn_runtime_finish_closes_sentence_channel_and_keeps_session() {
        let (sentence_tx, mut sentence_rx) = mpsc::channel::<(usize, String)>(8);
        let audio_bus = crate::adapter::headless::audio_output::AudioBus::new();
        let audio_output = audio_bus.output;
        // 消费者不启动：会话停在队列，synth 的 push_encoded 处于背压中
        let _consumer = audio_bus.consumer;
        let session = audio_output.open_tts_session().await.unwrap();
        let shared_session: SharedTtsSession = Arc::new(tokio::sync::Mutex::new(Some(session)));

        // 挂起任务模拟未收尾的 synth：旧实现会在 5s 超时后 drop 会话，消费者随即取消播放
        let synth_task = tokio::spawn(std::future::pending::<()>());

        let playback_cancel = Arc::new(AtomicBool::new(false));
        let runtime = TtsTurnRuntime {
            callbacks: StreamCallbacks::default(),
            shared_tx: Arc::new(std::sync::Mutex::new(Some(sentence_tx))),
            shared_session: shared_session.clone(),
            synth_task,
            playback_cancel: playback_cancel.clone(),
        };

        runtime.finish();

        assert!(
            sentence_rx.recv().await.is_none(),
            "finish must close the sentence channel"
        );
        assert!(
            shared_session.lock().await.is_some(),
            "finish must not drop an unfinished session (drop sets cancel and cuts queued audio)"
        );
        // finish 是正常收尾：已入队音频照常播完
        assert!(!playback_cancel.load(Ordering::SeqCst));
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

        let playback_cancel = Arc::new(AtomicBool::new(false));
        let runtime = TtsTurnRuntime {
            callbacks: StreamCallbacks::default(),
            shared_tx: Arc::new(std::sync::Mutex::new(Some(sentence_tx))),
            shared_session: shared_session.clone(),
            synth_task,
            playback_cancel: playback_cancel.clone(),
        };

        runtime.abort().await;
        assert!(shared_session.lock().await.is_none());
        assert!(playback_cancel.load(Ordering::SeqCst));
    }

    /// 会话已被 synth 任务取走（播放中）时 abort：仍必须置播放取消位，否则已交给消费者的音频不会停
    #[tokio::test]
    async fn tts_turn_runtime_abort_cancels_playing_audio_without_the_session() {
        let (sentence_tx, _sentence_rx) = mpsc::channel::<(usize, String)>(8);
        let audio_bus = crate::adapter::headless::audio_output::AudioBus::new();
        let audio_output = audio_bus.output;
        let _consumer = audio_bus.consumer;
        let session = audio_output.open_tts_session().await.unwrap();
        let shared_session: SharedTtsSession = Arc::new(tokio::sync::Mutex::new(Some(session)));

        let playback_cancel = Arc::new(AtomicBool::new(false));
        let runtime = TtsTurnRuntime {
            callbacks: StreamCallbacks::default(),
            shared_tx: Arc::new(std::sync::Mutex::new(Some(sentence_tx))),
            shared_session: shared_session.clone(),
            synth_task: tokio::spawn(std::future::pending::<()>()),
            playback_cancel: playback_cancel.clone(),
        };
        // 模拟 synth 任务已把会话取走并 await finish_drained
        let taken = shared_session.lock().await.take();
        assert!(taken.is_some());
        drop(taken);

        runtime.abort().await;
        assert!(shared_session.lock().await.is_none());
        assert!(playback_cancel.load(Ordering::SeqCst));
    }
}

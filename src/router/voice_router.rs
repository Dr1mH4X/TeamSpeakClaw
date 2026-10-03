use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use crate::adapter::headless::audio_output::{AudioOutput, TtsSession};
use crate::adapter::headless::speech::{
    detect_audio_format, is_speakable, pcm16_mono_to_wav_bytes, preprocess_stt_text,
    preprocess_text_message, OpenAiSpeechProvider, OpusSttPipeline, SpeechChunk, SttFrameOutcome,
};
use crate::adapter::headless::tsbot::voice::v1 as voicev1;
use crate::adapter::headless::wakeword::{
    WakewordGate, WakewordModels, CHUNK_SIZE as WAKE_BLOCK_SAMPLES,
};
use crate::adapter::headless::{
    parse_server_groups, TsAdapter, VoiceBridgeState, INTERNAL_GRPC_ADDR,
};
use crate::adapter::reconnect::{abort_managed_tasks, now_unix_ms};
use crate::config::{reply_target_mode, AppConfig, PromptsConfig};
use crate::llm::tool_loop::AsyncTokenCallback;
use crate::llm::{LlmEngine, SessionSource, StreamCallbacks};
use crate::permission::PermissionGate;
use crate::router::voice_feedback::{
    release_when_played, FeedbackGroup, FeedbackPlayer, ToolFeedback, WakeUtterance,
};
use crate::router::voice_turns::{
    decide_wakeword_action, ActiveTurn, TurnRegistry, WakewordAction,
};
use crate::router::{resolve_ts_inbound, TurnError, TurnInput, TurnPermit, TurnRequest, TurnSink};

use crate::skills::{SkillRegistry, TsCaller, UnifiedExecutionContext};
use tokio_util::sync::CancellationToken;
use voicev1::voice_service_client::VoiceServiceClient;

const AUDIO_MAX_IN_FLIGHT: usize = 8;
/// 单条 TTS 流的句段通道容量
const TTS_SENTENCE_CAPACITY: usize = 128;

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

/// 单轮对话内跨流共享的 TTS 资源：当前流的句段通道 + 该流独占的会话。
///
/// 会话按流创建（首个可播句段到达时）与关闭（finish_reason）：一轮对话里工具轮与最终回复
/// 各占一条流，一条流结束时它的音频 job 随即收尾，下一条流再开一条新会话。
/// 每条流的会话由其 synth 任务独占持有，任务被中止（插话）时会话随 future 一起 drop。
/// 所有会话共用回合的播放取消位（`playback_cancel`），插话一次全停。
struct TtsStreamState {
    audio_output: AudioOutput,
    speech_provider: Arc<OpenAiSpeechProvider>,
    /// 回合句柄：每个流的 synth 任务持一份直到本流音频播完，
    /// 「机器人正在产出」因此覆盖播放尾，最后一条流播完才放掉
    keepalive: Arc<ActiveTurn>,
    playback_cancel: Arc<AtomicBool>,
    trace_id: String,
    /// 当前流的句段通道；None 表示这条流已收尾
    tx: StdMutex<Option<mpsc::Sender<(usize, String)>>>,
    /// 本轮已起的 synth 任务；abort 时逐个中止
    tasks: StdMutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl TtsStreamState {
    /// 保证当前流有会话与 synth 任务：首个可播句段到达时开一条流。
    /// 没有可播内容就不开会话，避免空会话在 FIFO 上白占一个 job。
    async fn ensure_stream(self: &Arc<Self>) {
        if self.tx.lock().expect("tts tx poisoned").is_some() {
            return;
        }
        let session = match self
            .audio_output
            .open_tts_session_with_cancel(self.playback_cancel.clone())
            .await
        {
            Ok(session) => session,
            Err(error) => {
                warn!(
                    trace_id = %self.trace_id,
                    error = %error,
                    "tts session open failed"
                );
                return;
            }
        };
        let (tx, rx) = mpsc::channel::<(usize, String)>(TTS_SENTENCE_CAPACITY);
        *self.tx.lock().expect("tts tx poisoned") = Some(tx);
        let state = self.clone();
        // 会话交给该流的任务独占：任务被中止时会话随 future drop，job 的段通道随之关闭。
        // 任务另持一份回合句柄，直到本流音频播完才放掉「正在产出」。
        // 这些 synth 任务不在 router 的 JoinSet 里：它们各自持一份回合句柄，靠会话释放在
        // `finish_drained` 返回后自行收敛，插话时由 `TtsTurnRuntime::abort` 逐个中止
        let keepalive = self.keepalive.clone();
        let task = tokio::spawn(async move { state.run_synth(session, rx, keepalive).await });
        self.tasks.lock().expect("tts tasks poisoned").push(task);
    }

    /// 关闭当前流的句段通道：当前流的 synth 任务收尾会话并放掉自己那份回合句柄
    fn close_stream(&self) {
        *self.tx.lock().expect("tts tx poisoned") = None;
    }

    /// 单条流的合成任务：句段 → TTS → 会话；通道关闭后等本流音频播完
    async fn run_synth(
        self: Arc<Self>,
        mut session: TtsSession,
        mut rx: mpsc::Receiver<(usize, String)>,
        keepalive: Arc<ActiveTurn>,
    ) {
        let mut segment_index = 0usize;
        while let Some((_index, sentence)) = rx.recv().await {
            segment_index += 1;
            if !is_speakable(&sentence) {
                debug!(
                    trace_id = %self.trace_id,
                    segment = segment_index,
                    "skipping unspeakable tts segment"
                );
                continue;
            }
            match self.speech_provider.synthesize(&sentence).await {
                Ok(audio) => {
                    let codec = detect_audio_format(&audio);
                    if let Err(error) = session.push_encoded(audio, codec).await {
                        warn!(
                            trace_id = %self.trace_id,
                            segment = segment_index,
                            error = %error,
                            "tts push_encoded failed"
                        );
                        break;
                    }
                }
                Err(e) => warn!(
                    trace_id = %self.trace_id,
                    segment = segment_index,
                    error = %e,
                    "tts synthesis failed"
                ),
            }
        }
        // 等本流音频真正播完再收尾；被插话时 job 已取消，立即返回
        if let Err(error) = session.finish_drained().await {
            warn!(
                trace_id = %self.trace_id,
                error = %error,
                "tts session drain failed"
            );
        }
        // 到这里本流音频已播完（或被取消），放掉本流的回合句柄；
        // 最后一条流结束时回合条目自然失效，准入回到空闲
        drop(keepalive);
        drop(self);
    }
}

/// 每轮 TTS 运行时：一轮对话的跨流状态与 synth 任务句柄；`finish` 关当前流、`abort` 全停
struct TtsTurnRuntime {
    callbacks: StreamCallbacks,
    state: Arc<TtsStreamState>,
    /// 本回合各流音频 job 的播放取消位（回合创建即持有）：abort 先置位，
    /// 已交给消费者播放的音频同样停声
    playback_cancel: Arc<AtomicBool>,
}

impl TtsTurnRuntime {
    fn callbacks(&self) -> Option<&StreamCallbacks> {
        Some(&self.callbacks)
    }

    /// 关闭当前流的句段通道并释放本轮状态；各流的 synth 任务继续跑完剩余合成与播放
    ///
    /// synth 的 `push_encoded` 受播放实时性背压，等待任务结束等于等待剩余音频播完；
    /// 任务在句段通道关闭后 finish 本流会话（不置 cancel），消费者播完已入队音频后自然收尾。
    fn finish(self) {
        self.state.close_stream();
    }

    /// 取消：先置播放取消位，再 abort 全部 synth 任务；未收尾的会话随任务 future 一起 drop
    async fn abort(self) {
        self.playback_cancel.store(true, Ordering::SeqCst);
        self.state.close_stream();
        let tasks: Vec<_> = self
            .state
            .tasks
            .lock()
            .expect("tts tasks poisoned")
            .drain(..)
            .collect();
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
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
    /// 唤醒门：`[headless.wakeword]` 启用时 Some；per-clid 会话状态只在事件循环任务里访问
    /// （收帧路径喂门、收尾路径只读窗口）
    wakeword: Option<Mutex<WakewordGate>>,
    /// 在飞 utterance 的流式唤醒状态（键为 clid）：收帧路径累计命中与命中后活跃语音，
    /// 收尾时取走做准入裁决；只在事件循环任务里访问
    pending_wake: StdMutex<HashMap<u32, WakeUtterance>>,
    /// per-clid 活跃回合：准入判断「忙」与插话取消目标
    turns: TurnRegistry,
    /// 预生成短反馈音频：工具调用提示音与唤醒确认音共用；TTS provider 不可用时为 None
    feedback: Option<Arc<FeedbackPlayer>>,
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

/// 唤醒门对一条已收尾 utterance 的裁决：音频已在收帧路径逐帧喂过门，
/// 这里只携带结果，准入任务不再重复喂门
#[derive(Debug, Clone, Copy)]
struct WakeVerdict {
    /// 门是否放行（本条命中，或仍在既有开门窗口内）
    open: bool,
    /// 本条 utterance 是否命中唤醒词
    detected: bool,
    /// 命中块锚点之后没有成段活跃语音：本条只有唤醒词
    wakeword_only: bool,
    /// 命中块绝对起始采样位置（16k）；未命中为 None。确认音日志用它换算块序号，
    /// 与命中时的 `voice.wakeword.calibration` 互校
    anchor_sample: Option<usize>,
    /// 锚点之后的 VAD 活跃毫秒：判据实际用到的量，确认音日志据此回看阈值取舍
    anchor_active_ms: u64,
    /// 本条 utterance 首个 VAD 活跃帧的绝对采样位置（16k）：给出命令开头相对 utterance 起点的位置
    utterance_start_sample: Option<usize>,
}

impl Default for WakeVerdict {
    /// 唤醒门未启用时的等价裁决：放行、未命中、无独占可言
    fn default() -> Self {
        Self {
            open: true,
            detected: false,
            wakeword_only: false,
            anchor_sample: None,
            anchor_active_ms: 0,
            utterance_start_sample: None,
        }
    }
}

/// 合成一条 utterance 的准入裁决：命中即开门（命中状态来自收帧路径的在飞记录），
/// 未命中时沿用门窗口（窗口内续说算正常对话）；命中块锚点之后没有成段活跃语音的判为
/// 「只有唤醒词」，准入据此播确认音而不进 STT/LLM。锚点与活跃毫秒一并带出，供日志实测滞后
fn combine_wake_verdict(wake: WakeUtterance, window_open: bool) -> WakeVerdict {
    WakeVerdict {
        open: wake.detected() || window_open,
        detected: wake.detected(),
        wakeword_only: wake.detected() && !wake.has_command_tail(),
        anchor_sample: wake.anchor_sample(),
        anchor_active_ms: wake.active_ms_after(),
        utterance_start_sample: wake.first_voiced_sample(),
    }
}

/// 取消一条回合并按身份从注册表摘除：置 LLM 取消令牌与 TTS 播放取消位，再把这一条摘出集合。
/// 取消幂等，摘除只匹配 `Arc::ptr_eq` 的条目，同 clid 的其他回合不动。
/// 三处取消（插话、准入顶替、确认音顶替）共用它，避免有一处漏摘让取消句柄不可达
fn cancel_turn(turns: &TurnRegistry, turn: &Arc<ActiveTurn>) {
    turn.cancel.cancel();
    turn.playback_cancel().store(true, Ordering::SeqCst);
    turns.remove_turn(turn);
}

/// 新回合顶替同 clid 旧回合时的取消分工（`begin_turn` 返回旧条目后调用）。
///
/// 尚未产出的旧回合两种配置下都取消并摘除：它不在「忙」的视野里，不取消就会变成两条并发
/// LLM 回合，其中一条的播放取消句柄再也不可达。
///
/// 正在产出的旧回合看插话是否可用：`barge_in_available`（唤醒门开启）为真时保留在集合里，
/// 夺回话语权由用户喊唤醒词决定——出站只有一路音频，准入不替用户静音，留得住才找得到它、
/// 才取消得掉；为假时没有任何路径能取消它（既无命中插话，也无第一道 busy 闸），就地取消并
/// 摘除，否则它的播放取消句柄无人可达，回复会一直播到自然结束。
fn cancel_displaced_turns(
    turns: &TurnRegistry,
    displaced: Vec<Arc<ActiveTurn>>,
    barge_in_available: bool,
) {
    for displaced in displaced {
        if barge_in_available && displaced.is_producing() {
            continue;
        }
        cancel_turn(turns, &displaced);
    }
}

/// 插话：取消全部正在产出的回合（不分归属）。出站音频只有一路，但产出回合可以不止一条，
/// 只停最早准入的那条会留下仍在出声的回合；逐条留下 clid/age 日志便于定位谁被打断。
/// 摘除即时生效，条目失效让后续语音立刻回到空闲准入，不必等旧回合任务收敛；
/// 若某条已被顶替或自行收敛，`remove_turn` 按身份匹配不动别的条目。
///
/// 抽成自由函数：它只依赖注册表，单测可直接驱动插话的取消展开，无需装配整个 `VoiceRouter`。
fn apply_barge_in(
    turns: &TurnRegistry,
    barge_in_by: u32,
    targets: Vec<(u32, Arc<ActiveTurn>)>,
    speaker: &str,
) {
    for (owner_clid, target) in targets {
        info!(
            event = "voice.wakeword.barge_in",
            barge_in_by,
            cancelled_owner = owner_clid,
            speaker,
            turn_age_ms = target.age_ms(),
            "wakeword barge-in; cancelling a turn that owns the output"
        );
        cancel_turn(turns, &target);
    }
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
        // 短反馈短语的合成走同一 provider：STT 也用它，故 provider 不随 tts.enabled 关掉；
        // tts 未开启时根本不建缓存器，既不预热也不会有提示音/确认音
        let feedback = speech_provider
            .as_ref()
            .filter(|_| config.headless.tts.enabled)
            .map(|provider| Arc::new(FeedbackPlayer::new(provider.clone(), audio_output.clone())));
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
            pending_wake: StdMutex::new(HashMap::new()),
            turns: TurnRegistry::default(),
            feedback,
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

        // 预热短反馈短语（三个池共五条）：把 TTS 首包冷启动挪到启动期，
        // 触发时只剩进程内 opus 编码。失败不影响路由，首次触发会按需重试
        if router.is_tts_effectively_enabled() {
            if let Some(player) = router.feedback.clone() {
                tokio::spawn(async move { player.prewarm().await });
            }
        }

        // 完整 utterance 的独立有界队列：满时丢弃最新完整语音段，聊天不受影响。
        // 唤醒裁决随段一起交给准入任务：音频已在收帧路径喂过唤醒门，这里只带结果
        let (audio_chunk_tx, mut audio_chunk_rx) =
            tokio::sync::mpsc::channel::<(voicev1::AudioFrameEvent, SpeechChunk, WakeVerdict)>(
                AUDIO_MAX_IN_FLIGHT,
            );

        // 准入与执行分离：准入任务只按裁决表调度，回合在执行任务里跑，
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
                let Some((audio, chunk, verdict)) = next else {
                    break;
                };
                if let Err(error) = drain_router
                    .admit_audio_chunk(&drain_channel, audio, chunk, verdict, &mut turn_tasks)
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
                                Ok(Some(frame)) => {
                                    // 先喂唤醒门：命中立刻夺回话语权（停声），
                                    // 不必等 utterance 收尾
                                    router
                                        .feed_wakeword_frame(
                                            audio.from_client_id,
                                            &audio.from_client_name,
                                            &frame,
                                        )
                                        .await;
                                    if let Some(chunk) = frame.chunk {
                                        let verdict = router
                                            .take_wake_verdict(chunk.speaker_client_id)
                                            .await;
                                        if let Err(error) =
                                            audio_chunk_tx.try_send((audio, chunk, verdict))
                                        {
                                            warn!(
                                                dropped = 1,
                                                error = %error,
                                                "audio worker queue full; dropping latest utterance"
                                            );
                                        }
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
                        let verdict = router.take_wake_verdict(chunk.speaker_client_id).await;
                        if let Err(error) = audio_chunk_tx.try_send((audio, chunk, verdict)) {
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
        // 文本不经唤醒门，不夺取话语权：正在播报的语音回合继续播完。忽略 `begin_turn`
        // 的返回值（不取消、不摘除那些旧条目），它们留在集合里，插话仍能取到并取消
        let active = Arc::new(ActiveTurn::new());
        self.turns.begin_turn(ctx.caller_id, &active);
        // 桥接文本不是 STT 路径：不挂工具调用提示音
        self.handle_user_input(channel, ctx, clean_text, &active, false)
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
    ) -> Result<Option<SttFrameOutcome>> {
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

    /// 收帧路径：把这一帧喂唤醒门。命中即夺回话语权——TTS 立刻停声，
    /// 不必等整条 utterance 收尾（旧实现整段喂门，插话要等用户把指令说完再断句）。
    /// 命中同时记入该 clid 的在飞状态，供收尾时的准入裁决使用。
    async fn feed_wakeword_frame(&self, clid: u32, speaker: &str, frame: &SttFrameOutcome) {
        let Some(gate) = &self.wakeword else {
            return;
        };
        if frame.utterance_started {
            // 新的 utterance：清掉上一段残留的命中状态，避免独占判定串到新一段
            self.pending_wake
                .lock()
                .expect("wake pending poisoned")
                .remove(&clid);
        }
        let event = {
            let mut gate = gate.lock().await;
            gate.feed(clid, &frame.mono_16k, Instant::now())
        };
        // 本帧在门时间轴上的绝对起点：与 `fire_sample`（命中块绝对起点）同一坐标系
        let frame_sample = event.fed_samples_before;
        let frame_ms = frame.mono_16k.len() as u64 * 1000 / 16_000;
        let (anchor_active_ms, utterance_start_sample) = {
            let mut pending = self.pending_wake.lock().expect("wake pending poisoned");
            let wake = pending.entry(clid).or_default();
            // 命中时 `feed` 必给出命中块起点（`fire_sample` 与 `detected` 同源置位），
            // 用绝对锚点而不是本帧位置：命中块的起点在这之前最多 carry 采样处
            if let Some(fire_sample) = event.fire_sample {
                wake.note_detection(fire_sample);
            }
            wake.note_frame(frame_sample, frame.voiced, frame_ms);
            (wake.active_ms_after(), wake.first_voiced_sample())
        };
        let Some(fire_sample) = event.fire_sample else {
            // 未命中：本帧只进在飞状态，不触发插话与打点
            return;
        };
        info!(
            event = "voice.wakeword",
            clid,
            speaker,
            probability = event.verdict.probability,
            fire_sample,
            detection_block_index = fire_sample / WAKE_BLOCK_SAMPLES,
            input_samples = event.fed_samples_before + frame.mono_16k.len(),
            window_open_before_feed = event.window_open_before_feed,
            "wakeword detected; opening gate"
        );
        debug!(
            event = "voice.wakeword.calibration",
            clid,
            fire_sample,
            // 命中块序号：可与 fire_sample / 块长 互校，也可与 raw_scores 的 chunk_index 对齐
            detection_block_index = fire_sample / WAKE_BLOCK_SAMPLES,
            // utterance 起点与锚点后活跃毫秒：把命中位置换算成「utterance 内命令开头在哪」，
            // 用于实测 T_rep − T_end 这条判据滞后（旧打点只有命中时刻，无法对齐 VAD 时间轴）
            utterance_start_sample = ?utterance_start_sample,
            anchor_active_ms,
            input_samples = event.fed_samples_before + frame.mono_16k.len(),
            first_chunk_index = event.calibration.first_chunk_index,
            peak_chunk_index = event.calibration.peak_chunk_index().unwrap_or(0),
            peak_score = event.calibration.peak_score().unwrap_or(0.0),
            raw_scores = %event.calibration.scores_compact(),
            "wakeword raw score history for boundary calibration"
        );
        // 命中即抢占：出站音频只有一路，产出中的回合就是通道持有者。
        // 取「忙」与取取消目标必须来自同一次快照，否则会在两次加锁之间漂移。
        // 产出回合可以不止一条（后准入但先入队的回合已经出声），全部取消
        let targets = self.turns.producing_turns();
        if !targets.is_empty() {
            apply_barge_in(&self.turns, clid, targets, speaker);
        }
    }

    /// 收段路径：取走这条 utterance 的唤醒裁决。音频已经逐帧喂过门，
    /// 这里只读窗口状态，不再喂音频（重复喂会打乱推理窗口）
    async fn take_wake_verdict(&self, clid: u32) -> WakeVerdict {
        let Some(gate) = &self.wakeword else {
            return WakeVerdict::default();
        };
        let wake = self
            .pending_wake
            .lock()
            .expect("wake pending poisoned")
            .remove(&clid)
            .unwrap_or_default();
        let window_open = {
            let mut gate = gate.lock().await;
            gate.window_state(clid, Instant::now()).open
        };
        combine_wake_verdict(wake, window_open)
    }

    /// 准入：按唤醒裁决表调度 → 丢弃 / 插话 / 注册并派发回合任务。
    /// 这里不做 gRPC 解析与 STT，保证机器人播报期间仍能即时裁决后续语音。
    async fn admit_audio_chunk(
        self: &Arc<Self>,
        channel: &VoiceChannel,
        audio: voicev1::AudioFrameEvent,
        chunk: SpeechChunk,
        verdict: WakeVerdict,
        turn_tasks: &mut JoinSet<()>,
    ) -> Result<()> {
        let clid = chunk.speaker_client_id;
        // 唤醒门开启 = 存在插话机制：收帧路径的命中插话与第一道 busy 闸都由它提供。
        // 第二道闸据此决定产出中的旧回合交给谁取消，所以求值一次、两处共用
        let barge_in_available = self.wakeword.is_some();
        // 唤醒门先于 gRPC 解析：关门丢弃不产生解析/查询开销
        if barge_in_available {
            // 「忙」与插话取消目标取自同一次快照：出站音频只有一路，产出中的回合就是通道持有者。
            // 分两次查询（先判忙再取目标）会在两次加锁之间漂移，出现「判定为忙却取不到目标」
            // 或反过来该插话却判成空闲
            let producing = self.turns.producing_turns();
            let busy = !producing.is_empty();
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
                // 收帧路径命中时已经插话停声；这里兜住「命中之后又有新回合开始产出」的竞态
                WakewordAction::BargeIn => {
                    if producing.is_empty() {
                        // 防御分支：音频路径上不 panic。busy 与目标取自同一次快照，
                        // BargeIn 理论上不会配空集合；真取不到目标就按 Talk 处理，
                        // 本条语音照常走回合，不因为取不到取消目标而丢话
                        debug!(
                            event = "voice.wakeword.barge_in.empty",
                            clid,
                            speaker = %chunk.speaker_name,
                            "wakeword barge-in without a producing target; treating as talk"
                        );
                    } else {
                        apply_barge_in(&self.turns, clid, producing, &chunk.speaker_name);
                    }
                }
            }

            // 命中块锚点之后没有成段活跃语音 = 这条 utterance 只有唤醒词：播确认音代替 STT/LLM，
            // 让用户立刻知道唤醒了；真正的指令在窗口内随后到达，走正常回合
            if verdict.detected && verdict.wakeword_only {
                info!(
                    event = "voice.wakeword.confirm",
                    clid,
                    speaker = %chunk.speaker_name,
                    input_samples = chunk.pcm16_mono_16k.len(),
                    // 与命中时的 `voice.wakeword.calibration` 同一组锚点字段，
                    // 便于事后核对「判成独占的这段为什么没够到阈值」
                    anchor_sample = ?verdict.anchor_sample,
                    detection_block_index = ?verdict.anchor_sample.map(|sample| sample / WAKE_BLOCK_SAMPLES),
                    utterance_start_sample = ?verdict.utterance_start_sample,
                    anchor_active_ms = verdict.anchor_active_ms,
                    "wakeword-only utterance; playing the confirmation phrase"
                );
                if let Err(error) = self.play_wake_confirmation(clid, &chunk.speaker_name).await {
                    warn!(
                        clid,
                        error = %error,
                        "wakeword confirmation playback failed"
                    );
                }
                return Ok(());
            }
        }

        let active = Arc::new(ActiveTurn::new());
        // 第二道闸：同说话人可能还有尚未进入产出的旧回合（正在解析/STT）。它不在「忙」的视野里，
        // 第一道闸看不到它，却已持有回合状态与播放取消句柄；取消与保留的分工见
        // `cancel_displaced_turns`
        cancel_displaced_turns(
            &self.turns,
            self.turns.begin_turn(clid, &active),
            barge_in_available,
        );
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

    /// 播报小回合：唤醒确认音没有真实回合，就地登记一个轻量回合并标记产出，
    /// 让回来的回声在准入表里走「忙」分支、让后续唤醒词能插话取消它。
    ///
    /// 先入队片段再登记：入队失败时不留一条「永远产出」的条目——那会永久挡住后续语音。
    /// 取消位由播报回合自己持有，与真实回合的播放取消位同语义。
    async fn play_wake_confirmation(&self, clid: u32, speaker: &str) -> Result<()> {
        let Some(player) = &self.feedback else {
            debug!(
                clid,
                "wakeword confirmation skipped: feedback disabled (tts off or provider unavailable)"
            );
            return Ok(());
        };
        if !self.is_tts_effectively_enabled() {
            debug!(clid, "wakeword confirmation skipped: tts disabled");
            return Ok(());
        }
        let announcement = Arc::new(ActiveTurn::new());
        let playback_cancel = announcement.playback_cancel();
        // 续窗先于入队：确认即代表「我在听」，用户的接话按确认时刻重新计时
        if let Some(gate) = &self.wakeword {
            gate.lock().await.refresh_window(clid, Instant::now());
        }
        let (phrase, handle) = player
            .play(FeedbackGroup::WakeConfirm, playback_cancel)
            .await?;
        for displaced in self.turns.begin_turn(clid, &announcement) {
            // 同 clid 的旧条目被顶替：确认音本身就是唤醒词夺回话语权的可听结果，
            // 全部取消并摘除，不留下不可达的取消句柄
            cancel_turn(&self.turns, &displaced);
        }
        announcement.mark_producing();
        debug!(
            clid,
            speaker,
            phrase = %phrase,
            "wakeword confirmation queued"
        );
        release_when_played(handle, announcement);
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

        // STT 语音回合：挂工具调用提示音
        self.handle_user_input(&channel, ctx, text, &active, true)
            .await
    }

    async fn handle_omni_audio_chunk(
        &self,
        channel: &VoiceChannel,
        ctx: CallerContext,
        chunk: SpeechChunk,
        active: &Arc<ActiveTurn>,
    ) -> Result<()> {
        let wav_bytes = pcm16_mono_to_wav_bytes(&chunk.pcm16_mono_16k, 16_000);

        // 并发门禁：TurnCoordinator 管 LLM 轮；AudioOutput FIFO 管出站，不再使用 tts_lock
        let (system_prompt, user_ctx, allowed_skills, session_source) =
            self.build_llm_request(&ctx).await;
        let Ok(permit) = TurnPermit::reserve(&self.llm) else {
            warn!(
                caller_uid = %ctx.caller_uid,
                "Voice LLM turn queue full; dropping audio chunk"
            );
            return Ok(());
        };
        let tts_runtime = if self.is_tts_effectively_enabled() {
            // omni 与 STT 同属语音回合：挂工具调用提示音，走 STT 路径同一块反馈基建
            Some(self.build_tts_callbacks(active, true).await?)
        } else {
            None
        };
        active.mark_producing();

        let sink = VoiceNoticeSink {
            channel: channel.clone(),
            target_mode: ctx.reply_target_mode,
            target_client_id: ctx.reply_target_client_id,
        };
        let request = TurnRequest {
            llm: &self.llm,
            registry: &self.registry,
            source: &session_source,
            system_prompt: &system_prompt,
            user_ctx: &user_ctx,
            allowed_skills: &allowed_skills,
            input: TurnInput::Audio(&wav_bytes),
            callbacks: tts_runtime.as_ref().and_then(TtsTurnRuntime::callbacks),
            cancel: &active.cancel,
        };
        let result = request
            .run(
                permit,
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
                &sink,
            )
            .await;

        if let Some(runtime) = tts_runtime {
            if result.is_ok() {
                runtime.finish();
            } else {
                runtime.abort().await;
            }
        }
        match result {
            // 插话取消：不回错误文案、不落上下文；播放取消句柄已由插话方置位
            Err(TurnError::Cancelled) => {
                debug!(
                    caller_uid = %ctx.caller_uid,
                    "voice omni turn cancelled by barge-in"
                );
                Ok(())
            }
            Err(TurnError::Failed(error)) | Err(TurnError::ReplyFailed(error)) => Err(error),
            Ok(()) => Ok(()),
        }
    }

    /// `tool_feedback` 为真时挂工具调用提示音：STT 与 omni 两条语音回合都挂，
    /// 桥接文本不挂（回复本身就是文本反馈）
    async fn handle_user_input(
        &self,
        channel: &VoiceChannel,
        ctx: CallerContext,
        user_msg: String,
        active: &Arc<ActiveTurn>,
        tool_feedback: bool,
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
        let Ok(permit) = TurnPermit::reserve(&self.llm) else {
            warn!(
                caller_uid = %ctx.caller_uid,
                "Voice LLM turn queue full; dropping message"
            );
            return Ok(());
        };

        let tts_runtime = if self.is_tts_effectively_enabled() {
            Some(self.build_tts_callbacks(active, tool_feedback).await?)
        } else {
            None
        };
        active.mark_producing();

        let sink = VoiceNoticeSink {
            channel: channel.clone(),
            target_mode: ctx.reply_target_mode,
            target_client_id: ctx.reply_target_client_id,
        };
        let request = TurnRequest {
            llm: &self.llm,
            registry: &self.registry,
            source: &session_source,
            system_prompt: &system_prompt,
            user_ctx: &user_ctx,
            allowed_skills: &allowed_skills,
            input: TurnInput::Text(&user_msg),
            callbacks: tts_runtime.as_ref().and_then(TtsTurnRuntime::callbacks),
            cancel: &active.cancel,
        };
        let result = request
            .run(
                permit,
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
                &sink,
            )
            .await;

        if let Some(runtime) = tts_runtime {
            if result.is_ok() {
                runtime.finish();
            } else {
                runtime.abort().await;
            }
        }
        match result {
            // 插话取消：不回错误文案、不落上下文；播放取消句柄已由插话方置位
            Err(TurnError::Cancelled) => {
                debug!(
                    caller_uid = %ctx.caller_uid,
                    "voice turn cancelled by barge-in"
                );
                Ok(())
            }
            Err(TurnError::Failed(error)) | Err(TurnError::ReplyFailed(error)) => Err(error),
            Ok(()) => Ok(()),
        }
    }

    /// 每轮 TTS：会话按 LLM 流创建（首个可播句段）与关闭（finish_reason），
    /// 一条流结束时它的音频 job 随即收尾——工具提示音因此不会被排在仍开着的会话后面；
    /// 工具轮之后的最终回复按需开新会话，不会被吞掉。
    /// `keepalive` 是准入注册的活跃回合：每个流的 synth 任务持一份直到该流音频播放收尾，
    /// 让「机器人正在产出」覆盖播放尾，插话在 LLM 流结束后仍能取消播放。
    /// `tool_feedback` 为真时挂上工具调用提示音（语音回合：STT 与 omni 同路）
    async fn build_tts_callbacks(
        &self,
        keepalive: &Arc<ActiveTurn>,
        tool_feedback: bool,
    ) -> Result<TtsTurnRuntime> {
        let speech_provider = self
            .speech_provider
            .clone()
            .ok_or_else(|| anyhow::anyhow!("TTS provider missing"))?;
        // 播放取消位由回合持有：本轮的多条 TTS 流与提示音片段共用它，插话一次全停
        let playback_cancel = keepalive.playback_cancel();
        let tool_feedback = match (tool_feedback, &self.feedback) {
            (true, Some(player)) => Some(ToolFeedback::new(
                player.clone(),
                playback_cancel.clone(),
                keepalive.cancel.clone(),
            )),
            _ => None,
        };
        let state = Arc::new(TtsStreamState {
            audio_output: self.audio_output.clone(),
            speech_provider,
            keepalive: keepalive.clone(),
            playback_cancel: playback_cancel.clone(),
            trace_id: format!("tts-{}", now_unix_ms()),
            tx: StdMutex::new(None),
            tasks: StdMutex::new(Vec::new()),
        });

        let chunker = Arc::new(std::sync::Mutex::new(StreamingSentenceChunker::new(
            Self::STREAM_TTS_MIN_CHARS,
            Self::STREAM_TTS_WEAK_PUNCT_MIN_CHARS,
            Self::STREAM_TTS_MAX_CHARS,
        )));

        let on_text_token_state = state.clone();
        let on_text_token_chunker = chunker.clone();
        let on_text_token = move |token: &str| {
            let token = token.to_string();
            let chunker = on_text_token_chunker.clone();
            let state = on_text_token_state.clone();
            Box::pin(async move {
                let segments: Vec<(usize, String)> = {
                    let mut chunker_guard = chunker.lock().expect("chunker poisoned");
                    chunker_guard
                        .push_token(&token)
                        .into_iter()
                        .map(|segment| (0, segment))
                        .collect()
                };
                if segments.is_empty() {
                    return;
                }
                // 有可播句段才开会话：工具轮结束后的最终回复在这里开新的一条流
                state.ensure_stream().await;
                let tx = state.tx.lock().expect("tts tx poisoned").as_ref().cloned();
                if let Some(tx) = tx {
                    for (index, segment) in segments {
                        if tx.send((index, segment)).await.is_err() {
                            break;
                        }
                    }
                }
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        };

        let on_turn_end_state = state.clone();
        let on_turn_end_chunker = chunker.clone();
        let on_turn_end = move |finish_reason: &str| {
            let finish_reason = finish_reason.to_string();
            let chunker = on_turn_end_chunker.clone();
            let state = on_turn_end_state.clone();
            Box::pin(async move {
                // 会话生命周期 = 单次 LLM 流：不论 finish_reason（含 `tool_calls`）都关掉当前流，
                // 让这条流已合成的音频立刻进 FIFO 播放——工具提示音因此不会被排在仍开着的会话后面；
                // 工具执行完的下一次流（最终回复）由首个可播句段按需开新会话，不会被吞掉
                debug!(
                    trace_id = %state.trace_id,
                    finish_reason = %finish_reason,
                    "closing the tts stream at llm stream end"
                );
                let segments: Vec<(usize, String)> = {
                    let mut chunker_guard = chunker.lock().expect("chunker poisoned");
                    chunker_guard
                        .finish()
                        .into_iter()
                        .map(|segment| (0, segment))
                        .collect()
                };
                if !segments.is_empty() {
                    state.ensure_stream().await;
                    let tx = state.tx.lock().expect("tts tx poisoned").as_ref().cloned();
                    if let Some(tx) = tx {
                        for (index, segment) in segments {
                            if tx.send((index, segment)).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                // 含 tool_calls：先关当前流（synth finish 会话）再执行 tool，
                // 工具提示音与新的一条流（最终回复）都排在它后面
                state.close_stream();
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        };

        let on_tool_call_start = tool_feedback.map(make_tool_callback);

        Ok(TtsTurnRuntime {
            callbacks: StreamCallbacks {
                on_text_token: Some(Box::new(on_text_token)),
                on_turn_end: Some(Box::new(on_turn_end)),
                on_tool_call_start,
            },
            state,
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
        send_notice(
            channel,
            ctx.reply_target_mode,
            ctx.reply_target_client_id,
            text,
        )
        .await
    }
}

/// 经语音桥发送一条文本通知：回合回复与命令回执共用一条出站路径
async fn send_notice(
    channel: &VoiceChannel,
    target_mode: i32,
    target_client_id: u32,
    text: &str,
) -> Result<()> {
    let req = voicev1::NoticeRequest {
        message: text.to_string(),
        target_mode,
        target_client_id,
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

/// 语音桥回复落点：回复目标在准入时解析后固定
struct VoiceNoticeSink {
    channel: VoiceChannel,
    target_mode: i32,
    target_client_id: u32,
}

#[async_trait]
impl TurnSink for VoiceNoticeSink {
    async fn send(&self, text: &str) -> Result<()> {
        send_notice(&self.channel, self.target_mode, self.target_client_id, text).await
    }
}

/// 工具开始回调槽：每次工具调用开始执行时播一句短反馈
fn make_tool_callback(feedback: Arc<ToolFeedback>) -> AsyncTokenCallback {
    Box::new(move |tool: &str| {
        let feedback = feedback.clone();
        let tool = tool.to_string();
        Box::pin(async move {
            feedback.on_start(&tool).await;
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    })
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
        let (tx, mut rx) = mpsc::channel::<(voicev1::AudioFrameEvent, SpeechChunk, WakeVerdict)>(1);
        let audio = voicev1::AudioFrameEvent::default();
        let chunk = SpeechChunk {
            speaker_client_id: 1,
            speaker_name: "a".to_string(),
            pcm16_mono_16k: Vec::new(),
        };
        tx.try_send((audio.clone(), chunk, WakeVerdict::default()))
            .unwrap();

        // 队列满：最新 utterance 被拒绝而非等待
        assert!(tx
            .try_send((
                audio,
                SpeechChunk {
                    speaker_client_id: 2,
                    speaker_name: "b".to_string(),
                    pcm16_mono_16k: Vec::new(),
                },
                WakeVerdict::default(),
            ))
            .is_err());

        let (_, first, _) = rx.recv().await.unwrap();
        assert_eq!(first.speaker_client_id, 1);
    }

    /// 准入裁决的合成：命中即放行且独占与否由「命中块锚点之后的活跃语音」决定，
    /// 未命中时只由门窗口决定放行——这条语义决定「唤醒词+指令」是否立刻进 STT/LLM
    #[test]
    fn wake_verdict_combines_detection_and_window_state() {
        // 命中、锚点之后无活跃语音：播确认音，不进 STT/LLM
        let mut only_wake = WakeUtterance::default();
        only_wake.note_detection(1_280);
        let verdict = combine_wake_verdict(only_wake, false);
        assert!(verdict.open && verdict.detected && verdict.wakeword_only);
        // 锚点与活跃毫秒随裁决带出，供确认音日志
        assert_eq!(verdict.anchor_sample, Some(1_280));
        assert_eq!(verdict.anchor_active_ms, 0);

        // 命中且随后有成段指令：正常对话（窗口查询结果不影响命中即放行）
        let mut command = WakeUtterance::default();
        command.note_detection(0);
        for index in 0..10 {
            command.note_frame(index * 320, true, 20);
        }
        let verdict = combine_wake_verdict(command, false);
        assert!(verdict.open && verdict.detected && !verdict.wakeword_only);
        // 10 帧 × 20ms = 200ms，恰好等于 `WAKE_ONLY_TAIL_MS`
        assert_eq!(verdict.anchor_active_ms, 200);

        // 未命中但在窗口内：续说，正常对话
        let verdict = combine_wake_verdict(WakeUtterance::default(), true);
        assert!(verdict.open && !verdict.detected && !verdict.wakeword_only);
        assert_eq!(verdict.anchor_sample, None);

        // 未命中且窗口已关：丢弃
        let verdict = combine_wake_verdict(WakeUtterance::default(), false);
        assert!(!verdict.open && !verdict.detected);
    }

    /// 锚点口径的端到端合成：命中块内、报告之前喂入的活跃帧被回补计入，
    /// 「唤醒词 停」因此不再被判成唤醒词独占；命中块之后的静音帧不推高计数
    #[test]
    fn wake_verdict_backfills_the_hit_block_at_the_threshold_boundary() {
        // 命中块 4 帧 = 80ms，报告帧是块的最后一帧（前 3 帧在报告之前已喂入）
        let mut wake = WakeUtterance::default();
        for index in 0..3 {
            wake.note_frame(index * 320, true, 20);
        }
        wake.note_detection(0);
        wake.note_frame(960, true, 20);
        let verdict = combine_wake_verdict(wake.clone(), false);
        assert!(verdict.detected && verdict.wakeword_only);
        assert_eq!(verdict.anchor_active_ms, 80);
        assert_eq!(verdict.utterance_start_sample, Some(0));

        // 「停」再攒 6 帧 = 120ms，合计恰好 200ms：判为有命令，进 STT/LLM
        let mut command = wake;
        for index in 4..10 {
            command.note_frame(index * 320, true, 20);
        }
        let verdict = combine_wake_verdict(command, false);
        assert!(verdict.detected && !verdict.wakeword_only);
        assert_eq!(verdict.anchor_active_ms, 200);

        // 命中块结束后的 VAD 静音不把计数推过阈值；锚点仍可换算命中块序号
        let mut silent = WakeUtterance::default();
        silent.note_frame(0, true, 20);
        silent.note_detection(1_280);
        silent.note_frame(1_280, false, 20);
        silent.note_frame(1_600, false, 20);
        let verdict = combine_wake_verdict(silent, false);
        assert!(verdict.wakeword_only);
        assert_eq!(verdict.anchor_active_ms, 0);
        assert_eq!(
            verdict
                .anchor_sample
                .map(|sample| sample / WAKE_BLOCK_SAMPLES),
            Some(1)
        );
    }

    /// 同一 clid 两条产出回合：插话一次把两条都取消——LLM 取消令牌与 TTS 播放取消位都置位，
    /// 且两条都从集合摘除。出站 FIFO 的先后由入队时刻决定，只挑最早准入的那条会留下
    /// 仍在出声的回合，用户听到的回复继续播
    #[test]
    fn barge_in_cancels_every_producing_turn_of_one_clid() {
        let turns = TurnRegistry::default();
        let first = Arc::new(ActiveTurn::new());
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = Arc::new(ActiveTurn::new());
        let first_playback = first.playback_cancel();
        let second_playback = second.playback_cancel();
        turns.begin_turn(4, &first);
        turns.begin_turn(4, &second);
        first.mark_producing();
        second.mark_producing();
        assert_eq!(turns.producing_turns().len(), 2);

        apply_barge_in(&turns, 9, turns.producing_turns(), "bob");

        assert!(first.cancel.is_cancelled());
        assert!(second.cancel.is_cancelled());
        assert!(first_playback.load(Ordering::SeqCst));
        assert!(second_playback.load(Ordering::SeqCst));
        assert!(turns.producing_turns().is_empty());
        assert!(turns.active(4).is_none());
    }

    /// 不同 clid 两条产出回合：一次插话全部取消；未产出的回合不受影响，
    /// 仍在集合里等它自己进入产出（并被后续插话取消）
    #[test]
    fn barge_in_cancels_producing_turns_across_clids() {
        let turns = TurnRegistry::default();
        let speaker = Arc::new(ActiveTurn::new());
        let other = Arc::new(ActiveTurn::new());
        let idle = Arc::new(ActiveTurn::new());
        let speaker_playback = speaker.playback_cancel();
        let other_playback = other.playback_cancel();
        turns.begin_turn(1, &speaker);
        turns.begin_turn(2, &other);
        turns.begin_turn(3, &idle);
        speaker.mark_producing();
        other.mark_producing();

        apply_barge_in(&turns, 2, turns.producing_turns(), "carol");

        assert!(speaker.cancel.is_cancelled());
        assert!(other.cancel.is_cancelled());
        assert!(speaker_playback.load(Ordering::SeqCst));
        assert!(other_playback.load(Ordering::SeqCst));
        assert!(turns.producing_turns().is_empty());
        assert!(!idle.cancel.is_cancelled());
        assert!(turns.active(3).is_some());
    }

    /// 插话后同 clid 的新回合仍可被取消，且不被上一条的陈旧摘除误删：
    /// 取消按身份精确匹配，新条目接管该 clid 后照常进入产出
    #[test]
    fn barge_in_leaves_a_later_turn_of_the_same_clid_cancellable() {
        let turns = TurnRegistry::default();
        let first = Arc::new(ActiveTurn::new());
        turns.begin_turn(7, &first);
        first.mark_producing();
        let stale = turns.producing_turns();
        apply_barge_in(&turns, 8, stale, "dave");

        let successor = Arc::new(ActiveTurn::new());
        turns.begin_turn(7, &successor);
        successor.mark_producing();
        apply_barge_in(&turns, 8, turns.producing_turns(), "dave");

        assert!(successor.cancel.is_cancelled());
        assert!(successor.playback_cancel().load(Ordering::SeqCst));
        assert!(turns.active(7).is_none());
    }

    /// 插话可用（唤醒门开启）时，新语音顶替同 clid 正在产出的旧回合：不取消也不摘除，
    /// 留给用户喊唤醒词决定何时夺回话语权；旧回合仍留在集合里，后续插话找得到、取消得掉
    #[test]
    fn displaced_producing_turn_survives_while_barge_in_is_available() {
        let turns = TurnRegistry::default();
        let voice = Arc::new(ActiveTurn::new());
        turns.begin_turn(5, &voice);
        voice.mark_producing();
        let playback = voice.playback_cancel();

        let successor = Arc::new(ActiveTurn::new());
        let displaced = turns.begin_turn(5, &successor);
        cancel_displaced_turns(&turns, displaced, true);

        assert!(!voice.cancel.is_cancelled());
        assert!(!playback.load(Ordering::SeqCst));
        let producing = turns.producing_turns();
        assert_eq!(producing.len(), 1);
        assert!(Arc::ptr_eq(&producing[0].1, &voice));
        let latest = turns.active(5).expect("new turn is registered");
        assert!(Arc::ptr_eq(&latest, &successor));

        // 留下的旧回合确实还能被插话取消
        apply_barge_in(&turns, 6, turns.producing_turns(), "erin");
        assert!(voice.cancel.is_cancelled());
        assert!(playback.load(Ordering::SeqCst));
        assert!(turns.producing_turns().is_empty());
        assert!(turns.active(5).is_some(), "successor stays registered");
    }

    /// 插话不可用（无唤醒词）时没有任何路径能取消产出中的回合：顶替它的语音准入就地取消
    /// 并摘除，cancel 令牌与播放取消位都置位，不留无人可达的播放取消句柄；后继回合照常留下
    #[test]
    fn displaced_producing_turn_is_cancelled_without_a_barge_in_path() {
        let turns = TurnRegistry::default();
        let voice = Arc::new(ActiveTurn::new());
        turns.begin_turn(5, &voice);
        voice.mark_producing();
        let playback = voice.playback_cancel();

        let successor = Arc::new(ActiveTurn::new());
        let displaced = turns.begin_turn(5, &successor);
        cancel_displaced_turns(&turns, displaced, false);

        assert!(voice.cancel.is_cancelled());
        assert!(playback.load(Ordering::SeqCst));
        assert!(turns.producing_turns().is_empty());
        assert!(!turns.remove_turn(&voice), "cancelled turn was removed");
        let registered = turns.active(5).expect("successor stays registered");
        assert!(Arc::ptr_eq(&registered, &successor));
        assert!(!successor.cancel.is_cancelled());
    }

    /// 尚未产出的旧回合两种配置下都取消并摘除：它不在「忙」的视野里，不取消会成为两条并发
    /// LLM 回合；插话可用与否都不改变这条分工
    #[test]
    fn pending_displaced_turns_are_cancelled_with_or_without_barge_in() {
        for barge_in_available in [true, false] {
            let turns = TurnRegistry::default();
            let pending = Arc::new(ActiveTurn::new());
            turns.begin_turn(9, &pending);
            let playback = pending.playback_cancel();
            let successor = Arc::new(ActiveTurn::new());

            let displaced = turns.begin_turn(9, &successor);
            cancel_displaced_turns(&turns, displaced, barge_in_available);

            assert!(!pending.is_producing());
            assert!(pending.cancel.is_cancelled());
            assert!(playback.load(Ordering::SeqCst));
            assert!(!turns.remove_turn(&pending), "pending was removed");
            let registered = turns.active(9).expect("successor stays registered");
            assert!(Arc::ptr_eq(&registered, &successor));
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

    fn test_tts_state(
        audio_output: crate::adapter::headless::audio_output::AudioOutput,
        keepalive: Arc<ActiveTurn>,
        playback_cancel: Arc<AtomicBool>,
    ) -> Arc<TtsStreamState> {
        let provider = Arc::new(
            OpenAiSpeechProvider::new(Arc::new(AppConfig::default()), String::new())
                .expect("speech provider"),
        );
        Arc::new(TtsStreamState {
            audio_output,
            speech_provider: provider,
            keepalive,
            playback_cancel,
            trace_id: "tts-test".to_string(),
            tx: StdMutex::new(None),
            tasks: StdMutex::new(Vec::new()),
        })
    }

    /// 会话生命周期 = 单次 LLM 流：工具轮关掉当前会话后，最终回复必须再开一条新会话，
    /// 否则最终回复的合成会被丢掉（工具提示音与回复都排在会话后面）
    #[tokio::test]
    async fn tts_stream_state_opens_a_separate_session_per_stream() {
        let bus = crate::adapter::headless::audio_output::AudioBus::new();
        let audio_output = bus.output.clone();
        // 消费者不启动：会话作为 job 停在 FIFO 里，可以直接数出来
        let _consumer = bus.consumer;
        let keepalive = Arc::new(ActiveTurn::new());
        let state = test_tts_state(
            audio_output.clone(),
            keepalive.clone(),
            keepalive.playback_cancel(),
        );

        state.ensure_stream().await;
        assert_eq!(audio_output.status().queued_jobs, 1);

        // tool_calls：关当前流，然后最终回复的第一个可播句段再开一条
        state.close_stream();
        state.ensure_stream().await;
        assert_eq!(
            audio_output.status().queued_jobs,
            2,
            "工具轮之后的最终回复必须有自己的会话"
        );

        // 同一条流内重复调用不重复开会话
        state.ensure_stream().await;
        assert_eq!(audio_output.status().queued_jobs, 2);

        for task in state.tasks.lock().expect("tts tasks poisoned").drain(..) {
            task.abort();
        }
    }

    #[tokio::test]
    async fn tts_turn_runtime_finish_closes_the_stream_without_cancelling() {
        let bus = crate::adapter::headless::audio_output::AudioBus::new();
        let _consumer = bus.consumer;
        let keepalive = Arc::new(ActiveTurn::new());
        let playback_cancel = keepalive.playback_cancel();
        let state = test_tts_state(bus.output, keepalive, playback_cancel.clone());
        let (tx, mut rx) = mpsc::channel::<(usize, String)>(8);
        *state.tx.lock().expect("tts tx poisoned") = Some(tx);

        let runtime = TtsTurnRuntime {
            callbacks: StreamCallbacks::default(),
            state: state.clone(),
            playback_cancel: playback_cancel.clone(),
        };
        runtime.finish();

        assert!(
            rx.recv().await.is_none(),
            "finish must close the sentence channel"
        );
        assert!(
            !playback_cancel.load(Ordering::SeqCst),
            "finish is a normal teardown: queued audio keeps playing"
        );
    }

    /// abort：置播放取消位并中止全部流的 synth 任务；未收尾的会话随任务 future 一起 drop
    #[tokio::test]
    async fn tts_turn_runtime_abort_cancels_playback_and_every_stream() {
        let bus = crate::adapter::headless::audio_output::AudioBus::new();
        let audio_output = bus.output.clone();
        let _consumer = bus.consumer;
        let keepalive = Arc::new(ActiveTurn::new());
        let playback_cancel = keepalive.playback_cancel();
        let state = test_tts_state(audio_output.clone(), keepalive, playback_cancel.clone());
        state.ensure_stream().await;
        assert_eq!(state.tasks.lock().expect("tts tasks poisoned").len(), 1);

        let runtime = TtsTurnRuntime {
            callbacks: StreamCallbacks::default(),
            state: state.clone(),
            playback_cancel: playback_cancel.clone(),
        };
        runtime.abort().await;

        assert!(playback_cancel.load(Ordering::SeqCst));
        assert!(state.tx.lock().expect("tts tx poisoned").is_none());
        assert!(state.tasks.lock().expect("tts tasks poisoned").is_empty());
    }
}

use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use crate::adapter::headless::audio_output::{AudioOutput, TtsSession};
use crate::adapter::headless::speech::{
    detect_audio_format, is_speakable, pcm16_mono_to_wav_bytes, preprocess_stt_text,
    preprocess_text_message, OpenAiSpeechProvider, OpusSttPipeline, SpeechChunk, SttFrameOutcome,
};
use crate::adapter::headless::tsbot::voice::v1 as voicev1;
use crate::adapter::headless::wakeword::{WakeFrame, WakeVerdict, WakewordGate, WakewordModels};
use crate::adapter::headless::{parse_server_groups, TsAdapter, INTERNAL_GRPC_ADDR};
use crate::adapter::lifecycle::{BridgeComponent, BridgeReadiness};
use crate::adapter::reconnect::{abort_managed_tasks, now_unix_ms};
use crate::config::{reply_target_mode, AppConfig};
use crate::llm::tool_loop::AsyncTokenCallback;
use crate::llm::{LlmEngine, SessionSource, StreamCallbacks};
use crate::permission::PermissionGate;
use crate::router::voice_feedback::{
    release_when_played, FeedbackGroup, FeedbackPlayer, ToolFeedback,
};
use crate::router::voice_turns::{
    decide_wakeword_action, ActiveTurn, TurnRegistry, WakewordAction,
};
use crate::router::{
    resolve_ts_inbound, ts_caller_admission_uid, ts_caller_clid_uid, ts_caller_session_uid,
    TurnError, TurnInput, TurnPermit, TurnRequest, TurnSession, TurnSink,
};

use crate::tools::{ToolRegistry, TsCaller, UnifiedExecutionContext};
use tokio_util::sync::CancellationToken;
use voicev1::voice_service_client::VoiceServiceClient;

const AUDIO_MAX_IN_FLIGHT: usize = 8;
/// 单条 TTS 流的句段通道容量
const TTS_SENTENCE_CAPACITY: usize = 128;

struct ChatTurnAdmission {
    source: SessionSource,
    permit: TurnPermit,
    ticket: crate::llm::context::TurnTicket,
}

impl ChatTurnAdmission {
    async fn prepare<F, T>(self, llm: &LlmEngine, prepare: F) -> Result<(TurnSession, T)>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        let session = self
            .permit
            .acquire_session(llm, &self.source, self.ticket)
            .await;
        let prepared = prepare.await?;
        Ok((session, prepared))
    }
}

struct PreparedChat {
    text: String,
    reply_mode: i32,
    reply_target: u32,
    admission: ChatAdmission,
}

enum ChatAdmission {
    DirectReplay(crate::tools::voice_replay::DirectReplayCommand),
    Llm {
        admission: ChatTurnAdmission,
        /// 事件循环里就登记好的回合：任务被派发后、首次被轮询前，音频回合也要能找到并取消它
        active: Arc<ActiveTurn>,
    },
}

fn check_chat_caller_identity(chat: &voicev1::ChatEvent, online_uid: &str) -> Result<()> {
    if !chat.invoker_unique_id.is_empty() && chat.invoker_unique_id != online_uid {
        anyhow::bail!(
            "chat caller {} identity changed while queued",
            chat.invoker_client_id
        );
    }
    Ok(())
}

/// 由排队聊天的两次在线快照解析调用者：`online` 是列表快照，`clientinfo_uid` 与
/// `channel_group_id` 出自同一次 clientinfo 查询。
///
/// 两次快照都要过身份校验：列表查询与 clientinfo 查询之间 clid 可能被复用，
/// 只按列表校验就会把旧调用者的 server groups 与新占用者的频道组拼在一起。
/// 会话与历史键取 clientinfo 快照里的真实 UID（就是收到回复的那个 clid 的占用者），
/// 拿不到才退回准入键；`clid` 复用不能让两任占用者共享历史。
fn resolve_chat_caller(
    chat: &voicev1::ChatEvent,
    reply_target_mode: i32,
    reply_target_client_id: u32,
    online: &[tsclient_rs::ClientInfo],
    channel_group_id: u32,
    clientinfo_uid: &str,
) -> Result<CallerContext> {
    let caller = online
        .iter()
        .find(|client| u32::try_from(client.id).ok() == Some(chat.invoker_client_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "chat caller {} not found in online clients",
                chat.invoker_client_id
            )
        })?;
    check_chat_caller_identity(chat, &caller.uid)?;
    check_chat_caller_identity(chat, clientinfo_uid)?;
    debug!(
        "Resolved chat caller '{}' from online list: clid={}",
        chat.invoker_name, caller.id
    );
    let admission_uid = ts_caller_admission_uid(&chat.invoker_unique_id, chat.invoker_client_id);
    Ok(CallerContext {
        caller_id: chat.invoker_client_id,
        caller_uid: ts_caller_session_uid(clientinfo_uid, &admission_uid),
        caller_name: chat.invoker_name.clone(),
        groups: parse_server_groups(&caller.server_groups),
        channel_group_id,
        channel_id: caller.channel_id,
        reply_target_mode,
        reply_target_client_id,
    })
}

// 在事件循环内登记票据与回合：caller 与在线名单查询必须在取得会话锁后执行，
// 而回合必须在派发任务前就可见，否则音频事件找不到它、取消不掉排队中的文本回复。
fn prepare_chat(
    config: &AppConfig,
    llm: &LlmEngine,
    turns: &TurnRegistry,
    bot_clid: u32,
    chat: &voicev1::ChatEvent,
) -> Option<PreparedChat> {
    if chat.invoker_name == config.bot.nickname
        || config.is_music_bot_name(&chat.invoker_name)
        || (bot_clid != 0 && chat.invoker_client_id == bot_clid)
    {
        return None;
    }
    let decision = resolve_ts_inbound(
        &chat.message,
        chat.target_mode as u8,
        chat.invoker_client_id,
        &config.bot,
    )?;
    if !decision.should_trigger_llm {
        return None;
    }
    let text = preprocess_text_message(&decision.text)?;
    if let Err(error) = llm.check_user_text_bounds(&text) {
        warn!(error = %error, "voice chat dropped for exceeding size limit");
        return None;
    }
    let direct_replay = if config.voice_replay.enabled {
        crate::tools::voice_replay::parse_direct_command(
            &text,
            &config.voice_replay.direct_commands,
        )
    } else {
        None
    };
    let admission = if let Some(command) = direct_replay {
        ChatAdmission::DirectReplay(command)
    } else {
        let Ok(permit) = TurnPermit::reserve(llm) else {
            warn!("Voice LLM turn queue full; dropping message");
            return None;
        };
        let source = SessionSource::TeamSpeak {
            uid: ts_caller_admission_uid(&chat.invoker_unique_id, chat.invoker_client_id),
        };
        let ticket = llm.enqueue_turn_ticket(&source);
        // 文本触发的回复同样经 TTS 出声：与语音回合共用产出登记与取消令牌。
        // 文本不经唤醒门，不夺取话语权：正在播报的语音回合继续播完。忽略 `begin_turn`
        // 的返回值（不取消、不摘除那些旧条目），它们留在集合里，插话仍能取到并取消。
        // 登记必须在这里、派发任务之前：任务是独立任务，等它首次被轮询时音频事件可能已经
        // 处理过，那时它找不到这条回合，插话取消不掉排队中的文本回复
        let active = Arc::new(ActiveTurn::new());
        turns.begin_turn(chat.invoker_client_id, &active);
        ChatAdmission::Llm {
            admission: ChatTurnAdmission {
                source,
                permit,
                ticket,
            },
            active,
        }
    };
    Some(PreparedChat {
        text,
        reply_mode: i32::from(decision.reply_target_mode),
        reply_target: decision.reply_target,
        admission,
    })
}

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
    /// 正常收尾标记：`finish` 置位后剩余合成与播放交给 synth 任务自己跑完，
    /// Drop 不再中止它们
    detached: bool,
}

impl TtsTurnRuntime {
    fn callbacks(&self) -> Option<&StreamCallbacks> {
        Some(&self.callbacks)
    }

    /// 关闭当前流的句段通道并释放本轮状态；各流的 synth 任务继续跑完剩余合成与播放
    ///
    /// synth 的 `push_encoded` 受播放实时性背压，等待任务结束等于等待剩余音频播完；
    /// 任务在句段通道关闭后 finish 本流会话（不置 cancel），消费者播完已入队音频后自然收尾。
    fn finish(mut self) {
        self.detached = true;
        self.state.close_stream();
    }

    /// 取消：先置播放取消位，再 abort 全部 synth 任务；未收尾的会话随任务 future 一起 drop
    async fn abort(mut self) {
        self.detached = true;
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

impl Drop for TtsTurnRuntime {
    /// 兜底收尾：回合 future 在 `finish`/`abort` 之前被丢弃（JoinSet 收尾、任务被中止）时，
    /// 不留游离的 synth 任务与播放会话。
    ///
    /// 没有这一步，synth 任务会停在句段通道上继续持有会话与回合句柄，
    /// 出站 job 只能等段间空闲超时收敛，回合名额也随任务一起滞留。
    fn drop(&mut self) {
        if self.detached {
            return;
        }
        self.playback_cancel.store(true, Ordering::SeqCst);
        self.state.close_stream();
        let tasks: Vec<_> = self
            .state
            .tasks
            .lock()
            .expect("tts tasks poisoned")
            .drain(..)
            .collect();
        for task in tasks {
            task.abort();
        }
    }
}

struct VoiceBridgeReadyGuard {
    bridge_state: BridgeReadiness,
}

impl VoiceBridgeReadyGuard {
    fn new(bridge_state: BridgeReadiness) -> Self {
        bridge_state.set_up(BridgeComponent::Stream);
        Self { bridge_state }
    }
}

impl Drop for VoiceBridgeReadyGuard {
    fn drop(&mut self) {
        self.bridge_state.set_down(BridgeComponent::Stream);
    }
}

pub struct VoiceRouter {
    config: Arc<AppConfig>,
    gate: Arc<PermissionGate>,
    llm: Arc<LlmEngine>,
    registry: Arc<ToolRegistry>,
    ts_adapter: Arc<TsAdapter>,
    audio_pipeline: Mutex<Option<OpusSttPipeline>>,
    speech_provider: Option<Arc<OpenAiSpeechProvider>>,
    /// 完整语音回合在飞上限，覆盖 caller 查询与 STT，不只限制待处理队列
    audio_turn_slots: Arc<Semaphore>,
    bridge_state: BridgeReadiness,
    /// 统一出站：TTS/clip/外部流 FIFO；并发由 TurnCoordinator + 本层队列承担
    audio_output: AudioOutput,
    /// 直呼/工具共享的录制与出站句柄
    voice_audio: crate::tools::VoiceAudioHandles,
    /// 唤醒门：`[headless.wakeword]` 启用时 Some；per-clid 会话状态（含本条 utterance 的
    /// 命中与命令尾累计）只在事件循环任务里访问（收帧路径喂门、收尾路径取裁决）
    wakeword: Option<Mutex<WakewordGate>>,
    /// per-clid 活跃回合：准入判断「忙」与插话取消目标
    turns: TurnRegistry,
    /// 预生成短反馈音频：工具调用提示音与唤醒确认音共用；TTS provider 不可用时为 None
    feedback: Option<Arc<FeedbackPlayer>>,
}

/// VoiceRouter 装配句柄（避免构造参数列表过长）
pub struct VoiceRouterHandles {
    pub(crate) config: Arc<AppConfig>,
    pub(crate) gate: Arc<PermissionGate>,
    pub(crate) llm: Arc<LlmEngine>,
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) ts_adapter: Arc<TsAdapter>,
    pub(crate) bridge_state: BridgeReadiness,
    pub(crate) audio_output: AudioOutput,
    pub(crate) voice_audio: crate::tools::VoiceAudioHandles,
    /// 已加载的唤醒词模型；`[headless.wakeword]` 未启用时为 None
    pub(crate) wakeword: Option<Arc<WakewordModels>>,
}

/// 取消一条回合并按身份从注册表摘除：置 LLM 取消令牌与 TTS 播放取消位，再把这一条摘出集合。
/// 取消幂等，摘除只匹配 `Arc::ptr_eq` 的条目，同 clid 的其他回合不动。
/// 三处取消（插话、准入顶替、确认音顶替）共用它，避免有一处漏摘让取消句柄不可达
fn cancel_turn(turns: &TurnRegistry, turn: &Arc<ActiveTurn>) {
    turn.cancel.cancel();
    turn.playback_cancel().store(true, Ordering::SeqCst);
    turns.remove_turn(turn);
}

/// 占一个在飞名额：满额返回 None，调用点据此只丢新段。
///
/// 必须先于任何取消/顶替调用：先取消再占名额会出现「旧回合已取消、新段又被拒」，
/// 两条回合一起丢。
fn try_reserve_audio_slot(slots: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    slots.clone().try_acquire_owned().ok()
}

/// 派发已占名额的回合任务；名额随任务结束（正常返回或被取消）释放
fn spawn_audio_turn(
    tasks: &mut JoinSet<()>,
    permit: OwnedSemaphorePermit,
    turn: impl std::future::Future<Output = ()> + Send + 'static,
) {
    tasks.spawn(async move {
        let _permit = permit;
        turn.await;
    });
}

/// 可取消的等待：取消时丢弃内层 future，连同它持有的 HTTP 请求、回合占位或会话锁一起释放。
///
/// 只用在必须立刻放下的等待上（caller 查询、STT、等会话锁）；回合其余部分保持协作式取消，
/// 让工具调用与「送达成功才落库」各自保持原子，也保证 TTS 收尾（`TtsTurnRuntime::abort`）
/// 一定跑到，不会留下仍在发请求的 synth 任务。
async fn cancellable_turn_step<T>(
    cancel: &CancellationToken,
    step: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        value = step => Some(value),
    }
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

/// 语音回合准入：按到达顺序等前一条同会话回合结束，再取会话锁（票据由调用点在同处登记）。
///
/// 这里不登记产出：`mark_producing` 由调用点在可失败的 TTS 初始化之后打，
/// 排队等锁的回合因此不算「产出中」，跨说话人插话只取消真正在产出的回合。
async fn acquire_voice_turn_session(
    permit: TurnPermit,
    llm: &LlmEngine,
    source: &SessionSource,
    ticket: crate::llm::context::TurnTicket,
) -> TurnSession {
    permit.acquire_session(llm, source, ticket).await
}

impl VoiceRouter {
    const STREAM_TTS_MIN_CHARS: usize = 4;
    const STREAM_TTS_WEAK_PUNCT_MIN_CHARS: usize = 8;
    const STREAM_TTS_MAX_CHARS: usize = 28;

    pub fn new(handles: VoiceRouterHandles) -> Self {
        let VoiceRouterHandles {
            config,
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
            OpenAiSpeechProvider::new(config.clone(), crate::prompt::TTS_STYLE_PROMPT.to_string())
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
            gate,
            llm,
            registry,
            ts_adapter,
            speech_provider,
            audio_turn_slots: Arc::new(Semaphore::new(AUDIO_MAX_IN_FLIGHT)),
            bridge_state,
            audio_output,
            voice_audio,
            wakeword,
            turns: TurnRegistry::default(),
            feedback,
        }
    }

    fn is_tts_effectively_enabled(&self) -> bool {
        self.config.headless.tts.enabled && self.speech_provider.is_some()
    }

    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        self.bridge_state.set_down(BridgeComponent::Stream);
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
                            let Some(prepared) = prepare_chat(
                                &router.config,
                                &router.llm,
                                &router.turns,
                                router.ts_adapter.get_bot_clid(),
                                &chat,
                            ) else {
                                continue;
                            };
                            let router = router.clone();
                            let channel = channel.clone();
                            tasks.spawn(async move {
                                if let Err(error) = router.handle_chat_event(&channel, chat, prepared).await {
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
        let clients = self
            .ts_adapter
            .list_clients()
            .await
            .map_err(|error| anyhow::anyhow!("list chat caller failed: {error}"))?;
        let (channel_group_id, clientinfo_uid) = self
            .ts_adapter
            .get_client_channel_group_and_uid(chat.invoker_client_id)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "resolve chat caller {} channel group failed: {error}",
                    chat.invoker_client_id
                )
            })?;
        resolve_chat_caller(
            chat,
            reply_target_mode,
            reply_target_client_id,
            &clients,
            channel_group_id,
            &clientinfo_uid,
        )
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
            caller_uid: ts_caller_session_uid(
                &caller.uid,
                &ts_caller_clid_uid(audio.from_client_id),
            ),
            caller_name: caller.nickname.clone(),
            groups,
            channel_group_id,
            channel_id: caller.channel_id,
            reply_target_mode,
            reply_target_client_id,
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

    async fn handle_chat_event(
        &self,
        channel: &VoiceChannel,
        chat: voicev1::ChatEvent,
        prepared: PreparedChat,
    ) -> Result<()> {
        let resolve =
            self.resolve_caller_from_chat(&chat, prepared.reply_mode, prepared.reply_target);
        let (session, ctx, active) = match prepared.admission {
            ChatAdmission::Llm { admission, active } => {
                // 等 LLM 容量与会话锁期间被取消：丢弃等待 future，占位、票据与锁随之释放
                let Some(admitted) =
                    cancellable_turn_step(&active.cancel, admission.prepare(&self.llm, resolve))
                        .await
                else {
                    debug!(
                        caller_uid = %ts_caller_admission_uid(
                            &chat.invoker_unique_id,
                            chat.invoker_client_id
                        ),
                        "voice text turn cancelled while waiting for the LLM slot"
                    );
                    return Ok(());
                };
                let (session, ctx) = admitted?;
                (session, ctx, active)
            }
            ChatAdmission::DirectReplay(command) => {
                let ctx = resolve.await?;
                return self.handle_direct_replay(channel, &ctx, command).await;
            }
        };
        // 桥接文本不是 STT 路径：不挂工具调用提示音
        self.handle_user_input(channel, ctx, prepared.text, &active, false, session)
            .await
    }

    /// 直呼：与工具同一 ACL（voice_replay），不占 LLM 回合容量。
    async fn handle_direct_replay(
        &self,
        channel: &VoiceChannel,
        ctx: &CallerContext,
        command: crate::tools::voice_replay::DirectReplayCommand,
    ) -> Result<()> {
        if !crate::tools::voice_replay::direct_command_allowed(
            &self.gate,
            &ctx.groups,
            ctx.channel_group_id,
        ) {
            self.send_reply(channel, ctx, "voice_replay denied by ACL")
                .await?;
            return Ok(());
        }
        let Some(runtime) = self.voice_audio.get() else {
            self.send_reply(channel, ctx, "voice replay runtime not ready")
                .await?;
            return Ok(());
        };
        match crate::tools::voice_replay::execute_direct_command(command, &runtime) {
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
        Ok(())
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
    /// 命中与 VAD 记账落在门的 per-clid 会话上，收尾时由 `take_wake_verdict` 取走。
    async fn feed_wakeword_frame(&self, clid: u32, speaker: &str, frame: &SttFrameOutcome) {
        let Some(gate) = &self.wakeword else {
            return;
        };
        let event = {
            let mut gate = gate.lock().await;
            gate.feed(
                clid,
                WakeFrame {
                    pcm16: &frame.mono_16k,
                    voiced: frame.voiced,
                    utterance_started: frame.utterance_started,
                },
                Instant::now(),
            )
        };
        let Some(fire_sample) = event.fire_sample else {
            // 未命中：本帧只进门的累计状态，不触发插话与打点
            return;
        };
        info!(
            event = "voice.wakeword",
            clid,
            speaker,
            probability = event.verdict.probability,
            fire_sample,
            detection_block_index = event.detection_block_index().unwrap_or(0),
            input_samples = event.fed_samples,
            window_open_before_feed = event.window_open_before_feed,
            "wakeword detected; opening gate"
        );
        debug!(
            event = "voice.wakeword.calibration",
            clid,
            fire_sample,
            // 命中块序号：可与 fire_sample 互校，也可与 raw_scores 的 chunk_index 对齐
            detection_block_index = event.detection_block_index().unwrap_or(0),
            // utterance 起点与锚点后活跃毫秒：把命中位置换算成「utterance 内命令开头在哪」，
            // 用于实测 T_rep − T_end 这条判据滞后（旧打点只有命中时刻，无法对齐 VAD 时间轴）
            utterance_start_sample = ?event.utterance_start_sample,
            anchor_active_ms = event.anchor_active_ms,
            input_samples = event.fed_samples,
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
    /// 裁决（命中 / 窗口 / 是否只喊了唤醒词）全部由门的 per-clid 会话给出，这里不再喂音频
    async fn take_wake_verdict(&self, clid: u32) -> WakeVerdict {
        let Some(gate) = &self.wakeword else {
            return WakeVerdict::default();
        };
        let mut gate = gate.lock().await;
        gate.take_utterance_verdict(clid, Instant::now())
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
        // 名额先于任何取消与插话：满额时既不取消产出中的旧回合，也不顶替未产出的旧回合，
        // 只丢这一条新段。先取消再占名额会出现「旧回合已取消、新段又被拒」两条一起丢
        let Some(permit) = try_reserve_audio_slot(&self.audio_turn_slots) else {
            warn!(clid, "voice API capacity full; dropping audio chunk");
            return Ok(());
        };
        // 唤醒门先于 gRPC 解析：关门丢弃不产生解析/查询开销
        if barge_in_available {
            // 「忙」与插话取消目标取自同一次快照：出站音频只有一路，产出中的回合就是通道持有者。
            // 分两次查询（先判忙再取目标）会在两次加锁之间漂移，出现「判定为忙却取不到目标」
            // 或反过来该插话却判成空闲
            let producing = self.turns.producing_turns();
            let busy = !producing.is_empty();
            match decide_wakeword_action(&verdict, busy) {
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
            if verdict.wakeword_only {
                info!(
                    event = "voice.wakeword.confirm",
                    clid,
                    speaker = %chunk.speaker_name,
                    input_samples = chunk.pcm16_mono_16k.len(),
                    // 与命中时的 `voice.wakeword.calibration` 同一组锚点字段，
                    // 便于事后核对「判成独占的这段为什么没够到阈值」
                    anchor_sample = ?verdict.anchor_sample,
                    detection_block_index = ?verdict.detection_block_index(),
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
        // `cancel_displaced_turns`。先登记拿到旧条目，等名额到手（上面已占）再决定取消
        let displaced = self.turns.begin_turn(clid, &active);
        let router = self.clone();
        let channel = channel.clone();
        spawn_audio_turn(turn_tasks, permit, async move {
            if let Err(error) = router
                .run_admitted_utterance(channel, active, audio, chunk)
                .await
            {
                error!("Voice turn failed: {error}");
            }
        });
        cancel_displaced_turns(&self.turns, displaced, barge_in_available);
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
        let ctx = match cancellable_turn_step(
            &active.cancel,
            self.resolve_audio_chunk_caller(&audio, chunk.speaker_client_id, &chunk.speaker_name),
        )
        .await
        {
            Some(ctx) => ctx?,
            None => {
                debug!(
                    clid = chunk.speaker_client_id,
                    "voice turn cancelled during caller lookup"
                );
                return Ok(());
            }
        };
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
        // 取消即丢弃 STT 请求 future：不让在飞转写占着名额空转
        let Some(transcription) =
            cancellable_turn_step(&active.cancel, speech_provider.transcribe_wav(wav)).await
        else {
            debug!(
                clid = chunk.speaker_client_id,
                "voice turn cancelled during stt; dropping the request"
            );
            return Ok(());
        };
        let raw_text = match transcription {
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

        if let Err(error) = self.llm.check_user_text_bounds(&text) {
            warn!(error = %error, caller_uid = %ctx.caller_uid, "voice message dropped for exceeding size limit");
            return Ok(());
        }
        let source = SessionSource::TeamSpeak {
            uid: ctx.caller_uid.clone(),
        };
        let Ok(permit) = TurnPermit::reserve(&self.llm) else {
            warn!(caller_uid = %ctx.caller_uid, "Voice LLM turn queue full; dropping message");
            return Ok(());
        };
        let ticket = self.llm.enqueue_turn_ticket(&source);
        // 等 LLM 容量与会话锁期间被取消：丢弃等待 future，占位、票据与锁随之释放
        let Some(session) = cancellable_turn_step(
            &active.cancel,
            acquire_voice_turn_session(permit, &self.llm, &source, ticket),
        )
        .await
        else {
            debug!(
                caller_uid = %ctx.caller_uid,
                "voice turn cancelled while waiting for the LLM slot"
            );
            return Ok(());
        };
        // STT 语音回合：挂工具调用提示音
        self.handle_user_input(&channel, ctx, text, &active, true, session)
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

        // 并发门禁：TurnCoordinator 管 LLM 轮；AudioOutput FIFO 管出站，不再使用 tts_lock。
        // 与文本/STT 同一顺序：先占名额与到达顺序、再取会话锁，最后才查在线名单，
        // 名单查询的耗时不再让票据顺序与呼叫者解析互相漂移
        let source = SessionSource::TeamSpeak {
            uid: ctx.caller_uid.clone(),
        };
        let Ok(permit) = TurnPermit::reserve(&self.llm) else {
            warn!(
                caller_uid = %ctx.caller_uid,
                "Voice LLM turn queue full; dropping audio chunk"
            );
            return Ok(());
        };
        let ticket = self.llm.enqueue_turn_ticket(&source);
        // 等 LLM 容量与会话锁期间被取消：丢弃等待 future，占位、票据与锁随之释放
        let Some(session) = cancellable_turn_step(
            &active.cancel,
            acquire_voice_turn_session(permit, &self.llm, &source, ticket),
        )
        .await
        else {
            debug!(
                caller_uid = %ctx.caller_uid,
                "voice turn cancelled while waiting for the LLM slot"
            );
            return Ok(());
        };
        // 查在线名单同样是要放下的等待：取消时丢弃查询 future，会话锁与容量立刻归还
        let Some((user_ctx, allowed_tools, session_source)) =
            cancellable_turn_step(&active.cancel, self.build_llm_request(&ctx)).await
        else {
            debug!(
                caller_uid = %ctx.caller_uid,
                "voice turn cancelled while preparing the request"
            );
            return Ok(());
        };
        let tts_runtime = if self.is_tts_effectively_enabled() {
            // omni 与 STT 同属语音回合：挂工具调用提示音，走 STT 路径同一块反馈基建
            Some(self.build_tts_callbacks(active, true).await?)
        } else {
            None
        };
        // TTS 初始化成功后才登记产出：排队等锁与初始化失败都不算「产出中」
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
            user_ctx: &user_ctx,
            allowed_tools: &allowed_tools,
            input: TurnInput::Audio(&wav_bytes),
            callbacks: tts_runtime.as_ref().and_then(TtsTurnRuntime::callbacks),
            cancel: &active.cancel,
        };
        let result = request
            .run(
                session,
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

    /// 执行已取得会话锁的文本/STT 回合，持锁到回复投递和落库完成。
    /// `tool_feedback` 为真时挂工具调用提示音：STT 与 omni 两条语音回合都挂，
    /// 桥接文本不挂（回复本身就是文本反馈）
    async fn handle_user_input(
        &self,
        channel: &VoiceChannel,
        ctx: CallerContext,
        user_msg: String,
        active: &Arc<ActiveTurn>,
        tool_feedback: bool,
        session: TurnSession,
    ) -> Result<()> {
        info!(
            event = "voice.user_message",
            caller_uid = %ctx.caller_uid,
            message_chars = user_msg.chars().count(),
            "Voice user message received"
        );
        // 会话锁已到手，接着查在线名单：这同样是要放下的等待，
        // 取消时丢弃查询 future，让锁与 LLM 容量立刻归还给顶替它的回合
        let Some((user_ctx, allowed_tools, session_source)) =
            cancellable_turn_step(&active.cancel, self.build_llm_request(&ctx)).await
        else {
            debug!(
                caller_uid = %ctx.caller_uid,
                "voice turn cancelled while preparing the request"
            );
            return Ok(());
        };
        let tts_runtime = if self.is_tts_effectively_enabled() {
            Some(self.build_tts_callbacks(active, tool_feedback).await?)
        } else {
            None
        };
        // TTS 初始化成功后才登记产出：排队等锁与初始化失败都不算「产出中」
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
            user_ctx: &user_ctx,
            allowed_tools: &allowed_tools,
            input: TurnInput::Text(&user_msg),
            callbacks: tts_runtime.as_ref().and_then(TtsTurnRuntime::callbacks),
            cancel: &active.cancel,
        };
        let result = request
            .run(
                session,
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
                // 含 tool_calls：先关当前流（synth finish 会话）再执行工具，
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
            detached: false,
        })
    }

    async fn build_llm_base_context(&self, ctx: &CallerContext) -> (String, Vec<String>) {
        let (online_clients, _) = self.ts_adapter.list_clients_json(0).await;

        let user_ctx = crate::prompt::context::ts_context(
            &ctx.caller_name,
            ctx.caller_id,
            ctx.channel_id,
            &online_clients,
        );
        let allowed_tools = self
            .gate
            .get_allowed_tools(&ctx.groups, ctx.channel_group_id);
        (user_ctx, allowed_tools)
    }

    async fn build_llm_request(&self, ctx: &CallerContext) -> (String, Vec<String>, SessionSource) {
        let (user_ctx, allowed_tools) = self.build_llm_base_context(ctx).await;
        let source = SessionSource::TeamSpeak {
            uid: ctx.caller_uid.clone(),
        };
        (user_ctx, allowed_tools, source)
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
    use crate::llm::provider::{LlmProvider, LlmStreamEvent};
    use futures_util::stream::BoxStream;
    use serde_json::Value;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct ChatOrderProvider {
        messages: StdMutex<Vec<Vec<Value>>>,
    }

    #[async_trait]
    impl LlmProvider for Arc<ChatOrderProvider> {
        async fn chat_completion_stream(
            &self,
            messages: Vec<Value>,
            _tools: Vec<Value>,
        ) -> Result<BoxStream<'static, Result<LlmStreamEvent>>> {
            let input: Value =
                serde_json::from_str(messages.last().unwrap()["content"].as_str().unwrap())?;
            let reply = format!("r-{}", input["user_message"].as_str().unwrap());
            self.messages.lock().unwrap().push(messages);
            Ok(Box::pin(futures_util::stream::iter([
                Ok(LlmStreamEvent::Token(reply)),
                Ok(LlmStreamEvent::Done {
                    finish_reason: "stop".to_string(),
                    tool_calls: Vec::new(),
                }),
            ])))
        }
    }

    #[derive(Default)]
    struct ChatOrderSink(StdMutex<Vec<String>>);

    #[async_trait]
    impl TurnSink for ChatOrderSink {
        async fn send(&self, text: &str) -> Result<()> {
            self.0.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    fn chat_event(text: &str) -> voicev1::ChatEvent {
        voicev1::ChatEvent {
            target_mode: 1,
            invoker_unique_id: "chat-user".to_string(),
            invoker_name: "caller".to_string(),
            message: text.to_string(),
            invoker_client_id: 7,
        }
    }

    async fn finish_prepared_chat(
        llm: &LlmEngine,
        text: &str,
        session: TurnSession,
        sink: &ChatOrderSink,
    ) {
        let source = SessionSource::TeamSpeak {
            uid: "chat-user".to_string(),
        };
        let cancel = CancellationToken::new();
        let registry = ToolRegistry::default();
        let request = TurnRequest {
            llm,
            registry: &registry,
            source: &source,
            user_ctx: "prepared caller",
            allowed_tools: &[],
            input: TurnInput::Text(text),
            callbacks: None,
            cancel: &cancel,
        };
        assert!(request
            .run(session, || panic!("no tools expected"), sink)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn chat_admission_orders_slow_caller_preparation_and_reverse_task_polling() {
        let config = AppConfig::default();
        let provider = Arc::new(ChatOrderProvider::default());
        let llm = LlmEngine::with_provider(Box::new(provider.clone()), 8);
        let first = prepare_chat(
            &config,
            &llm,
            &TurnRegistry::default(),
            99,
            &chat_event("first"),
        )
        .unwrap();
        let second = prepare_chat(
            &config,
            &llm,
            &TurnRegistry::default(),
            99,
            &chat_event("second"),
        )
        .unwrap();
        let sink = ChatOrderSink::default();
        let second_prepared = AtomicBool::new(false);
        let (release_caller, caller_ready) = tokio::sync::oneshot::channel();
        let ChatAdmission::Llm {
            admission: first_admission,
            ..
        } = first.admission
        else {
            panic!("first chat must reserve an LLM turn");
        };
        let ChatAdmission::Llm {
            admission: second_admission,
            ..
        } = second.admission
        else {
            panic!("second chat must reserve an LLM turn");
        };
        let first_task = async {
            let (session, ()) = first_admission
                .prepare(&llm, async {
                    caller_ready.await.unwrap();
                    Ok(())
                })
                .await
                .unwrap();
            finish_prepared_chat(&llm, &first.text, session, &sink).await;
        };
        let second_task = async {
            let (session, ()) = second_admission
                .prepare(&llm, async {
                    second_prepared.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .unwrap();
            finish_prepared_chat(&llm, &second.text, session, &sink).await;
        };
        tokio::pin!(first_task, second_task);
        assert!(futures_util::poll!(&mut second_task).is_pending());
        assert!(!second_prepared.load(Ordering::SeqCst));
        assert!(futures_util::poll!(&mut first_task).is_pending());
        assert!(futures_util::poll!(&mut second_task).is_pending());
        assert!(provider.messages.lock().unwrap().is_empty());
        release_caller.send(()).unwrap();
        first_task.await;
        second_task.await;

        assert_eq!(*sink.0.lock().unwrap(), vec!["r-first", "r-second"]);
        let messages = provider.messages.lock().unwrap();
        assert_eq!(messages.len(), 2);
        let first_input: Value =
            serde_json::from_str(messages[0].last().unwrap()["content"].as_str().unwrap()).unwrap();
        let second_input: Value =
            serde_json::from_str(messages[1].last().unwrap()["content"].as_str().unwrap()).unwrap();
        assert_eq!(first_input["user_message"], "first");
        assert_eq!(second_input["user_message"], "second");
        assert_eq!(messages[1][1]["content"], "first");
        assert_eq!(messages[1][2]["content"], "r-first");
        let history = llm.stored_history(&SessionSource::TeamSpeak {
            uid: "chat-user".to_string(),
        });
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].user.as_text(), Some("first"));
        assert_eq!(history[1].user.as_text(), Some("second"));
    }

    #[test]
    fn chat_admission_ignores_invalid_input_before_capacity_reservation() {
        let config = AppConfig::default();
        let llm = LlmEngine::new(Arc::new(config.clone())).unwrap();
        for chat in [
            chat_event("   "),
            chat_event(&"x".repeat(crate::llm::engine::MAX_USER_TEXT_BYTES + 1)),
            voicev1::ChatEvent {
                invoker_name: config.bot.nickname.clone(),
                ..chat_event("hello")
            },
            voicev1::ChatEvent {
                invoker_client_id: 99,
                ..chat_event("hello")
            },
            voicev1::ChatEvent {
                target_mode: 2,
                ..chat_event("not triggered")
            },
        ] {
            assert!(prepare_chat(&config, &llm, &TurnRegistry::default(), 99, &chat).is_none());
        }
        let admissions: Vec<_> = (0..4)
            .map(|_| {
                prepare_chat(
                    &config,
                    &llm,
                    &TurnRegistry::default(),
                    99,
                    &chat_event("hello"),
                )
                .unwrap()
            })
            .collect();
        assert_eq!(admissions.len(), 4);
        assert!(prepare_chat(
            &config,
            &llm,
            &TurnRegistry::default(),
            99,
            &chat_event("hello")
        )
        .is_none());
        let replay = prepare_chat(
            &config,
            &llm,
            &TurnRegistry::default(),
            99,
            &chat_event("!replay"),
        )
        .unwrap();
        assert!(matches!(replay.admission, ChatAdmission::DirectReplay(_)));
    }

    /// 事件不带 UID 时准入键退到 clid（票据与会话锁必须在解析之前定下），
    /// 但会话与历史键用解析出的真实 UID，同一个人不会因事件样式不同分成两份历史
    #[test]
    fn chat_admission_falls_back_to_clid_while_the_session_uses_the_resolved_uid() {
        let config = AppConfig::default();
        let llm = LlmEngine::new(Arc::new(config.clone())).unwrap();
        let chat = voicev1::ChatEvent {
            invoker_unique_id: String::new(),
            ..chat_event("hello")
        };
        let prepared = prepare_chat(&config, &llm, &TurnRegistry::default(), 99, &chat).unwrap();
        let ChatAdmission::Llm { admission, .. } = prepared.admission else {
            panic!("ordinary chat must reserve an LLM turn");
        };
        assert_eq!(admission.source.to_string(), "sq:clid:7");

        let online = vec![online_chat_caller("replacement-user")];
        let ctx = resolve_chat_caller(&chat, 1, 7, &online, 42, "replacement-user").unwrap();
        assert_eq!(ctx.caller_uid, "replacement-user");
        assert_eq!(
            SessionSource::TeamSpeak {
                uid: ctx.caller_uid,
            }
            .to_string(),
            "sq:replacement-user"
        );
    }

    /// 文本回合在派发任务前登记：同 clid 的音频顶替不等任务被轮询就能取消排队中的回复
    #[test]
    fn queued_chat_turn_is_cancellable_before_its_task_is_polled() {
        let config = AppConfig::default();
        let llm = LlmEngine::new(Arc::new(config.clone())).unwrap();
        let turns = TurnRegistry::default();
        let prepared = prepare_chat(&config, &llm, &turns, 99, &chat_event("hello")).unwrap();
        let ChatAdmission::Llm { active, .. } = &prepared.admission else {
            panic!("ordinary chat must reserve an LLM turn");
        };

        // 任务还没被轮询，登记表里已经有这条回合
        assert!(turns.active(7).is_some());
        // 音频顶替：旧回合没进 mark_producing，插话路径可用也一律取消
        let audio = Arc::new(ActiveTurn::new());
        let displaced = turns.begin_turn(7, &audio);
        cancel_displaced_turns(&turns, displaced, true);

        assert!(active.cancel.is_cancelled());
        // 取消同时按身份摘除，表里只留下顶替者
        let remaining = turns.active(7).expect("successor turn stays registered");
        assert!(Arc::ptr_eq(&remaining, &audio));
    }

    #[test]
    fn queued_chat_rejects_client_id_reuse_by_a_different_user() {
        let mut chat = chat_event("hello");
        assert!(check_chat_caller_identity(&chat, "chat-user").is_ok());
        assert!(check_chat_caller_identity(&chat, "replacement-user").is_err());
        chat.invoker_unique_id.clear();
        assert!(check_chat_caller_identity(&chat, "replacement-user").is_ok());
    }

    fn online_chat_caller(uid: &str) -> tsclient_rs::ClientInfo {
        tsclient_rs::ClientInfo {
            nickname: "caller".to_string(),
            uid: uid.to_string(),
            server_groups: vec!["6".to_string(), "not-a-number".to_string()],
            channel_id: 3,
            r#type: 0,
            id: 7,
        }
    }

    /// 排队消息的调用者解析走两次快照：列表里的 UID 与事件不一致就拒收
    #[test]
    fn chat_resolution_rejects_a_replacement_caller_in_the_online_list() {
        let chat = chat_event("hello");
        let online = vec![online_chat_caller("replacement-user")];

        assert!(resolve_chat_caller(&chat, 1, 7, &online, 42, "replacement-user").is_err());
    }

    /// 列表与 clientinfo 之间 clid 被复用：两次快照都要校验，
    /// 只查列表就会把旧调用者的身份与新占用者的频道组拼在一起
    #[test]
    fn chat_resolution_rejects_a_client_id_reused_between_snapshots() {
        let chat = chat_event("hello");
        let online = vec![online_chat_caller("chat-user")];

        assert!(resolve_chat_caller(&chat, 1, 7, &online, 42, "replacement-user").is_err());
    }

    #[test]
    fn chat_resolution_builds_the_caller_from_matching_snapshots() {
        let chat = chat_event("hello");
        let online = vec![online_chat_caller("chat-user")];

        let ctx = resolve_chat_caller(&chat, 2, 9, &online, 42, "chat-user").unwrap();

        assert_eq!(ctx.caller_id, 7);
        assert_eq!(ctx.caller_uid, "chat-user");
        assert_eq!(ctx.caller_name, "caller");
        // 无法解析的 server group 项被忽略
        assert_eq!(ctx.groups, vec![6]);
        assert_eq!(ctx.channel_group_id, 42);
        assert_eq!(ctx.channel_id, 3);
        assert_eq!((ctx.reply_target_mode, ctx.reply_target_client_id), (2, 9));
    }

    /// 事件不带 UID 时无从比对（桥接可能省略），会话键用快照里的真实 UID——
    /// 回复发给的是这个 clid 当前的占用者，`clid` 复用不能把两任占用者写进同一份历史
    #[test]
    fn chat_resolution_uses_the_resolved_uid_without_an_event_uid() {
        let mut chat = chat_event("hello");
        chat.invoker_unique_id.clear();
        let online = vec![online_chat_caller("replacement-user")];

        let ctx = resolve_chat_caller(&chat, 1, 7, &online, 42, "replacement-user").unwrap();

        assert_eq!(ctx.caller_uid, "replacement-user");
    }

    /// 连快照都拿不到真实 UID 时才退回准入键，绝不落到空串（空串会让所有无 UID 调用者共用历史）
    #[test]
    fn chat_resolution_falls_back_to_the_admission_key_without_any_uid() {
        let mut chat = chat_event("hello");
        chat.invoker_unique_id.clear();
        let online = vec![online_chat_caller("")];

        let ctx = resolve_chat_caller(&chat, 1, 7, &online, 42, "").unwrap();

        assert_eq!(ctx.caller_uid, "clid:7");
    }

    /// 名单里没有这个 clid 时报错，不拿另一个人的身份兜底
    #[test]
    fn chat_resolution_rejects_a_missing_caller() {
        let chat = chat_event("hello");

        assert!(resolve_chat_caller(&chat, 1, 7, &[], 42, "chat-user").is_err());
    }

    struct DropMarker(Arc<AtomicBool>);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn audio_api_capacity_rejects_excess_work_before_polling() {
        let slots = Arc::new(Semaphore::new(AUDIO_MAX_IN_FLIGHT));
        let mut tasks = JoinSet::new();
        for _ in 0..AUDIO_MAX_IN_FLIGHT {
            let permit = try_reserve_audio_slot(&slots).expect("slot");
            spawn_audio_turn(&mut tasks, permit, std::future::pending());
        }
        assert!(try_reserve_audio_slot(&slots).is_none());
        // 被拒的新段连 future 都不会被 poll
        let polled = Arc::new(AtomicBool::new(false));
        let attempted = polled.clone();
        if let Some(permit) = try_reserve_audio_slot(&slots) {
            spawn_audio_turn(&mut tasks, permit, async move {
                attempted.store(true, Ordering::SeqCst)
            });
        }
        assert!(!polled.load(Ordering::SeqCst));
        tasks.abort_all();
        while let Some(result) = tasks.join_next().await {
            assert!(result.unwrap_err().is_cancelled());
        }
        assert_eq!(slots.available_permits(), AUDIO_MAX_IN_FLIGHT);
    }

    #[tokio::test]
    async fn cancelling_slow_audio_api_work_drops_request_and_releases_capacity() {
        let slots = Arc::new(Semaphore::new(1));
        let mut tasks = JoinSet::new();
        let cancel = CancellationToken::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let cleaned = Arc::new(AtomicBool::new(false));
        let request = DropMarker(dropped.clone());
        let (started, ready) = tokio::sync::oneshot::channel();
        let permit = try_reserve_audio_slot(&slots).expect("slot");
        let step_cancel = cancel.clone();
        let cleanup = cleaned.clone();
        spawn_audio_turn(&mut tasks, permit, async move {
            started.send(()).unwrap();
            let _ = cancellable_turn_step(&step_cancel, async move {
                let _request = request;
                std::future::pending::<()>().await;
            })
            .await;
            // 回合自己的收尾（TTS abort、放行后继票据之类）必须照常跑到：
            // 取消只丢等待中的那个 future，不能把整条回合任务砍掉
            cleanup.store(true, Ordering::SeqCst);
        });
        ready.await.unwrap();
        assert_eq!(slots.available_permits(), 0);
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), tasks.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            dropped.load(Ordering::SeqCst),
            "the in-flight request future must be dropped on cancel"
        );
        assert!(
            cleaned.load(Ordering::SeqCst),
            "cancellation must unwind the turn, not drop it"
        );
        assert_eq!(slots.available_permits(), 1);
    }

    /// 满额时新段连登记都不做，更不取消旧回合：否则旧回合被取消、新段又被拒，两条一起丢
    #[tokio::test]
    async fn full_capacity_drops_the_new_segment_without_touching_the_old_turn() {
        let slots = Arc::new(Semaphore::new(AUDIO_MAX_IN_FLIGHT));
        let held: Vec<_> = (0..AUDIO_MAX_IN_FLIGHT)
            .map(|_| try_reserve_audio_slot(&slots).expect("slot"))
            .collect();
        let turns = TurnRegistry::default();
        let old = Arc::new(ActiveTurn::new());
        old.mark_producing();
        turns.begin_turn(7, &old);

        // admit_audio_chunk 的第一步：占不到名额就直接丢段，displaced 仍是未处理状态
        assert!(try_reserve_audio_slot(&slots).is_none());
        assert!(
            !old.cancel.is_cancelled(),
            "the producing turn must keep the floor"
        );
        assert_eq!(turns.producing_turns().len(), 1);

        // 名额归还后同一 clid 的新段照常顶替旧回合
        drop(held);
        let permit = try_reserve_audio_slot(&slots).expect("slot after release");
        let new = Arc::new(ActiveTurn::new());
        assert_eq!(turns.begin_turn(7, &new).len(), 1);
        drop(permit);
    }

    /// 造一份能开流的 TTS 运行时：消费者随测试任务活着，会话才入得了队
    fn tts_runtime_for_test() -> (TtsTurnRuntime, mpsc::Sender<(Vec<u8>, i32)>) {
        let bus = crate::adapter::headless::audio_output::AudioBus::new();
        let (sink_tx, sink_rx) = mpsc::channel::<(Vec<u8>, i32)>(8);
        let keepalive_sink = sink_tx.clone();
        let consumer = bus.consumer;
        tokio::spawn(async move { consumer.run(sink_tx).await });
        let keepalive = Arc::new(ActiveTurn::new());
        let playback_cancel = keepalive.playback_cancel();
        let state = test_tts_state(bus.output, keepalive, playback_cancel.clone());
        let runtime = TtsTurnRuntime {
            callbacks: StreamCallbacks::default(),
            playback_cancel,
            state,
            detached: false,
        };
        drop(sink_rx);
        (runtime, keepalive_sink)
    }

    /// 兜底：回合 future 被丢弃（JoinSet 收尾、任务中止）时必须中止 synth 任务并停声，
    /// 否则任务停在句段通道上，会话与出站 job 只能等空闲超时收敛
    #[tokio::test]
    async fn dropping_the_tts_runtime_tears_down_its_synth_tasks() {
        let (runtime, _sink) = tts_runtime_for_test();
        let state = runtime.state.clone();
        state.ensure_stream().await;
        assert_eq!(
            state.tasks.lock().unwrap().len(),
            1,
            "setup: one synth task"
        );
        drop(runtime);

        tokio::time::timeout(Duration::from_secs(1), async {
            while Arc::strong_count(&state) > 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the synth task must be aborted on drop");
        assert!(state.playback_cancel.load(Ordering::SeqCst));
    }

    /// 正常收尾相反：`finish` 只关流，synth 任务自己收尾（不是被 abort），也不置播放取消位
    #[tokio::test]
    async fn finishing_the_tts_runtime_lets_the_synth_task_end_on_its_own() {
        let (runtime, _sink) = tts_runtime_for_test();
        let state = runtime.state.clone();
        state.ensure_stream().await;
        let task = state.tasks.lock().unwrap().pop().expect("synth task");

        runtime.finish();
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("finish must close the stream so the synth task can end");
        assert!(result.is_ok(), "finish must not abort the synth task");
        assert!(!state.playback_cancel.load(Ordering::SeqCst));
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

    /// 同会话排队的文本尚未产出：另一说话人插话只停当前回合，排队消息仍可执行。
    #[tokio::test]
    async fn cross_speaker_barge_in_preserves_text_waiting_for_its_session() {
        let engine = LlmEngine::new(Arc::new(AppConfig::default())).unwrap();
        let source = SessionSource::TeamSpeak {
            uid: "text-caller".to_string(),
        };
        let turns = TurnRegistry::default();
        let producer = Arc::new(ActiveTurn::new());
        turns.begin_turn(5, &producer);
        let first = acquire_voice_turn_session(
            TurnPermit::reserve(&engine).unwrap(),
            &engine,
            &source,
            engine.enqueue_turn_ticket(&source),
        )
        .await;
        // 调用点在 TTS 初始化成功后才登记产出；这里代表已经进入产出的前一条回合
        producer.mark_producing();

        let queued = Arc::new(ActiveTurn::new());
        turns.begin_turn(5, &queued);
        let pending = acquire_voice_turn_session(
            TurnPermit::reserve(&engine).unwrap(),
            &engine,
            &source,
            engine.enqueue_turn_ticket(&source),
        );
        tokio::pin!(pending);
        assert!(futures_util::poll!(&mut pending).is_pending());
        assert!(producer.is_producing());
        assert!(
            !queued.is_producing(),
            "session wait must not count as production"
        );

        apply_barge_in(&turns, 9, turns.producing_turns(), "other-caller");
        assert!(producer.cancel.is_cancelled());
        assert!(
            !queued.cancel.is_cancelled(),
            "queued text must survive another caller's barge-in"
        );
        assert!(!queued.playback_cancel().load(Ordering::SeqCst));
        assert!(turns.producing_turns().is_empty());
        let registered = turns.active(5).expect("queued text stays registered");
        assert!(Arc::ptr_eq(&registered, &queued));

        drop(first);
        let _session = pending.await;
        // 取到会话锁后由调用点在 TTS 初始化之后登记产出
        queued.mark_producing();
        assert!(queued.is_producing());
        assert!(!queued.cancel.is_cancelled());
        let producing = turns.producing_turns();
        assert_eq!(producing.len(), 1);
        assert!(Arc::ptr_eq(&producing[0].1, &queued));
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
            detached: false,
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
            detached: false,
        };
        runtime.abort().await;

        assert!(playback_cancel.load(Ordering::SeqCst));
        assert!(state.tx.lock().expect("tts tx poisoned").is_none());
        assert!(state.tasks.lock().expect("tts tasks poisoned").is_empty());
    }
}

//! 预生成短反馈音频：工具调用提示音与唤醒确认音共用同一块基建。
//!
//! 短语在启动期（首次触发兜底）经 TTS 合成一次，解码成 48k 立体声 PCM 缓存；
//! 触发时直接以 `PcmClipPayload` 入队 `AudioOutput`，不再发起实时 TTS 请求，
//! 因而首帧延迟只有进程内 opus 编码。片段与回复共用同一条出站 FIFO，
//! 取消位复用回合的播放取消位：插话或中断时片段与回复一起停声。
//!
//! 两条功能的差别只在触发点与是否持有真实回合：工具提示属于正在跑的回合，
//! 唤醒确认没有回合，需要就地登记一个「播报回合」让回声与插话走既有准入语义。
//! 短语按触发点分组（搜索类、其余工具、唤醒确认），同一组内轮换取用，
//! 连续触发时不会反复同一句。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::adapter::headless::audio_codec::{CHANNELS, SAMPLE_RATE_HZ};
use crate::adapter::headless::audio_output::{
    decode_to_pcm_48k_stereo, AudioOutput, ClipHandle, PcmClipPayload,
};
use crate::adapter::headless::speech::{detect_audio_format, OpenAiSpeechProvider};
use crate::adapter::headless::wakeword::CHUNK_SIZE as WAKE_BLOCK_SAMPLES;
use crate::router::voice_turns::ActiveTurn;

/// `web_search` 的提示音池：同组内轮换取用
const TOOL_FEEDBACK_SEARCH: [&str; 2] = ["我来看看", "我来搜索下"];
/// 其余工具的提示音池
const TOOL_FEEDBACK_DEFAULT: [&str; 1] = ["我来研究下"];
/// 唤醒确认音池：短促回应；完整句（如「我在听您说话」）会压住用户接话
const WAKE_CONFIRM: [&str; 2] = ["嗯哼", "在呢"];
/// 命中块锚点之后活跃语音短于该时长，即判定这条 utterance 只有唤醒词。
///
/// 累计量是锚点之后的 VAD 活跃毫秒，包含唤醒词自身落在命中块内的尾音，上限为一个命中块
/// `CHUNK_SIZE = 1280` 采样 @16k = 80ms。锚点比旧口径（只累计报告帧之后的活跃帧）早一个
/// 命中块，因此命令开头落在命中块内的短命令不再被漏判；代价是纯唤醒词自身最多计入 80ms，
/// 阈值 200ms 留给环境噪声与命令本体的余量同为 200 − 80 = 120ms。取更小的阈值会更不容易
/// 漏掉短命令，但纯唤醒词后同样的有声抖动就会被判成含命令、多走一次 STT/LLM。
/// 分类器自身的滞后（原始分数越阈位置可能落在唤醒词中段，`T_rep − T_end` 未被测量）
/// 不含在这个几何关系里，阈值只能按真实语料标定后定。
const WAKE_ONLY_TAIL_MS: u64 = 200;
/// 播报回合句柄的最长持有时间（见 `release_when_played`）
const FEEDBACK_HOLD_MAX: Duration = Duration::from_secs(10);

/// 短反馈的触发点：决定用哪一组短语，以及该组自己的轮换游标
///
/// 短语表为常量驱动：命中率依赖模型对工具名的选择，不做配置
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedbackGroup {
    /// `web_search` 工具调用
    ToolSearch,
    /// 其余工具调用
    ToolDefault,
    /// 只喊唤醒词时的确认音
    WakeConfirm,
}

impl FeedbackGroup {
    /// 游标数组长度：与枚举次序一一对应
    const COUNT: usize = 3;

    fn for_tool(tool: &str) -> Self {
        if tool == "web_search" {
            Self::ToolSearch
        } else {
            Self::ToolDefault
        }
    }

    fn phrases(self) -> &'static [&'static str] {
        match self {
            Self::ToolSearch => &TOOL_FEEDBACK_SEARCH,
            Self::ToolDefault => &TOOL_FEEDBACK_DEFAULT,
            Self::WakeConfirm => &WAKE_CONFIRM,
        }
    }
}

/// 启动期预热的全部短语：各池的并集（池之间不重复，无需再去重）
pub(crate) fn feedback_phrases() -> impl Iterator<Item = &'static str> {
    use FeedbackGroup::{ToolDefault, ToolSearch, WakeConfirm};
    [ToolSearch, ToolDefault, WakeConfirm]
        .into_iter()
        .flat_map(|group| group.phrases().iter().copied())
}

/// 在飞 utterance 的流式唤醒状态：收帧路径逐帧喂门时累计，收尾时取走做准入裁决。
///
/// 「只喊了唤醒词」不能只看命中那一刻——唤醒词与命令连说会命中同一段。这里把命中记成
/// **命中块的绝对起始采样位置**（`WakeEvent::fire_sample`，见 `wakeword.rs`），
/// 只累计锚点及其之后的 VAD 活跃帧毫秒数，阈值见 `WAKE_ONLY_TAIL_MS`。
/// 命中块的帧在报告之前就已喂进来（生产帧长 320 采样、块长 1280，报告帧是块的最后一帧），
/// 所以保留最近一个命中块跨度的帧记录，命中时按锚点回补：不回补的话「唤醒词 停」这类
/// 紧贴唤醒词的短命令，开头 60ms 落在报告之前，会被判成唤醒词独占而丢掉。
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct WakeUtterance {
    /// 本条 utterance 是否命中过唤醒词
    detected: bool,
    /// 命中块绝对起始采样位置（16k）；未命中为 None
    anchor_sample: Option<usize>,
    /// 锚点及其之后的 VAD 活跃毫秒
    active_ms_after: u64,
    /// 本条 utterance 首个 VAD 活跃帧的绝对采样位置（16k）：打点用，区分「命中在开头」与「命中在尾部」
    first_voiced_sample: Option<usize>,
    /// 最近一个命中块跨度的帧记录，供命中时回补
    recent_frames: VecDeque<RecentFrame>,
}

/// 回补用的单帧记录：位置与锚点同为 16k 采样数，避免单位换算引入边界误差
#[derive(Debug, Clone, Copy, PartialEq)]
struct RecentFrame {
    /// 帧起点绝对采样位置（16k）
    abs_sample: usize,
    /// 帧长毫秒：计数与阈值都以它为单位
    frame_ms: u64,
    /// 该帧被 VAD 判为活跃
    voiced: bool,
}

impl WakeUtterance {
    /// 命中：把锚点定到命中块的绝对起点、清零计数，再回补锚点之后已喂入的活跃帧
    pub(crate) fn note_detection(&mut self, anchor_sample: usize) {
        self.detected = true;
        self.anchor_sample = Some(anchor_sample);
        self.active_ms_after = 0;
        for frame in &self.recent_frames {
            if frame.abs_sample >= anchor_sample && frame.voiced {
                self.active_ms_after = self.active_ms_after.saturating_add(frame.frame_ms);
            }
        }
    }

    /// 逐帧喂入：记录本帧；已命中时只累计锚点及其之后的 VAD 活跃帧
    pub(crate) fn note_frame(&mut self, abs_sample: usize, voiced: bool, frame_ms: u64) {
        if voiced && self.first_voiced_sample.is_none() {
            self.first_voiced_sample = Some(abs_sample);
        }
        if let Some(anchor) = self.anchor_sample {
            if voiced && abs_sample >= anchor {
                self.active_ms_after = self.active_ms_after.saturating_add(frame_ms);
            }
        }
        self.recent_frames.push_back(RecentFrame {
            abs_sample,
            frame_ms,
            voiced,
        });
        // 只留最近一个命中块跨度：锚点最早也只可能落在这里（更早的帧必在命中块之前）。
        // 用采样位置而非帧数，帧长变化时回补窗口不随之漂移
        while self
            .recent_frames
            .front()
            .is_some_and(|front| abs_sample.saturating_sub(front.abs_sample) >= WAKE_BLOCK_SAMPLES)
        {
            self.recent_frames.pop_front();
        }
    }

    pub(crate) fn detected(&self) -> bool {
        self.detected
    }

    /// 命中块锚点（16k 绝对采样位置）：打点与命中块序号换算
    pub(crate) fn anchor_sample(&self) -> Option<usize> {
        self.anchor_sample
    }

    /// 锚点之后的 VAD 活跃毫秒：新口径实际计得的量，打点用
    pub(crate) fn active_ms_after(&self) -> u64 {
        self.active_ms_after
    }

    /// 本条 utterance 首个 VAD 活跃帧的绝对采样位置（16k）
    pub(crate) fn first_voiced_sample(&self) -> Option<usize> {
        self.first_voiced_sample
    }

    /// 锚点之后有成段活跃语音：本条 utterance 含命令。
    /// 边界按「≥ 阈值」判定；计入量含唤醒词自身在命中块内的尾音，语义与余量见 `WAKE_ONLY_TAIL_MS`
    pub(crate) fn has_command_tail(&self) -> bool {
        self.detected && self.active_ms_after >= WAKE_ONLY_TAIL_MS
    }
}

/// 预生成短反馈音频的缓存与播放
pub(crate) struct FeedbackPlayer {
    provider: Arc<OpenAiSpeechProvider>,
    audio_output: AudioOutput,
    /// 短语 → 48k 立体声 PCM；命中后不再合成
    cache: Mutex<HashMap<&'static str, Arc<Vec<i16>>>>,
    /// 串行化合成：启动期预热与首次触发可能并发请求同一短语
    synth_lock: tokio::sync::Mutex<()>,
    /// 每个触发点的轮换游标（下标与 `FeedbackGroup` 次序一一对应）
    cursors: [AtomicUsize; FeedbackGroup::COUNT],
}

impl FeedbackPlayer {
    pub(crate) fn new(provider: Arc<OpenAiSpeechProvider>, audio_output: AudioOutput) -> Self {
        Self {
            provider,
            audio_output,
            cache: Mutex::new(HashMap::new()),
            synth_lock: tokio::sync::Mutex::new(()),
            cursors: std::array::from_fn(|_| AtomicUsize::new(0)),
        }
    }

    /// 启动期预热全部短语；失败只记日志，首次触发时按需重试
    pub(crate) async fn prewarm(&self) {
        for phrase in feedback_phrases() {
            if let Err(error) = self.pcm_for(phrase).await {
                warn!(
                    phrase = %phrase,
                    error = %error,
                    "feedback phrase prewarm failed; will retry on first trigger"
                );
            }
        }
    }

    /// 取短语 PCM：缓存命中直接返回，未命中则合成 + 解码后入缓存
    async fn pcm_for(&self, phrase: &'static str) -> Result<Arc<Vec<i16>>> {
        if let Some(cached) = self.cached(phrase) {
            return Ok(cached);
        }
        let _serialize = self.synth_lock.lock().await;
        if let Some(cached) = self.cached(phrase) {
            return Ok(cached);
        }
        let encoded = self.provider.synthesize(phrase).await?;
        let codec = detect_audio_format(&encoded);
        let samples = decode_to_pcm_48k_stereo(&encoded, codec).await?;
        if samples.is_empty() {
            return Err(anyhow!("decoded feedback audio is empty for '{phrase}'"));
        }
        let pcm = Arc::new(samples);
        self.cache
            .lock()
            .expect("feedback cache poisoned")
            .insert(phrase, pcm.clone());
        info!(
            phrase = %phrase,
            samples = pcm.len(),
            "feedback phrase cached"
        );
        Ok(pcm)
    }

    fn cached(&self, phrase: &'static str) -> Option<Arc<Vec<i16>>> {
        self.cache
            .lock()
            .expect("feedback cache poisoned")
            .get(phrase)
            .cloned()
    }

    /// 取该触发点轮换到的短语并入队为 PCM 片段，返回实际选中的短语供日志关联。
    /// `cancel` 由调用方提供（回合或播报回合的播放取消位）
    pub(crate) async fn play(
        &self,
        group: FeedbackGroup,
        cancel: Arc<AtomicBool>,
    ) -> Result<(&'static str, ClipHandle)> {
        let phrase = self.next_phrase(group);
        let pcm = self.pcm_for(phrase).await?;
        let handle = self
            .audio_output
            .enqueue_pcm_clip_with_cancel(
                PcmClipPayload {
                    samples: pcm.as_ref().clone(),
                    sample_rate: SAMPLE_RATE_HZ,
                    channels: CHANNELS,
                },
                cancel,
            )
            .map_err(|error| anyhow!("enqueue feedback clip failed: {error}"))?;
        Ok((phrase, handle))
    }

    /// 按轮换取该触发点的一条短语：连续触发时换着说，不会反复同一句
    fn next_phrase(&self, group: FeedbackGroup) -> &'static str {
        let pool = group.phrases();
        // 池均为非空常量表；游标溢出用 wrapping 语义，取模后仍是合法下标
        let index = self.cursors[group as usize].fetch_add(1, Ordering::Relaxed) % pool.len();
        pool[index]
    }
}

/// 播完后释放回合句柄：句柄存活期间「机器人正在产出」为真，与正在播的回复一致。
///
/// 等待有上限：消费者任务若已停摆，`wait` 永不返回，而滞留的产出标记会让准入把所有
/// 后续语音判成「忙」丢掉——宁可丢掉一次回声保护，也不能让语音路径整体哑掉。
pub(crate) fn release_when_played(handle: ClipHandle, keepalive: Arc<ActiveTurn>) {
    // 该等待任务不登记进 router 的 JoinSet：句柄释放后它自行收敛（`FEEDBACK_HOLD_MAX` 兜底），
    // 不参与路由关闭时的统一 abort，属于有意为之
    tokio::spawn(async move {
        match tokio::time::timeout(FEEDBACK_HOLD_MAX, handle.wait()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => debug!(error = %error, "feedback clip ended without draining"),
            Err(_) => warn!(
                hold_secs = FEEDBACK_HOLD_MAX.as_secs(),
                "feedback clip did not drain in time; releasing the announcement turn"
            ),
        }
        drop(keepalive);
    });
}

/// 工具调用反馈：一轮对话里第一次工具调用开始执行时立刻入队提示音。
///
/// 模型常在一轮里连续调多个工具（单条回复里的并行调用，以及工具循环的多轮），
/// 每个都播会让几百毫秒的片段首尾相接、把正式回复越推越后，所以一轮只提示一次。
/// 提示音是排队播放的 PCM 片段，占一次出站 FIFO 位；它与本轮 TTS 的各条流共用
/// 回合播放取消位，插话时一起停声，所以起播前只需看一眼回合是否已被取消。
pub(crate) struct ToolFeedback {
    player: Arc<FeedbackPlayer>,
    /// 本回合的播放取消位：片段随回合被插话/中断一起停声
    playback_cancel: Arc<AtomicBool>,
    /// 本回合的取消令牌：已取消的回合不再入队提示音
    cancel: CancellationToken,
    /// 本轮是否已提示过：置位即静默，保证一轮对话只播一次
    announced: AtomicBool,
}

impl ToolFeedback {
    pub(crate) fn new(
        player: Arc<FeedbackPlayer>,
        playback_cancel: Arc<AtomicBool>,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            player,
            playback_cancel,
            cancel,
            announced: AtomicBool::new(false),
        })
    }

    /// 工具开始执行：本轮第一次调用时入队提示音，之后静默
    pub(crate) async fn on_start(&self, tool: &str) {
        if self.cancel.is_cancelled() {
            return;
        }
        // test-and-set：并发调用（同一条回复里的多个工具）也只有一个能播
        if self.announced.swap(true, Ordering::SeqCst) {
            debug!(tool = %tool, "tool call feedback already announced this turn");
            return;
        }
        let group = FeedbackGroup::for_tool(tool);
        match self.player.play(group, self.playback_cancel.clone()).await {
            Ok((phrase, _handle)) => {
                debug!(tool = %tool, phrase = %phrase, "played tool call feedback phrase")
            }
            Err(error) => warn!(tool = %tool, error = %error, "tool call feedback failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::headless::audio_output::AudioBus;

    fn test_player(output: AudioOutput) -> Arc<FeedbackPlayer> {
        let provider = Arc::new(
            OpenAiSpeechProvider::new(Arc::new(crate::config::AppConfig::default()), String::new())
                .expect("speech provider"),
        );
        let player = FeedbackPlayer::new(provider, output);
        {
            // 预置缓存：测试只关心「是否入队」，不触发合成与 ffmpeg 解码
            let mut cache = player.cache.lock().expect("feedback cache poisoned");
            for phrase in feedback_phrases() {
                cache.insert(phrase, Arc::new(vec![0i16; 960]));
            }
        }
        Arc::new(player)
    }

    /// 一轮对话只在第一次工具调用时立刻入队提示音，与工具耗时无关
    #[tokio::test(start_paused = true)]
    async fn tool_feedback_plays_once_on_the_first_tool_call() {
        let bus = AudioBus::new();
        let output = bus.output.clone();
        // 不跑消费者：只观察是否入队
        let _consumer = bus.consumer;
        let feedback = ToolFeedback::new(
            test_player(output.clone()),
            Arc::new(AtomicBool::new(false)),
            CancellationToken::new(),
        );

        feedback.on_start("web_search").await;
        assert_eq!(
            output.status().queued_jobs,
            1,
            "提示音在工具开始执行时即入队，不等工具返回"
        );

        // 同一轮里后续工具（串行或并行）不再出声
        feedback.on_start("play_music").await;
        feedback.on_start("web_search").await;
        assert_eq!(output.status().queued_jobs, 1);
    }

    /// 回合已被取消（插话）时不再入队提示音
    #[tokio::test(start_paused = true)]
    async fn tool_feedback_stays_silent_after_cancellation() {
        let bus = AudioBus::new();
        let output = bus.output.clone();
        let _consumer = bus.consumer;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let feedback = ToolFeedback::new(
            test_player(output.clone()),
            Arc::new(AtomicBool::new(false)),
            cancel,
        );

        feedback.on_start("web_search").await;
        assert_eq!(output.status().queued_jobs, 0);
    }

    /// 播报回合句柄活到片段播完：播完后条目自然失效，不再算「忙」
    #[tokio::test(start_paused = true)]
    async fn announcement_turn_is_released_when_the_clip_drains() {
        let bus = AudioBus::new();
        let output = bus.output.clone();
        let consumer = bus.consumer;
        let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel::<(Vec<u8>, i32)>(256);
        tokio::spawn(consumer.run(audio_tx));
        tokio::spawn(async move { while audio_rx.recv().await.is_some() {} });

        let player = test_player(output);
        let turn = Arc::new(ActiveTurn::new());
        let weak = Arc::downgrade(&turn);
        let (_phrase, handle) = player
            .play(FeedbackGroup::WakeConfirm, Arc::new(AtomicBool::new(false)))
            .await
            .expect("enqueue confirmation");
        release_when_played(handle, turn);

        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert!(
            weak.upgrade().is_none(),
            "announcement turn must be released"
        );
    }

    /// 消费者停摆时等待有上限：句柄必须被放掉，不能永久占住产出标记
    #[tokio::test(start_paused = true)]
    async fn announcement_turn_is_released_even_without_a_consumer() {
        let bus = AudioBus::new();
        let output = bus.output.clone();
        let _consumer = bus.consumer;
        let player = test_player(output);
        let turn = Arc::new(ActiveTurn::new());
        let weak = Arc::downgrade(&turn);
        let (_phrase, handle) = player
            .play(FeedbackGroup::WakeConfirm, Arc::new(AtomicBool::new(false)))
            .await
            .expect("enqueue confirmation");
        release_when_played(handle, turn);

        settle().await; // 先让释放任务注册超时，再推进时钟
        tokio::time::advance(FEEDBACK_HOLD_MAX + Duration::from_secs(1)).await;
        settle().await;
        assert!(weak.upgrade().is_none());
    }

    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// 锚点之后是 VAD 静音：只喊了唤醒词
    #[test]
    fn silent_tail_after_the_detection_point_is_wakeword_only() {
        let mut wake = WakeUtterance::default();
        wake.note_detection(640);
        wake.note_frame(640, false, 20);
        wake.note_frame(960, false, 20);
        assert!(wake.detected());
        assert_eq!(wake.active_ms_after(), 0);
        assert!(!wake.has_command_tail());
    }

    /// 命中块内、报告之前已喂入的活跃帧在命中时按锚点回补：
    /// 「唤醒词 停」的命令开头正落在报告之前，不回补就会被判成唤醒词独占而丢掉
    #[test]
    fn frames_inside_the_hit_block_are_backfilled_at_detection() {
        // 命中块 = 4 帧 × 320 采样 = 1280 采样（80ms）；报告帧是块的最后一帧，
        // 它的前 3 帧（绝对 0/320/640）在报告之前就已喂入
        let mut wake = WakeUtterance::default();
        for index in 0..3 {
            wake.note_frame(index * 320, true, 20);
        }
        wake.note_detection(0);
        wake.note_frame(960, true, 20);
        assert_eq!(wake.anchor_sample(), Some(0));
        assert_eq!(wake.first_voiced_sample(), Some(0));
        // 整块 80ms 都被计入，仍判「只有唤醒词」（纯唤醒词自身占不满阈值）
        assert_eq!(wake.active_ms_after(), 80);
        assert!(!wake.has_command_tail());

        // 「停」再贡献 6 帧 = 120ms → 恰好等于阈值，判为有命令
        for index in 4..10 {
            wake.note_frame(index * 320, true, 20);
        }
        assert_eq!(wake.active_ms_after(), WAKE_ONLY_TAIL_MS);
        assert!(wake.has_command_tail());
    }

    /// 阈值边界按「≥ 阈值」判定：纯唤醒词 + 一帧噪声（100ms）不误判为命令，
    /// 恰好等于阈值算有命令
    #[test]
    fn tail_active_ms_at_the_threshold_is_a_command() {
        let mut wake = WakeUtterance::default();
        for index in 0..3 {
            wake.note_frame(index * 320, true, 20);
        }
        wake.note_detection(0);
        wake.note_frame(960, true, 20);
        // 80ms 命中块 + 20ms 噪声
        wake.note_frame(1_280, true, 20);
        assert_eq!(wake.active_ms_after(), 100);
        assert!(!wake.has_command_tail());

        // 继续积累到恰好等于阈值
        for index in 5..10 {
            wake.note_frame(index * 320, true, 20);
        }
        assert_eq!(wake.active_ms_after(), WAKE_ONLY_TAIL_MS);
        assert!(wake.has_command_tail());
    }

    /// 锚点之前的活跃语音（唤醒词更早的部分）不计入
    #[test]
    fn voiced_frames_before_the_anchor_are_not_counted() {
        let mut wake = WakeUtterance::default();
        wake.note_frame(0, true, 20);
        wake.note_frame(320, true, 20);
        wake.note_detection(640);
        wake.note_frame(640, true, 20);
        assert_eq!(wake.active_ms_after(), 20);
        assert!(!wake.has_command_tail());
    }

    /// 未命中过唤醒词的 utterance 永远不算「有命令」
    #[test]
    fn utterance_without_detection_never_has_a_command_tail() {
        let mut never = WakeUtterance::default();
        never.note_frame(0, true, 5_000);
        assert!(!never.detected());
        assert_eq!(never.active_ms_after(), 0);
        assert_eq!(never.anchor_sample(), None);
        assert!(!never.has_command_tail());
    }

    /// 常量与注释里的几何依据一致：阈值 200ms 减去纯唤醒词自身最多计入的一个命中块 80ms，
    /// 余量 120ms；且阈值必须大于一个命中块，否则纯唤醒词自身的尾音就能凑满计数。
    /// 改常量必须同步注释与这条断言
    #[test]
    fn wake_only_tail_ms_matches_the_documented_geometry() {
        assert_eq!(WAKE_ONLY_TAIL_MS, 200);
        // 命中块 = 1280 采样 @16k = 80ms
        let block_ms = WAKE_BLOCK_SAMPLES * 1000 / 16_000;
        assert_eq!(block_ms, 80);
        // 阈值必须大于一个命中块，否则纯唤醒词自身就能凑满计数、确认音路径不可达
        assert!(WAKE_ONLY_TAIL_MS > block_ms as u64);
    }

    #[test]
    fn tool_phrases_follow_the_constant_table() {
        assert_eq!(
            FeedbackGroup::for_tool("web_search"),
            FeedbackGroup::ToolSearch
        );
        assert_eq!(
            FeedbackGroup::for_tool("play_music"),
            FeedbackGroup::ToolDefault
        );
        assert_eq!(FeedbackGroup::for_tool(""), FeedbackGroup::ToolDefault);
    }

    /// 同一触发点连续触发时轮换短语；各触发点的游标互不干扰
    #[test]
    fn feedback_phrases_rotate_within_a_group() {
        let bus = AudioBus::new();
        let player = test_player(bus.output);
        let _consumer = bus.consumer;

        assert_eq!(player.next_phrase(FeedbackGroup::ToolSearch), "我来看看");
        assert_eq!(player.next_phrase(FeedbackGroup::ToolSearch), "我来搜索下");
        assert_eq!(player.next_phrase(FeedbackGroup::ToolSearch), "我来看看");
        assert_eq!(player.next_phrase(FeedbackGroup::WakeConfirm), "嗯哼");
        assert_eq!(player.next_phrase(FeedbackGroup::WakeConfirm), "在呢");
        // 单条短语的池恒返回同一条
        assert_eq!(player.next_phrase(FeedbackGroup::ToolDefault), "我来研究下");
        assert_eq!(player.next_phrase(FeedbackGroup::ToolDefault), "我来研究下");
    }

    /// 触发点的枚举序号就是游标下标：新增变体时必须同步 `COUNT` 与游标数组
    #[test]
    fn group_indices_fit_the_cursor_array() {
        for (index, group) in [
            FeedbackGroup::ToolSearch,
            FeedbackGroup::ToolDefault,
            FeedbackGroup::WakeConfirm,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(group as usize, index);
        }
    }

    /// 预热清单覆盖每个池的每条短语，且池之间不重复（否则同一句被合成两遍）
    #[test]
    fn prewarm_list_covers_every_pool_phrase_once() {
        let phrases: Vec<&str> = feedback_phrases().collect();
        assert_eq!(
            phrases,
            vec!["我来看看", "我来搜索下", "我来研究下", "嗯哼", "在呢"]
        );
        let unique: std::collections::HashSet<&str> = phrases.iter().copied().collect();
        assert_eq!(unique.len(), phrases.len(), "池之间不得重复短语");
    }
}

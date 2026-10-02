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

use std::collections::HashMap;
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
use crate::router::voice_turns::ActiveTurn;

/// `web_search` 的提示音池：同组内轮换取用
const TOOL_FEEDBACK_SEARCH: [&str; 2] = ["我来看看", "我来搜索下"];
/// 其余工具的提示音池
const TOOL_FEEDBACK_DEFAULT: [&str; 1] = ["我来研究下"];
/// 唤醒确认音池：短促回应；完整句（如「我在听您说话」）会压住用户接话
const WAKE_CONFIRM: [&str; 2] = ["嗯哼", "在呢"];
/// 命中点之后活跃语音短于该时长，即判定这条 utterance 只有唤醒词
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
/// 「只喊了唤醒词」不能只看命中那一刻——唤醒词与命令连说会命中同一段，命令部分
/// 构成命中之后的活跃语音。这里按 VAD 结果累计命中之后的活跃语音毫秒，
/// 阈值见 `WAKE_ONLY_TAIL_MS`：尾音抖动、极短语气词都低于它，一次真正的指令高于它；
/// 代价是紧跟唤醒词、总长不足 200ms 的极短指令会被判成唤醒词独占。
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct WakeUtterance {
    /// 本条 utterance 是否命中过唤醒词
    detected: bool,
    /// 命中之后的活跃语音毫秒
    active_ms_after: u64,
}

impl WakeUtterance {
    /// 命中：判定以命中点为界，活跃语音计数清零
    pub(crate) fn note_detection(&mut self) {
        self.detected = true;
        self.active_ms_after = 0;
    }

    /// 未命中的帧：命中之后按 VAD 结果累计活跃语音
    pub(crate) fn note_frame(&mut self, voiced: bool, frame_ms: u64) {
        if self.detected && voiced {
            self.active_ms_after = self.active_ms_after.saturating_add(frame_ms);
        }
    }

    pub(crate) fn detected(&self) -> bool {
        self.detected
    }

    /// 命中点之后没有成段活跃语音：本条 utterance 只有唤醒词
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

    /// 命中点之后是 VAD 静音：只喊了唤醒词
    #[test]
    fn silent_tail_after_the_detection_point_is_wakeword_only() {
        let mut wake = WakeUtterance::default();
        wake.note_detection();
        wake.note_frame(false, 20);
        wake.note_frame(false, 20);
        assert!(wake.detected());
        assert!(!wake.has_command_tail());
    }

    /// 命中点之后仍有成段语音：唤醒词+命令连说，不播确认音
    #[test]
    fn voiced_tail_after_the_detection_point_is_a_command() {
        let mut wake = WakeUtterance::default();
        wake.note_detection();
        // 10 帧 × 20ms = 200ms 恰在阈值上：算有命令
        for _ in 0..10 {
            wake.note_frame(true, 20);
        }
        assert!(wake.has_command_tail());

        // 9 帧 = 180ms：仍在阈值内，按独占处理
        let mut short = WakeUtterance::default();
        short.note_detection();
        for _ in 0..9 {
            short.note_frame(true, 20);
        }
        assert!(!short.has_command_tail());
    }

    /// 阈值内的少量抖动仍按唤醒词独占处理；未命中的帧不计入
    #[test]
    fn short_burst_below_the_threshold_stays_wakeword_only() {
        let mut wake = WakeUtterance::default();
        // 命中之前的活跃语音不属于「命中之后」，不得计入
        wake.note_frame(true, 500);
        wake.note_detection();
        wake.note_frame(true, 20);
        assert!(wake.detected());
        assert!(!wake.has_command_tail());

        // 未命中过唤醒词的 utterance 永远不算「有命令」
        let mut never = WakeUtterance::default();
        never.note_frame(true, 5_000);
        assert!(!never.detected());
        assert!(!never.has_command_tail());
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

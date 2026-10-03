//! OpenWakeWord 语音唤醒门：推理核心 vendor 自 oww_rs（MIT，commit d945876）。
//! 三个 onnx（melspectrogram/embedding 前端 + 分类器）均从 `models_dir()` 运行时加载。

mod calibration;
mod model;

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tracing::info;

use crate::config::headless::HeadlessWakewordConfig;
use crate::config::models_dir;

pub use calibration::{calibration_stats, CalibrationStats, ScorePoint};
pub use model::WakewordModels;
// 块长是门内几何：命中块序号由 `WakeEvent::detection_block_index` 与
// `WakeVerdict::detection_block_index` 给出，不对外暴露
use model::{Detection, WakewordModel, CHUNK_SIZE, DETECTION_THRESHOLD};

/// 门时间轴的采样率：帧长到毫秒、锚点坐标与回补窗口都基于它（16k 单声道）
const SAMPLE_RATE_16K_HZ: u64 = 16_000;

/// 唤醒门推理实现：生产为 OWW 推理，测试注入桩
trait Detector: Send {
    fn detect(&mut self, chunk: Vec<f32>) -> Detection;

    /// 打点用原始分数序列；测试桩可用默认空实现
    fn score_history(&self) -> Vec<ScorePoint> {
        Vec::new()
    }
}

impl Detector for WakewordModel {
    fn detect(&mut self, chunk: Vec<f32>) -> Detection {
        self.detection(chunk)
    }

    fn score_history(&self) -> Vec<ScorePoint> {
        self.raw_score_history()
    }
}

type DetectorFactory = Box<dyn Fn(u32) -> Box<dyn Detector> + Send + Sync>;

/// 唤醒词模型文件名（`models/` 目录内固定存在，按 models/README.md 自行下载）
const MEL_MODEL_FILE: &str = "melspectrogram.onnx";
const EMBEDDING_MODEL_FILE: &str = "embedding_model.onnx";

/// 说话人会话池上限：超出按最久未见驱逐（对齐 voice_replay 的 LRU 上限）
const MAX_SPEAKERS: usize = 6;
/// 空闲驱逐下限：无音频的会话移除，重连后自然重建
const MIN_IDLE_EVICT_AFTER: Duration = Duration::from_secs(300);
/// 驱逐余量：空闲驱逐不得早于开门窗口结束后 60s，避免 `window_secs` 取上限时窗口尾被截断
const IDLE_EVICT_MARGIN: Duration = Duration::from_secs(60);

/// 唤醒门裁决：`open` 放行本次语音；`detected` 为本段新命中（INFO 日志）；`probability` 仅命中时有意义
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateVerdict {
    pub open: bool,
    pub detected: bool,
    pub probability: f32,
}

/// 唤醒门单次裁决 + 打点数据。
///
/// `fire_sample` 是命中块在该说话人喂入门的时间轴上的**绝对起始采样位置**（16k 单声道）。
/// 检测滞后不是「约 1s 平滑均值」：`model::calculate_average` 只对已越阈的块求平均，
/// 且需 ≥`MIN_POSITIVE_DETECTIONS` 个越阈块才返回非零，12 块缓冲只是约 0.96s 的回看环
/// 且命中即清空，`NO_DETECTION_MS` 是不应期——三者都不产生秒级平滑滞后。
/// 真正无法从门内几何推出的是原始分数相对唤醒词声学末端（`T_end`）的越阈位置与报告时刻
/// （`T_rep`）之差，它由分类器与短语决定，可能落在词中段；因此 `WAKE_ONLY_TAIL_MS`
/// 所依赖的滞后量必须实测，本字段只消掉**可论证的几何部分**：报告帧永远晚于命中块起点，
/// 生产 320 采样帧下固定偏晚 `CHUNK_SIZE − 帧长 = 960` 采样（60ms）。
#[derive(Debug, Clone, PartialEq)]
pub struct WakeEvent {
    pub verdict: GateVerdict,
    /// 命中块的绝对起始采样位置（该说话人累计喂入采样数）；`detected` 为真时恒为 Some
    pub fire_sample: Option<usize>,
    /// 最近 3.2s 分类器原始分数序列（`model::SCORE_HISTORY_SIZE` 块 × 80ms；绝对块序号 + 原始分数）
    pub calibration: CalibrationStats,
    /// 本段输入是否在既有开门窗口内（区分「窗口内续说」与「关门外新命中」）
    pub window_open_before_feed: bool,
    /// 喂入本帧后该说话人累计喂入的采样数：命中日志据此与门时间轴对齐
    pub fed_samples: usize,
    /// 命中块锚点之后累计的 VAD 活跃毫秒；未命中恒为 0
    pub anchor_active_ms: u64,
    /// 本条 utterance 首个 VAD 活跃帧的绝对采样位置（16k）：命中日志据此给出命令开头的位置
    pub utterance_start_sample: Option<usize>,
}

impl WakeEvent {
    /// 命中块序号：与门内推理的块边界同源，下游不需要知道块长
    pub fn detection_block_index(&self) -> Option<usize> {
        self.fire_sample.map(|sample| sample / CHUNK_SIZE)
    }
}

/// 锚点之后活跃语音短于 `WAKE_ONLY_TAIL_MS` 即判本条只有唤醒词。
///
/// 累计量是锚点之后的 VAD 活跃毫秒，包含唤醒词自身落在命中块内的尾音，上限为一个命中块
/// `CHUNK_SIZE = 1280` 采样 @16k = 80ms。锚点取命中块起点而非报告帧起点，所以命令开头
/// 落在命中块内的短命令不会被漏判；代价是纯唤醒词自身最多计入 80ms，阈值 200ms 留给
/// 环境噪声与命令本体的余量同为 200 − 80 = 120ms。取更小的阈值更不容易漏掉短命令，
/// 但纯唤醒词后同样的有声抖动就会被判成含命令、多走一次 STT/LLM。
/// 分类器自身的滞后（原始分数越阈位置可能落在唤醒词中段）不含在这个几何关系里，
/// 阈值只能按真实语料标定后定。
const WAKE_ONLY_TAIL_MS: u64 = 200;

/// 一条 utterance 收尾时的唤醒裁决：放行与否、是否命中唤醒词、是否只喊了唤醒词。
///
/// 三项都由门在 per-clid 会话里维护：命中与锚点来自收帧路径逐帧喂入的累计，窗口状态来自
/// 最近一次命中。路由层只按字段调度，不再自己拼装判定。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WakeVerdict {
    /// 门是否放行（本条命中，或仍在既有开门窗口内）
    pub open: bool,
    /// 本条 utterance 是否命中唤醒词
    pub detected: bool,
    /// 命中且锚点之后没有成段活跃语音：本条只有唤醒词；`detected` 为假时恒为假
    pub wakeword_only: bool,
    /// 命中块绝对起始采样位置（16k）；未命中为 None
    pub anchor_sample: Option<usize>,
    /// 锚点之后的 VAD 活跃毫秒：判据实际用到的量，确认音日志据此回看阈值取舍
    pub anchor_active_ms: u64,
    /// 本条 utterance 首个 VAD 活跃帧的绝对采样位置（16k）：给出命令开头相对 utterance 起点的位置
    pub utterance_start_sample: Option<usize>,
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

impl WakeVerdict {
    /// 该 clid 尚无会话：门关、未命中（还没被喂过音频）
    fn absent() -> Self {
        Self {
            open: false,
            ..Self::default()
        }
    }

    /// 命中块序号：确认音日志与命中时的 `voice.wakeword.calibration` 互校
    pub fn detection_block_index(&self) -> Option<usize> {
        self.anchor_sample.map(|sample| sample / CHUNK_SIZE)
    }
}

/// 收帧路径交给门的一帧：PCM 与它在本条 utterance 里的 VAD 状态。
///
/// 帧起点到绝对采样位置、帧长到毫秒的换算都在门内完成（只有门知道块边界与跨帧余量），
/// 路由层因此不重算时间轴。
pub struct WakeFrame<'a> {
    /// 本帧解码后的 16k 单声道 PCM
    pub pcm16: &'a [i16],
    /// 本帧被 VAD 判为活跃语音：命中之后据此累计命令尾
    pub voiced: bool,
    /// 本帧开启了新的 utterance 缓冲：命中累计与锚点随之重置，开门窗口跨 utterance 保留
    pub utterance_started: bool,
}

impl<'a> WakeFrame<'a> {
    /// 门自身单测用：只关心推理与窗口，不携带 VAD 记账
    #[cfg(test)]
    fn pcm(pcm16: &'a [i16]) -> Self {
        Self {
            pcm16,
            voiced: false,
            utterance_started: false,
        }
    }
}

/// 加载三个模型文件：启动期执行，失败按 anyhow 上抛终止启动
pub fn load_models(cfg: &HeadlessWakewordConfig) -> Result<Arc<WakewordModels>> {
    load_models_from(&models_dir(), cfg)
}

/// 从指定目录加载；目录作为注入缝，测试可用空目录断言缺文件错误
fn load_models_from(dir: &Path, cfg: &HeadlessWakewordConfig) -> Result<Arc<WakewordModels>> {
    let mel = read_model_file(&dir.join(MEL_MODEL_FILE))?;
    let emb = read_model_file(&dir.join(EMBEDDING_MODEL_FILE))?;
    let classifier = read_model_file(&dir.join(&cfg.model))?;
    // release 构建 panic = "abort"（Cargo.toml），catch_unwind 只在 debug 生效；
    // release 下非法模型靠上面的 metadata 预检兜底
    let models = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        WakewordModels::from_bytes(&mel, &emb, &classifier)
    }))
    .map_err(|payload| {
        anyhow!(
            "wakeword model construction panicked: {}",
            panic_message(payload)
        )
    })??;
    info!(
        dir = %dir.display(),
        model = %cfg.model,
        threshold = DETECTION_THRESHOLD,
        "wakeword models loaded"
    );
    Ok(models)
}

fn read_model_file(path: &Path) -> Result<Vec<u8>> {
    let meta = std::fs::metadata(path).with_context(|| {
        format!(
            "wakeword model not found: {} (see models/README.md to download it)",
            path.display()
        )
    })?;
    if meta.len() == 0 {
        bail!("wakeword model file is empty: {}", path.display());
    }
    std::fs::read(path).with_context(|| format!("read wakeword model: {}", path.display()))
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// 唤醒门：per-clid 实例池。命中即开门并刷新 `window` 窗口，窗口内持续喂模型允许续命中；
/// `feed`（语音到达）创建缺失的会话并刷新窗口，`take_utterance_verdict`（收尾）取走本条
/// utterance 的裁决，`refresh_window`（确认音）只刷新已存在会话的窗口、不做推理也不新建；
/// gRPC 解析在开门后才发生
pub struct WakewordGate {
    window: Duration,
    /// 空闲驱逐阈值：`MIN_IDLE_EVICT_AFTER` 与 `window + IDLE_EVICT_MARGIN` 取大
    idle_evict_after: Duration,
    speakers: HashMap<u32, SpeakerSession>,
    make_detector: DetectorFactory,
}

/// per-clid 会话：一条推理链与它上面累计的开门窗口、本条 utterance 的命中状态
struct SpeakerSession {
    detector: Box<dyn Detector>,
    /// 不足一块 1280 采样的余量，跨 utterance 携带
    acc: Vec<f32>,
    last_wake: Option<Instant>,
    last_seen: Instant,
    /// 累计喂入的采样数（帧起点与命中位置都在这个时间轴上）
    fed_samples: usize,
    /// 本条 utterance 的命中与命令尾累计
    utterance: UtteranceState,
}

impl SpeakerSession {
    /// 最近一次命中还在 `window` 内
    fn window_open(&self, now: Instant, window: Duration) -> bool {
        self.last_wake
            .is_some_and(|wake| now.duration_since(wake) < window)
    }
}

/// 一条 utterance 在门里的累计状态：命中锚点与锚点之后的 VAD 活跃语音。
///
/// 「只喊了唤醒词」不能只看命中那一刻——唤醒词与命令连说会命中同一段。命中记成命中块的
/// 绝对起始采样位置（`WakeEvent::fire_sample`），只累计锚点及其之后的 VAD 活跃帧毫秒数，
/// 阈值见 `WAKE_ONLY_TAIL_MS`。命中块的帧在报告之前就已喂进来（生产帧长 320 采样、块长
/// 1280，报告帧是块的最后一帧），所以保留最近一个命中块跨度的帧记录，命中时按锚点回补：
/// 不回补的话「唤醒词 停」这类紧贴唤醒词的短命令，开头 60ms 落在报告之前，会被判成
/// 唤醒词独占而丢掉。
#[derive(Default)]
struct UtteranceState {
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
struct RecentFrame {
    /// 帧起点绝对采样位置（16k）
    abs_sample: usize,
    /// 帧长毫秒：计数与阈值都以它为单位
    frame_ms: u64,
    /// 该帧被 VAD 判为活跃
    voiced: bool,
}

impl UtteranceState {
    /// 命中：把锚点定到命中块的绝对起点、清零计数，再回补锚点之后已喂入的活跃帧
    fn note_detection(&mut self, anchor_sample: usize) {
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
    fn note_frame(&mut self, abs_sample: usize, voiced: bool, frame_ms: u64) {
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
            .is_some_and(|front| abs_sample.saturating_sub(front.abs_sample) >= CHUNK_SIZE)
        {
            self.recent_frames.pop_front();
        }
    }

    /// 锚点之后有成段活跃语音：本条 utterance 含命令。
    /// 边界按「≥ 阈值」判定；计入量含唤醒词自身在命中块内的尾音，语义与余量见 `WAKE_ONLY_TAIL_MS`
    fn has_command_tail(&self) -> bool {
        self.detected && self.active_ms_after >= WAKE_ONLY_TAIL_MS
    }
}

impl WakewordGate {
    pub fn new(models: Arc<WakewordModels>, window_secs: u32) -> Self {
        Self::with_factory(window_secs, move |_| {
            Box::new(WakewordModel::new(models.clone()))
        })
    }

    fn with_factory(
        window_secs: u32,
        make_detector: impl Fn(u32) -> Box<dyn Detector> + Send + Sync + 'static,
    ) -> Self {
        let window = Duration::from_secs(u64::from(window_secs));
        Self {
            window,
            idle_evict_after: MIN_IDLE_EVICT_AFTER.max(window + IDLE_EVICT_MARGIN),
            speakers: HashMap::new(),
            make_detector: Box::new(make_detector),
        }
    }

    /// 喂入一帧 16kHz 单声道 PCM，返回本次裁决与打点数据。
    ///
    /// 生产路径按音频帧逐次喂入：门内部按 80ms 块推理，跨帧余量由 `acc` 携带，
    /// 所以通话期间的连续推理与一次性喂整段 utterance 等价，且命中能立刻返回
    /// （插话不必等 utterance 收尾）。帧起点到采样位置、帧长到毫秒的换算与命中累计
    /// 都在这里完成，结果落在该 clid 的会话上，供 `take_utterance_verdict` 取用。
    pub fn feed(&mut self, clid: u32, frame: WakeFrame<'_>, now: Instant) -> WakeEvent {
        let window = self.window;
        let frame_ms = frame.pcm16.len() as u64 * 1000 / SAMPLE_RATE_16K_HZ;
        self.evict_idle(now);
        let session = self.session_for(clid, now);
        if frame.utterance_started {
            // 新的 utterance：清掉上一段的命中累计，避免独占判定串到新一段；开门窗口不受影响
            session.utterance = UtteranceState::default();
        }
        let window_open_before_feed = session.window_open(now, window);
        // 本次输入开始前累积的余量：acc 前 carry 个采样来自上一次 feed，在本帧之前就已喂入，
        // 所以块的绝对起点要从 fed_samples_before 往回退 carry 个采样。
        // 必须在 extend 之前取，否则 carry 含进本次输入，回退量算错
        let carry = session.acc.len();
        let fed_samples_before = session.fed_samples;
        session.acc.extend(
            frame
                .pcm16
                .iter()
                .map(|sample| f32::from(*sample) / 32_768.0),
        );

        let mut consumed = 0usize;
        let mut detected = false;
        let mut probability = 0.0f32;
        let mut fire_sample = None;
        while session.acc.len() >= CHUNK_SIZE {
            // 命中块的绝对起点 = acc[0] 的绝对位置（上次喂入后剩下的 carry 个采样起于
            // fed_samples_before − carry；acc 只装已喂采样，故该差值恒不为负）+ 本次已排空的采样数。
            // 旧实现只报「本帧内偏移」，命中块起点落在上一帧余量里时被钳成 0；
            // 下游再加报告帧起点，绝对位置就系统性偏晚 carry（生产 320 采样帧下 960 采样 = 60ms）
            let chunk_abs_start = fed_samples_before - carry + consumed;
            let chunk: Vec<f32> = session.acc.drain(..CHUNK_SIZE).collect();
            consumed += CHUNK_SIZE;
            let detection = session.detector.detect(chunk);
            if detection.detected {
                detected = true;
                probability = detection.probability;
                // 取本次输入里**最早**命中块的起点：后续块可能同样越阈，
                // 若被覆盖，锚点会与真正触发的那一块错位
                if fire_sample.is_none() {
                    fire_sample = Some(chunk_abs_start);
                }
            }
        }
        session.fed_samples += frame.pcm16.len();

        if detected {
            session.last_wake = Some(now);
        }
        // 命中与帧记账都落在门内：命中块的绝对起点只有这里知道（跨帧余量在 acc 里），
        // 命中时回补所需的帧记录也只有这里能看见
        if let Some(fire_sample) = fire_sample {
            session.utterance.note_detection(fire_sample);
        }
        session
            .utterance
            .note_frame(fed_samples_before, frame.voiced, frame_ms);
        let open = detected || session.window_open(now, window);
        let calibration = calibration_stats(&session.detector.score_history());
        WakeEvent {
            verdict: GateVerdict {
                open,
                detected,
                probability,
            },
            fire_sample,
            calibration,
            window_open_before_feed,
            fed_samples: session.fed_samples,
            anchor_active_ms: session.utterance.active_ms_after,
            utterance_start_sample: session.utterance.first_voiced_sample,
        }
    }

    /// 收段路径：取走这条 utterance 的裁决。音频已经在收帧路径逐帧喂过门，
    /// 这里只读会话上累计的命中与窗口状态，不再喂音频（重复喂会打乱推理窗口），
    /// 命中累计同时清零，交给下一条 utterance
    pub fn take_utterance_verdict(&mut self, clid: u32, now: Instant) -> WakeVerdict {
        let window = self.window;
        self.evict_idle(now);
        let Some(session) = self.speakers.get_mut(&clid) else {
            return WakeVerdict::absent();
        };
        let window_open = session.window_open(now, window);
        let utterance = std::mem::take(&mut session.utterance);
        let detected = utterance.detected;
        WakeVerdict {
            open: detected || window_open,
            detected,
            wakeword_only: detected && !utterance.has_command_tail(),
            anchor_sample: utterance.anchor_sample,
            anchor_active_ms: utterance.active_ms_after,
            utterance_start_sample: utterance.first_voiced_sample,
        }
    }

    /// 从确认时刻续上开门窗口：确认音代表「我在听」，用户接话不必抢在原窗口到期前。
    /// 只刷新已存在的会话——能播确认音意味着该 clid 刚被喂过，不在此新建状态
    pub fn refresh_window(&mut self, clid: u32, now: Instant) {
        if let Some(session) = self.speakers.get_mut(&clid) {
            session.last_seen = now;
            session.last_wake = Some(now);
        }
    }

    fn evict_idle(&mut self, now: Instant) {
        let idle_evict_after = self.idle_evict_after;
        self.speakers
            .retain(|_, session| now.duration_since(session.last_seen) < idle_evict_after);
    }

    /// 取得或创建 clid 会话；池满时驱逐最久未见者
    fn session_for(&mut self, clid: u32, now: Instant) -> &mut SpeakerSession {
        if !self.speakers.contains_key(&clid) {
            while self.speakers.len() >= MAX_SPEAKERS {
                let oldest = self
                    .speakers
                    .iter()
                    .min_by_key(|(_, session)| session.last_seen)
                    .map(|(id, _)| *id)
                    .expect("speaker pool non-empty");
                self.speakers.remove(&oldest);
            }
            let detector = (self.make_detector)(clid);
            self.speakers.insert(
                clid,
                SpeakerSession {
                    detector,
                    acc: Vec::new(),
                    last_wake: None,
                    last_seen: now,
                    fed_samples: 0,
                    utterance: UtteranceState::default(),
                },
            );
        }
        let session = self
            .speakers
            .get_mut(&clid)
            .expect("speaker session just created");
        session.last_seen = now;
        session
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    struct HitStub;

    impl Detector for HitStub {
        fn detect(&mut self, _chunk: Vec<f32>) -> Detection {
            Detection {
                detected: true,
                probability: 0.9,
            }
        }
    }

    struct CountingStub {
        chunk_sizes: Arc<Mutex<Vec<usize>>>,
    }

    /// 只在第 `fire_after_blocks` 块命中；块计数按实例保存在闭包状态里
    /// （不得用进程级 static：并行的其他门控测试会共享同一计数器）
    struct NthBlockStub {
        seen: usize,
        fire_after_blocks: usize,
    }

    impl Detector for NthBlockStub {
        fn detect(&mut self, _chunk: Vec<f32>) -> Detection {
            self.seen += 1;
            let detected = self.seen == self.fire_after_blocks;
            Detection {
                detected,
                probability: if detected { 0.9 } else { 0.0 },
            }
        }
    }

    /// 打点桩：固定分数序列，末块命中，用于验证校准摘要与窗口标志
    struct SingleFireStub {
        scores: Vec<f32>,
        index: usize,
    }

    impl Detector for SingleFireStub {
        fn detect(&mut self, _chunk: Vec<f32>) -> Detection {
            let raw = self.scores.get(self.index).copied().unwrap_or(0.0);
            self.index += 1;
            let detected = self.index == self.scores.len();
            Detection {
                detected,
                probability: raw,
            }
        }

        fn score_history(&self) -> Vec<ScorePoint> {
            self.scores
                .iter()
                .take(self.index)
                .enumerate()
                .map(|(position, raw)| ScorePoint {
                    raw: *raw,
                    chunk_index: position as u64,
                })
                .collect()
        }
    }

    impl Detector for CountingStub {
        fn detect(&mut self, chunk: Vec<f32>) -> Detection {
            self.chunk_sizes
                .lock()
                .expect("stub lock")
                .push(chunk.len());
            Detection {
                detected: false,
                probability: 0.0,
            }
        }
    }

    fn hit_gate() -> WakewordGate {
        WakewordGate::with_factory(15, |_| Box::new(HitStub))
    }

    /// 只在第 1 块命中的门：生产帧长下第 4 帧报告命中，锚点为块首 0
    fn first_block_hit_gate() -> WakewordGate {
        WakewordGate::with_factory(15, |_| {
            Box::new(NthBlockStub {
                seen: 0,
                fire_after_blocks: 1,
            })
        })
    }

    /// 喂一帧生产帧长（320 采样 = 20ms）：`first` 标为本条 utterance 的首帧
    fn feed_320_frame(
        gate: &mut WakewordGate,
        clid: u32,
        voiced: bool,
        first: bool,
        now: Instant,
    ) -> WakeEvent {
        gate.feed(
            clid,
            WakeFrame {
                pcm16: &[0i16; 320],
                voiced,
                utterance_started: first,
            },
            now,
        )
    }

    /// 空临时目录：模型加载测试注入用，避免依赖 exe 同级 `models/` 的真实状态
    fn empty_temp_dir(tag: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tsclaw-{tag}-{stamp}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn load_models_without_model_files_errors() {
        let dir = empty_temp_dir("wakeword-missing");
        let cfg = HeadlessWakewordConfig {
            enabled: true,
            model: "missing.onnx".to_string(),
            ..Default::default()
        };
        let error = load_models_from(&dir, &cfg)
            .err()
            .expect("load_models_from should fail without model files");
        assert!(error.to_string().contains("wakeword model not found"));
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn load_models_rejects_empty_model_file() {
        let dir = empty_temp_dir("wakeword-empty");
        std::fs::write(dir.join(MEL_MODEL_FILE), b"").expect("write empty mel model");
        let cfg = HeadlessWakewordConfig {
            enabled: true,
            model: "wakeword.onnx".to_string(),
            ..Default::default()
        };
        let error = load_models_from(&dir, &cfg)
            .err()
            .expect("load_models_from should fail on empty model file");
        assert!(error.to_string().contains("model file is empty"));
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn gate_opens_on_detection_and_closes_after_window() {
        let mut gate = hit_gate();
        let now = Instant::now();

        let verdict = gate.feed(1, WakeFrame::pcm(&[0i16; 2560]), now).verdict;
        assert!(verdict.open);
        assert!(verdict.detected);
        assert_eq!(verdict.probability, 0.9);

        // 窗口内仍开门，但本段没有新命中
        let verdict = gate
            .feed(1, WakeFrame::pcm(&[]), now + Duration::from_secs(14))
            .verdict;
        assert!(verdict.open);
        assert!(!verdict.detected);

        let verdict = gate
            .feed(1, WakeFrame::pcm(&[]), now + Duration::from_secs(15))
            .verdict;
        assert!(!verdict.open);
    }

    /// 窗口与二次命中互不干扰：窗口内重命中仍报 `detected`，且窗口起点顺延到最近一次命中。
    /// 下游用 `detected` 表达「窗口内喊唤醒词」（插话权），`open` 只表达「未命中时是否放行」，
    /// 两者必须一直独立，否则插话语义会被窗口吞掉。
    #[test]
    fn gate_re_detection_inside_window_keeps_detected_and_refreshes_window() {
        let mut gate = hit_gate();
        let base = Instant::now();

        let first = gate
            .feed(1, WakeFrame::pcm(&[0i16; CHUNK_SIZE]), base)
            .verdict;
        assert!(first.detected);
        assert!(first.open);

        // 窗口内重命中：窗口状态不抑制命中，仍报 detected 并仍开门
        let second = gate
            .feed(
                1,
                WakeFrame::pcm(&[0i16; CHUNK_SIZE]),
                base + Duration::from_secs(10),
            )
            .verdict;
        assert!(second.detected);
        assert!(second.open);

        // 未再命中：距第二次命中 14s。此处仍开门只能由「窗口顺延到最近命中」解释，
        // 若窗口固定在第一次命中，距它 24s 早已关门
        let inside = gate
            .feed(1, WakeFrame::pcm(&[]), base + Duration::from_secs(24))
            .verdict;
        assert!(inside.open);
        assert!(!inside.detected);

        // 距第二次命中满 15s 才关门
        let closed = gate
            .feed(1, WakeFrame::pcm(&[]), base + Duration::from_secs(25))
            .verdict;
        assert!(!closed.open);
    }

    /// 确认播报续窗：把开门窗口从确认时刻重算，用户接话不必抢在原到期点前；
    /// 未建会话的 clid 不因此新建状态
    #[test]
    fn refresh_window_reopens_the_gate_from_the_confirmation() {
        let mut gate = hit_gate();
        let base = Instant::now();
        gate.feed(1, WakeFrame::pcm(&[0i16; CHUNK_SIZE]), base);

        // 距命中满 15s 关门
        assert!(
            !gate
                .feed(1, WakeFrame::pcm(&[]), base + Duration::from_secs(15))
                .verdict
                .open
        );

        // 续窗后重新计时：20s 处仍开门，35s（续窗 +15s）关门
        gate.refresh_window(1, base + Duration::from_secs(20));
        assert!(
            gate.feed(1, WakeFrame::pcm(&[]), base + Duration::from_secs(20))
                .verdict
                .open
        );
        assert!(
            !gate
                .feed(1, WakeFrame::pcm(&[]), base + Duration::from_secs(35))
                .verdict
                .open
        );

        // 没有会话的 clid：刷新是 no-op，不会凭空开门
        gate.refresh_window(9, base);
        assert!(!gate.feed(9, WakeFrame::pcm(&[]), base).verdict.open);
    }

    #[test]
    fn gate_isolates_speakers() {
        let mut gate = hit_gate();
        let now = Instant::now();

        gate.feed(1, WakeFrame::pcm(&[0i16; 2560]), now);
        let other = gate.feed(2, WakeFrame::pcm(&[]), now).verdict;
        assert!(!other.open);
        let own = gate.feed(1, WakeFrame::pcm(&[]), now).verdict;
        assert!(own.open);
    }

    /// 收尾裁决不喂音频、不改推理状态：收帧路径已喂过本条 utterance
    #[test]
    fn take_utterance_verdict_reads_the_gate_without_feeding() {
        let mut gate = hit_gate();
        let now = Instant::now();

        // 未建会话的 clid 恒关门
        assert!(!gate.take_utterance_verdict(9, now).open);

        gate.feed(1, WakeFrame::pcm(&[0i16; CHUNK_SIZE]), now);
        assert!(
            gate.take_utterance_verdict(1, now + Duration::from_secs(14))
                .open
        );
        assert!(
            !gate
                .take_utterance_verdict(1, now + Duration::from_secs(15))
                .open
        );
    }

    /// 收尾裁决不得再喂模型：收尾时重复喂会打乱推理窗口
    #[test]
    fn take_utterance_verdict_does_not_feed_the_detector() {
        let chunk_sizes = Arc::new(Mutex::new(Vec::new()));
        let mut gate = WakewordGate::with_factory(15, {
            let chunk_sizes = chunk_sizes.clone();
            move |_| {
                Box::new(CountingStub {
                    chunk_sizes: chunk_sizes.clone(),
                })
            }
        });
        let now = Instant::now();

        gate.feed(1, WakeFrame::pcm(&[0i16; CHUNK_SIZE]), now);
        assert_eq!(chunk_sizes.lock().expect("stub lock").len(), 1);

        gate.take_utterance_verdict(1, now);
        assert_eq!(
            chunk_sizes.lock().expect("stub lock").len(),
            1,
            "收尾裁决只读会话状态，不再推理"
        );
    }

    /// 逐帧喂入：`fed_samples` 是喂完本帧后的绝对采样位置，逐帧累计，
    /// 命中块序号与打点据此对齐到门的时间轴
    #[test]
    fn fed_samples_accumulates_across_frames() {
        let mut gate = WakewordGate::with_factory(15, |_| {
            Box::new(NthBlockStub {
                seen: 0,
                fire_after_blocks: 3,
            })
        });
        let now = Instant::now();

        assert_eq!(
            gate.feed(1, WakeFrame::pcm(&vec![0i16; 1_000]), now)
                .fed_samples,
            1_000
        );
        assert_eq!(
            gate.feed(1, WakeFrame::pcm(&vec![0i16; 1_000]), now)
                .fed_samples,
            2_000
        );
        let third = gate.feed(1, WakeFrame::pcm(&vec![0i16; 1_000]), now);
        assert_eq!(third.fed_samples, 3_000);
        assert!(!third.verdict.detected);

        // 第 3 块（绝对 2560..3840）在第四次喂入才凑齐，起点落在上一次 feed 的余量里：
        // 本帧的绝对起点是 3_000，而命中块的绝对起点是 2_560
        let hit = gate.feed(1, WakeFrame::pcm(&vec![0i16; 1_000]), now);
        assert_eq!(hit.fed_samples, 4_000);
        assert!(hit.verdict.detected);
        assert_eq!(hit.fire_sample, Some(2_560));
    }

    #[test]
    fn gate_carries_partial_chunk_across_feeds() {
        let chunk_sizes = Arc::new(Mutex::new(Vec::new()));
        let mut gate = WakewordGate::with_factory(15, {
            let chunk_sizes = chunk_sizes.clone();
            move |_| {
                Box::new(CountingStub {
                    chunk_sizes: chunk_sizes.clone(),
                })
            }
        });
        let now = Instant::now();

        gate.feed(1, WakeFrame::pcm(&[0i16; 1000]), now);
        assert!(chunk_sizes.lock().expect("stub lock").is_empty());

        gate.feed(1, WakeFrame::pcm(&[1i16; 280]), now);
        assert_eq!(*chunk_sizes.lock().expect("stub lock"), vec![CHUNK_SIZE]);
    }

    /// 生产帧长 320 采样（20ms）时，一块只在 `carry = 960` 的那一帧凑满：
    /// 命中块的绝对起点必须回到块首，而不是报告帧起点。旧口径只报帧内偏移（恒被钳成 0），
    /// 下游再加报告帧起点，绝对位置就固定偏晚 960 采样（60ms）
    #[test]
    fn fire_sample_is_the_absolute_hit_block_start_for_320_sample_frames() {
        for fire_block in [1usize, 2, 5] {
            let mut gate = WakewordGate::with_factory(15, move |_| {
                Box::new(NthBlockStub {
                    seen: 0,
                    fire_after_blocks: fire_block,
                })
            });
            let now = Instant::now();
            let hit = (0..4 * fire_block)
                .map(|_| gate.feed(1, WakeFrame::pcm(&[0i16; 320]), now))
                .find(|event| event.verdict.detected)
                .expect("320 采样帧下第 N 块恰在第 4N 帧凑满");

            let block_start = (fire_block - 1) * CHUNK_SIZE;
            assert_eq!(hit.fire_sample, Some(block_start));
            // 报告帧起点比命中块首晚 carry = CHUNK_SIZE − 320 = 960 采样；
            // 旧口径（报告帧起点 + 帧内偏移）正是差在这一段
            assert_eq!(hit.fed_samples, fire_block * CHUNK_SIZE);
            assert_eq!(
                hit.fed_samples - 320 - hit.fire_sample.expect("fire sample"),
                960,
                "固定误差就是这一帧携带的余量"
            );
            assert!(!hit.window_open_before_feed);
        }
    }

    /// 换任意帧长喂入，命中块绝对起点恒为 (N−1)×CHUNK_SIZE：块边界只由喂入的采样总数决定，
    /// 与切帧方式无关（旧口径只能报帧内偏移，跨帧时无法还原这个位置）
    #[test]
    fn fire_sample_is_absolute_regardless_of_frame_length() {
        for fire_block in [3usize, 5, 8] {
            for feed_len in [320usize, 1700, 2320, 2437] {
                let mut gate = WakewordGate::with_factory(15, move |_| {
                    Box::new(NthBlockStub {
                        seen: 0,
                        fire_after_blocks: fire_block,
                    })
                });
                let now = Instant::now();

                let hit = (0..64)
                    .find_map(|_| {
                        let event = gate.feed(1, WakeFrame::pcm(&vec![0i16; feed_len]), now);
                        event.verdict.detected.then_some(event)
                    })
                    .unwrap_or_else(|| {
                        panic!("block {fire_block} must fire with feed_len {feed_len}")
                    });

                assert_eq!(
                    hit.fire_sample,
                    Some((fire_block - 1) * CHUNK_SIZE),
                    "fire_block={fire_block} feed_len={feed_len}"
                );
                assert!(!hit.window_open_before_feed);
            }
        }
    }

    /// 生产帧长（320 采样 = 20ms）下逐帧喂门的收尾裁决：命中块内 80ms 的唤醒词尾音判
    /// 「只有唤醒词」，锚点后再攒够 `WAKE_ONLY_TAIL_MS` 的活跃语音才判含命令。
    ///
    /// 命中块的四帧里只有最后一帧触发报告，前 3 帧是在报告之前喂进来的：计入量必须是
    /// 回补后的 80ms（而不是报告帧起算的 20ms），否则「唤醒词 停」这类短命令会被漏判。
    #[test]
    fn utterance_verdict_separates_wakeword_only_from_a_command() {
        let now = Instant::now();

        let mut only_wake = first_block_hit_gate();
        for index in 0..4 {
            feed_320_frame(&mut only_wake, 1, true, index == 0, now);
        }
        // 命中块之后的静音帧不推高计数
        feed_320_frame(&mut only_wake, 1, false, false, now);
        let verdict = only_wake.take_utterance_verdict(1, now);
        assert!(verdict.open && verdict.detected && verdict.wakeword_only);
        assert_eq!(verdict.anchor_sample, Some(0));
        assert_eq!(verdict.anchor_active_ms, 80);
        assert_eq!(verdict.utterance_start_sample, Some(0));
        assert_eq!(verdict.detection_block_index(), Some(0));

        // 锚点之后再接 6 帧活跃语音 = 120ms，合计恰好 200ms：判含命令，进 STT/LLM
        let mut command = first_block_hit_gate();
        for index in 0..10 {
            feed_320_frame(&mut command, 1, true, index == 0, now);
        }
        let verdict = command.take_utterance_verdict(1, now);
        assert!(verdict.open && verdict.detected && !verdict.wakeword_only);
        assert_eq!(verdict.anchor_active_ms, 200);
    }

    /// 锚点之前的活跃语音不计入：命中在第 2 块时，第 1 块的 80ms 活跃语音既不在锚点之后，
    /// 也已超出回补窗口，只有锚点块自身的 80ms 计入
    #[test]
    fn voiced_frames_before_the_anchor_are_not_counted() {
        let mut gate = WakewordGate::with_factory(15, |_| {
            Box::new(NthBlockStub {
                seen: 0,
                fire_after_blocks: 2,
            })
        });
        let now = Instant::now();

        for index in 0..8 {
            feed_320_frame(&mut gate, 1, true, index == 0, now);
        }
        let verdict = gate.take_utterance_verdict(1, now);
        assert!(verdict.detected && verdict.wakeword_only);
        assert_eq!(verdict.anchor_sample, Some(CHUNK_SIZE));
        assert_eq!(verdict.anchor_active_ms, 80);
        assert_eq!(verdict.utterance_start_sample, Some(0));
    }

    /// 新 utterance 不继承上一段的命中；未命中的裁决只由开门窗口决定
    #[test]
    fn a_new_utterance_follows_the_window_without_inheriting_the_hit() {
        let mut gate = first_block_hit_gate();
        let now = Instant::now();

        for index in 0..4 {
            feed_320_frame(&mut gate, 1, true, index == 0, now);
        }
        assert!(gate.take_utterance_verdict(1, now).detected);

        // 新 utterance：首帧重置命中累计，本条自己没命中，但仍在窗口内 → 续说放行
        feed_320_frame(&mut gate, 1, true, true, now);
        let inside = gate.take_utterance_verdict(1, now);
        assert!(inside.open && !inside.detected && !inside.wakeword_only);
        assert_eq!(inside.anchor_sample, None);
        assert_eq!(inside.anchor_active_ms, 0);

        // 窗口到期后未命中的 utterance 关门丢弃
        let closed = gate.take_utterance_verdict(1, now + Duration::from_secs(15));
        assert!(!closed.open && !closed.detected && !closed.wakeword_only);
    }

    /// 打点数据：命中时带出最近原始分数序列与窗口内的续说标志
    #[test]
    fn wake_event_carries_raw_score_history_and_window_flag() {
        let scores = vec![0.01f32, 0.02, 0.65, 0.9];
        let mut gate = WakewordGate::with_factory(15, move |_| {
            Box::new(SingleFireStub {
                scores: scores.clone(),
                index: 0,
            })
        });
        let now = Instant::now();

        let hit = gate.feed(1, WakeFrame::pcm(&[0i16; CHUNK_SIZE * 4]), now);
        assert!(hit.verdict.detected);
        assert!(!hit.window_open_before_feed);
        assert_eq!(hit.calibration.first_chunk_index, 0);
        assert_eq!(hit.calibration.raw_scores, vec![0.01, 0.02, 0.65, 0.9]);
        assert_eq!(hit.calibration.peak_chunk_index(), Some(3));
        assert_eq!(hit.calibration.peak_score(), Some(0.9));

        // 窗口内续说：未再命中，但标记本段进入时门已开
        let inside = gate.feed(1, WakeFrame::pcm(&[]), now + Duration::from_secs(5));
        assert!(!inside.verdict.detected);
        assert!(inside.verdict.open);
        assert!(inside.window_open_before_feed);
    }

    #[test]
    fn gate_pool_evicts_overflow_and_idle_speakers() {
        let mut gate = hit_gate();
        let base = Instant::now();
        for (index, clid) in (1u32..=MAX_SPEAKERS as u32).enumerate() {
            let now = base + Duration::from_secs(index as u64);
            gate.feed(clid, WakeFrame::pcm(&[]), now);
        }
        assert_eq!(gate.speakers.len(), MAX_SPEAKERS);

        // 新说话人挤掉最久未见的 clid 1
        gate.feed(7, WakeFrame::pcm(&[]), base + Duration::from_secs(10));
        assert_eq!(gate.speakers.len(), MAX_SPEAKERS);
        assert!(!gate.speakers.contains_key(&1));

        // 全员空闲超 300s 后驱逐
        gate.evict_idle(base + Duration::from_secs(400));
        assert!(gate.speakers.is_empty());
    }

    #[test]
    fn gate_extends_idle_eviction_past_window_upper_bound() {
        // window_secs 取上限 300 时驱逐阈值放宽到 360s，窗口尾不被空闲驱逐截断
        let mut gate = WakewordGate::with_factory(300, |_| Box::new(HitStub));
        let now = Instant::now();
        gate.feed(1, WakeFrame::pcm(&[0i16; CHUNK_SIZE]), now);

        gate.evict_idle(now + Duration::from_secs(330));
        assert_eq!(gate.speakers.len(), 1);

        gate.evict_idle(now + Duration::from_secs(361));
        assert!(gate.speakers.is_empty());
    }
}

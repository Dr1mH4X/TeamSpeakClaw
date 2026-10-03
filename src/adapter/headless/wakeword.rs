//! OpenWakeWord 语音唤醒门：推理核心 vendor 自 oww_rs（MIT，commit d945876）。
//! 三个 onnx（melspectrogram/embedding 前端 + 分类器）均从 `models_dir()` 运行时加载。

mod calibration;
mod model;

use std::any::Any;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tracing::info;

use crate::config::headless::HeadlessWakewordConfig;
use crate::config::models_dir;

pub use calibration::{calibration_stats, CalibrationStats, ScorePoint};
use model::{Detection, WakewordModel, DETECTION_THRESHOLD};
/// 对外暴露块长：下游（唤醒判据的回补窗口、命中块序号打点）必须与门内推理的块边界同源
pub use model::{WakewordModels, CHUNK_SIZE};

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
    /// 本次输入之前该说话人累计喂入的采样数：也是本帧在门时间轴上的绝对起点，
    /// 逐帧喂入时下游据此把每帧的 VAD 状态对齐到 `fire_sample` 的同一时间轴
    pub fed_samples_before: usize,
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
/// `feed`（语音到达）创建缺失的会话并刷新窗口，`refresh_window`（确认音）只刷新已存在会话的
/// 窗口、不做推理也不新建，`window_state` 只读窗口状态（含空闲驱逐）；gRPC 解析在开门后才发生
pub struct WakewordGate {
    window: Duration,
    /// 空闲驱逐阈值：`MIN_IDLE_EVICT_AFTER` 与 `window + IDLE_EVICT_MARGIN` 取大
    idle_evict_after: Duration,
    speakers: HashMap<u32, SpeakerSession>,
    make_detector: DetectorFactory,
}

struct SpeakerSession {
    detector: Box<dyn Detector>,
    /// 不足一块 1280 采样的余量，跨 utterance 携带
    acc: Vec<f32>,
    last_wake: Option<Instant>,
    last_seen: Instant,
    /// 累计喂入的采样数（打点用绝对命中位置）
    fed_samples: usize,
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

    /// 喂入一段 16kHz 单声道 PCM，返回本次裁决与打点数据。
    ///
    /// 生产路径按音频帧逐次喂入：门内部按 80ms 块推理，跨帧余量由 `acc` 携带，
    /// 所以通话期间的连续推理与一次性喂整段 utterance 等价，且命中能立刻返回
    /// （插话不必等 utterance 收尾）。
    pub fn feed(&mut self, clid: u32, pcm16: &[i16], now: Instant) -> WakeEvent {
        let window = self.window;
        self.evict_idle(now);
        let session = self.session_for(clid, now);
        let window_open_before_feed = session
            .last_wake
            .is_some_and(|wake| now.duration_since(wake) < window);
        // 本次输入开始前累积的余量：acc 前 carry 个采样来自上一次 feed，在本帧之前就已喂入，
        // 所以块的绝对起点要从 fed_samples_before 往回退 carry 个采样。
        // 必须在 extend 之前取，否则 carry 含进本次输入，回退量算错
        let carry = session.acc.len();
        let fed_samples_before = session.fed_samples;
        session
            .acc
            .extend(pcm16.iter().map(|sample| f32::from(*sample) / 32_768.0));

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
        session.fed_samples += pcm16.len();

        if detected {
            session.last_wake = Some(now);
        }
        let open = detected
            || session
                .last_wake
                .is_some_and(|wake| now.duration_since(wake) < window);
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
            fed_samples_before,
        }
    }

    /// 当前开门窗口状态：收帧路径已经把这条 utterance 的音频喂过门，
    /// 收尾裁决只需要问「窗口还开着吗」，不能再喂一遍（重复喂会打乱推理窗口）
    pub fn window_state(&mut self, clid: u32, now: Instant) -> GateVerdict {
        self.evict_idle(now);
        let open = self.speakers.get(&clid).is_some_and(|session| {
            session
                .last_wake
                .is_some_and(|wake| now.duration_since(wake) < self.window)
        });
        GateVerdict {
            open,
            detected: false,
            probability: 0.0,
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

        let verdict = gate.feed(1, &[0i16; 2560], now).verdict;
        assert!(verdict.open);
        assert!(verdict.detected);
        assert_eq!(verdict.probability, 0.9);

        // 窗口内仍开门，但本段没有新命中
        let verdict = gate.feed(1, &[], now + Duration::from_secs(14)).verdict;
        assert!(verdict.open);
        assert!(!verdict.detected);

        let verdict = gate.feed(1, &[], now + Duration::from_secs(15)).verdict;
        assert!(!verdict.open);
    }

    /// 窗口与二次命中互不干扰：窗口内重命中仍报 `detected`，且窗口起点顺延到最近一次命中。
    /// 下游用 `detected` 表达「窗口内喊唤醒词」（插话权），`open` 只表达「未命中时是否放行」，
    /// 两者必须一直独立，否则插话语义会被窗口吞掉。
    #[test]
    fn gate_re_detection_inside_window_keeps_detected_and_refreshes_window() {
        let mut gate = hit_gate();
        let base = Instant::now();

        let first = gate.feed(1, &[0i16; CHUNK_SIZE], base).verdict;
        assert!(first.detected);
        assert!(first.open);

        // 窗口内重命中：窗口状态不抑制命中，仍报 detected 并仍开门
        let second = gate
            .feed(1, &[0i16; CHUNK_SIZE], base + Duration::from_secs(10))
            .verdict;
        assert!(second.detected);
        assert!(second.open);

        // 未再命中：距第二次命中 14s。此处仍开门只能由「窗口顺延到最近命中」解释，
        // 若窗口固定在第一次命中，距它 24s 早已关门
        let inside = gate.feed(1, &[], base + Duration::from_secs(24)).verdict;
        assert!(inside.open);
        assert!(!inside.detected);

        // 距第二次命中满 15s 才关门
        let closed = gate.feed(1, &[], base + Duration::from_secs(25)).verdict;
        assert!(!closed.open);
    }

    /// 确认播报续窗：把开门窗口从确认时刻重算，用户接话不必抢在原到期点前；
    /// 未建会话的 clid 不因此新建状态
    #[test]
    fn refresh_window_reopens_the_gate_from_the_confirmation() {
        let mut gate = hit_gate();
        let base = Instant::now();
        gate.feed(1, &[0i16; CHUNK_SIZE], base);

        // 距命中满 15s 关门
        assert!(
            !gate
                .feed(1, &[], base + Duration::from_secs(15))
                .verdict
                .open
        );

        // 续窗后重新计时：20s 处仍开门，35s（续窗 +15s）关门
        gate.refresh_window(1, base + Duration::from_secs(20));
        assert!(
            gate.feed(1, &[], base + Duration::from_secs(20))
                .verdict
                .open
        );
        assert!(
            !gate
                .feed(1, &[], base + Duration::from_secs(35))
                .verdict
                .open
        );

        // 没有会话的 clid：刷新是 no-op，不会凭空开门
        gate.refresh_window(9, base);
        assert!(!gate.feed(9, &[], base).verdict.open);
    }

    #[test]
    fn gate_isolates_speakers() {
        let mut gate = hit_gate();
        let now = Instant::now();

        gate.feed(1, &[0i16; 2560], now);
        let other = gate.feed(2, &[], now).verdict;
        assert!(!other.open);
        let own = gate.feed(1, &[], now).verdict;
        assert!(own.open);
    }

    /// 窗口状态查询不喂音频、不改推理状态：收帧路径已喂过本条 utterance
    #[test]
    fn window_state_reads_the_gate_without_feeding() {
        let mut gate = hit_gate();
        let now = Instant::now();

        // 未建会话的 clid 恒关门
        assert!(!gate.window_state(9, now).open);

        gate.feed(1, &[0i16; CHUNK_SIZE], now);
        assert!(gate.window_state(1, now + Duration::from_secs(14)).open);
        assert!(!gate.window_state(1, now + Duration::from_secs(15)).open);
    }

    /// 窗口状态查询不得再喂模型：收尾时重复喂会打乱推理窗口
    #[test]
    fn window_state_does_not_feed_the_detector() {
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

        gate.feed(1, &[0i16; CHUNK_SIZE], now);
        assert_eq!(chunk_sizes.lock().expect("stub lock").len(), 1);

        gate.window_state(1, now);
        assert_eq!(
            chunk_sizes.lock().expect("stub lock").len(),
            1,
            "查询只读窗口状态，不再推理"
        );
    }

    /// 逐帧喂入：`fed_samples_before` 是本次输入的绝对起点，逐帧累计，
    /// 下游用它把每帧的 VAD 状态对齐到 `fire_sample` 的时间轴
    #[test]
    fn fed_samples_before_accumulates_across_frames() {
        let mut gate = WakewordGate::with_factory(15, |_| {
            Box::new(NthBlockStub {
                seen: 0,
                fire_after_blocks: 3,
            })
        });
        let now = Instant::now();

        assert_eq!(gate.feed(1, &vec![0i16; 1_000], now).fed_samples_before, 0);
        assert_eq!(
            gate.feed(1, &vec![0i16; 1_000], now).fed_samples_before,
            1_000
        );
        let third = gate.feed(1, &vec![0i16; 1_000], now);
        assert_eq!(third.fed_samples_before, 2_000);
        assert!(!third.verdict.detected);

        // 第 3 块（绝对 2560..3840）在第四次喂入才凑齐，起点落在上一次 feed 的余量里：
        // 本帧的绝对起点是 3_000，而命中块的绝对起点是 2_560
        let hit = gate.feed(1, &vec![0i16; 1_000], now);
        assert_eq!(hit.fed_samples_before, 3_000);
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

        gate.feed(1, &[0i16; 1000], now);
        assert!(chunk_sizes.lock().expect("stub lock").is_empty());

        gate.feed(1, &[1i16; 280], now);
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
                .map(|_| gate.feed(1, &[0i16; 320], now))
                .find(|event| event.verdict.detected)
                .expect("320 采样帧下第 N 块恰在第 4N 帧凑满");

            let block_start = (fire_block - 1) * CHUNK_SIZE;
            assert_eq!(hit.fire_sample, Some(block_start));
            // 报告帧起点比命中块首晚 carry = CHUNK_SIZE − 320 = 960 采样；
            // 旧口径（报告帧起点 + 帧内偏移）正是差在这一段
            assert_eq!(hit.fed_samples_before, fire_block * CHUNK_SIZE - 320);
            assert_eq!(
                hit.fed_samples_before - hit.fire_sample.expect("fire sample"),
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
                        let event = gate.feed(1, &vec![0i16; feed_len], now);
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

        let hit = gate.feed(1, &[0i16; CHUNK_SIZE * 4], now);
        assert!(hit.verdict.detected);
        assert!(!hit.window_open_before_feed);
        assert_eq!(hit.calibration.first_chunk_index, 0);
        assert_eq!(hit.calibration.raw_scores, vec![0.01, 0.02, 0.65, 0.9]);
        assert_eq!(hit.calibration.peak_chunk_index(), Some(3));
        assert_eq!(hit.calibration.peak_score(), Some(0.9));

        // 窗口内续说：未再命中，但标记本段进入时门已开
        let inside = gate.feed(1, &[], now + Duration::from_secs(5));
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
            gate.feed(clid, &[], now);
        }
        assert_eq!(gate.speakers.len(), MAX_SPEAKERS);

        // 新说话人挤掉最久未见的 clid 1
        gate.feed(7, &[], base + Duration::from_secs(10));
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
        gate.feed(1, &[0i16; CHUNK_SIZE], now);

        gate.evict_idle(now + Duration::from_secs(330));
        assert_eq!(gate.speakers.len(), 1);

        gate.evict_idle(now + Duration::from_secs(361));
        assert!(gate.speakers.is_empty());
    }
}

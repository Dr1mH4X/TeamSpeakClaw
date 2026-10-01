//! OpenWakeWord 语音唤醒门：推理核心 vendor 自 oww_rs（MIT，commit d945876）。
//! 三个 onnx（melspectrogram/embedding 前端 + 分类器）均从 `models_dir()` 运行时加载。

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

pub use model::WakewordModels;
use model::{Detection, WakewordModel, CHUNK_SIZE};

/// 唤醒门推理实现：生产为 OWW 推理，测试注入桩
trait Detector: Send {
    fn detect(&mut self, chunk: Vec<f32>) -> Detection;
}

impl Detector for WakewordModel {
    fn detect(&mut self, chunk: Vec<f32>) -> Detection {
        self.detection(chunk)
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
        WakewordModels::from_bytes(&mel, &emb, &classifier, cfg.threshold)
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
        threshold = cfg.threshold,
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
/// 只有语音到达会创建/刷新会话状态，gRPC 解析在开门后才发生
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

    /// 喂入一段 16kHz 单声道 PCM，返回本段裁决
    pub fn feed(&mut self, clid: u32, pcm16: &[i16], now: Instant) -> GateVerdict {
        let window = self.window;
        self.evict_idle(now);
        let session = self.session_for(clid, now);
        session
            .acc
            .extend(pcm16.iter().map(|sample| f32::from(*sample) / 32_768.0));

        let mut detected = false;
        let mut probability = 0.0f32;
        while session.acc.len() >= CHUNK_SIZE {
            let chunk: Vec<f32> = session.acc.drain(..CHUNK_SIZE).collect();
            let detection = session.detector.detect(chunk);
            if detection.detected {
                detected = true;
                probability = detection.probability;
            }
        }

        if detected {
            session.last_wake = Some(now);
        }
        let open = detected
            || session
                .last_wake
                .is_some_and(|wake| now.duration_since(wake) < window);
        GateVerdict {
            open,
            detected,
            probability,
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

        let verdict = gate.feed(1, &[0i16; 2560], now);
        assert!(verdict.open);
        assert!(verdict.detected);
        assert_eq!(verdict.probability, 0.9);

        // 窗口内仍开门，但本段没有新命中
        let verdict = gate.feed(1, &[], now + Duration::from_secs(14));
        assert!(verdict.open);
        assert!(!verdict.detected);

        let verdict = gate.feed(1, &[], now + Duration::from_secs(15));
        assert!(!verdict.open);
    }

    #[test]
    fn gate_isolates_speakers() {
        let mut gate = hit_gate();
        let now = Instant::now();

        gate.feed(1, &[0i16; 2560], now);
        let other = gate.feed(2, &[], now);
        assert!(!other.open);
        let own = gate.feed(1, &[], now);
        assert!(own.open);
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

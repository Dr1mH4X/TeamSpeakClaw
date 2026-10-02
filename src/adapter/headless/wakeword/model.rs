// vendor 自 oww_rs（MIT，https://github.com/skoky/oww_rs @ d945876）的 OpenWakeWord 推理核心。
// 改动：rust-embed → 运行时按配置读文件、log → tracing、circular-buffer → VecDeque，
// 删除 mic/内置唤醒词清单胶水；三个 onnx 均来自模型目录。
use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tract_core::internal::{RunnableModel, TypedFact, TypedOp};
use tract_core::model::IntoRunnable;
use tract_core::prelude::multithread::{self, Executor};
use tract_onnx::prelude::*;

use super::calibration::ScorePoint;

/// 上游 OWW 音频块：1280 采样 = 80ms @ 16kHz
pub const CHUNK_SIZE: usize = 1280;
/// 检测平滑窗：12 次检测约 1 秒
const DETECTION_BUFFER_SIZE: usize = 12;
/// 平滑均值触发所需的最小正检次数
const MIN_POSITIVE_DETECTIONS: f32 = 2.0;
/// 分类器命中阈值：固定值，不随配置变化
pub const DETECTION_THRESHOLD: f32 = 0.3;
/// 同一发声的再触发不应期
const NO_DETECTION_MS: u32 = 2_000;
/// embedding 滑窗：16 个 [1,1,1,96] 特征
const FEATURE_BUFFER_SIZE: usize = 16;
/// melspectrogram 跨块 lookback：3 个 160 采样 hop，保证 mel 窗跨边界连续
const MEL_LOOKBACK: usize = 160 * 3;
/// mel 模型输入：480 lookback + 1280 新块
const MEL_INPUT_SIZE: usize = MEL_LOOKBACK + CHUNK_SIZE;
/// 每块产出 mel 帧：1760/160 - 3 = 8
const MELS_PER_CHUNK: usize = MEL_INPUT_SIZE / 160 - 3;
/// mel 滑窗：80 帧 / 每块 8 帧 = 10
const MEL_CIRC_SIZE: usize = 80 / MELS_PER_CHUNK;
/// 打点用的原始分数环形缓冲：40 块 × 80ms = 3.2s，覆盖唤醒词前后完整窗口。
/// 与 `detections_buffer` 分开：后者在命中时清空，无法用于回溯峰值
pub const SCORE_HISTORY_SIZE: usize = 40;

type ModelType = Arc<RunnableModel<TypedFact, Box<dyn TypedOp>>>;

#[derive(Debug, Clone, Copy)]
pub struct Detection {
    pub detected: bool,
    pub probability: f32,
}

/// 共享推理模型：前端（mel + embedding）与分类器各解析一次；
/// per-clid 会话 `WakewordModel` 只持有各自的滑窗状态
pub struct WakewordModels {
    mel: ModelType,
    emb: ModelType,
    classifier: ModelType,
}

impl WakewordModels {
    /// 从三个 onnx 字节构造；解析失败按 anyhow 上抛（调用方在 catch_unwind 内）
    pub fn from_bytes(
        mel_data: &[u8],
        emb_data: &[u8],
        classifier_data: &[u8],
    ) -> Result<Arc<Self>> {
        // 推理执行器按上游行为固定单线程（进程全局设置，此处设置一次）
        multithread::set_default_executor(Executor::SingleThread);

        let mel = parse_runnable(mel_data, Some(f32::fact([1, MEL_INPUT_SIZE])))
            .context("parse melspectrogram.onnx")?;
        let emb = parse_runnable(emb_data, Some(f32::fact([1, 76, 32, 1])))
            .context("parse embedding_model.onnx")?;
        let classifier =
            parse_runnable(classifier_data, None).context("parse wakeword classifier model")?;
        Ok(Arc::new(Self {
            mel,
            emb,
            classifier,
        }))
    }
}

fn parse_runnable(data: &[u8], input_fact: Option<TypedFact>) -> Result<ModelType> {
    let mut model = tract_onnx::onnx().model_for_read(&mut Cursor::new(data))?;
    if let Some(fact) = input_fact {
        model = model.with_input_fact(0, fact.into())?;
    }
    let runnable: ModelType = model.into_optimized()?.into_runnable()?;
    Ok(runnable)
}

/// per-clid 检测实例：共享 `WakewordModels`，各自持有特征滑窗与不应期状态
pub struct WakewordModel {
    models: Arc<WakewordModels>,
    raw_lookback: Vec<f32>,
    feature_buffer: VecDeque<Tensor>,
    mel_spectrogram_buffer: VecDeque<Tensor>,
    detections_buffer: VecDeque<f32>,
    /// 打点用：最近 `SCORE_HISTORY_SIZE` 块的原始分数与绝对块序号；命中不清空，
    /// 保证命中时刻仍能回溯峰值位置
    score_history: VecDeque<ScorePoint>,
    chunks_seen: u64,
    last_detection_time: Instant,
}

impl WakewordModel {
    pub fn new(models: Arc<WakewordModels>) -> Self {
        let mut feature_buffer = VecDeque::with_capacity(FEATURE_BUFFER_SIZE);
        for _ in 0..FEATURE_BUFFER_SIZE {
            feature_buffer.push_back(
                Tensor::from_shape(&[1, 1, 1, 96], &[0f32; 96]).expect("zero feature tensor"),
            );
        }
        let mut mel_spectrogram_buffer = VecDeque::with_capacity(MEL_CIRC_SIZE);
        for _ in 0..MEL_CIRC_SIZE {
            mel_spectrogram_buffer.push_back(
                Tensor::from_shape(&[MELS_PER_CHUNK, 32], &[0f32; MELS_PER_CHUNK * 32])
                    .expect("zero mel tensor"),
            );
        }
        Self {
            models,
            raw_lookback: vec![0f32; MEL_LOOKBACK],
            feature_buffer,
            mel_spectrogram_buffer,
            detections_buffer: VecDeque::with_capacity(DETECTION_BUFFER_SIZE),
            score_history: VecDeque::with_capacity(SCORE_HISTORY_SIZE),
            chunks_seen: 0,
            // 不应期只用于抑制同一发声的重复触发；初值置于不应期之前，
            // 让会话建立后第一次越过阈值的命中立即触发（会话按 utterance 建立，
            // 若以 now 初始化，本会话第一条语音的命中会被整体吞掉）
            last_detection_time: Instant::now()
                - Duration::from_millis(u64::from(NO_DETECTION_MS) + 1),
        }
    }

    /// 单块推理：特征提取 + 分类 + 平滑判定。shape 全为常量，失败即内部缺陷，按 FAILFAST 暴露
    pub fn detection(&mut self, chunk: Vec<f32>) -> Detection {
        let features = self.get_audio_features(&chunk);
        self.detect(features)
    }

    fn detect(&mut self, features: Tensor) -> Detection {
        let input = features
            .into_shape(&[1, 16, 96])
            .expect("feature reshape [1,16,96]");
        let out: TVec<TValue> = self
            .models
            .classifier
            .run(tvec!(input.into()))
            .expect("wakeword classifier inference");
        let t = out[0]
            .clone()
            .into_tensor()
            .cast_to::<f32>()
            .expect("classifier output to f32")
            .into_owned();
        let probability = t
            .into_plain_array::<f32>()
            .expect("classifier output as array")
            .as_slice()
            .expect("classifier output contiguous")[0];

        push_ring(
            &mut self.detections_buffer,
            probability,
            DETECTION_BUFFER_SIZE,
        );
        // 打点在原始输出处采集：此缓冲不清空，命中时仍能回溯峰值
        push_ring(
            &mut self.score_history,
            ScorePoint {
                raw: probability,
                chunk_index: self.chunks_seen,
            },
            SCORE_HISTORY_SIZE,
        );
        self.chunks_seen = self.chunks_seen.saturating_add(1);
        let average = self.calculate_average();
        let since_last_detection = self.last_detection_time.elapsed().as_millis();

        // 平滑均值越过阈值即触发（rising-edge，与 Python openWakeWord 一致）；
        // 触发后清空缓冲，同一发声在不应期内不会重复开门
        if average > DETECTION_THRESHOLD && since_last_detection > u128::from(NO_DETECTION_MS) {
            self.last_detection_time = Instant::now();
            self.detections_buffer.clear();
            return Detection {
                detected: true,
                probability: average,
            };
        }
        if average > 0.1 {
            tracing::debug!(
                probability,
                average,
                since_last_detection,
                "wakeword below threshold"
            );
        }
        Detection {
            detected: false,
            probability: average,
        }
    }

    /// 打点用原始分数序列（绝对块序号 + 原始分数，升序）
    pub fn raw_score_history(&self) -> Vec<ScorePoint> {
        self.score_history.iter().copied().collect()
    }

    fn calculate_average(&self) -> f32 {
        let mut cumulative = 0.0f32;
        let mut positive_count = 0.0f32;
        for &d in &self.detections_buffer {
            if d > DETECTION_THRESHOLD {
                positive_count += 1.0;
                cumulative += d;
            }
        }
        if positive_count < MIN_POSITIVE_DETECTIONS {
            return 0.0;
        }
        let avg = cumulative / positive_count;
        if avg > DETECTION_THRESHOLD {
            avg
        } else {
            0.0
        }
    }

    fn get_melspectrogram(&mut self, data: &[f32]) -> Tensor {
        // 前置上一块尾部，使 mel 窗跨块边界连续
        let mut input = Vec::with_capacity(MEL_INPUT_SIZE);
        input.extend_from_slice(&self.raw_lookback);
        input.extend_from_slice(data);
        self.raw_lookback
            .copy_from_slice(&data[data.len() - MEL_LOOKBACK..]);

        let tensor =
            Tensor::from_shape(&[1, MEL_INPUT_SIZE], &input).expect("melspectrogram input shape");
        let outputs: TVec<TValue> = self
            .models
            .mel
            .run(tvec!(tensor.into()))
            .expect("melspectrogram inference");
        let out_tensor = outputs[0].clone().into_tensor();
        let resized = out_tensor
            .into_shape(&[MELS_PER_CHUNK, 32])
            .expect("melspectrogram output shape");
        let array = resized
            .into_plain_array::<f32>()
            .expect("melspectrogram output array")
            .into_owned();
        array.mapv(|v| (v / 10.0) + 2.0).into_tensor()
    }

    fn get_audio_features(&mut self, data: &[f32]) -> Tensor {
        let mel_chunk = self.get_melspectrogram(data);
        push_ring(&mut self.mel_spectrogram_buffer, mel_chunk, MEL_CIRC_SIZE);
        let stacked_mels = Tensor::stack_tensors(
            0,
            &self
                .mel_spectrogram_buffer
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
        )
        .expect("stack mel spectrogram window");
        let smaller = stacked_mels.slice(0, 4, 80).expect("mel window slice");
        let reshaped = smaller
            .into_shape(&[1, 76, 32, 1])
            .expect("mel window reshape");
        let embeddings = self
            .models
            .emb
            .run(tvec!(reshaped.into()))
            .expect("embedding inference");
        push_ring(
            &mut self.feature_buffer,
            embeddings[0].clone().into_tensor(),
            FEATURE_BUFFER_SIZE,
        );
        let stacked =
            Tensor::stack_tensors(0, &self.feature_buffer.iter().cloned().collect::<Vec<_>>())
                .expect("stack feature window");
        stacked
            .into_shape(&[self.feature_buffer.len(), 96])
            .expect("feature window reshape")
    }
}

/// 环形 push：满时丢弃最旧（等价上游 CircularBuffer 的定长覆盖）
fn push_ring<T>(buffer: &mut VecDeque<T>, value: T, capacity: usize) {
    if buffer.len() >= capacity {
        buffer.pop_front();
    }
    buffer.push_back(value);
}

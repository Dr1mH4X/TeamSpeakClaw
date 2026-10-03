//! 统一音频出站：FIFO 队列 + 消费者任务，唯一持有 `ts3_audio_tx` 写端。
//! 所有 job 的 deadline 自消费者 dequeue 时起算，不从 enqueue 起算。
//! finish 语义：段被消费者通道接收后返回（入队即返回），不等待播完；`finish_drained` 额外等待本会话 job 收尾。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::{io::ErrorKind, process::Stdio};

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{timeout, Instant};
use tracing::{debug, warn};

use super::audio_codec::{
    new_opus_stereo_encoder, pcm_frame_to_float, PCM_FRAME_MS, PCM_FRAME_SAMPLES_STEREO,
};
use super::speech::detect_audio_format;

pub const JOB_QUEUE_CAPACITY: usize = 8;
const TTS_SEGMENT_CAPACITY: usize = 8;
const MIN_PCM_JOB_SECS: u64 = 30;
const PCM_JOB_EXTRA_SECS: u64 = 15;
const ENCODED_FIRST_SEGMENT_TIMEOUT_SECS: u64 = 15;
/// 段间空闲上限：覆盖 TTS 中途停流（TTS 无总时长上限）
const ENCODED_INTER_SEGMENT_IDLE_SECS: u64 = 30;
/// 外部源总时长上限（自 dequeue 起算）
const ENCODED_EXTERNAL_MAX_JOB_SECS: u64 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobSource {
    SkillClip,
    Tts,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    PcmClip,
    EncodedStream,
}

#[derive(Debug, Clone)]
pub struct CurrentJobInfo {
    pub source: JobSource,
    pub kind: JobKind,
    pub started_at: std::time::Instant,
}

#[derive(Debug, Clone)]
pub struct AudioOutputStatus {
    pub current: Option<CurrentJobInfo>,
    pub queued_jobs: usize,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PcmClipPayload {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u16,
}

struct EncodedSegment {
    payload: Vec<u8>,
    codec: String,
}

type JobDoneTx = oneshot::Sender<std::result::Result<(), String>>;
type JobDoneRx = oneshot::Receiver<std::result::Result<(), String>>;

enum AudioJob {
    PcmClip {
        source: JobSource,
        payload: PcmClipPayload,
        cancel: Arc<AtomicBool>,
        done: JobDoneTx,
        cancel_id: u64,
    },
    EncodedStream {
        source: JobSource,
        segment_rx: mpsc::Receiver<EncodedSegment>,
        cancel: Arc<AtomicBool>,
        done: JobDoneTx,
    },
}

struct AudioOutputStatusInner {
    current: Mutex<Option<CurrentJobInfo>>,
    queued_jobs: AtomicUsize,
    /// 消费者在 job 失败时写入；技能 status / 直呼只读，不在此 panic
    last_error: Mutex<Option<String>>,
    clip_cancels: Mutex<HashMap<u64, Arc<AtomicBool>>>,
    next_cancel_id: AtomicU64,
}

impl AudioOutputStatusInner {
    fn new() -> Self {
        Self {
            current: Mutex::new(None),
            queued_jobs: AtomicUsize::new(0),
            last_error: Mutex::new(None),
            clip_cancels: Mutex::new(HashMap::new()),
            next_cancel_id: AtomicU64::new(1),
        }
    }

    fn snapshot(&self) -> AudioOutputStatus {
        AudioOutputStatus {
            current: self.current.lock().expect("audio status poisoned").clone(),
            queued_jobs: self.queued_jobs.load(Ordering::SeqCst),
            last_error: self
                .last_error
                .lock()
                .expect("audio status poisoned")
                .clone(),
        }
    }

    /// 先递增计数再 try_send：消费者 dequeue 时的 fetch_sub 不会与生产者竞争导致下溢
    fn enqueue_job(
        &self,
        job_tx: &mpsc::Sender<AudioJob>,
        build: impl FnOnce(u64) -> AudioJob,
    ) -> Result<()> {
        let cancel_id = self.next_cancel_id.fetch_add(1, Ordering::SeqCst);
        self.queued_jobs.fetch_add(1, Ordering::SeqCst);
        let job = build(cancel_id);
        if job_tx.try_send(job).is_err() {
            self.queued_jobs.fetch_sub(1, Ordering::SeqCst);
            return Err(anyhow!("audio output queue full"));
        }
        Ok(())
    }

    fn dequeue_job(&self) {
        self.queued_jobs.fetch_sub(1, Ordering::SeqCst);
    }

    fn register_clip_cancel(&self, cancel_id: u64, cancel: Arc<AtomicBool>) {
        self.clip_cancels
            .lock()
            .expect("audio status poisoned")
            .insert(cancel_id, cancel);
    }

    fn unregister_clip_cancel(&self, cancel_id: u64) {
        self.clip_cancels
            .lock()
            .expect("audio status poisoned")
            .remove(&cancel_id);
    }
}

struct AudioOutputInner {
    job_tx: mpsc::Sender<AudioJob>,
    status: Arc<AudioOutputStatusInner>,
}

#[derive(Clone)]
pub struct AudioOutput {
    inner: Arc<AudioOutputInner>,
}

pub struct AudioOutputConsumer {
    job_rx: mpsc::Receiver<AudioJob>,
    status: Arc<AudioOutputStatusInner>,
}

pub struct ClipHandle {
    cancel: Arc<AtomicBool>,
    done: JobDoneRx,
}

impl ClipHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub async fn wait(self) -> Result<()> {
        match self.done.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(anyhow!(error)),
            Err(_) => Err(anyhow!("audio clip job dropped")),
        }
    }
}

pub struct TtsSession {
    segment_tx: Option<mpsc::Sender<EncodedSegment>>,
    done_rx: Option<JobDoneRx>,
    cancel: Arc<AtomicBool>,
    finished: bool,
}

impl TtsSession {
    /// 段 channel 容量见 `TTS_SEGMENT_CAPACITY`；满时 `send` 背压阻塞调用方（固有行为）
    pub async fn push_encoded(&mut self, payload: Vec<u8>, codec: &str) -> Result<()> {
        let tx = self
            .segment_tx
            .as_ref()
            .ok_or_else(|| anyhow!("tts session closed"))?;
        let codec = if codec.is_empty() {
            detect_audio_format(&payload).to_string()
        } else {
            codec.to_string()
        };
        tx.send(EncodedSegment { payload, codec })
            .await
            .map_err(|_| anyhow!("tts session consumer gone"))?;
        Ok(())
    }

    /// 关闭段通道并返回：消费者继续 FIFO 播完已入队内容；不在此等待播完
    pub async fn finish(mut self) -> Result<()> {
        self.finished = true;
        self.segment_tx = None;
        self.done_rx = None;
        Ok(())
    }

    /// 关闭段通道并等待消费者播完已入队内容：本会话 job 结束时返回，取消/失败同样收尾
    pub async fn finish_drained(mut self) -> Result<()> {
        self.finished = true;
        self.segment_tx = None;
        match self.done_rx.take() {
            Some(done_rx) => match done_rx.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(anyhow!(error)),
                Err(_) => Err(anyhow!("tts session job dropped")),
            },
            None => Ok(()),
        }
    }
}

impl Drop for TtsSession {
    fn drop(&mut self) {
        if !self.finished {
            self.cancel.store(true, Ordering::SeqCst);
        }
    }
}

/// PcmClip 时长预算：自 dequeue 起算
fn pcm_clip_deadline(dequeue_at: Instant, pcm_duration: std::time::Duration) -> Instant {
    let budget = std::cmp::max(
        std::time::Duration::from_secs(MIN_PCM_JOB_SECS),
        pcm_duration + std::time::Duration::from_secs(PCM_JOB_EXTRA_SECS),
    );
    dequeue_at + budget
}

fn pcm_payload_duration(payload: &PcmClipPayload) -> Result<std::time::Duration> {
    if payload.sample_rate != 48_000 || payload.channels != 2 {
        anyhow::bail!(
            "pcm clip must be 48k stereo, got {}Hz {}ch",
            payload.sample_rate,
            payload.channels
        );
    }
    let frames = payload.samples.len() / PCM_FRAME_SAMPLES_STEREO;
    Ok(std::time::Duration::from_millis(
        frames as u64 * PCM_FRAME_MS,
    ))
}

/// 出站总线：输出句柄 + 消费者（消费者唯一写 ts3_audio_tx）
pub struct AudioBus {
    pub output: AudioOutput,
    pub consumer: AudioOutputConsumer,
}

impl AudioBus {
    pub fn new() -> Self {
        let (job_tx, job_rx) = mpsc::channel(JOB_QUEUE_CAPACITY);
        let status = Arc::new(AudioOutputStatusInner::new());
        Self {
            output: AudioOutput {
                inner: Arc::new(AudioOutputInner {
                    job_tx,
                    status: status.clone(),
                }),
            },
            consumer: AudioOutputConsumer { job_rx, status },
        }
    }
}

impl AudioOutput {
    pub fn status(&self) -> AudioOutputStatus {
        self.inner.status.snapshot()
    }

    pub fn enqueue_pcm_clip(&self, pcm: PcmClipPayload) -> Result<ClipHandle> {
        let cancel = Arc::new(AtomicBool::new(false));
        self.enqueue_clip_job(pcm, cancel, true)
    }

    /// 用调用方提供的取消位入队 PCM 片段：回合持有的播放取消位（插话、中断）同时作用于该片段。
    ///
    /// 这类片段不登记进 `stop_clips` 的集合——那批取消位属于技能片段，
    /// 把本片段的取消位登记进去等于让「停止播放」连带取消本回合的 TTS 播放。
    pub fn enqueue_pcm_clip_with_cancel(
        &self,
        pcm: PcmClipPayload,
        cancel: Arc<AtomicBool>,
    ) -> Result<ClipHandle> {
        self.enqueue_clip_job(pcm, cancel, false)
    }

    fn enqueue_clip_job(
        &self,
        pcm: PcmClipPayload,
        cancel: Arc<AtomicBool>,
        skill_scoped: bool,
    ) -> Result<ClipHandle> {
        pcm_payload_duration(&pcm)?;
        let cancel_for_job = cancel.clone();
        let (done_tx, done_rx) = oneshot::channel();
        let status = &self.inner.status;
        status.enqueue_job(&self.inner.job_tx, |cancel_id| {
            if skill_scoped {
                status.register_clip_cancel(cancel_id, cancel_for_job.clone());
            }
            AudioJob::PcmClip {
                // 非 skill_scoped 的片段来自回合（反馈音等）而非技能：它不登记进
                // `stop_clips` 的取消集合，`JobSource` 只影响状态展示与编码流超时，故沿用 `Tts`
                source: if skill_scoped {
                    JobSource::SkillClip
                } else {
                    JobSource::Tts
                },
                payload: pcm,
                cancel: cancel_for_job,
                done: done_tx,
                cancel_id,
            }
        })?;
        Ok(ClipHandle {
            cancel,
            done: done_rx,
        })
    }

    /// open 即占 FIFO 槽；占位等待不消耗该 job 的 deadline
    pub async fn open_tts_session(&self) -> Result<TtsSession> {
        self.open_encoded_session(JobSource::Tts, Arc::new(AtomicBool::new(false)))
            .await
    }

    /// 用调用方提供的取消位开 TTS 会话：一轮对话里可以有多条流（工具轮 + 最终回复），
    /// 它们共用回合持有的同一个取消位，插话时一次全停
    pub async fn open_tts_session_with_cancel(
        &self,
        cancel: Arc<AtomicBool>,
    ) -> Result<TtsSession> {
        self.open_encoded_session(JobSource::Tts, cancel).await
    }

    async fn open_encoded_session(
        &self,
        source: JobSource,
        cancel: Arc<AtomicBool>,
    ) -> Result<TtsSession> {
        let (segment_tx, segment_rx) = mpsc::channel(TTS_SEGMENT_CAPACITY);
        let (done_tx, done_rx) = oneshot::channel();
        let cancel_for_job = cancel.clone();
        self.inner
            .status
            .enqueue_job(&self.inner.job_tx, |_| AudioJob::EncodedStream {
                source,
                segment_rx,
                cancel: cancel_for_job,
                done: done_tx,
            })?;
        Ok(TtsSession {
            segment_tx: Some(segment_tx),
            done_rx: Some(done_rx),
            cancel,
            finished: false,
        })
    }

    /// 外部一次性媒体：段入队即返回，不等待播完
    pub async fn play_encoded_media(&self, payload: Vec<u8>, codec: &str) -> Result<()> {
        let mut session = self
            .open_encoded_session(JobSource::External, Arc::new(AtomicBool::new(false)))
            .await?;
        let codec = if codec.is_empty() {
            detect_audio_format(&payload).to_string()
        } else {
            codec.to_string()
        };
        session.push_encoded(payload, &codec).await?;
        session.finish().await
    }

    /// 外部一次性媒体同步确认：等待消费者播完；仅供短音频/自检，长 TTS 勿用
    pub async fn play_encoded_media_wait(&self, payload: Vec<u8>, codec: &str) -> Result<()> {
        let mut session = self
            .open_encoded_session(JobSource::External, Arc::new(AtomicBool::new(false)))
            .await?;
        session.push_encoded(payload, codec).await?;
        session.finish_drained().await
    }

    /// 级联取消全部仍在队列/播放中的 SkillClip
    pub fn stop_clips(&self) -> usize {
        let mut map = self
            .inner
            .status
            .clip_cancels
            .lock()
            .expect("audio status poisoned");
        let mut stopped = 0usize;
        map.retain(|_, cancel| {
            if cancel.load(Ordering::SeqCst) {
                // 已取消/已完成标志：丢弃，避免陈旧条目
                return false;
            }
            cancel.store(true, Ordering::SeqCst);
            stopped += 1;
            // 消费者在 job 收尾时 unregister；此处保留条目直至收尾
            true
        });
        stopped
    }
}

impl AudioOutputConsumer {
    pub async fn run(self, ts3_audio_tx: mpsc::Sender<(Vec<u8>, i32)>) {
        let AudioOutputConsumer { mut job_rx, status } = self;
        while let Some(job) = job_rx.recv().await {
            let dequeue_at = Instant::now();
            status.dequeue_job();
            match job {
                AudioJob::PcmClip {
                    source,
                    payload,
                    cancel,
                    done,
                    cancel_id,
                } => {
                    status.unregister_clip_cancel(cancel_id);
                    if cancel.load(Ordering::SeqCst) {
                        let _ = done.send(Ok(()));
                        continue;
                    }
                    *status.current.lock().expect("audio status poisoned") = Some(CurrentJobInfo {
                        source,
                        kind: JobKind::PcmClip,
                        started_at: std::time::Instant::now(),
                    });
                    let result = play_pcm_clip(&payload, &cancel, dequeue_at, &ts3_audio_tx).await;
                    *status.current.lock().expect("audio status poisoned") = None;
                    if let Err(error) = &result {
                        *status.last_error.lock().expect("audio status poisoned") =
                            Some(error.to_string());
                    }
                    let mapped = result.map_err(|error| error.to_string());
                    let _ = done.send(mapped);
                }
                AudioJob::EncodedStream {
                    source,
                    segment_rx,
                    cancel,
                    done,
                } => {
                    if cancel.load(Ordering::SeqCst) {
                        let _ = done.send(Ok(()));
                        continue;
                    }
                    *status.current.lock().expect("audio status poisoned") = Some(CurrentJobInfo {
                        source,
                        kind: JobKind::EncodedStream,
                        started_at: std::time::Instant::now(),
                    });
                    let result =
                        play_encoded_stream(source, segment_rx, &cancel, dequeue_at, &ts3_audio_tx)
                            .await;
                    *status.current.lock().expect("audio status poisoned") = None;
                    if let Err(error) = &result {
                        *status.last_error.lock().expect("audio status poisoned") =
                            Some(error.to_string());
                    }
                    let mapped = result.map_err(|error| error.to_string());
                    let _ = done.send(mapped);
                }
            }
        }
    }
}

async fn play_pcm_clip(
    payload: &PcmClipPayload,
    cancel: &AtomicBool,
    dequeue_at: Instant,
    ts3_audio_tx: &mpsc::Sender<(Vec<u8>, i32)>,
) -> Result<()> {
    let pcm_duration = pcm_payload_duration(payload)?;
    let deadline = pcm_clip_deadline(dequeue_at, pcm_duration);
    let encoder = new_opus_stereo_encoder()?;
    let mut float_buf = vec![0f32; PCM_FRAME_SAMPLES_STEREO];
    let mut opus_out = [0u8; 1275];

    let mut offset = 0usize;
    while offset + PCM_FRAME_SAMPLES_STEREO <= payload.samples.len() {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(anyhow!("pcm clip deadline exceeded after dequeue"));
        }
        let frame = &payload.samples[offset..offset + PCM_FRAME_SAMPLES_STEREO];
        pcm_frame_to_float(frame, &mut float_buf);
        let len = encoder
            .encode_float(&float_buf, &mut opus_out)
            .map_err(|e| anyhow!("opus encode failed: {e}"))?;
        if ts3_audio_tx
            .send((opus_out[..len].to_vec(), 5))
            .await
            .is_err()
        {
            return Err(anyhow!("ts3 audio channel closed"));
        }
        offset += PCM_FRAME_SAMPLES_STEREO;
        // 单 pacer：生产者不 sleep，节奏由 actor 消费端按累计 20ms 发送；通道满则背压
    }
    Ok(())
}

/// EncodedStream 时限：TTS 无总上限（段间空闲兜底）；外部源 120s 总上限
fn encoded_idle_timeout(source: JobSource) -> std::time::Duration {
    match source {
        JobSource::Tts => std::time::Duration::from_secs(ENCODED_INTER_SEGMENT_IDLE_SECS),
        JobSource::External | JobSource::SkillClip => {
            std::time::Duration::from_secs(ENCODED_EXTERNAL_MAX_JOB_SECS)
        }
    }
}

fn encoded_total_deadline(source: JobSource, dequeue_at: Instant) -> Option<Instant> {
    match source {
        JobSource::Tts => None,
        JobSource::External | JobSource::SkillClip => {
            Some(dequeue_at + std::time::Duration::from_secs(ENCODED_EXTERNAL_MAX_JOB_SECS))
        }
    }
}

async fn play_encoded_stream(
    source: JobSource,
    mut segment_rx: mpsc::Receiver<EncodedSegment>,
    cancel: &AtomicBool,
    dequeue_at: Instant,
    ts3_audio_tx: &mpsc::Sender<(Vec<u8>, i32)>,
) -> Result<()> {
    let first_timeout = std::time::Duration::from_secs(ENCODED_FIRST_SEGMENT_TIMEOUT_SECS);
    let idle_timeout = encoded_idle_timeout(source);
    let total_deadline = encoded_total_deadline(source, dequeue_at);

    let first = match timeout(first_timeout, segment_rx.recv()).await {
        Ok(Some(segment)) => segment,
        Ok(None) => return Err(anyhow!("encoded stream closed before first segment")),
        Err(_) => {
            return Err(anyhow!(
                "encoded stream first segment timeout after dequeue"
            ))
        }
    };
    if cancel.load(Ordering::SeqCst) {
        return Ok(());
    }

    let mut segment = first;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(total) = total_deadline {
            if Instant::now() >= total {
                return Err(anyhow!(
                    "encoded stream job deadline exceeded after dequeue"
                ));
            }
        }
        if !segment.payload.is_empty() {
            match total_deadline {
                Some(total) => {
                    let remaining = total.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(anyhow!(
                            "encoded stream job deadline exceeded after dequeue"
                        ));
                    }
                    // 段处理期间也强制总截止时间，避免超大 payload 拖穿 MAX_JOB_SECS
                    match timeout(
                        remaining,
                        process_encoded_segment(&segment, cancel, ts3_audio_tx),
                    )
                    .await
                    {
                        Ok(result) => result?,
                        Err(_) => {
                            return Err(anyhow!(
                                "encoded stream job deadline exceeded after dequeue"
                            ))
                        }
                    }
                }
                None => process_encoded_segment(&segment, cancel, ts3_audio_tx).await?,
            }
        }
        let recv_timeout = match total_deadline {
            Some(total) => {
                let remaining = total.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(anyhow!(
                        "encoded stream job deadline exceeded after dequeue"
                    ));
                }
                remaining.min(idle_timeout)
            }
            None => idle_timeout,
        };
        match timeout(recv_timeout, segment_rx.recv()).await {
            Ok(Some(next)) => segment = next,
            Ok(None) => break,
            Err(_) => {
                if let Some(total) = total_deadline {
                    if Instant::now() >= total {
                        return Err(anyhow!(
                            "encoded stream job deadline exceeded after dequeue"
                        ));
                    }
                }
                return Err(anyhow!("encoded stream segment idle timeout after dequeue"));
            }
        }
    }
    Ok(())
}

struct ChildKillOnDrop {
    child: Option<Child>,
}

impl Drop for ChildKillOnDrop {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

/// 解码输入格式：wav 直给，mp3/空 codec 按文件头探测；其余 codec 不受支持
fn encoded_input_format(payload: &[u8], codec: &str) -> Option<&'static str> {
    if codec.eq_ignore_ascii_case("wav") {
        Some("wav")
    } else if codec.eq_ignore_ascii_case("mp3") || codec.is_empty() {
        Some(detect_audio_format(payload))
    } else {
        None
    }
}

/// 统一的解码命令：任意输入 → 48k 立体声 s16le，走 stdin/stdout 管道
fn ffmpeg_decode_to_s16le(input_format: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("ffmpeg");
    command
        .arg("-nostdin")
        .arg("-loglevel")
        .arg("error")
        .arg("-f")
        .arg(input_format)
        .arg("-i")
        .arg("pipe:0")
        .arg("-f")
        .arg("s16le")
        .arg("-ar")
        .arg("48000")
        .arg("-ac")
        .arg("2")
        .arg("pipe:1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// ffmpeg 的 stderr 逐行转日志；不读会让写满的管道阻塞解码
fn drain_ffmpeg_stderr(child: &mut ChildKillOnDrop) {
    if let Some(stderr) = child.child.as_mut().and_then(|c| c.stderr.take()) {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                warn!("audio_output ffmpeg: {line}");
            }
        });
    }
}

async fn process_encoded_segment(
    segment: &EncodedSegment,
    cancel: &AtomicBool,
    ts3_audio_tx: &mpsc::Sender<(Vec<u8>, i32)>,
) -> Result<()> {
    if segment.codec.starts_with("warmup-probe") {
        debug!(
            codec = %segment.codec,
            "audio output warmup probe skipped (expected)"
        );
        return Ok(());
    }
    let Some(input_format) = encoded_input_format(&segment.payload, &segment.codec) else {
        warn!("unsupported encoded codec: {}, skipping", segment.codec);
        return Ok(());
    };

    let child = ffmpeg_decode_to_s16le(input_format)
        .spawn()
        .context("failed to start ffmpeg")?;

    let mut child = ChildKillOnDrop { child: Some(child) };
    drain_ffmpeg_stderr(&mut child);

    let mut stdin = child
        .child
        .as_mut()
        .and_then(|c| c.stdin.take())
        .ok_or_else(|| anyhow!("ffmpeg stdin missing"))?;
    let mut stdout = child
        .child
        .as_mut()
        .and_then(|c| c.stdout.take())
        .ok_or_else(|| anyhow!("ffmpeg stdout missing"))?;

    let payload = segment.payload.clone();
    let stdin_task = tokio::spawn(async move {
        let result = stdin.write_all(&payload).await;
        drop(stdin);
        result
    });

    let encoder = new_opus_stereo_encoder()?;
    let mut pcm = vec![0u8; PCM_FRAME_SAMPLES_STEREO * 2];
    let mut float_buf = vec![0f32; PCM_FRAME_SAMPLES_STEREO];
    let mut opus_out = [0u8; 1275];

    loop {
        // 逐帧检查取消：插话不必等当前段（整句）播完
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        match stdout.read_exact(&mut pcm).await {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(anyhow!("read ffmpeg pcm failed: {e}")),
        }
        for (i, item) in float_buf.iter_mut().enumerate() {
            let lo = pcm[i * 2];
            let hi = pcm[i * 2 + 1];
            *item = i16::from_le_bytes([lo, hi]) as f32 / 32768.0;
        }
        let len = encoder
            .encode_float(&float_buf, &mut opus_out)
            .map_err(|e| anyhow!("opus encode failed: {e}"))?;
        if ts3_audio_tx
            .send((opus_out[..len].to_vec(), 5))
            .await
            .is_err()
        {
            return Err(anyhow!("ts3 audio channel closed"));
        }
    }

    if let Some(mut c) = child.child.take() {
        let _ = c.start_kill();
        let _ = c.wait().await;
    }
    match stdin_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("write ffmpeg stdin failed: {e}"),
        Err(e) => warn!("ffmpeg stdin task failed: {e}"),
    }
    Ok(())
}

/// 编码音频整段解码为 48k 立体声 PCM，供启动期预热短反馈音频用。
///
/// 播放热路径不经过这里：预热后的片段以 `PcmClipPayload` 入队，由进程内的 opus 编码器送出，
/// 不再触发 ffmpeg 进程。整帧读取，尾部不足一帧的采样丢弃（与播放路径一致）。
pub async fn decode_to_pcm_48k_stereo(payload: &[u8], codec: &str) -> Result<Vec<i16>> {
    let input_format = encoded_input_format(payload, codec)
        .ok_or_else(|| anyhow!("unsupported audio codec for pcm decode: {codec}"))?;
    let child = ffmpeg_decode_to_s16le(input_format)
        .spawn()
        .context("failed to start ffmpeg")?;
    let mut child = ChildKillOnDrop { child: Some(child) };
    drain_ffmpeg_stderr(&mut child);

    let mut stdin = child
        .child
        .as_mut()
        .and_then(|c| c.stdin.take())
        .ok_or_else(|| anyhow!("ffmpeg stdin missing"))?;
    let mut stdout = child
        .child
        .as_mut()
        .and_then(|c| c.stdout.take())
        .ok_or_else(|| anyhow!("ffmpeg stdout missing"))?;

    let payload = payload.to_vec();
    let stdin_task = tokio::spawn(async move {
        let result = stdin.write_all(&payload).await;
        drop(stdin);
        result
    });

    let mut raw = Vec::new();
    let mut frame = vec![0u8; PCM_FRAME_SAMPLES_STEREO * 2];
    let read_result = loop {
        match stdout.read(&mut frame).await {
            Ok(0) => break Ok(()),
            Ok(read) => raw.extend_from_slice(&frame[..read]),
            Err(e) => break Err(anyhow!("read ffmpeg pcm failed: {e}")),
        }
    };

    if let Some(mut c) = child.child.take() {
        let _ = c.start_kill();
        let _ = c.wait().await;
    }
    match stdin_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("write ffmpeg stdin failed: {e}"),
        Err(e) => warn!("ffmpeg stdin task failed: {e}"),
    }
    read_result?;

    Ok(raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::sleep;

    fn silence_clip(ms: u64) -> PcmClipPayload {
        let samples = vec![0i16; (ms as usize) * 96];
        PcmClipPayload {
            samples,
            sample_rate: 48_000,
            channels: 2,
        }
    }

    #[test]
    fn pcm_clip_deadline_counts_from_dequeue_not_enqueue() {
        let base = tokio::time::Instant::now();
        let enqueue_at = base;
        let dequeue_at = base + Duration::from_secs(60);
        let deadline = pcm_clip_deadline(dequeue_at, Duration::from_millis(100));
        assert!(deadline >= dequeue_at + Duration::from_secs(30));
        // 若误从 enqueue 起算，deadline 会在 dequeue 前到期
        assert!(deadline > enqueue_at + Duration::from_secs(50));
    }

    #[test]
    fn pcm_payload_rejects_wrong_format() {
        let payload = PcmClipPayload {
            samples: vec![0; 96],
            sample_rate: 44_100,
            channels: 2,
        };
        assert!(pcm_payload_duration(&payload).is_err());
    }

    #[tokio::test]
    async fn try_send_full_rejects_enqueue() {
        let bus = AudioBus::new();
        let output = bus.output;
        let consumer = bus.consumer;
        // 不启动消费者，占满 FIFO
        for _ in 0..JOB_QUEUE_CAPACITY {
            output.enqueue_pcm_clip(silence_clip(20)).unwrap();
        }
        assert!(output.enqueue_pcm_clip(silence_clip(20)).is_err());
        assert_eq!(output.status().queued_jobs, JOB_QUEUE_CAPACITY);
        drop(consumer);
    }

    #[tokio::test(start_paused = true)]
    async fn long_prefix_job_does_not_expire_queued_clip() {
        let bus = AudioBus::new();
        let output = bus.output;
        let consumer = bus.consumer;
        let (audio_tx, mut audio_rx) = mpsc::channel::<(Vec<u8>, i32)>(4096);
        tokio::spawn(consumer.run(audio_tx));

        // 前置长 clip（约 35s）占住消费者；排队 clip 的 deadline 必须自 dequeue 起算
        let long = output.enqueue_pcm_clip(silence_clip(35_000)).unwrap();
        let queued = output.enqueue_pcm_clip(silence_clip(100)).unwrap();

        tokio::spawn(async move { while audio_rx.recv().await.is_some() {} });

        long.wait().await.expect("long prefix clip must finish");
        queued
            .wait()
            .await
            .expect("queued clip must not be killed by prefix job");
        assert_eq!(output.status().queued_jobs, 0);
        assert!(output.status().current.is_none());
    }

    #[tokio::test]
    async fn status_inner_updates_across_job_lifetime() {
        let bus = AudioBus::new();
        let output = bus.output;
        let consumer = bus.consumer;
        let (audio_tx, mut audio_rx) = mpsc::channel::<(Vec<u8>, i32)>(4096);
        tokio::spawn(consumer.run(audio_tx));

        assert!(output.status().current.is_none());
        let handle = output
            .enqueue_pcm_clip(silence_clip(40))
            .expect("enqueue clip");
        assert!(output.status().queued_jobs <= 1);

        let drain = tokio::spawn(async move { while audio_rx.recv().await.is_some() {} });
        tokio::time::timeout(Duration::from_secs(5), handle.wait())
            .await
            .expect("clip wait must finish")
            .expect("clip must succeed");
        let after = output.status();
        assert!(after.current.is_none());
        assert_eq!(after.queued_jobs, 0);
        assert!(after.last_error.is_none());
        if let Some(info) = after.current {
            let _ = (info.source, info.kind, info.started_at);
        }
        drop(output);
        let _ = tokio::time::timeout(Duration::from_millis(200), drain).await;
    }

    #[tokio::test]
    async fn stop_clips_cancels_queued_skill_clips() {
        let bus = AudioBus::new();
        let output = bus.output;
        let consumer = bus.consumer;
        let (audio_tx, _audio_rx) = mpsc::channel::<(Vec<u8>, i32)>(8);
        // 消费者不启动，clip 停在队列
        let _consumer = consumer;
        let h1 = output.enqueue_pcm_clip(silence_clip(100)).unwrap();
        let h2 = output.enqueue_pcm_clip(silence_clip(100)).unwrap();
        let cancelled = output.stop_clips();
        assert_eq!(cancelled, 2);
        drop(audio_tx);
        drop(h1);
        drop(h2);
    }

    #[tokio::test]
    async fn clip_cancel_registry_does_not_accumulate_finished_jobs() {
        let bus = AudioBus::new();
        let output = bus.output.clone();
        let (audio_tx, mut audio_rx) = mpsc::channel::<(Vec<u8>, i32)>(256);
        tokio::spawn(bus.consumer.run(audio_tx));
        tokio::spawn(async move { while audio_rx.recv().await.is_some() {} });

        for _ in 0..5 {
            let handle = output.enqueue_pcm_clip(silence_clip(20)).unwrap();
            handle.wait().await.unwrap();
        }
        assert_eq!(output.stop_clips(), 0);
        let map_len = output
            .inner
            .status
            .clip_cancels
            .lock()
            .expect("status poisoned")
            .len();
        assert_eq!(map_len, 0);
    }

    #[tokio::test]
    async fn queued_jobs_counter_does_not_underflow_under_race() {
        let bus = AudioBus::new();
        let output = bus.output.clone();
        let (audio_tx, mut audio_rx) = mpsc::channel::<(Vec<u8>, i32)>(1024);
        tokio::spawn(bus.consumer.run(audio_tx));
        tokio::spawn(async move { while audio_rx.recv().await.is_some() {} });

        for _ in 0..32 {
            let h = output.enqueue_pcm_clip(silence_clip(20)).unwrap();
            let _ = h.wait().await;
        }
        assert_eq!(output.status().queued_jobs, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn tts_session_drop_aborts_job() {
        let bus = AudioBus::new();
        let output = bus.output;
        let consumer = bus.consumer;
        let (audio_tx, mut audio_rx) = mpsc::channel::<(Vec<u8>, i32)>(64);
        tokio::spawn(consumer.run(audio_tx));
        tokio::spawn(async move { while audio_rx.recv().await.is_some() {} });

        let session = output.open_tts_session().await.unwrap();
        drop(session); // 未 finish → abort
        sleep(Duration::from_millis(50)).await;
        // 消费者应处理掉被取消的 job，队列清空
        assert_eq!(output.status().queued_jobs, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn encoded_first_segment_timeout_starts_after_dequeue() {
        let bus = AudioBus::new();
        let output = bus.output;
        let consumer = bus.consumer;
        let (audio_tx, mut audio_rx) = mpsc::channel::<(Vec<u8>, i32)>(64);
        tokio::spawn(consumer.run(audio_tx));
        tokio::spawn(async move { while audio_rx.recv().await.is_some() {} });

        // 先占一个会立刻完成的短 clip，再 open 无段 session
        let clip = output.enqueue_pcm_clip(silence_clip(20)).unwrap();
        let session = output.open_tts_session().await.unwrap();
        clip.wait().await.unwrap();
        // 保持 session 打开：dequeue 后首段超时计入 last_error
        sleep(Duration::from_secs(20)).await;
        let err = output
            .status()
            .last_error
            .expect("first segment timeout after dequeue");
        assert!(err.contains("first segment timeout") || err.contains("idle timeout"));
        session.finish().await.expect("accepted finish");
    }

    #[tokio::test(start_paused = true)]
    async fn tts_session_finish_is_accepted_not_playback_wait() {
        let bus = AudioBus::new();
        let output = bus.output;
        let consumer = bus.consumer;
        let (audio_tx, mut audio_rx) = mpsc::channel::<(Vec<u8>, i32)>(4096);
        tokio::spawn(consumer.run(audio_tx));
        tokio::spawn(async move { while audio_rx.recv().await.is_some() {} });

        let mut session = output.open_tts_session().await.unwrap();
        session
            .push_encoded(vec![0xff, 0xfb, 0x90, 0x00], "mp3")
            .await
            .unwrap();
        session.finish().await.expect("accepted finish");
    }

    #[tokio::test(start_paused = true)]
    async fn tts_session_finish_does_not_cancel_queued_segments() {
        let bus = AudioBus::new();
        let output = bus.output;
        let consumer = bus.consumer;
        let (audio_tx, mut audio_rx) = mpsc::channel::<(Vec<u8>, i32)>(64);
        tokio::spawn(consumer.run(audio_tx));
        tokio::spawn(async move { while audio_rx.recv().await.is_some() {} });

        let mut session = output.open_tts_session().await.unwrap();
        let cancel = session.cancel.clone();
        session
            .push_encoded(vec![0u8; 2], "warmup-probe")
            .await
            .unwrap();
        session.finish().await.expect("accepted finish");
        assert!(
            !cancel.load(Ordering::SeqCst),
            "finish must not cancel queued segments"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn tts_session_finish_drained_waits_for_queued_segments() {
        let bus = AudioBus::new();
        let output = bus.output.clone();
        let consumer = bus.consumer;
        let (audio_tx, mut audio_rx) = mpsc::channel::<(Vec<u8>, i32)>(64);
        tokio::spawn(consumer.run(audio_tx));
        tokio::spawn(async move { while audio_rx.recv().await.is_some() {} });

        let mut session = output.open_tts_session().await.unwrap();
        let cancel = session.cancel.clone();
        session
            .push_encoded(vec![0u8; 2], "warmup-probe")
            .await
            .unwrap();
        session
            .finish_drained()
            .await
            .expect("drain wait must end when the job completes");

        assert!(!cancel.load(Ordering::SeqCst));
        assert_eq!(output.status().queued_jobs, 0);
        assert!(output.status().current.is_none());
        assert!(output.status().last_error.is_none());
    }

    #[test]
    fn tts_source_has_no_total_deadline_but_external_does() {
        let dequeue = Instant::now();
        // TTS 源无总时长上限（由 LLM 流 STREAM_TOTAL=300s / idle=30s 间接收敛）；
        // 外部源保留 dequeue 起算 120s。超长 TTS 不被本层砍断。
        assert!(encoded_total_deadline(JobSource::Tts, dequeue).is_none());
        let external =
            encoded_total_deadline(JobSource::External, dequeue).expect("external total deadline");
        assert!(external > dequeue);
        assert_eq!(
            encoded_idle_timeout(JobSource::Tts),
            Duration::from_secs(ENCODED_INTER_SEGMENT_IDLE_SECS)
        );
    }

    #[tokio::test]
    async fn enqueue_rejects_empty_queue_after_drop_consumer() {
        let bus = AudioBus::new();
        let output = bus.output;
        let consumer = bus.consumer;
        drop(consumer);
        assert!(output.enqueue_pcm_clip(silence_clip(20)).is_err());
    }

    #[tokio::test]
    async fn warmup_probe_codec_does_not_warn_path() {
        // 构造路径：unsupported warmup-probe 走 debug，不进 last_error
        let bus = AudioBus::new();
        let output = bus.output.clone();
        let (audio_tx, _rx) = mpsc::channel::<(Vec<u8>, i32)>(8);
        tokio::spawn(bus.consumer.run(audio_tx));
        assert!(output
            .play_encoded_media(vec![0u8; 2], "warmup-probe")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn play_encoded_media_skips_unknown_codec() {
        let bus = AudioBus::new();
        let output = bus.output;
        let consumer = bus.consumer;
        let (audio_tx, _rx) = mpsc::channel::<(Vec<u8>, i32)>(8);
        tokio::spawn(consumer.run(audio_tx));
        let result = output.play_encoded_media(vec![0u8; 4], "not-a-codec").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn play_encoded_media_without_ffmpeg_records_error() {
        // 无 ffmpeg 时 EncodedStream 应失败并写入 last_error，而非 panic
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            let bus = AudioBus::new();
            let output = bus.output;
            let consumer = bus.consumer;
            let (audio_tx, _rx) = mpsc::channel::<(Vec<u8>, i32)>(8);
            tokio::spawn(consumer.run(audio_tx));
            let result = output
                .play_encoded_media_wait(vec![1, 2, 3, 4], "mp3")
                .await;
            assert!(result.is_err());
            assert!(output.status().last_error.is_some());
        }
    }

    fn ffmpeg_available() -> bool {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    /// 预热路径：编码音频整段解码为 48k 立体声 PCM（16k 单声道 wav ×3 倍采样 ×2 声道）
    #[tokio::test]
    async fn decode_to_pcm_resamples_encoded_audio_to_48k_stereo() {
        if !ffmpeg_available() {
            return;
        }
        let mono_16k = vec![0i16; 1_600]; // 100ms
        let wav = crate::adapter::headless::speech::pcm16_mono_to_wav_bytes(&mono_16k, 16_000);

        let pcm = decode_to_pcm_48k_stereo(&wav, "wav")
            .await
            .expect("wav decode must succeed");
        // 4800 采样/声道（100ms @48k）× 2 声道 = 9600；重采样边界可能有少量出入
        assert!(
            pcm.len() >= 9_000 && pcm.len() <= 9_600,
            "unexpected pcm length {}",
            pcm.len()
        );
        assert_eq!(pcm.len() % 2, 0);
    }

    #[tokio::test]
    async fn decode_to_pcm_rejects_unsupported_codec() {
        let error = decode_to_pcm_48k_stereo(&[0u8; 4], "not-a-codec")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unsupported audio codec"));
    }

    /// 调用方提供的取消位直接作用于该片段：回合插话即停声
    #[tokio::test(start_paused = true)]
    async fn injected_cancel_stops_a_turn_scoped_clip() {
        let bus = AudioBus::new();
        let output = bus.output;
        let consumer = bus.consumer;
        let (audio_tx, _audio_rx) = mpsc::channel::<(Vec<u8>, i32)>(4096);
        tokio::spawn(consumer.run(audio_tx));

        let playback_cancel = Arc::new(AtomicBool::new(false));
        let handle = output
            .enqueue_pcm_clip_with_cancel(silence_clip(20_000), playback_cancel.clone())
            .expect("enqueue clip");
        // 排队中即置位：消费者 dequeue 时直接收尾，不播一帧
        playback_cancel.store(true, Ordering::SeqCst);
        handle.wait().await.expect("cancelled clip finishes");

        assert_eq!(output.stop_clips(), 0, "回合片段不进技能片段的取消集合");
    }

    /// 回合片段不登记到 stop_clips：停止技能片段不得连带取消本回合的 TTS 播放
    #[tokio::test(start_paused = true)]
    async fn turn_scoped_clip_is_not_registered_for_stop_clips() {
        let bus = AudioBus::new();
        let output = bus.output;
        let _consumer = bus.consumer;
        let playback_cancel = Arc::new(AtomicBool::new(false));
        let _queued = output
            .enqueue_pcm_clip_with_cancel(silence_clip(100), playback_cancel.clone())
            .expect("enqueue clip");

        assert_eq!(output.stop_clips(), 0);
        assert!(
            !playback_cancel.load(Ordering::SeqCst),
            "stop_clips must not cancel the turn playback flag"
        );
    }
}

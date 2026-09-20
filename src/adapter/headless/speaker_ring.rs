//! 分说话人环形录制：双时钟（墙钟留存/驱逐/断 run；播放时钟 run 内链式续写）。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tracing::warn;

use super::audio_codec::{
    new_opus_stereo_decoder, pcm_stereo_48k_duration, stereo_48k_sample_count, CHANNELS,
    SAMPLE_RATE_HZ,
};

const MAX_TRACKED_SPEAKERS: usize = 6;
/// 断 run：墙钟距上一帧超过该值则开新 run（取代未登记的 MERGE_GAP=2ms）
const RUN_BREAK: Duration = Duration::from_millis(200);
/// 单 run 内存上限（毫秒）；强制切段时按播放时钟链式续写
const MAX_SEGMENT_MS: u64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayFilter {
    All,
    Speaker { clid: u32 },
}

#[derive(Debug, Clone)]
pub struct SpeakerStat {
    pub clid: u32,
    pub name: String,
    pub active_ms: u64,
}

#[derive(Debug, Clone)]
pub struct RingSnapshot {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u16,
    pub speakers: Vec<SpeakerStat>,
    pub buffered_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameResolve {
    Unique(u32),
    Ambiguous(Vec<String>),
    None,
}

pub struct SpeakerSegment {
    pub start: Instant,
    pub samples: Vec<i16>,
}

pub struct SpeakerTrack {
    pub clid: u32,
    pub name: String,
    pub last_active: Instant,
    /// 上一帧入环墙钟；驱逐不清除
    pub last_frame_at: Option<Instant>,
    pub segments: VecDeque<SpeakerSegment>,
    pub decoder: audiopus::coder::Decoder,
}

struct Inner {
    tracks: HashMap<u32, SpeakerTrack>,
    window: Duration,
    max_tracked_speakers: usize,
    created_at: Instant,
}

pub struct SpeakerRings {
    inner: Mutex<Inner>,
}

fn stereo_48k_duration(sample_count: usize) -> Duration {
    pcm_stereo_48k_duration(sample_count)
}

/// active_ms：段区间与 [now-window, now] 求交后累加；墙钟语义
fn derive_active_ms(track: &SpeakerTrack, now: Instant, window: Duration) -> u64 {
    let window_start = now - window;
    track
        .segments
        .iter()
        .map(|seg| {
            let segment_end = seg.start + stereo_48k_duration(seg.samples.len());
            let clipped_start = seg.start.max(window_start);
            let clipped_end = segment_end.min(now);
            clipped_end
                .saturating_duration_since(clipped_start)
                .as_millis() as u64
        })
        .sum()
}

/// P1-2 回溯轴长：min(window, ring_age, request_N)
fn axis_ms(inner: &Inner, now: Instant, seconds: Option<u32>) -> u64 {
    let window_ms = inner.window.as_millis() as u64;
    let age_ms = now.duration_since(inner.created_at).as_millis() as u64;
    let mut axis = window_ms.min(age_ms);
    if let Some(secs) = seconds {
        axis = axis.min(u64::from(secs) * 1000);
    }
    axis
}

/// soft-clip：范围内透传；超限 tanh 渐近饱和
pub fn soft_clip_i32_to_i16(sum: i32) -> i16 {
    const MAX: f32 = i16::MAX as f32;
    let x = sum as f32;
    if x.abs() <= MAX {
        return sum as i16;
    }
    let y = MAX * (x / MAX).tanh();
    y.clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

impl SpeakerRings {
    pub fn new(window: Duration) -> Self {
        Self::new_with_created_at(window, Instant::now())
    }

    fn new_with_created_at(window: Duration, created_at: Instant) -> Self {
        Self {
            inner: Mutex::new(Inner {
                tracks: HashMap::new(),
                window,
                max_tracked_speakers: MAX_TRACKED_SPEAKERS,
                created_at,
            }),
        }
    }

    pub fn push_opus_frame(
        &self,
        clid: u32,
        name: &str,
        codec: i32,
        frame: &[u8],
        received_at: Instant,
    ) -> Result<()> {
        let pcm = self.decode_opus_frame(clid, name, codec, frame, received_at)?;
        if let Some(samples) = pcm {
            self.append_pcm(clid, name, samples, received_at);
        }
        Ok(())
    }

    fn decode_opus_frame(
        &self,
        clid: u32,
        name: &str,
        codec: i32,
        frame: &[u8],
        received_at: Instant,
    ) -> Result<Option<Vec<i16>>> {
        // TS 语音流 1 字节包是控制/边界标记，不入 run、不重置 last_frame_at
        if frame.len() <= 1 {
            return Ok(None);
        }

        let mut inner = self.inner.lock().expect("speaker rings poisoned");
        Self::ensure_track(&mut inner, clid, name, received_at);
        let track = inner
            .tracks
            .get_mut(&clid)
            .ok_or_else(|| anyhow!("speaker track missing after ensure"))?;

        let mut decoded = vec![0i16; 5760 * CHANNELS as usize];
        let packet = match frame.try_into() {
            Ok(packet) => packet,
            Err(error) => {
                warn!(clid, codec, len = frame.len(), error = %error, "speaker ring drop invalid opus packet");
                return Ok(None);
            }
        };
        let decoded_mut = (&mut decoded)
            .try_into()
            .map_err(|e: audiopus::Error| anyhow!("opus output buffer invalid: {e}"))?;
        let samples_per_channel = match track.decoder.decode(Some(packet), decoded_mut, false) {
            Ok(n) => n,
            Err(error) => {
                warn!(clid, codec, len = frame.len(), error = %error, "speaker ring drop undecodable opus frame");
                return Ok(None);
            }
        };
        if samples_per_channel == 0 {
            return Ok(None);
        }
        Ok(Some(
            decoded[..samples_per_channel * CHANNELS as usize].to_vec(),
        ))
    }

    /// 写入路径：建轨 / run 链式续写或断 run / 墙钟驱逐
    fn append_pcm(&self, clid: u32, name: &str, samples: Vec<i16>, received_at: Instant) {
        let mut inner = self.inner.lock().expect("speaker rings poisoned");
        Self::ensure_track(&mut inner, clid, name, received_at);
        let track = inner
            .tracks
            .get_mut(&clid)
            .expect("speaker track missing after ensure");
        Self::append_samples(track, samples, received_at);
        Self::evict_locked(&mut inner, received_at);
    }

    fn ensure_track(inner: &mut Inner, clid: u32, name: &str, now: Instant) {
        if let Some(track) = inner.tracks.get_mut(&clid) {
            track.name = name.to_string();
            track.last_active = now;
            return;
        }

        if inner.tracks.len() >= inner.max_tracked_speakers {
            let victim = inner
                .tracks
                .iter()
                .filter(|(id, _)| **id != clid)
                .min_by_key(|(_, track)| track.last_active)
                .map(|(id, _)| *id);
            if let Some(dropped) = victim {
                if let Some(track) = inner.tracks.remove(&dropped) {
                    warn!(
                        clid = dropped,
                        name = %track.name,
                        "speaker ring LRU evicted track"
                    );
                }
            }
        }

        let decoder = new_opus_stereo_decoder().expect("opus stereo decoder init");
        inner.tracks.insert(
            clid,
            SpeakerTrack {
                clid,
                name: name.to_string(),
                last_active: now,
                last_frame_at: None,
                segments: VecDeque::new(),
                decoder,
            },
        );
    }

    fn append_samples(track: &mut SpeakerTrack, samples: Vec<i16>, at: Instant) {
        if samples.is_empty() {
            return;
        }
        track.last_active = at;
        let same_run = matches!(
            track.last_frame_at,
            Some(prev) if at.saturating_duration_since(prev) <= RUN_BREAK
        );

        if same_run {
            if let Some(last) = track.segments.back_mut() {
                let last_ms = stereo_48k_duration(last.samples.len()).as_millis() as u64;
                if last_ms < MAX_SEGMENT_MS {
                    last.samples.extend_from_slice(&samples);
                    track.last_frame_at = Some(at);
                    return;
                }
                // 内存上限强制切段：播放时钟链式，不回到墙钟
                let next_start = last.start + stereo_48k_duration(last.samples.len());
                track.segments.push_back(SpeakerSegment {
                    start: next_start,
                    samples,
                });
                track.last_frame_at = Some(at);
                return;
            }
            // 同 run 但段已被驱逐：退化为以 received_at 起新段
        }

        track
            .segments
            .push_back(SpeakerSegment { start: at, samples });
        track.last_frame_at = Some(at);
    }

    /// 驱逐只删段，不清 last_frame_at / run 追加状态
    fn evict_locked(inner: &mut Inner, now: Instant) {
        let window = inner.window;
        for track in inner.tracks.values_mut() {
            while let Some(seg) = track.segments.front() {
                let end = seg.start + stereo_48k_duration(seg.samples.len());
                if end < now - window {
                    track.segments.pop_front();
                } else {
                    break;
                }
            }
        }
        inner.tracks.retain(|_, track| {
            !track.segments.is_empty() || now.duration_since(track.last_active) < window
        });
    }

    pub fn snapshot(&self, filter: ReplayFilter, seconds: Option<u32>) -> RingSnapshot {
        self.snapshot_at(filter, seconds, Instant::now())
    }

    pub fn snapshot_at(
        &self,
        filter: ReplayFilter,
        seconds: Option<u32>,
        now: Instant,
    ) -> RingSnapshot {
        let mut inner = self.inner.lock().expect("speaker rings poisoned");
        Self::evict_locked(&mut inner, now);
        let axis_len = axis_ms(&inner, now, seconds);
        let axis_start = now - Duration::from_millis(axis_len);
        let window_start = now - inner.window;

        // 轴尾：窗内 run 的播放终点越过 now 时扩展（P1-2 受控放宽）
        let mut playback_end = now;
        let selected: Vec<&SpeakerTrack> = match filter {
            ReplayFilter::All => inner.tracks.values().collect(),
            ReplayFilter::Speaker { clid } => inner.tracks.get(&clid).into_iter().collect(),
        };
        for track in &selected {
            for seg in &track.segments {
                if seg.start < window_start {
                    continue;
                }
                let end = seg.start + stereo_48k_duration(seg.samples.len());
                if end > playback_end {
                    playback_end = end;
                }
            }
        }
        let axis_end = playback_end.max(now);
        let snapshot_span = axis_end.saturating_duration_since(axis_start);
        let sample_count = stereo_48k_sample_count(snapshot_span);
        let mut samples = vec![0i16; sample_count];

        for track in &selected {
            mix_track_into(&mut samples, track, axis_start);
        }

        let speakers = inner
            .tracks
            .values()
            .map(|track| SpeakerStat {
                clid: track.clid,
                name: track.name.clone(),
                active_ms: derive_active_ms(track, now, inner.window),
            })
            .collect();

        let buffered_ms = sample_count as u64 * 1000 / (SAMPLE_RATE_HZ as u64 * CHANNELS as u64);
        RingSnapshot {
            buffered_ms,
            samples,
            sample_rate: SAMPLE_RATE_HZ,
            channels: CHANNELS,
            speakers,
        }
    }

    pub fn stats(&self) -> Vec<SpeakerStat> {
        self.stats_at(Instant::now())
    }

    pub fn stats_at(&self, now: Instant) -> Vec<SpeakerStat> {
        let mut inner = self.inner.lock().expect("speaker rings poisoned");
        Self::evict_locked(&mut inner, now);
        let window = inner.window;
        inner
            .tracks
            .values()
            .map(|track| SpeakerStat {
                clid: track.clid,
                name: track.name.clone(),
                active_ms: derive_active_ms(track, now, window),
            })
            .collect()
    }

    /// 精确匹配；0 个 → None；多个同名 → Ambiguous
    pub fn resolve_name(&self, name: &str) -> NameResolve {
        let inner = self.inner.lock().expect("speaker rings poisoned");
        let matches: Vec<&SpeakerTrack> = inner
            .tracks
            .values()
            .filter(|track| track.name == name)
            .collect();
        match matches.len() {
            0 => NameResolve::None,
            1 => NameResolve::Unique(matches[0].clid),
            _ => NameResolve::Ambiguous(matches.iter().map(|t| t.name.clone()).collect()),
        }
    }

    #[cfg(test)]
    fn segment_count(&self, clid: u32) -> usize {
        let inner = self.inner.lock().expect("speaker rings poisoned");
        inner
            .tracks
            .get(&clid)
            .map(|track| track.segments.len())
            .unwrap_or(0)
    }
}

fn mix_track_into(out: &mut [i16], track: &SpeakerTrack, axis_start: Instant) {
    let mut write_end = 0usize;
    for seg in &track.segments {
        let (dest_idx, samples) = if seg.start >= axis_start {
            let offset = seg.start.saturating_duration_since(axis_start);
            (stereo_48k_sample_count(offset), seg.samples.as_slice())
        } else {
            let skip = stereo_48k_sample_count(axis_start - seg.start);
            if skip >= seg.samples.len() {
                continue;
            }
            (0usize, &seg.samples[skip..])
        };
        // 同 track 重叠：debug 断言；release 相加 + soft-clip
        if dest_idx < write_end {
            debug_assert!(
                dest_idx >= write_end,
                "speaker ring same-track segment overlap dest={dest_idx} write_end={write_end}"
            );
        }
        for (i, sample) in samples.iter().enumerate() {
            let idx = dest_idx + i;
            if idx >= out.len() {
                break;
            }
            let sum = i32::from(out[idx]) + i32::from(*sample);
            out[idx] = if sum.abs() > i32::from(i16::MAX) {
                soft_clip_i32_to_i16(sum)
            } else {
                sum as i16
            };
            write_end = idx + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::headless::audio_codec::stereo_48k_sample_count;
    use std::time::Duration;

    const DEFAULT_REPLAY_WINDOW: Duration = Duration::from_secs(30);

    fn pcm_ms(ms: u64) -> Vec<i16> {
        vec![100i16; stereo_48k_sample_count(Duration::from_millis(ms))]
    }

    fn pcm_ms_val(ms: u64, val: i16) -> Vec<i16> {
        vec![val; stereo_48k_sample_count(Duration::from_millis(ms))]
    }

    fn count_value(samples: &[i16], val: i16) -> usize {
        samples.iter().filter(|s| **s == val).count()
    }

    #[test]
    fn one_byte_voice_marker_is_skipped_without_decode() {
        let rings = SpeakerRings::new(Duration::from_secs(30));
        rings
            .push_opus_frame(1, "alice", 5, &[0x02], Instant::now())
            .unwrap();
        rings
            .push_opus_frame(1, "alice", 5, &[0x07], Instant::now())
            .unwrap();
        assert_eq!(rings.segment_count(1), 0);
    }

    #[test]
    fn one_byte_marker_mid_run_does_not_break_run() {
        let rings = SpeakerRings::new(DEFAULT_REPLAY_WINDOW);
        let t0 = Instant::now();
        rings.append_pcm(1, "alice", pcm_ms(20), t0);
        rings
            .push_opus_frame(1, "alice", 5, &[0x02], t0 + Duration::from_millis(10))
            .unwrap();
        rings.append_pcm(1, "alice", pcm_ms(20), t0 + Duration::from_millis(20));
        assert_eq!(rings.segment_count(1), 1);
    }

    #[test]
    fn run_break_boundary_199ms_same_run_201ms_new_run() {
        let rings = SpeakerRings::new(DEFAULT_REPLAY_WINDOW);
        let t0 = Instant::now();
        rings.append_pcm(1, "alice", pcm_ms(20), t0);
        rings.append_pcm(1, "alice", pcm_ms(20), t0 + Duration::from_millis(199));
        assert_eq!(rings.segment_count(1), 1);

        let rings2 = SpeakerRings::new(DEFAULT_REPLAY_WINDOW);
        rings2.append_pcm(1, "alice", pcm_ms(20), t0);
        rings2.append_pcm(1, "alice", pcm_ms(20), t0 + Duration::from_millis(201));
        assert_eq!(rings2.segment_count(1), 2);
    }

    #[test]
    fn jitter_40ms_stays_one_run_without_micro_gaps() {
        let created = Instant::now() - Duration::from_secs(60);
        let rings = SpeakerRings::new_with_created_at(DEFAULT_REPLAY_WINDOW, created);
        let now = Instant::now();
        // 连续 5 帧，到达间隔 40ms（抖动），每帧 20ms 音频
        let base = now - Duration::from_millis(250);
        for i in 0..5u64 {
            rings.append_pcm(1, "alice", pcm_ms(20), base + Duration::from_millis(i * 40));
        }
        assert_eq!(rings.segment_count(1), 1);
        let snap = rings.snapshot_at(ReplayFilter::Speaker { clid: 1 }, None, now);
        // run 起点 base，链式 100ms 连续样本，中间无 0
        let start_ms = base
            .saturating_duration_since(now - Duration::from_secs(30))
            .as_millis() as u64;
        let begin = stereo_48k_sample_count(Duration::from_millis(start_ms));
        let span = stereo_48k_sample_count(Duration::from_millis(100));
        let window =
            &snap.samples[begin.min(snap.samples.len())..(begin + span).min(snap.samples.len())];
        assert_eq!(count_value(window, 100), span);
        assert_eq!(count_value(window, 0), 0);
    }

    #[test]
    fn burst_frames_chain_without_timeline_overlap() {
        let created = Instant::now() - Duration::from_secs(60);
        let rings = SpeakerRings::new_with_created_at(DEFAULT_REPLAY_WINDOW, created);
        let now = Instant::now();
        let t = now - Duration::from_millis(200);
        // 突发：几乎同一墙钟时刻连续入环
        for _ in 0..5 {
            rings.append_pcm(1, "alice", pcm_ms(20), t);
        }
        assert_eq!(rings.segment_count(1), 1);
        let snap = rings.snapshot_at(ReplayFilter::Speaker { clid: 1 }, None, now);
        // 100ms 样本从 t 起连续，不应因重叠把 100 叠成 500
        let begin = stereo_48k_sample_count(Duration::from_millis(
            t.saturating_duration_since(now - Duration::from_secs(30))
                .as_millis() as u64,
        ));
        let span = stereo_48k_sample_count(Duration::from_millis(100));
        let window = &snap.samples[begin..(begin + span).min(snap.samples.len())];
        assert!(count_value(window, 100) > 0);
        assert_eq!(count_value(window, 500), 0);
    }

    #[test]
    fn rings_do_not_cross_pollute_speakers() {
        let created = Instant::now() - Duration::from_secs(60);
        let rings = SpeakerRings::new_with_created_at(DEFAULT_REPLAY_WINDOW, created);
        let now = Instant::now();
        let alice_at = now - Duration::from_millis(300);
        let bob_at = now - Duration::from_millis(200);
        rings.append_pcm(1, "alice", pcm_ms(200), alice_at);
        rings.append_pcm(2, "bob", pcm_ms(100), bob_at);

        let snap = rings.snapshot_at(ReplayFilter::Speaker { clid: 1 }, None, now);
        assert_eq!(snap.buffered_ms, 30_000);
        assert_eq!(snap.sample_rate, 48_000);
        assert_eq!(snap.channels, 2);
        assert_eq!(snap.speakers.len(), 2);
        assert_eq!(
            count_value(&snap.samples, 100),
            stereo_48k_sample_count(Duration::from_millis(200))
        );

        let bob_only = rings.snapshot_at(ReplayFilter::Speaker { clid: 2 }, None, now);
        assert_eq!(
            count_value(&bob_only.samples, 100),
            stereo_48k_sample_count(Duration::from_millis(100))
        );

        let all = rings.snapshot_at(ReplayFilter::All, None, now);
        assert_eq!(all.buffered_ms, 30_000);
        let stats = rings.stats_at(now);
        assert_eq!(stats.len(), 2);
        for stat in &stats {
            assert!(stat.name == "alice" || stat.name == "bob");
            assert!(stat.active_ms > 0);
        }
    }

    #[test]
    fn snapshot_without_future_playback_matches_window() {
        let now = Instant::now();
        let created = now - Duration::from_secs(60);
        let rings = SpeakerRings::new_with_created_at(DEFAULT_REPLAY_WINDOW, created);
        // 音频终点在 now 之前 → 无轴尾
        rings.append_pcm(1, "alice", pcm_ms(500), now - Duration::from_millis(600));

        let snap = rings.snapshot_at(ReplayFilter::All, None, now);
        assert_eq!(snap.buffered_ms, 30_000);
        assert_eq!(
            snap.samples.len(),
            stereo_48k_sample_count(Duration::from_secs(30))
        );
    }

    #[test]
    fn partial_window_snapshot_uses_ring_age_not_full_window() {
        let created = Instant::now() - Duration::from_secs(8);
        let rings = SpeakerRings::new_with_created_at(DEFAULT_REPLAY_WINDOW, created);
        let now = Instant::now();
        rings.append_pcm(1, "alice", pcm_ms(200), now - Duration::from_millis(250));

        let snap = rings.snapshot_at(ReplayFilter::All, None, now);
        assert!(snap.buffered_ms >= 7_000 && snap.buffered_ms <= 9_000);
        assert!(snap.buffered_ms < 20_000);
        assert_eq!(
            snap.buffered_ms,
            u64::from(snap.samples.len() as u32) * 1000 / 96_000
        );
    }

    #[test]
    fn axis_tail_extends_when_playback_crosses_now() {
        let created = Instant::now() - Duration::from_secs(60);
        let rings = SpeakerRings::new_with_created_at(DEFAULT_REPLAY_WINDOW, created);
        let now = Instant::now();
        // 500ms 音频起点 now-100ms → 播放终点 now+400ms
        rings.append_pcm(1, "alice", pcm_ms(500), now - Duration::from_millis(100));
        let snap = rings.snapshot_at(ReplayFilter::All, Some(30), now);
        // 回溯仍 30s，轴尾 +约 400ms → buffered_ms 可略 > 30000
        assert!(snap.buffered_ms >= 30_000);
        assert!(snap.buffered_ms <= 30_000 + 500);
        assert_eq!(
            snap.buffered_ms,
            u64::from(snap.samples.len() as u32) * 1000 / 96_000
        );
    }

    #[test]
    fn snapshot_speaker_keeps_full_timeline_axis() {
        let created = Instant::now() - Duration::from_secs(20);
        let rings = SpeakerRings::new_with_created_at(DEFAULT_REPLAY_WINDOW, created);
        let now = Instant::now();
        rings.append_pcm(1, "alice", pcm_ms(100), now - Duration::from_secs(10));
        rings.append_pcm(2, "bob", pcm_ms(100), now - Duration::from_secs(5));

        let snap = rings.snapshot_at(ReplayFilter::Speaker { clid: 1 }, None, now);
        assert_eq!(snap.buffered_ms, 20_000);
        assert_eq!(snap.speakers.len(), 2);
        let nonzero: Vec<usize> = snap
            .samples
            .iter()
            .enumerate()
            .filter(|(_, s)| **s != 0)
            .map(|(i, _)| i)
            .collect();
        assert!(!nonzero.is_empty());
        let first = nonzero[0] as u64 * 1000 / 96_000;
        assert!((9_000..11_000).contains(&first));
    }

    #[test]
    fn seconds_clamp_to_window() {
        let created = Instant::now() - Duration::from_secs(60);
        let rings = SpeakerRings::new_with_created_at(DEFAULT_REPLAY_WINDOW, created);
        let snap = rings.snapshot_at(ReplayFilter::All, Some(120), Instant::now());
        assert_eq!(snap.buffered_ms, 30_000);
    }

    #[test]
    fn active_ms_derives_and_decreases_after_eviction() {
        let rings = SpeakerRings::new(DEFAULT_REPLAY_WINDOW);
        let t0 = Instant::now();
        rings.append_pcm(1, "alice", pcm_ms(1000), t0);
        rings.append_pcm(1, "alice", pcm_ms(1000), t0 + Duration::from_secs(20));

        let partial = rings.stats_at(t0 + Duration::from_millis(400));
        assert_eq!(partial[0].active_ms, 400);

        let mid = rings.stats_at(t0 + Duration::from_secs(25));
        assert_eq!(mid.len(), 1);
        assert_eq!(mid[0].active_ms, 2000);

        let late = rings.stats_at(t0 + Duration::from_secs(35));
        assert_eq!(late.len(), 1);
        assert_eq!(late[0].active_ms, 1000);
        assert!(late[0].active_ms < mid[0].active_ms);
    }

    #[test]
    fn snapshot_clips_segment_prefix_before_axis_start() {
        let created = Instant::now() - Duration::from_secs(60);
        let rings = SpeakerRings::new_with_created_at(DEFAULT_REPLAY_WINDOW, created);
        let now = Instant::now();
        rings.append_pcm(1, "alice", pcm_ms(200), now - Duration::from_millis(150));
        let snap = rings.snapshot_at(ReplayFilter::Speaker { clid: 1 }, Some(1), now);
        let nonzero: Vec<usize> = snap
            .samples
            .iter()
            .enumerate()
            .filter(|(_, s)| **s == 100)
            .map(|(i, _)| i)
            .collect();
        assert!(!nonzero.is_empty());
        let first_ms = nonzero[0] as u64 * 1000 / 96_000;
        let last_ms = *nonzero.last().unwrap() as u64 * 1000 / 96_000;
        assert!(first_ms >= 800, "prefix must be clipped, first={first_ms}");
        // 轴尾扩展：终点 now+50ms，缓冲可略超 1s
        assert!(last_ms < 1100);
        assert_eq!(
            nonzero.len(),
            stereo_48k_sample_count(Duration::from_millis(200))
        );
    }

    #[test]
    fn segment_merge_on_run_break_threshold() {
        let rings = SpeakerRings::new(DEFAULT_REPLAY_WINDOW);
        let t0 = Instant::now();
        rings.append_pcm(1, "alice", pcm_ms(20), t0);
        rings.append_pcm(1, "alice", pcm_ms(20), t0 + Duration::from_millis(20));
        rings.append_pcm(1, "alice", pcm_ms(20), t0 + Duration::from_millis(80));
        assert_eq!(rings.segment_count(1), 1);

        rings.append_pcm(1, "alice", pcm_ms(20), t0 + Duration::from_millis(300));
        assert_eq!(rings.segment_count(1), 2);
    }

    #[test]
    fn force_split_chains_by_playback_clock() {
        let rings = SpeakerRings::new(DEFAULT_REPLAY_WINDOW);
        let t0 = Instant::now();
        // 连续入环铺满 > MAX_SEGMENT_MS
        let frame = pcm_ms(20);
        let mut at = t0;
        for _ in 0..60 {
            rings.append_pcm(1, "alice", frame.clone(), at);
            at += Duration::from_millis(10);
        }
        assert!(rings.segment_count(1) >= 2);
        let inner = rings.inner.lock().unwrap();
        let track = inner.tracks.get(&1).unwrap();
        let segs: Vec<_> = track.segments.iter().collect();
        for w in segs.windows(2) {
            let end = w[0].start + stereo_48k_duration(w[0].samples.len());
            assert_eq!(w[1].start, end, "force split must chain by playback clock");
        }
    }

    #[test]
    fn eviction_preserves_run_chaining_for_remaining_segments() {
        let rings = SpeakerRings::new(DEFAULT_REPLAY_WINDOW);
        let t0 = Instant::now();
        let frame = pcm_ms(20);
        let mut at = t0;
        // 长 run：跨驱逐边界（窗 30s 不易驱逐；用短窗）
        drop(rings);
        let rings = SpeakerRings::new(Duration::from_millis(500));
        for _ in 0..80 {
            rings.append_pcm(1, "alice", frame.clone(), at);
            at += Duration::from_millis(20);
        }
        let now = at;
        let _ = rings.snapshot_at(ReplayFilter::Speaker { clid: 1 }, None, now);
        let inner = rings.inner.lock().unwrap();
        if let Some(track) = inner.tracks.get(&1) {
            let segs: Vec<_> = track.segments.iter().collect();
            for w in segs.windows(2) {
                let end = w[0].start + stereo_48k_duration(w[0].samples.len());
                assert_eq!(w[1].start, end);
            }
        }
    }

    #[test]
    fn lru_evicts_oldest_track_when_over_capacity() {
        let rings = SpeakerRings::new(DEFAULT_REPLAY_WINDOW);
        let t0 = Instant::now();
        for i in 0..6u32 {
            rings.append_pcm(
                i,
                &format!("s{i}"),
                pcm_ms(20),
                t0 + Duration::from_millis(u64::from(i)),
            );
        }
        rings.append_pcm(99, "newcomer", pcm_ms(20), t0 + Duration::from_secs(1));

        let stats = rings.stats_at(t0 + Duration::from_secs(1));
        assert_eq!(stats.len(), 6);
        assert!(stats.iter().all(|s| s.clid != 0));
        assert!(stats.iter().any(|s| s.clid == 99));
    }

    #[test]
    fn resolve_name_exact_ambiguous_and_none() {
        let rings = SpeakerRings::new(DEFAULT_REPLAY_WINDOW);
        let now = Instant::now();
        rings.append_pcm(1, "Alice", pcm_ms(20), now);
        rings.append_pcm(2, "Alice", pcm_ms(20), now);
        rings.append_pcm(3, "Bob", pcm_ms(20), now);

        assert_eq!(rings.resolve_name("Bob"), NameResolve::Unique(3));
        assert!(matches!(
            rings.resolve_name("Alice"),
            NameResolve::Ambiguous(names) if names.len() == 2
        ));
        assert_eq!(rings.resolve_name("Carol"), NameResolve::None);
    }

    #[test]
    fn speaker_filter_excludes_other_speakers_samples() {
        let created = Instant::now() - Duration::from_secs(10);
        let rings = SpeakerRings::new_with_created_at(DEFAULT_REPLAY_WINDOW, created);
        let now = Instant::now();
        rings.append_pcm(2, "bob", pcm_ms(500), now - Duration::from_millis(100));

        let snap = rings.snapshot_at(ReplayFilter::Speaker { clid: 1 }, None, now);
        assert!(snap.samples.iter().all(|s| *s == 0));
        assert!(snap.speakers.iter().any(|s| s.clid == 2 && s.active_ms > 0));
    }

    #[test]
    fn multi_speaker_mix_soft_clips_without_overflow() {
        let created = Instant::now() - Duration::from_secs(60);
        let rings = SpeakerRings::new_with_created_at(DEFAULT_REPLAY_WINDOW, created);
        let now = Instant::now();
        let at = now - Duration::from_millis(100);
        rings.append_pcm(1, "a", pcm_ms_val(50, 20_000), at);
        rings.append_pcm(2, "b", pcm_ms_val(50, 20_000), at);
        rings.append_pcm(3, "c", pcm_ms_val(50, 20_000), at);

        let snap = rings.snapshot_at(ReplayFilter::All, None, now);
        let peak = snap
            .samples
            .iter()
            .map(|s| i32::from(*s).abs())
            .max()
            .unwrap_or(0);
        assert!(peak > 0);
        assert!(peak <= i32::from(i16::MAX));
        // 60_000 超限后 soft-clip，应仍接近饱和而非截成 0
        assert!(peak >= 20_000);
    }

    #[test]
    fn soft_clip_transmits_in_range_and_saturates_overflow() {
        assert_eq!(soft_clip_i32_to_i16(100), 100);
        assert_eq!(soft_clip_i32_to_i16(-100), -100);
        assert_eq!(soft_clip_i32_to_i16(i32::from(i16::MAX)), i16::MAX);
        let over = soft_clip_i32_to_i16(50_000);
        assert!((20_000..=i16::MAX).contains(&over));
        let over_neg = soft_clip_i32_to_i16(-50_000);
        assert!((i16::MIN..=-20_000).contains(&over_neg));
    }
}

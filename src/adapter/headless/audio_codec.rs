//! 48k 立体声 Opus 编解码公共封装与包序抖动恢复，供 audio_output / speaker_ring / speech 复用。

use anyhow::{anyhow, bail, Result};
use rusty_opus::{Application, OpusDecoder as RawOpusDecoder, OpusEncoder as RawOpusEncoder};
use std::time::{Duration, Instant};
use tracing::debug;

pub const SAMPLE_RATE_HZ: u32 = 48_000;
pub const CHANNELS: u16 = 2;
pub const PCM_FRAME_MS: u64 = 20;
pub const PCM_FRAME_SAMPLES_PER_CHANNEL: usize = SAMPLE_RATE_HZ as usize / 50;
pub const PCM_FRAME_SAMPLES_STEREO: usize = PCM_FRAME_SAMPLES_PER_CHANNEL * CHANNELS as usize;
/// Opus 协议单包最大解码采样数（120ms @ 48kHz）
pub const MAX_DECODE_FRAME_SAMPLES_PER_CHANNEL: usize = 5_760;
pub const MAX_DECODE_FRAME_SAMPLES_STEREO: usize =
    MAX_DECODE_FRAME_SAMPLES_PER_CHANNEL * CHANNELS as usize;
/// 出站 Opus 码率（codec 5 / 重编码路径）
pub const OUTBOUND_BITRATE_BPS: i32 = 128_000;
/// TS3 UDP 语音载荷留足包头余量后的单包字节上限（不超出 500 字节限制）
pub const MAX_VOICE_PACKET_BYTES: usize = 484;

const REORDER_MAX_OFFSET: i16 = 3;
const REORDER_MAX_BUFFERED: usize = 3;
const MAX_JUMP_RESYNC_DIFF: i16 = 32;
const MAX_PLC_FRAMES: u8 = 3;
const NEW_TURN_SILENCE_THRESHOLD: Duration = Duration::from_millis(600);

/// 48kHz 立体声出站 Opus 编码器（Audio 模式，128kbps CVBR）。
pub struct OpusEncoder {
    inner: RawOpusEncoder,
}

impl OpusEncoder {
    pub fn new_stereo() -> Result<Self> {
        let mut encoder =
            RawOpusEncoder::new(SAMPLE_RATE_HZ as i32, CHANNELS as usize, Application::Audio)
                .map_err(|e| anyhow!("opus stereo encoder init failed: {e}"))?;
        encoder.bitrate_bps = OUTBOUND_BITRATE_BPS;
        // rusty-opus 内部默认开启 CeltEncoder::constrained_vbr；此处显式固定非 CBR 即为 CVBR。
        encoder.use_cbr = false;
        Ok(Self { inner: encoder })
    }

    pub fn encode_frame(
        &mut self,
        float_pcm: &[f32],
        output: &mut [u8; MAX_VOICE_PACKET_BYTES],
    ) -> Result<usize> {
        if float_pcm.len() != PCM_FRAME_SAMPLES_STEREO {
            bail!(
                "invalid opus input frame len: got {}, expected {}",
                float_pcm.len(),
                PCM_FRAME_SAMPLES_STEREO
            );
        }
        let len = self
            .inner
            .encode(float_pcm, PCM_FRAME_SAMPLES_PER_CHANNEL, output)
            .map_err(|e| anyhow!("opus encode failed: {e}"))?;
        if len > MAX_VOICE_PACKET_BYTES {
            bail!(
                "opus packet exceeds UDP limit: {} > {} bytes",
                len,
                MAX_VOICE_PACKET_BYTES
            );
        }
        Ok(len)
    }
}

/// 48kHz 立体声入站 Opus 解码器，显式区分常规解码、FEC 恢复与单帧 PLC。
pub struct OpusDecoder {
    inner: RawOpusDecoder,
    scratch: Vec<f32>,
}

impl OpusDecoder {
    pub fn new_stereo() -> Result<Self> {
        let inner = RawOpusDecoder::new(SAMPLE_RATE_HZ as i32, CHANNELS as usize)
            .map_err(|e| anyhow!("opus stereo decoder init failed: {e}"))?;
        Ok(Self {
            inner,
            scratch: vec![0.0; MAX_DECODE_FRAME_SAMPLES_STEREO],
        })
    }

    /// 解码正常收到的非空 Opus 包。
    pub fn decode_normal(&mut self, packet: &[u8]) -> Result<Vec<i16>> {
        if packet.is_empty() {
            bail!("empty opus packet cannot be passed to decode_normal");
        }
        let samples_per_channel = self
            .inner
            .decode(
                packet,
                MAX_DECODE_FRAME_SAMPLES_PER_CHANNEL,
                &mut self.scratch,
            )
            .map_err(|e| anyhow!("opus decode failed: {e}"))?;
        Ok(scratch_to_i16(
            &self.scratch[..samples_per_channel * CHANNELS as usize],
        ))
    }

    /// 用下一包带内 FEC 数据恢复紧邻丢失的 20ms 帧（严格传入单帧长度 960/ch）。
    pub fn decode_fec(&mut self, next_packet: &[u8]) -> Result<Vec<i16>> {
        if next_packet.len() <= 1 {
            bail!("fec packet too short: {} bytes", next_packet.len());
        }
        let out_slice = &mut self.scratch[..PCM_FRAME_SAMPLES_STEREO];
        let samples_per_channel = self
            .inner
            .decode_fec(next_packet, PCM_FRAME_SAMPLES_PER_CHANNEL, out_slice)
            .map_err(|e| anyhow!("opus decode_fec failed: {e}"))?;
        Ok(scratch_to_i16(
            &self.scratch[..samples_per_channel * CHANNELS as usize],
        ))
    }

    /// 无包可借时按单帧 20ms（960/ch）做丢包隐藏。
    pub fn decode_plc(&mut self) -> Result<Vec<i16>> {
        let out_slice = &mut self.scratch[..PCM_FRAME_SAMPLES_STEREO];
        let samples_per_channel = self
            .inner
            .decode(&[], PCM_FRAME_SAMPLES_PER_CHANNEL, out_slice)
            .map_err(|e| anyhow!("opus decode_plc failed: {e}"))?;
        Ok(scratch_to_i16(
            &self.scratch[..samples_per_channel * CHANNELS as usize],
        ))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitterStats {
    pub normal_frames: u64,
    pub fec_frames: u64,
    pub plc_frames: u64,
    pub late_dropped: u64,
    pub marker_skipped: u64,
    pub eos_resets: u64,
    pub resyncs: u64,
}

#[derive(Debug, Clone)]
enum BufferedPacket {
    Audio(Vec<u8>),
    Marker,
}

/// 按说话人包序做轻量重排与 FEC/PLC 恢复的 Opus 解码状态机。
pub struct OpusJitterDecoder {
    decoder: OpusDecoder,
    expected_seq: Option<u16>,
    reorder: Vec<(u16, BufferedPacket)>,
    last_packet_at: Option<Instant>,
    plc_streak: u8,
    stats: JitterStats,
}

impl OpusJitterDecoder {
    pub fn new_stereo() -> Result<Self> {
        Ok(Self {
            decoder: OpusDecoder::new_stereo()?,
            expected_seq: None,
            reorder: Vec::with_capacity(REORDER_MAX_BUFFERED + 1),
            last_packet_at: None,
            plc_streak: 0,
            stats: JitterStats::default(),
        })
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> JitterStats {
        self.stats
    }

    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.reorder.len()
    }

    /// 送入一帧入站语音包，返回按时间序还原出的 0 或多个 48kHz 立体声 PCM 帧。
    pub fn push(&mut self, seq: u16, frame: &[u8], received_at: Instant) -> Result<Vec<Vec<i16>>> {
        // TS3 0 字节尾包标记说话结束：清空重排缓冲并重置包序，不做 PLC。
        if frame.is_empty() {
            self.reorder.clear();
            self.expected_seq = None;
            self.last_packet_at = None;
            self.plc_streak = 0;
            self.stats.eos_resets += 1;
            if self.stats.normal_frames > 0
                || self.stats.fec_frames > 0
                || self.stats.plc_frames > 0
            {
                debug!(
                    normal = self.stats.normal_frames,
                    fec = self.stats.fec_frames,
                    plc = self.stats.plc_frames,
                    late = self.stats.late_dropped,
                    marker = self.stats.marker_skipped,
                    resyncs = self.stats.resyncs,
                    eos = self.stats.eos_resets,
                    "opus jitter EOS summary"
                );
            }
            return Ok(Vec::new());
        }

        // 1 字节控制/边界包静默跳过解码；若位于当前期望包序（或小幅乱序窗），推进/占位避免误判丢包，但不刷新音频时间锚点。
        if frame.len() == 1 {
            self.stats.marker_skipped += 1;
            if let Some(exp) = self.expected_seq {
                let diff = seq.wrapping_sub(exp) as i16;
                if diff == 0 {
                    self.expected_seq = Some(exp.wrapping_add(1));
                    let mut out = Vec::new();
                    self.drain_contiguous(&mut out)?;
                    return Ok(out);
                }
                if (1..=REORDER_MAX_OFFSET).contains(&diff)
                    && !self.reorder.iter().any(|(s, _)| *s == seq)
                {
                    self.reorder.push((seq, BufferedPacket::Marker));
                }
            }
            return Ok(Vec::new());
        }

        if let Some(prev_at) = self.last_packet_at {
            if received_at.saturating_duration_since(prev_at) >= NEW_TURN_SILENCE_THRESHOLD {
                self.reorder.clear();
                self.expected_seq = None;
                self.plc_streak = 0;
                self.stats.resyncs += 1;
            }
        }
        self.last_packet_at = Some(received_at);

        let Some(exp) = self.expected_seq else {
            self.expected_seq = Some(seq.wrapping_add(1));
            return self.decode_one_normal(frame).map(|pcm| vec![pcm]);
        };

        let diff = seq.wrapping_sub(exp) as i16;
        if diff < 0 {
            self.stats.late_dropped += 1;
            return Ok(Vec::new());
        }

        if diff > MAX_JUMP_RESYNC_DIFF {
            self.reorder.clear();
            self.expected_seq = Some(seq.wrapping_add(1));
            self.plc_streak = 0;
            self.stats.resyncs += 1;
            return self.decode_one_normal(frame).map(|pcm| vec![pcm]);
        }

        let mut out = Vec::new();
        if diff == 0 {
            self.expected_seq = Some(exp.wrapping_add(1));
            let pcm = self.decode_one_normal(frame)?;
            out.push(pcm);
            self.drain_contiguous(&mut out)?;
            return Ok(out);
        }

        // 1 <= diff <= 32：先按当前包序入重排槽（去重），再按小窗/突发门限推进 expected_seq。
        if !self.reorder.iter().any(|(s, _)| *s == seq) {
            self.reorder
                .push((seq, BufferedPacket::Audio(frame.to_vec())));
        }

        if diff <= REORDER_MAX_OFFSET {
            if self.reorder.len() >= REORDER_MAX_BUFFERED {
                self.step_missing_expected(&mut out)?;
                self.drain_contiguous(&mut out)?;
            }
        } else {
            while let Some(cur_exp) = self.expected_seq {
                if (seq.wrapping_sub(cur_exp) as i16) < 0 {
                    break;
                }
                if self.reorder.iter().any(|(s, _)| *s == cur_exp) {
                    self.drain_contiguous(&mut out)?;
                } else {
                    self.step_missing_expected(&mut out)?;
                    self.drain_contiguous(&mut out)?;
                }
            }
        }

        Ok(out)
    }

    fn decode_one_normal(&mut self, packet: &[u8]) -> Result<Vec<i16>> {
        let pcm = self.decoder.decode_normal(packet)?;
        self.plc_streak = 0;
        self.stats.normal_frames += 1;
        Ok(pcm)
    }

    fn take_seq(&mut self, target: u16) -> Option<BufferedPacket> {
        let idx = self.reorder.iter().position(|(s, _)| *s == target)?;
        Some(self.reorder.swap_remove(idx).1)
    }

    fn step_missing_expected(&mut self, out: &mut Vec<Vec<i16>>) -> Result<()> {
        let Some(exp) = self.expected_seq else {
            return Ok(());
        };
        let next_seq = exp.wrapping_add(1);
        let fec_payload = self
            .reorder
            .iter()
            .find(|(s, entry)| {
                *s == next_seq && matches!(entry, BufferedPacket::Audio(pkt) if pkt.len() > 1)
            })
            .and_then(|(_, entry)| match entry {
                BufferedPacket::Audio(pkt) => Some(pkt.clone()),
                BufferedPacket::Marker => None,
            });

        self.expected_seq = Some(next_seq);

        if let Some(next_pkt) = fec_payload {
            let pcm = self.decoder.decode_fec(&next_pkt)?;
            self.plc_streak = 0;
            self.stats.fec_frames += 1;
            out.push(pcm);
        } else if self.plc_streak < MAX_PLC_FRAMES {
            let pcm = self.decoder.decode_plc()?;
            self.plc_streak += 1;
            self.stats.plc_frames += 1;
            out.push(pcm);
        }
        Ok(())
    }

    fn drain_contiguous(&mut self, out: &mut Vec<Vec<i16>>) -> Result<()> {
        while let Some(exp) = self.expected_seq {
            let Some(entry) = self.take_seq(exp) else {
                break;
            };
            self.expected_seq = Some(exp.wrapping_add(1));
            if let BufferedPacket::Audio(pkt) = entry {
                out.push(self.decode_one_normal(&pkt)?);
            }
        }
        Ok(())
    }
}

#[inline]
fn float_to_i16(sample: f32) -> i16 {
    if !sample.is_finite() {
        return 0;
    }
    (sample * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

fn scratch_to_i16(samples: &[f32]) -> Vec<i16> {
    samples.iter().copied().map(float_to_i16).collect()
}

pub fn pcm_stereo_48k_duration(sample_count: usize) -> Duration {
    Duration::from_millis(sample_count as u64 * 1000 / (SAMPLE_RATE_HZ as u64 * CHANNELS as u64))
}

pub fn stereo_48k_sample_count(duration: Duration) -> usize {
    (duration.as_millis() as u64 * SAMPLE_RATE_HZ as u64 * CHANNELS as u64 / 1000) as usize
}

pub fn pcm_frame_to_float(frame: &[i16], out: &mut [f32]) {
    for (i, sample) in frame.iter().enumerate() {
        out[i] = *sample as f32 / 32768.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>() {}

    fn synth_stereo_frame(frame_idx: usize, freq_hz: f32, amp: f32) -> Vec<f32> {
        let mut pcm = Vec::with_capacity(PCM_FRAME_SAMPLES_STEREO);
        let base_sample = frame_idx * PCM_FRAME_SAMPLES_PER_CHANNEL;
        for i in 0..PCM_FRAME_SAMPLES_PER_CHANNEL {
            let t = (base_sample + i) as f32 / SAMPLE_RATE_HZ as f32;
            let sample = amp * (2.0 * std::f32::consts::PI * freq_hz * t).sin();
            pcm.push(sample);
            pcm.push(sample * 0.9);
        }
        pcm
    }

    fn encode_test_frames(count: usize) -> Vec<Vec<u8>> {
        let mut encoder = OpusEncoder::new_stereo().expect("encoder init");
        let mut out = [0u8; MAX_VOICE_PACKET_BYTES];
        (0..count)
            .map(|idx| {
                let pcm = synth_stereo_frame(idx, 440.0 + idx as f32 * 5.0, 0.5);
                let len = encoder.encode_frame(&pcm, &mut out).expect("encode");
                out[..len].to_vec()
            })
            .collect()
    }

    fn encode_voip_fec_frames(count: usize) -> Vec<Vec<u8>> {
        let mut encoder =
            RawOpusEncoder::new(SAMPLE_RATE_HZ as i32, CHANNELS as usize, Application::Voip)
                .expect("voip encoder");
        encoder.bitrate_bps = 24_000;
        encoder.use_inband_fec = true;
        encoder.packet_loss_perc = 25;
        let mut out = [0u8; MAX_VOICE_PACKET_BYTES];
        (0..count)
            .map(|idx| {
                let pcm = synth_stereo_frame(idx, 350.0, 0.6);
                let len = encoder
                    .encode(&pcm, PCM_FRAME_SAMPLES_PER_CHANNEL, &mut out)
                    .expect("voip encode");
                out[..len].to_vec()
            })
            .collect()
    }

    #[test]
    fn opus_wrappers_are_send() {
        assert_send::<OpusEncoder>();
        assert_send::<OpusDecoder>();
        assert_send::<OpusJitterDecoder>();
    }

    #[test]
    fn outbound_encoder_cvbr_and_packet_cap_484() {
        let mut encoder = OpusEncoder::new_stereo().expect("encoder init");
        assert_eq!(encoder.inner.bitrate_bps, OUTBOUND_BITRATE_BPS);
        assert!(!encoder.inner.use_cbr);

        let mut decoder = OpusDecoder::new_stereo().expect("decoder init");
        let mut packet_buf = [0u8; MAX_VOICE_PACKET_BYTES];
        let mut silence_lens = Vec::new();
        let silence = vec![0.0f32; PCM_FRAME_SAMPLES_STEREO];
        for _ in 0..20 {
            let len = encoder
                .encode_frame(&silence, &mut packet_buf)
                .expect("silence encode");
            assert!(len <= MAX_VOICE_PACKET_BYTES);
            silence_lens.push(len);
        }

        // 5 秒（250 帧）多频叠加 + 振幅扫频 + 伪随机高频噪声压测，验证全程不超 484 字节且呈 CVBR 波动
        let mut max_complex_len = 0usize;
        let mut min_complex_len = usize::MAX;
        let mut rng_state: u32 = 0x1234_5678;
        for frame_idx in 0..250 {
            let mut pcm = Vec::with_capacity(PCM_FRAME_SAMPLES_STEREO);
            let base = frame_idx * PCM_FRAME_SAMPLES_PER_CHANNEL;
            for i in 0..PCM_FRAME_SAMPLES_PER_CHANNEL {
                let t = (base + i) as f32 / SAMPLE_RATE_HZ as f32;
                rng_state = rng_state.wrapping_mul(1664525).wrapping_add(1013904223);
                let noise = ((rng_state >> 16) as f32 / 32768.0) - 1.0;
                let chirp = (2.0 * std::f32::consts::PI * (200.0 + 30.0 * t * t) * t).sin();
                let s = (0.4 * chirp + 0.55 * noise).clamp(-0.99, 0.99);
                pcm.push(s);
                pcm.push((0.4 * (chirp * 0.7) - 0.55 * noise).clamp(-0.99, 0.99));
            }
            let len = encoder
                .encode_frame(&pcm, &mut packet_buf)
                .expect("complex encode");
            assert!(
                len <= MAX_VOICE_PACKET_BYTES,
                "frame {frame_idx} packet len {len} exceeded {MAX_VOICE_PACKET_BYTES}"
            );
            let decoded = decoder
                .decode_normal(&packet_buf[..len])
                .expect("decode outbound frame");
            assert_eq!(decoded.len(), PCM_FRAME_SAMPLES_STEREO);
            max_complex_len = max_complex_len.max(len);
            min_complex_len = min_complex_len.min(len);
        }
        assert!(min_complex_len > 0);
        assert!(max_complex_len > *silence_lens.iter().min().expect("min"));
    }

    #[test]
    fn decode_fec_and_decode_plc_strictly_return_20ms_frame() {
        let voip_pkts = encode_voip_fec_frames(8);
        let mut decoder = OpusDecoder::new_stereo().expect("decoder init");
        for pkt in &voip_pkts[..3] {
            let pcm = decoder.decode_normal(pkt).expect("normal decode");
            assert_eq!(pcm.len(), PCM_FRAME_SAMPLES_STEREO);
        }

        // 模拟丢掉第 3 帧，用第 4 帧做 FEC 恢复
        let fec_pcm = decoder.decode_fec(&voip_pkts[4]).expect("decode_fec");
        assert_eq!(fec_pcm.len(), PCM_FRAME_SAMPLES_STEREO);
        let fec_energy: f64 = fec_pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
        assert!(
            fec_energy > 0.0,
            "decode_fec must reconstruct non-silent audio"
        );

        // 再用 PLC 严格补 1 帧 20ms（1920 样本），绝非 120ms
        let plc_pcm = decoder.decode_plc().expect("decode_plc");
        assert_eq!(plc_pcm.len(), PCM_FRAME_SAMPLES_STEREO);
    }

    #[test]
    fn jitter_decoder_handles_in_order_and_small_reorder_without_loss() {
        let pkts = encode_test_frames(6);
        let t0 = Instant::now();
        let mut jitter = OpusJitterDecoder::new_stereo().expect("jitter init");

        // 顺序：[0, 2, 1, 3, 5, 4]
        assert_eq!(jitter.push(0, &pkts[0], t0).unwrap().len(), 1);
        assert!(jitter
            .push(2, &pkts[2], t0 + Duration::from_millis(20))
            .unwrap()
            .is_empty());
        assert_eq!(jitter.pending_len(), 1);

        let recovered = jitter
            .push(1, &pkts[1], t0 + Duration::from_millis(40))
            .unwrap();
        assert_eq!(recovered.len(), 2);
        assert_eq!(jitter.pending_len(), 0);

        assert_eq!(
            jitter
                .push(3, &pkts[3], t0 + Duration::from_millis(60))
                .unwrap()
                .len(),
            1
        );
        assert!(jitter
            .push(5, &pkts[5], t0 + Duration::from_millis(80))
            .unwrap()
            .is_empty());
        assert_eq!(
            jitter
                .push(4, &pkts[4], t0 + Duration::from_millis(100))
                .unwrap()
                .len(),
            2
        );

        let stats = jitter.stats();
        assert_eq!(stats.normal_frames, 6);
        assert_eq!(stats.fec_frames, 0);
        assert_eq!(stats.plc_frames, 0);
    }

    #[test]
    fn jitter_decoder_drops_late_and_duplicate_packets() {
        let pkts = encode_test_frames(4);
        let t0 = Instant::now();
        let mut jitter = OpusJitterDecoder::new_stereo().expect("jitter init");

        assert_eq!(jitter.push(10, &pkts[0], t0).unwrap().len(), 1);
        assert_eq!(
            jitter
                .push(11, &pkts[1], t0 + Duration::from_millis(20))
                .unwrap()
                .len(),
            1
        );
        // 迟到包与重复包（diff < 0）均直接丢弃
        assert!(jitter
            .push(10, &pkts[0], t0 + Duration::from_millis(30))
            .unwrap()
            .is_empty());
        assert!(jitter
            .push(9, &pkts[0], t0 + Duration::from_millis(35))
            .unwrap()
            .is_empty());
        assert_eq!(jitter.stats().late_dropped, 2);
    }

    #[test]
    fn jitter_decoder_handles_u16_wraparound() {
        let pkts = encode_test_frames(6);
        let t0 = Instant::now();
        let mut jitter = OpusJitterDecoder::new_stereo().expect("jitter init");

        assert_eq!(jitter.push(65534, &pkts[0], t0).unwrap().len(), 1);
        // 跨回绕乱序：0 先于 65535 到达
        assert!(jitter
            .push(0, &pkts[2], t0 + Duration::from_millis(20))
            .unwrap()
            .is_empty());
        let out = jitter
            .push(65535, &pkts[1], t0 + Duration::from_millis(40))
            .unwrap();
        assert_eq!(out.len(), 2);

        // 丢 1，随后 2, 3, 4 到达触发对 1 的 FEC 恢复
        assert!(jitter
            .push(2, &pkts[3], t0 + Duration::from_millis(60))
            .unwrap()
            .is_empty());
        assert!(jitter
            .push(3, &pkts[4], t0 + Duration::from_millis(80))
            .unwrap()
            .is_empty());
        let recovered = jitter
            .push(4, &pkts[5], t0 + Duration::from_millis(100))
            .unwrap();
        assert_eq!(recovered.len(), 4);
        assert_eq!(jitter.stats().fec_frames, 1);
        assert_eq!(jitter.stats().plc_frames, 0);
    }

    #[test]
    fn jitter_decoder_triggers_fec_on_single_loss_and_caps_plc_at_three_frames() {
        let pkts = encode_voip_fec_frames(15);
        let t0 = Instant::now();
        let mut jitter = OpusJitterDecoder::new_stereo().expect("jitter init");

        // 0 正常，丢 1，随后 2, 3, 4 到达 -> 用 pkt[2] 做 FEC 恢复第 1 帧
        assert_eq!(jitter.push(0, &pkts[0], t0).unwrap().len(), 1);
        assert!(jitter
            .push(2, &pkts[2], t0 + Duration::from_millis(20))
            .unwrap()
            .is_empty());
        assert!(jitter
            .push(3, &pkts[3], t0 + Duration::from_millis(40))
            .unwrap()
            .is_empty());
        let out = jitter
            .push(4, &pkts[4], t0 + Duration::from_millis(60))
            .unwrap();
        assert_eq!(out.len(), 4);
        assert_eq!(jitter.stats().fec_frames, 1);
        assert_eq!(jitter.stats().plc_frames, 0);

        // 连续丢失 5..=12（8 帧），第 13 帧到达 -> PLC 最多补 3 帧（+ 第 12 帧用 13 做 FEC）
        let burst = jitter
            .push(13, &pkts[13], t0 + Duration::from_millis(240))
            .unwrap();
        assert_eq!(jitter.stats().plc_frames, 3);
        assert_eq!(jitter.stats().fec_frames, 2);
        assert_eq!(burst.len(), 5);
    }

    #[test]
    fn jitter_decoder_resyncs_on_large_jump_and_prolonged_silence() {
        let pkts = encode_test_frames(4);
        let t0 = Instant::now();
        let mut jitter = OpusJitterDecoder::new_stereo().expect("jitter init");

        assert_eq!(jitter.push(10, &pkts[0], t0).unwrap().len(), 1);
        // 大跳变 diff = 50 > 32 -> 直接重同步，不触发 PLC
        assert_eq!(
            jitter
                .push(60, &pkts[1], t0 + Duration::from_millis(20))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(jitter.stats().plc_frames, 0);
        assert_eq!(jitter.stats().resyncs, 1);

        // 长时间静默后新话轮直接重同步，即使 seq 跳变或回退
        assert_eq!(
            jitter
                .push(1, &pkts[2], t0 + Duration::from_millis(900))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(jitter.stats().plc_frames, 0);
        assert_eq!(jitter.stats().resyncs, 2);
    }

    #[test]
    fn jitter_decoder_skips_one_byte_markers_and_clears_buffer_on_eos() {
        let pkts = encode_test_frames(4);
        let t0 = Instant::now();
        let mut jitter = OpusJitterDecoder::new_stereo().expect("jitter init");

        assert_eq!(jitter.push(10, &pkts[0], t0).unwrap().len(), 1);
        // 1 字节边界标记静默跳过，不报错且推进 seq 避免下一帧误判丢包
        assert!(jitter
            .push(11, &[0x02], t0 + Duration::from_millis(20))
            .unwrap()
            .is_empty());
        assert_eq!(
            jitter
                .push(12, &pkts[1], t0 + Duration::from_millis(40))
                .unwrap()
                .len(),
            1
        );

        // 先缓存乱序未来包，再收到 0 字节尾包 -> 清空待重排缓冲、无 PLC
        assert!(jitter
            .push(14, &pkts[2], t0 + Duration::from_millis(60))
            .unwrap()
            .is_empty());
        assert_eq!(jitter.pending_len(), 1);
        assert!(jitter
            .push(15, &[], t0 + Duration::from_millis(80))
            .unwrap()
            .is_empty());
        assert_eq!(jitter.pending_len(), 0);
        assert_eq!(jitter.stats().eos_resets, 1);
        assert_eq!(jitter.stats().plc_frames, 0);
        assert_eq!(jitter.stats().marker_skipped, 1);

        // EOS 后新说话人流任意新 seq 直接解码
        assert_eq!(
            jitter
                .push(500, &pkts[3], t0 + Duration::from_millis(100))
                .unwrap()
                .len(),
            1
        );
    }
}

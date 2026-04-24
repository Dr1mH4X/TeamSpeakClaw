use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncBufReadExt;

use anyhow::{anyhow, Result};
use audiopus::coder::Encoder;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::{watch, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use tsproto_packets::packets::OutPacket;
use tsproto_packets::packets::{AudioData, CodecType, OutAudio};

use crate::adapter::headless::types::SharedStatus;

struct ChildKillOnDrop {
    child: Option<tokio::process::Child>,
}

impl ChildKillOnDrop {
    fn new(child: tokio::process::Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.child.as_mut().expect("child missing")
    }
}

impl Drop for ChildKillOnDrop {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

struct ReverbChannel {
    comb_bufs: [Vec<f32>; 2],
    comb_idx: [usize; 2],
    allpass_buf: Vec<f32>,
    allpass_idx: usize,
}

impl ReverbChannel {
    fn new(comb_lens: [usize; 2], allpass_len: usize) -> Self {
        Self {
            comb_bufs: [vec![0.0; comb_lens[0]], vec![0.0; comb_lens[1]]],
            comb_idx: [0, 0],
            allpass_buf: vec![0.0; allpass_len],
            allpass_idx: 0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let comb_feedback = 0.78f32;
        let mut s = 0.0f32;
        for i in 0..2 {
            let idx = self.comb_idx[i];
            let y = self.comb_bufs[i][idx];
            self.comb_bufs[i][idx] = x + y * comb_feedback;
            self.comb_idx[i] = (idx + 1) % self.comb_bufs[i].len();
            s += y;
        }
        s *= 0.5;

        let ap_feedback = 0.5f32;
        let idx = self.allpass_idx;
        let buf = self.allpass_buf[idx];
        let y = -s + buf;
        self.allpass_buf[idx] = s + buf * ap_feedback;
        self.allpass_idx = (idx + 1) % self.allpass_buf.len();
        y
    }
}

struct SimpleReverb {
    l: ReverbChannel,
    r: ReverbChannel,
}

impl SimpleReverb {
    fn new() -> Self {
        Self {
            l: ReverbChannel::new([1487, 1601], 556),
            r: ReverbChannel::new([1559, 1699], 579),
        }
    }

    fn process_stereo(&mut self, l: f32, r: f32, mix: f32) -> (f32, f32) {
        if mix <= 0.0001 {
            return (l, r);
        }
        let mix = mix.clamp(0.0, 1.0);
        let wet_gain = 0.28f32;
        let in_l = l;
        let in_r = r;
        let wet_l = self.l.process(in_l);
        let wet_r = self.r.process(in_r);
        (
            in_l * (1.0 - mix) + wet_l * (mix * wet_gain),
            in_r * (1.0 - mix) + wet_r * (mix * wet_gain),
        )
    }
}

pub async fn playback_loop(
    source_url: String,
    ts3_audio_tx: tokio::sync::mpsc::Sender<OutPacket>,
    mut paused_rx: watch::Receiver<bool>,
    cancel: CancellationToken,
    status: Arc<Mutex<SharedStatus>>,
) -> Result<()> {
    let playback_started = Instant::now();
    debug!(source_url = %source_url, "playback starting");

    let child = tokio::process::Command::new("ffmpeg")
        .arg("-nostdin")
        .arg("-loglevel")
        .arg("error")
        .arg("-reconnect")
        .arg("1")
        .arg("-reconnect_streamed")
        .arg("1")
        .arg("-reconnect_delay_max")
        .arg("5")
        .arg("-rw_timeout")
        .arg("15000000")
        .arg("-i")
        .arg(&source_url)
        .arg("-f")
        .arg("s16le")
        .arg("-ar")
        .arg("48000")
        .arg("-ac")
        .arg("2")
        .arg("pipe:1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("failed to start ffmpeg: {e}"))?;

    let mut child = ChildKillOnDrop::new(child);

    if let Some(stderr) = child.child_mut().stderr.take() {
        let src = source_url.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(source_url = %src, "ffmpeg: {line}");
            }
        });
    }

    let mut stdout = child
        .child_mut()
        .stdout
        .take()
        .ok_or_else(|| anyhow!("ffmpeg stdout missing"))?;

    let pcm_channel_capacity: usize = if cfg!(windows) { 200 } else { 50 };
    let (pcm_tx, mut pcm_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(pcm_channel_capacity);

    let encoder = Encoder::new(
        audiopus::SampleRate::Hz48000,
        audiopus::Channels::Stereo,
        audiopus::Application::Audio,
    )
    .map_err(|e| anyhow!("opus encoder init failed: {e}"))?;

    let frame_samples_per_channel = 48000 / 50;
    let channels = 2usize;
    let bytes_per_sample = 2usize;
    let frame_bytes = frame_samples_per_channel * channels * bytes_per_sample;
    let frame_duration = Duration::from_millis(20);

    let mut pcm = vec![0u8; frame_bytes];
    let mut float_buf = vec![0f32; frame_samples_per_channel * channels];
    let mut opus_out = [0u8; 1275];

    let mut reverb = SimpleReverb::new();
    let bass_cutoff_hz: f32 = 150.0;
    let fs: f32 = 48000.0;
    let bass_alpha: f32 = (2.0 * std::f32::consts::PI * bass_cutoff_hz)
        / (fs + 2.0 * std::f32::consts::PI * bass_cutoff_hz);
    let mut bass_lp_l: f32 = 0.0;
    let mut bass_lp_r: f32 = 0.0;

    let reader_cancel = cancel.clone();
    let reader_src = source_url.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; frame_bytes];
        loop {
            if reader_cancel.is_cancelled() {
                break;
            }
            let t0 = Instant::now();
            if stdout.read_exact(&mut buf).await.is_err() {
                break;
            }
            let dt = t0.elapsed();
            if dt >= Duration::from_millis(200) {
                warn!(source_url = %reader_src, read_ms = %dt.as_millis(), "ffmpeg pcm read stalled");
            }
            if pcm_tx.send(buf.clone()).await.is_err() {
                break;
            }
        }
    });

    let mut ticker = tokio::time::interval(frame_duration);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut underruns_total: u64 = 0;
    let mut underruns_window: u64 = 0;
    let mut underruns_consecutive: u64 = 0;
    let mut logged_first_pcm = false;

    let mut pcm_buf: VecDeque<Vec<u8>> = VecDeque::new();

    let prebuffer_target: usize = if cfg!(windows) { 15 } else { 5 };
    let late_frame_wait = if cfg!(windows) {
        Duration::from_millis(8)
    } else {
        Duration::from_millis(3)
    };
    let mut prebuffering = true;

    let mut last_tick = Instant::now();
    let mut reset_tick_after_pause = false;
    let mut tick_jitter_max_ms: u128 = 0;
    let mut clipped_samples: u64 = 0;
    let mut max_abs_sample: f32 = 0.0;
    let mut diag_next = Instant::now() + Duration::from_secs(5);

    let fade_total_samples_per_channel: usize = 48000 / 1000 * 80;
    let mut fade_pos_samples_per_channel: usize = 0;

    'main: loop {
        if cancel.is_cancelled() {
            break;
        }

        while *paused_rx.borrow() {
            reset_tick_after_pause = true;
            tokio::select! {
                _ = cancel.cancelled() => { break 'main; }
                r = paused_rx.changed() => {
                    if r.is_err() {
                        break 'main;
                    }
                }
            }
        }

        if reset_tick_after_pause {
            ticker.reset();
            last_tick = Instant::now();
            reset_tick_after_pause = false;
        }

        tokio::select! {
            _ = cancel.cancelled() => { break; }
            _ = ticker.tick() => {}
        }

        while let Ok(frame) = pcm_rx.try_recv() {
            if frame.len() == frame_bytes {
                pcm_buf.push_back(frame);
            }
        }

        if !logged_first_pcm {
            if !pcm_buf.is_empty() {
                logged_first_pcm = true;
                debug!(source_url = %source_url, first_pcm_ms = %playback_started.elapsed().as_millis(), "first pcm frame received");
            } else if playback_started.elapsed() >= Duration::from_secs(5) {
                return Err(anyhow!("no pcm received from ffmpeg"));
            }
        }

        if prebuffering {
            prebuffering = pcm_buf.len() < prebuffer_target;
        }

        let now = Instant::now();
        let dt = now.duration_since(last_tick);
        last_tick = now;
        let dt_ms = dt.as_millis();
        if dt_ms > tick_jitter_max_ms {
            tick_jitter_max_ms = dt_ms;
        }

        let mut got_real_frame = false;
        if !prebuffering {
            if let Some(frame) = pcm_buf.pop_front() {
                if frame.len() == frame_bytes {
                    pcm.copy_from_slice(&frame);
                    got_real_frame = true;
                }
            } else {
                match tokio::time::timeout(late_frame_wait, pcm_rx.recv()).await {
                    Ok(Some(frame)) => {
                        if frame.len() == frame_bytes {
                            pcm.copy_from_slice(&frame);
                            got_real_frame = true;
                        }
                    }
                    Ok(None) => {
                        break;
                    }
                    Err(_) => {}
                }
            }
        }

        if got_real_frame {
            underruns_consecutive = 0;
        } else {
            pcm.fill(0);
            underruns_total += 1;
            underruns_window += 1;
            underruns_consecutive += 1;
        }

        if logged_first_pcm && underruns_consecutive >= 150 {
            return Err(anyhow!(
                "sustained pcm underrun ({} frames, {} ms)",
                underruns_consecutive,
                underruns_consecutive * 20
            ));
        }

        if underruns_total > 0 && underruns_total % 50 == 0 {
            debug!(underruns_total = %underruns_total, "playback underrun (sending silence frames to keep cadence)");
        }

        let (vol, fx_pan, fx_width, fx_swap_lr, fx_bass_db, fx_reverb_mix) = {
            let st = status.lock().await;
            let r = (st.volume_percent as f32 / 100.0).clamp(0.0, 2.0);
            let vol = if r <= 1.0 { r.powf(1.6) } else { r };
            (
                vol,
                st.fx_pan.clamp(-1.0, 1.0),
                st.fx_width.clamp(0.0, 3.0),
                st.fx_swap_lr,
                st.fx_bass_db.clamp(0.0, 18.0),
                st.fx_reverb_mix.clamp(0.0, 1.0),
            )
        };

        for i in 0..(frame_samples_per_channel * channels) {
            let lo = pcm[i * 2];
            let hi = pcm[i * 2 + 1];
            let s = i16::from_le_bytes([lo, hi]) as f32;
            let v = (s / 32768.0) * vol;
            float_buf[i] = v;
        }

        if got_real_frame && fade_pos_samples_per_channel < fade_total_samples_per_channel {
            let denom = fade_total_samples_per_channel as f32;
            for i in 0..frame_samples_per_channel {
                let s = fade_pos_samples_per_channel + i;
                let g = ((s as f32) / denom).clamp(0.0, 1.0);
                let idx = i * 2;
                float_buf[idx] *= g;
                float_buf[idx + 1] *= g;
            }
            fade_pos_samples_per_channel = (fade_pos_samples_per_channel
                + frame_samples_per_channel)
                .min(fade_total_samples_per_channel);
        }

        let bass_gain = 10.0_f32.powf(fx_bass_db / 20.0);
        if (bass_gain - 1.0).abs() > 0.0001 || fx_reverb_mix > 0.0001 {
            for i in 0..frame_samples_per_channel {
                let idx = i * 2;
                let mut l = float_buf[idx];
                let mut r = float_buf[idx + 1];

                if (bass_gain - 1.0).abs() > 0.0001 {
                    bass_lp_l += bass_alpha * (l - bass_lp_l);
                    bass_lp_r += bass_alpha * (r - bass_lp_r);
                    let low_l = bass_lp_l;
                    let low_r = bass_lp_r;
                    l = (l - low_l) + low_l * bass_gain;
                    r = (r - low_r) + low_r * bass_gain;
                }

                let (l2, r2) = reverb.process_stereo(l, r, fx_reverb_mix);
                float_buf[idx] = l2;
                float_buf[idx + 1] = r2;
            }
        }

        if fx_swap_lr || (fx_width - 1.0).abs() > 0.0001 || fx_pan.abs() > 0.0001 {
            let pan = fx_pan;
            let (lg, rg) = if pan >= 0.0 {
                ((1.0 - pan).clamp(0.0, 1.0), 1.0)
            } else {
                (1.0, (1.0 + pan).clamp(0.0, 1.0))
            };
            for i in 0..frame_samples_per_channel {
                let idx = i * 2;
                let mut l = float_buf[idx];
                let mut r = float_buf[idx + 1];
                if fx_swap_lr {
                    std::mem::swap(&mut l, &mut r);
                }
                if (fx_width - 1.0).abs() > 0.0001 {
                    let m = 0.5 * (l + r);
                    let s = 0.5 * (l - r) * fx_width;
                    l = m + s;
                    r = m - s;
                }
                l *= lg;
                r *= rg;
                float_buf[idx] = l;
                float_buf[idx + 1] = r;

                let a_l = l.abs();
                let a_r = r.abs();
                let a = if a_l > a_r { a_l } else { a_r };
                if a > max_abs_sample {
                    max_abs_sample = a;
                }
                if a_l > 1.0 {
                    clipped_samples += 1;
                }
                if a_r > 1.0 {
                    clipped_samples += 1;
                }
            }
        } else {
            for i in 0..(frame_samples_per_channel * channels) {
                let v = float_buf[i];
                let a = v.abs();
                if a > max_abs_sample {
                    max_abs_sample = a;
                }
                if a > 1.0 {
                    clipped_samples += 1;
                }
            }
        }

        let len = encoder
            .encode_float(&float_buf, &mut opus_out)
            .map_err(|e| anyhow!("opus encode failed: {e}"))?;

        let packet = OutAudio::new(&AudioData::C2S {
            id: 0,
            codec: CodecType::OpusMusic,
            data: &opus_out[..len],
        });

        let _ = ts3_audio_tx.send(packet).await;

        if now >= diag_next {
            diag_next = now + Duration::from_secs(5);
            if underruns_window > 0 || clipped_samples > 0 || tick_jitter_max_ms > 25 {
                warn!(
                    source_url = %source_url,
                    underruns_total = %underruns_total,
                    underruns_window = %underruns_window,
                    tick_jitter_max_ms = %tick_jitter_max_ms,
                    clipped_samples = %clipped_samples,
                    max_abs_sample = %max_abs_sample,
                    "audio_encode_diag"
                );
            } else {
                debug!(
                    source_url = %source_url,
                    underruns_total = %underruns_total,
                    underruns_window = %underruns_window,
                    tick_jitter_max_ms = %tick_jitter_max_ms,
                    clipped_samples = %clipped_samples,
                    max_abs_sample = %max_abs_sample,
                    "audio_encode_diag"
                );
            }
            tick_jitter_max_ms = 0;
            clipped_samples = 0;
            max_abs_sample = 0.0;
            underruns_window = 0;
        }
    }

    let eos = OutAudio::new(&AudioData::C2S {
        id: 0,
        codec: CodecType::OpusMusic,
        data: &[],
    });
    let _ = ts3_audio_tx.send(eos).await;

    if let Some(mut c) = child.child.take() {
        let _ = c.start_kill();
        let _ = c.wait().await;
    }
    Ok(())
}

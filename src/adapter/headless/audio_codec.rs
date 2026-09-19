//! 48k 立体声 Opus 编解码公共几何与初始化，供 audio_output / speaker_ring 复用。

use anyhow::{anyhow, Result};
use audiopus::coder::{Decoder, Encoder};
use audiopus::{Application, Channels, SampleRate};

pub const SAMPLE_RATE_HZ: u32 = 48_000;
pub const CHANNELS: u16 = 2;
pub const PCM_FRAME_MS: u64 = 20;
pub const PCM_FRAME_SAMPLES_PER_CHANNEL: usize = SAMPLE_RATE_HZ as usize / 50;
pub const PCM_FRAME_SAMPLES_STEREO: usize = PCM_FRAME_SAMPLES_PER_CHANNEL * CHANNELS as usize;

pub fn new_opus_stereo_decoder() -> Result<Decoder> {
    Decoder::new(SampleRate::Hz48000, Channels::Stereo)
        .map_err(|e| anyhow!("opus stereo decoder init failed: {e}"))
}

pub fn new_opus_stereo_encoder() -> Result<Encoder> {
    Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
        .map_err(|e| anyhow!("opus stereo encoder init failed: {e}"))
}

pub fn pcm_stereo_48k_duration(sample_count: usize) -> std::time::Duration {
    std::time::Duration::from_millis(
        sample_count as u64 * 1000 / (SAMPLE_RATE_HZ as u64 * CHANNELS as u64),
    )
}

pub fn stereo_48k_sample_count(duration: std::time::Duration) -> usize {
    (duration.as_millis() as u64 * SAMPLE_RATE_HZ as u64 * CHANNELS as u64 / 1000) as usize
}

pub fn pcm_frame_to_float(frame: &[i16], out: &mut [f32]) {
    for (i, sample) in frame.iter().enumerate() {
        out[i] = *sample as f32 / 32768.0;
    }
}

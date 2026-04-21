use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use audiopus::coder::Decoder;
use audiopus::{Channels, SampleRate};
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde_json::Value;
use std::convert::TryInto;

use crate::config::AppConfig;

use super::tsbot::voice::v1 as voicev1;

pub struct SpeechChunk {
    pub speaker_client_id: u32,
    pub speaker_name: String,
    pub pcm16_mono_16k: Vec<i16>,
}

struct SpeakerState {
    decoder: Decoder,
    pcm16_mono_16k: Vec<i16>,
    speaking: bool,
    speech_ms: u64,
    silence_ms: u64,
}

pub struct OpusSttPipeline {
    vad_energy_threshold: f32,
    vad_silence_ms: u64,
    min_chunk_ms: u64,
    max_chunk_ms: u64,
    speakers: HashMap<u32, SpeakerState>,
}

impl OpusSttPipeline {
    pub fn new() -> Self {
        const VAD_ENERGY_THRESHOLD: f32 = 0.015;
        const VAD_SILENCE_MS: u64 = 600;
        const MIN_CHUNK_MS: u64 = 400;
        const MAX_CHUNK_MS: u64 = 12000;
        Self {
            vad_energy_threshold: VAD_ENERGY_THRESHOLD,
            vad_silence_ms: VAD_SILENCE_MS,
            min_chunk_ms: MIN_CHUNK_MS,
            max_chunk_ms: MAX_CHUNK_MS,
            speakers: HashMap::new(),
        }
    }

    pub fn process_audio_frame(
        &mut self,
        event: &voicev1::AudioFrameEvent,
    ) -> Result<Option<SpeechChunk>> {
        let codec = voicev1::audio_frame_event::Codec::try_from(event.codec)
            .unwrap_or(voicev1::audio_frame_event::Codec::Unspecified);
        if !matches!(
            codec,
            voicev1::audio_frame_event::Codec::OpusVoice
                | voicev1::audio_frame_event::Codec::OpusMusic
        ) {
            return Ok(None);
        }

        if !self.speakers.contains_key(&event.from_client_id) {
            let decoder = Decoder::new(SampleRate::Hz48000, Channels::Stereo)
                .map_err(|e| anyhow!("opus decoder init failed: {e}"))?;
            self.speakers.insert(
                event.from_client_id,
                SpeakerState {
                    decoder,
                    pcm16_mono_16k: Vec::new(),
                    speaking: false,
                    speech_ms: 0,
                    silence_ms: 0,
                },
            );
        }
        let state = self
            .speakers
            .get_mut(&event.from_client_id)
            .ok_or_else(|| anyhow!("speaker state missing"))?;

        let mut decoded = vec![0i16; 5760 * 2];
        let packet = (&event.frame)
            .try_into()
            .map_err(|e: audiopus::Error| anyhow!("opus packet invalid: {e}"))?;
        let decoded_mut = (&mut decoded)
            .try_into()
            .map_err(|e: audiopus::Error| anyhow!("opus output buffer invalid: {e}"))?;
        let samples_per_channel = state
            .decoder
            .decode(Some(packet), decoded_mut, false)
            .map_err(|e| anyhow!("opus decode failed: {e}"))?;

        if samples_per_channel == 0 {
            return Ok(None);
        }

        let stereo_samples = &decoded[..samples_per_channel * 2];
        let mono_16k = downsample_48k_stereo_to_16k_mono(stereo_samples);
        if mono_16k.is_empty() {
            return Ok(None);
        }

        let frame_ms = ((samples_per_channel as u64) * 1000 / 48000).max(1);
        let energy = normalized_average_abs(&mono_16k);
        let is_voiced = energy >= self.vad_energy_threshold;

        if is_voiced {
            state.speaking = true;
            state.silence_ms = 0;
            state.speech_ms = state.speech_ms.saturating_add(frame_ms);
            state.pcm16_mono_16k.extend_from_slice(&mono_16k);
        } else if state.speaking {
            state.silence_ms = state.silence_ms.saturating_add(frame_ms);
            if state.silence_ms <= self.vad_silence_ms {
                state.pcm16_mono_16k.extend_from_slice(&mono_16k);
            }
        }

        let should_flush = state.speaking
            && state.speech_ms >= self.min_chunk_ms
            && (state.silence_ms >= self.vad_silence_ms || state.speech_ms >= self.max_chunk_ms);

        if !should_flush {
            return Ok(None);
        }

        let chunk = SpeechChunk {
            speaker_client_id: event.from_client_id,
            speaker_name: event.from_client_name.clone(),
            pcm16_mono_16k: std::mem::take(&mut state.pcm16_mono_16k),
        };
        state.speaking = false;
        state.speech_ms = 0;
        state.silence_ms = 0;

        Ok(Some(chunk))
    }
}

pub struct OpenAiSpeechProvider {
    client: Client,
    config: std::sync::Arc<AppConfig>,
}

impl OpenAiSpeechProvider {
    pub fn new(config: std::sync::Arc<AppConfig>) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .context("build speech http client failed")?;
        Ok(Self { client, config })
    }

    pub async fn transcribe_wav(&self, wav_bytes: Vec<u8>) -> Result<String> {
        let stt = &self.config.headless.stt;
        if !stt.enabled {
            return Err(anyhow!("stt disabled"));
        }
        if stt.provider != "openai" {
            return Err(anyhow!("unsupported stt provider: {}", stt.provider));
        }

        let url = format!(
            "{}/audio/transcriptions",
            resolve_base_url(&stt.base_url, &self.config.llm.base_url)
        );
        let api_key = fallback_str(&stt.api_key, &self.config.llm.api_key);

        let mut form = Form::new().text("model", stt.model.clone());
        if !stt.language.is_empty() {
            form = form.text("language", stt.language.clone());
        }
        form = form.part(
            "file",
            Part::bytes(wav_bytes)
                .file_name("speech.wav")
                .mime_str("audio/wav")
                .context("set wav mime failed")?,
        );

        let resp = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {api_key}"))
            .multipart(form)
            .send()
            .await?;
        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            return Err(anyhow!("stt request failed: {err}"));
        }
        let data: Value = resp.json().await?;
        let text = data
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if text.is_empty() {
            return Err(anyhow!("stt returned empty text"));
        }
        Ok(text)
    }

    pub async fn synthesize(&self, text: &str) -> Result<Vec<u8>> {
        let tts = &self.config.headless.tts;
        if !tts.enabled {
            return Err(anyhow!("tts disabled"));
        }
        if tts.provider != "openai" {
            return Err(anyhow!("unsupported tts provider: {}", tts.provider));
        }

        let url = format!(
            "{}/audio/speech",
            resolve_base_url(&tts.base_url, &self.config.llm.base_url)
        );
        let api_key = fallback_str(&tts.api_key, &self.config.llm.api_key);

        let body = serde_json::json!({
            "model": tts.model,
            "input": text,
            "voice": tts.voice,
            "response_format": "mp3",
        });

        let resp = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            return Err(anyhow!("tts request failed: {err}"));
        }

        Ok(resp.bytes().await?.to_vec())
    }
}

pub fn pcm16_mono_to_wav_bytes(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let byte_rate: u32 = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
    let block_align: u16 = channels * (bits_per_sample / 8);
    let data_size: u32 = (samples.len() * 2) as u32;
    let chunk_size: u32 = 36 + data_size;

    let mut out = Vec::with_capacity((44 + data_size) as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&chunk_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

fn normalized_average_abs(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| (*s as f32).abs()).sum();
    (sum / samples.len() as f32) / 32768.0
}

fn downsample_48k_stereo_to_16k_mono(stereo: &[i16]) -> Vec<i16> {
    if stereo.len() < 6 {
        return Vec::new();
    }
    let frames = stereo.len() / 2;
    let mut mono_16k = Vec::with_capacity(frames / 3 + 1);
    let mut i = 0usize;
    while i + 2 < frames {
        let mut acc: i32 = 0;
        for j in 0..3 {
            let idx = (i + j) * 2;
            let l = stereo[idx] as i32;
            let r = stereo[idx + 1] as i32;
            acc += (l + r) / 2;
        }
        mono_16k.push((acc / 3).clamp(i16::MIN as i32, i16::MAX as i32) as i16);
        i += 3;
    }
    mono_16k
}

fn fallback_str<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() {
        fallback
    } else {
        value
    }
}

fn resolve_base_url(value: &str, fallback: &str) -> String {
    let selected = if value.is_empty() { fallback } else { value };
    selected.trim_end_matches('/').to_string()
}

pub fn preprocess_stt_text(
    raw: &str,
    cfg: &crate::config::headless::HeadlessSttConfig,
) -> Option<String> {
    const STT_TEXT_MAX_LEN: usize = 240;
    let mut text = normalize_text(raw);

    if text.is_empty() {
        return None;
    }

    let mut wake_hit = cfg.wake_words.is_empty();
    if !cfg.wake_words.is_empty() {
        let lower = text.to_ascii_lowercase();
        for wake in &cfg.wake_words {
            let wake = wake.trim().to_ascii_lowercase();
            if wake.is_empty() {
                continue;
            }
            if lower == wake {
                wake_hit = true;
                text.clear();
                break;
            }
            if let Some(rem) = lower.strip_prefix(&(wake.clone() + " ")) {
                let consumed = text.len() - rem.len();
                text = text[consumed..].trim_start().to_string();
                wake_hit = true;
                break;
            }
            if let Some(rem) = lower.strip_prefix(&(wake.clone() + ",")) {
                let consumed = text.len() - rem.len();
                text = text[consumed..].trim_start().to_string();
                wake_hit = true;
                break;
            }
            if let Some(rem) = lower.strip_prefix(&(wake.clone() + ":")) {
                let consumed = text.len() - rem.len();
                text = text[consumed..].trim_start().to_string();
                wake_hit = true;
                break;
            }
        }
    }

    if cfg.wake_word_required && !wake_hit {
        return None;
    }

    text = strip_leading_punct(&text);
    if text.is_empty() {
        return None;
    }

    if text.chars().count() > STT_TEXT_MAX_LEN {
        text = text.chars().take(STT_TEXT_MAX_LEN).collect();
    }
    Some(text)
}

pub fn preprocess_text_message(raw: &str) -> Option<String> {
    let text = normalize_text(raw);
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn normalize_text(raw: &str) -> String {
    let replaced = raw.replace(['\r', '\n', '\t'], " ");
    replaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_leading_punct(input: &str) -> String {
    input
        .trim_start_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_string()
}

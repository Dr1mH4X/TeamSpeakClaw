use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use audiopus::coder::Decoder;
use audiopus::{Channels, SampleRate};
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde_json::Value;
use tracing::{debug, error, warn};

use crate::config::AppConfig;
use base64::Engine;

use super::tsbot::voice::v1 as voicev1;

pub struct SpeechChunk {
    pub speaker_client_id: u32,
    pub speaker_name: String,
    pub speaker_uid: String,
    pub pcm16_mono_16k: Vec<i16>,
}

struct SpeakerState {
    decoder: Decoder,
    pcm16_mono_16k: Vec<i16>,
    speaking: bool,
    speech_ms: u64,
    silence_ms: u64,
    last_seen: Instant,
    /// 该 clid 当前的 UID；非空且与事件 UID 不一致时清空状态，防止 clid 复用串音
    uid: String,
    name: String,
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

    fn evict_idle_speakers(&mut self, now: Instant, active_id: u32) {
        const SPEAKER_IDLE_EVICT_AFTER_SECS: u64 = 300;
        self.speakers.retain(|client_id, state| {
            if *client_id == active_id {
                return true;
            }
            if state.speaking || !state.pcm16_mono_16k.is_empty() {
                return true;
            }
            now.duration_since(state.last_seen).as_secs() < SPEAKER_IDLE_EVICT_AFTER_SECS
        });
    }

    /// clid 复用且 UID 变化时，必须清空旧 decoder 与 PCM 防止串音
    fn should_reset_for_identity_change(state_uid: &str, event_uid: &str) -> bool {
        !event_uid.is_empty() && state_uid != event_uid
    }

    pub fn process_audio_frame(
        &mut self,
        event: &voicev1::AudioFrameEvent,
    ) -> Result<Option<SpeechChunk>> {
        let now = Instant::now();
        self.evict_idle_speakers(now, event.from_client_id);

        // TS3 协议 codec: 4=OPUS_VOICE, 5=OPUS_MUSIC
        if !matches!(event.codec, 4 | 5) {
            debug!(
                "跳过非Opus音频帧: codec={} frame_len={}",
                event.codec,
                event.frame.len()
            );
            return Ok(None);
        }

        if let std::collections::hash_map::Entry::Vacant(e) =
            self.speakers.entry(event.from_client_id)
        {
            let decoder = Decoder::new(SampleRate::Hz48000, Channels::Stereo)
                .map_err(|e| anyhow!("opus decoder init failed: {e}"))?;
            e.insert(SpeakerState {
                decoder,
                pcm16_mono_16k: Vec::new(),
                speaking: false,
                speech_ms: 0,
                silence_ms: 0,
                last_seen: now,
                uid: event.from_client_uid.clone(),
                name: event.from_client_name.clone(),
            });
        }
        let state = self
            .speakers
            .get_mut(&event.from_client_id)
            .ok_or_else(|| anyhow!("speaker state missing"))?;

        // clid 复用但 UID 变化时，清空旧 decoder 与 PCM，避免串音
        if Self::should_reset_for_identity_change(&state.uid, &event.from_client_uid) {
            warn!(
                clid = event.from_client_id,
                old_uid = %state.uid,
                new_uid = %event.from_client_uid,
                "speaker identity changed; resetting decoder and buffer"
            );
            state.decoder = Decoder::new(SampleRate::Hz48000, Channels::Stereo)
                .map_err(|e| anyhow!("opus decoder re-init failed: {e}"))?;
            state.pcm16_mono_16k.clear();
            state.speaking = false;
            state.speech_ms = 0;
            state.silence_ms = 0;
            state.uid = event.from_client_uid.clone();
        }

        state.last_seen = now;
        state.name = event.from_client_name.clone();

        let mut decoded = vec![0i16; 5760 * 2];
        let packet = match (&event.frame).try_into() {
            Ok(packet) => packet,
            Err(e) => {
                debug!(
                    clid = event.from_client_id,
                    error = %e,
                    "drop invalid opus packet"
                );
                return Ok(None);
            }
        };
        let decoded_mut = (&mut decoded)
            .try_into()
            .map_err(|e: audiopus::Error| anyhow!("opus output buffer invalid: {e}"))?;
        let samples_per_channel = match state.decoder.decode(Some(packet), decoded_mut, false) {
            Ok(samples) => samples,
            Err(e) => {
                debug!(
                    clid = event.from_client_id,
                    error = %e,
                    "drop undecodable opus frame"
                );
                return Ok(None);
            }
        };

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
            speaker_uid: event.from_client_uid.clone(),
            pcm16_mono_16k: std::mem::take(&mut state.pcm16_mono_16k),
        };
        state.speaking = false;
        state.speech_ms = 0;
        state.silence_ms = 0;

        Ok(Some(chunk))
    }

    /// 定时冲刷不活跃 speaker：达到最短语音长度则产出完整 utterance；
    /// 过短的突发噪音直接丢弃并复位；超空闲时间的 speaker 移除。
    /// 由外部每 100ms 调用一次，配合 VAD 尾音（PTT 松键后无尾帧）触发。
    pub fn drain_inactive(&mut self, now: Instant) -> Vec<SpeechChunk> {
        const IDLE_FLUSH_AFTER_MS: u64 = 600;
        const SPEAKER_IDLE_EVICT_AFTER_SECS: u64 = 300;

        let mut chunks = Vec::new();
        let mut discard_ids = Vec::new();
        for (client_id, state) in self.speakers.iter_mut() {
            if !state.speaking && state.pcm16_mono_16k.is_empty() {
                continue;
            }
            let idle_ms = now.duration_since(state.last_seen).as_millis() as u64;
            if idle_ms < IDLE_FLUSH_AFTER_MS {
                continue;
            }

            if state.speech_ms >= self.min_chunk_ms {
                // 达到最短语音长度：冲刷为完整 utterance
                chunks.push(SpeechChunk {
                    speaker_client_id: *client_id,
                    speaker_name: state.name.clone(),
                    speaker_uid: state.uid.clone(),
                    pcm16_mono_16k: std::mem::take(&mut state.pcm16_mono_16k),
                });
            } else {
                // 过短突发：视为噪音，丢弃并复位
                debug!(
                    clid = *client_id,
                    speech_ms = state.speech_ms,
                    "discard short audio burst without speech"
                );
                discard_ids.push(*client_id);
            }
            state.speaking = false;
            state.speech_ms = 0;
            state.silence_ms = 0;
        }

        if !discard_ids.is_empty() {
            for id in discard_ids {
                if let Some(state) = self.speakers.get_mut(&id) {
                    state.pcm16_mono_16k.clear();
                    state.uid.clear();
                    state.name.clear();
                }
            }
        }

        // 超空闲时间且无缓冲的 speaker 移除
        self.speakers.retain(|_, state| {
            if state.speaking || !state.pcm16_mono_16k.is_empty() {
                return true;
            }
            now.duration_since(state.last_seen).as_secs() < SPEAKER_IDLE_EVICT_AFTER_SECS
        });

        chunks
    }
}

pub struct OpenAiSpeechProvider {
    client: Client,
    config: std::sync::Arc<AppConfig>,
    tts_style_prompt: String,
}

impl OpenAiSpeechProvider {
    pub fn new(config: std::sync::Arc<AppConfig>, tts_style_prompt: String) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .context("build speech http client failed")?;
        Ok(Self {
            client,
            config,
            tts_style_prompt,
        })
    }

    pub async fn transcribe_wav(&self, wav_bytes: Vec<u8>) -> Result<String> {
        let stt = &self.config.headless.stt;
        if !stt.enabled {
            return Err(anyhow!("stt disabled"));
        }
        if !is_openai_compatible_provider(&stt.provider) {
            return Err(anyhow!("unsupported stt provider: {}", stt.provider));
        }

        let url = resolve_stt_url(&stt.base_url, &self.config.llm.base_url);
        let wav_size = wav_bytes.len();
        debug!(
            event = "headless.stt.request",
            url = %url,
            model = %stt.model,
            language = %stt.language,
            payload_bytes = wav_size,
            "sending stt request"
        );

        let api_key = resolve_speech_api_key(&stt.api_key, &stt.base_url, &self.config.llm.api_key);

        let mut form = Form::new();
        if !stt.model.is_empty() {
            form = form.text("model", stt.model.clone());
        }
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

        let mut request = self.client.post(url).multipart(form);
        if !api_key.is_empty() {
            request = request.bearer_auth(api_key);
        }
        let resp = request.send().await?;
        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            return Err(anyhow!("stt request failed: {err}"));
        }
        let body = resp.text().await?;
        let text = parse_stt_text(&body);
        if text.is_empty() {
            return Err(anyhow!("stt returned empty text"));
        }
        debug!(
            event = "headless.stt.response",
            text_len = text.chars().count(),
            "stt response received"
        );
        Ok(text)
    }

    pub async fn synthesize(&self, text: &str) -> Result<Vec<u8>> {
        let tts = &self.config.headless.tts;
        if !tts.enabled {
            error!("tts unavailable: tts disabled in config");
            return Err(anyhow!("tts disabled"));
        }

        let api_key = resolve_speech_api_key(&tts.api_key, &tts.base_url, &self.config.llm.api_key);

        // MiMo TTS: uses /chat/completions with audio field
        if tts.provider == "mimo" {
            return self.synthesize_mimo(text, api_key).await;
        }

        // OpenAI-compatible format
        if !is_openai_compatible_provider(&tts.provider) {
            error!("tts unavailable: unsupported provider {}", tts.provider);
            return Err(anyhow!("unsupported tts provider: {}", tts.provider));
        }

        let url = resolve_tts_url(&tts.base_url, &self.config.llm.base_url);

        let body = serde_json::json!({
            "model": tts.model.as_str(),
            "input": text,
            "voice": tts.voice.as_str(),
            "response_format": "mp3",
        });

        let mut request = self.client.post(url).json(&body);
        if !api_key.is_empty() {
            request = request.bearer_auth(api_key);
        }
        let resp = request.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            error!(
                "tts unavailable: request failed with status {}: {}",
                status, err
            );
            return Err(anyhow!("tts request failed: status {} - {}", status, err));
        }

        Ok(resp.bytes().await?.to_vec())
    }

    /// MiMo TTS: uses /chat/completions with messages + audio field
    async fn synthesize_mimo(&self, text: &str, api_key: &str) -> Result<Vec<u8>> {
        let tts = &self.config.headless.tts;
        // Resolve URL: use base_url or fallback to llm.base_url
        let base = if tts.base_url.is_empty() {
            &self.config.llm.base_url
        } else {
            &tts.base_url
        };
        let base = base.trim_end_matches('/');
        let url = format!("{}/chat/completions", base);

        let body = serde_json::json!({
            "model": tts.model,
            "messages": [
                {
                    "role": "user",
                    "content": self.tts_style_prompt
                },
                {
                    "role": "assistant",
                    "content": text
                }
            ],
            "audio": {
                "format": "wav",
                "voice": tts.voice
            }
        });

        let mut request = self.client.post(&url).json(&body);
        if !api_key.is_empty() {
            request = request.bearer_auth(api_key);
        }
        let resp = request.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            error!("mimo tts request failed with status {}: {}", status, err);
            return Err(anyhow!(
                "mimo tts request failed: status {} - {}",
                status,
                err
            ));
        }

        // MiMo TTS returns JSON response with base64-encoded audio data
        let resp_text = resp.text().await?;
        let resp_json: serde_json::Value = serde_json::from_str(&resp_text)
            .context("Failed to parse MiMo TTS response as JSON")?;

        // Extract base64 audio data from choices[0].message.audio.data
        let audio_data = resp_json
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("audio"))
            .and_then(|a| a.get("data"))
            .and_then(|d| d.as_str())
            .context("MiMo TTS response missing audio data field")?;

        // Decode base64 to raw audio bytes
        let audio_bytes = base64::prelude::BASE64_STANDARD
            .decode(audio_data)
            .context("Failed to decode base64 audio data")?;

        Ok(audio_bytes)
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

fn resolve_speech_api_key<'a>(
    service_api_key: &'a str,
    service_base_url: &str,
    llm_api_key: &'a str,
) -> &'a str {
    if !service_api_key.is_empty() {
        service_api_key
    } else if service_base_url.is_empty() {
        llm_api_key
    } else {
        ""
    }
}

fn resolve_base_url(value: &str, fallback: &str) -> String {
    let selected = if value.is_empty() { fallback } else { value };
    selected.trim_end_matches('/').to_string()
}

fn resolve_stt_url(value: &str, fallback: &str) -> String {
    let base = resolve_base_url(value, fallback);
    if base.ends_with("/audio/transcriptions") || base.ends_with("/inference") {
        base
    } else {
        format!("{base}/audio/transcriptions")
    }
}

fn resolve_tts_url(value: &str, fallback: &str) -> String {
    let base = resolve_base_url(value, fallback);
    if base.ends_with("/audio/speech") {
        base
    } else {
        format!("{base}/audio/speech")
    }
}

fn parse_stt_text(body: &str) -> String {
    if let Ok(data) = serde_json::from_str::<Value>(body) {
        if let Some(text) = data.get("text").and_then(|v| v.as_str()) {
            return text.trim().to_string();
        }
        if let Some(text) = data.get("result").and_then(|v| v.as_str()) {
            return text.trim().to_string();
        }
        if let Some(text) = data.as_str() {
            return text.trim().to_string();
        }
    }
    body.trim().to_string()
}

fn is_openai_compatible_provider(provider: &str) -> bool {
    provider == "openai-compatibility" || provider == "openai"
}

pub fn detect_audio_format(data: &[u8]) -> &'static str {
    if data.len() >= 4 && &data[0..4] == b"RIFF" {
        "wav"
    } else {
        "mp3"
    }
}

pub fn preprocess_stt_text(
    raw: &str,
    cfg: &crate::config::headless::HeadlessSttConfig,
) -> Option<String> {
    const STT_TEXT_MAX_LEN: usize = 240;
    const STT_MIN_CJK_LEN_WITHOUT_WAKE_WORD: usize = 4;
    let mut text = normalize_text(raw);

    if text.is_empty() {
        debug!(raw = %raw, "STT text is empty after normalization");
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
            if let Some(rem) = lower.strip_prefix(&wake) {
                let consumed = text.len() - rem.len();
                let after_wake = &text[consumed..];
                let after_trimmed = skip_punct_and_whitespace(after_wake);
                text = after_trimmed.to_string();
                wake_hit = true;
                break;
            }
        }
    }

    if cfg.wake_word_required && !wake_hit {
        debug!(
            text = %text,
            wake_words = ?cfg.wake_words,
            "STT wake word not found"
        );
        return None;
    }

    text = strip_leading_punct(&text);
    if text.is_empty() {
        debug!("STT text is empty after stripping leading punctuation");
        return None;
    }
    if !cfg.wake_word_required {
        let cjk_count = count_cjk_chars(&text);
        if cjk_count > 0 && cjk_count < STT_MIN_CJK_LEN_WITHOUT_WAKE_WORD {
            debug!(
                text = %text,
                cjk_count = cjk_count,
                min_len = STT_MIN_CJK_LEN_WITHOUT_WAKE_WORD,
                "STT text too short without wake word"
            );
            return None;
        }
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

fn skip_punct_and_whitespace(input: &str) -> &str {
    let cjk_punct = [
        '，', '。', '！', '？', '；', '：', '、', '—', '…', '（', '）', '【', '】', '《', '》',
        '“', '”', '‘', '’',
    ];
    input.trim_start_matches(|c: char| {
        c.is_whitespace() || c.is_ascii_punctuation() || cjk_punct.contains(&c)
    })
}

fn strip_leading_punct(input: &str) -> String {
    input
        .trim_start_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_string()
}

fn count_cjk_chars(input: &str) -> usize {
    input
        .chars()
        .filter(|c| {
            ('\u{4E00}'..='\u{9FFF}').contains(c)
                || ('\u{3400}'..='\u{4DBF}').contains(c)
                || ('\u{F900}'..='\u{FAFF}').contains(c)
        })
        .count()
}

pub fn is_speakable(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    if count_cjk_chars(text) > 0 {
        return true;
    }
    let total = text.chars().count();
    if total <= 3 {
        return true;
    }
    let garbled_chars = text
        .chars()
        .filter(|c| matches!(c, '+' | '/' | '='))
        .count();
    garbled_chars == 0 || total <= 8
}

#[cfg(test)]
mod tests {
    use super::{resolve_speech_api_key, OpusSttPipeline, SpeakerState};
    use audiopus::coder::Decoder;
    use audiopus::{Channels, SampleRate};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    #[test]
    fn explicit_speech_endpoint_does_not_inherit_llm_key() {
        assert_eq!(
            resolve_speech_api_key("", "https://speech.example/v1", "llm-secret"),
            ""
        );
    }

    #[test]
    fn llm_endpoint_fallback_inherits_llm_key() {
        assert_eq!(resolve_speech_api_key("", "", "llm-secret"), "llm-secret");
    }

    #[test]
    fn explicit_speech_key_takes_precedence() {
        assert_eq!(
            resolve_speech_api_key("speech-secret", "https://speech.example/v1", "llm-secret"),
            "speech-secret"
        );
    }

    #[test]
    fn same_uid_does_not_reset_speaker_state() {
        assert!(!OpusSttPipeline::should_reset_for_identity_change(
            "uid-a", "uid-a"
        ));
    }

    #[test]
    fn uid_change_resets_speaker_state() {
        assert!(OpusSttPipeline::should_reset_for_identity_change(
            "uid-a", "uid-b"
        ));
    }

    #[test]
    fn empty_event_uid_never_resets_speaker_state() {
        assert!(!OpusSttPipeline::should_reset_for_identity_change(
            "uid-a", ""
        ));
    }

    fn speaker_state(speech_ms: u64, speaking: bool, buffered: bool) -> SpeakerState {
        SpeakerState {
            decoder: Decoder::new(SampleRate::Hz48000, Channels::Stereo).unwrap(),
            pcm16_mono_16k: if buffered {
                vec![0i16; 160]
            } else {
                Vec::new()
            },
            speaking,
            speech_ms,
            silence_ms: 0,
            last_seen: Instant::now(),
            uid: "uid-a".to_string(),
            name: "speaker-a".to_string(),
        }
    }

    #[test]
    fn drain_inactive_flushes_long_tail_utterance() {
        let mut pipeline = OpusSttPipeline::new();
        pipeline.speakers = HashMap::from([(1, speaker_state(800, true, true))]);
        // 空闲超过 600ms 且语音长度达到 400ms 最短阈值 → 冲刷为完整 utterance
        let now = Instant::now() + Duration::from_millis(700);

        let chunks = pipeline.drain_inactive(now);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].speaker_client_id, 1);
        assert_eq!(chunks[0].speaker_uid, "uid-a");
        assert_eq!(chunks[0].speaker_name, "speaker-a");
    }

    #[test]
    fn drain_inactive_discards_short_burst() {
        let mut pipeline = OpusSttPipeline::new();
        // 200ms 突发低于 400ms 最短语音长度 → 丢弃并复位
        pipeline.speakers = HashMap::from([(1, speaker_state(200, true, true))]);
        let now = Instant::now() + Duration::from_millis(700);

        let chunks = pipeline.drain_inactive(now);

        assert!(chunks.is_empty());
        assert!(pipeline.speakers[&1].pcm16_mono_16k.is_empty());
        assert!(!pipeline.speakers[&1].speaking);
    }

    #[test]
    fn drain_inactive_ignores_fresh_speakers() {
        let mut pipeline = OpusSttPipeline::new();
        // 最近仍有活动的 speaker 不被冲刷
        pipeline.speakers = HashMap::from([(1, speaker_state(800, true, true))]);
        let now = Instant::now() + Duration::from_millis(100);

        let chunks = pipeline.drain_inactive(now);

        assert!(chunks.is_empty());
        assert_eq!(pipeline.speakers[&1].speech_ms, 800);
    }

    #[test]
    fn drain_inactive_evicts_long_idle_speakers() {
        let mut pipeline = OpusSttPipeline::new();
        // 无语音、无缓冲且空闲超过 300s → 移除
        pipeline.speakers = HashMap::from([(1, speaker_state(0, false, false))]);
        let now = Instant::now() + Duration::from_secs(301);

        pipeline.drain_inactive(now);

        assert!(pipeline.speakers.is_empty());
    }
}

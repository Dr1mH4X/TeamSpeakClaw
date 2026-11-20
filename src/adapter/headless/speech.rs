use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use reqwest::multipart::{Form, Part};
use reqwest::{Client, Response};
use serde_json::Value;
use tracing::{debug, error};

use super::audio_codec::OpusJitterDecoder;
use crate::config::AppConfig;
use base64::Engine;

use super::tsbot::voice::v1 as voicev1;

pub struct SpeechChunk {
    pub speaker_client_id: u32,
    pub speaker_name: String,
    pub pcm16_mono_16k: Vec<i16>,
}

/// 单帧音频的处理产物：唤醒门要在收帧路径上实时喂入，所以这里同时给出
/// 本帧的 16k 单声道 PCM 与 VAD 结果，以及本帧是否冲刷出完整 utterance。
pub struct SttFrameOutcome {
    /// 本帧解码后的 16k 单声道 PCM；无论是否落在 utterance 缓冲里都要喂唤醒门
    pub mono_16k: Vec<i16>,
    /// 本帧 VAD 判为活跃语音（命中唤醒词之后的活跃语音据此累计）
    pub voiced: bool,
    /// 本帧开启了新的 utterance 缓冲（本帧之前没有在飞的 utterance）
    pub utterance_started: bool,
    /// 本帧达到断句端点时冲刷出的完整 utterance
    pub chunk: Option<SpeechChunk>,
}

struct SpeakerState {
    decoder: OpusJitterDecoder,
    pcm16_mono_16k: Vec<i16>,
    speaking: bool,
    speech_ms: u64,
    silence_ms: u64,
    last_seen: Instant,
    name: String,
}

pub(crate) const VAD_ENERGY_THRESHOLD: f32 = 0.015;
/// 断句端点：连续静音（帧内 VAD）或连续无帧（对端停止发送）达到该时长即冲刷当前 utterance；
/// 两条路径共用同一常量，避免调其一时另一条仍在旧值上切句
const VAD_SILENCE_MS: u64 = 1200;
const MIN_CHUNK_MS: u64 = 400;
const MAX_CHUNK_MS: u64 = 12000;

pub struct OpusSttPipeline {
    speakers: HashMap<u32, SpeakerState>,
}

impl OpusSttPipeline {
    pub fn new() -> Self {
        Self {
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

    pub fn process_audio_frame(
        &mut self,
        event: &voicev1::AudioFrameEvent,
    ) -> Result<Option<SttFrameOutcome>> {
        self.process_audio_frame_at(event, Instant::now())
    }

    pub(crate) fn process_audio_frame_at(
        &mut self,
        event: &voicev1::AudioFrameEvent,
        now: Instant,
    ) -> Result<Option<SttFrameOutcome>> {
        self.evict_idle_speakers(now, event.from_client_id);

        // TS3 协议 codec: 4=OPUS_VOICE, 5=OPUS_MUSIC
        if !matches!(event.codec, 4 | 5) {
            debug!(
                "skipping non-Opus audio frame: codec={} frame_len={}",
                event.codec,
                event.frame.len()
            );
            return Ok(None);
        }

        let seq = event.seq as u16;
        // 0 字节尾包 / 1 字节控制包：仅在已有 speaker 上下文中更新抖动状态（清空重排/跳过），静默跳过解码与 PLC
        if event.frame.len() <= 1 {
            if let Some(state) = self.speakers.get_mut(&event.from_client_id) {
                state.last_seen = now;
                let _ = state.decoder.push(seq, &event.frame, now);
            }
            return Ok(None);
        }

        if let std::collections::hash_map::Entry::Vacant(e) =
            self.speakers.entry(event.from_client_id)
        {
            let decoder = OpusJitterDecoder::new_stereo()?;
            e.insert(SpeakerState {
                decoder,
                pcm16_mono_16k: Vec::new(),
                speaking: false,
                speech_ms: 0,
                silence_ms: 0,
                last_seen: now,
                name: event.from_client_name.clone(),
            });
        }
        let state = self
            .speakers
            .get_mut(&event.from_client_id)
            .ok_or_else(|| anyhow!("speaker state missing"))?;

        state.last_seen = now;
        state.name = event.from_client_name.clone();

        let decoded_frames = match state.decoder.push(seq, &event.frame, now) {
            Ok(frames) => frames,
            Err(e) => {
                debug!(
                    clid = event.from_client_id,
                    seq,
                    error = %e,
                    "drop undecodable opus frame"
                );
                return Ok(None);
            }
        };

        let mut combined_mono_16k = Vec::new();
        let mut any_voiced = false;
        let mut any_utterance_started = false;
        let mut flushed_chunk = None;

        for stereo_samples in decoded_frames {
            let samples_per_channel = stereo_samples.len() / 2;
            if samples_per_channel == 0 {
                continue;
            }
            let mono_16k = downsample_48k_stereo_to_16k_mono(&stereo_samples);
            if mono_16k.is_empty() {
                continue;
            }

            let frame_ms = ((samples_per_channel as u64) * 1000 / 48_000).max(1);
            let energy = normalized_average_abs(&mono_16k);
            let is_voiced = energy >= VAD_ENERGY_THRESHOLD;
            let utterance_started = is_voiced && !state.speaking;

            any_voiced |= is_voiced;
            any_utterance_started |= utterance_started;
            combined_mono_16k.extend_from_slice(&mono_16k);

            if is_voiced {
                state.speaking = true;
                state.silence_ms = 0;
                state.speech_ms = state.speech_ms.saturating_add(frame_ms);
                state.pcm16_mono_16k.extend_from_slice(&mono_16k);
            } else if state.speaking {
                state.silence_ms = state.silence_ms.saturating_add(frame_ms);
                if state.silence_ms <= VAD_SILENCE_MS {
                    state.pcm16_mono_16k.extend_from_slice(&mono_16k);
                }
            }

            let should_flush = state.speaking
                && state.speech_ms >= MIN_CHUNK_MS
                && (state.silence_ms >= VAD_SILENCE_MS || state.speech_ms >= MAX_CHUNK_MS);

            if should_flush {
                let chunk = SpeechChunk {
                    speaker_client_id: event.from_client_id,
                    speaker_name: event.from_client_name.clone(),
                    pcm16_mono_16k: std::mem::take(&mut state.pcm16_mono_16k),
                };
                state.speaking = false;
                state.speech_ms = 0;
                state.silence_ms = 0;
                flushed_chunk = Some(chunk);
            }
        }

        if combined_mono_16k.is_empty() {
            return Ok(None);
        }

        Ok(Some(SttFrameOutcome {
            mono_16k: combined_mono_16k,
            voiced: any_voiced,
            utterance_started: any_utterance_started,
            chunk: flushed_chunk,
        }))
    }

    /// 定时冲刷不活跃 speaker：达到最短语音长度则产出完整 utterance；
    /// 过短的突发噪音直接丢弃并复位；超空闲时间的 speaker 移除。
    /// 由外部每 100ms 调用一次，配合 VAD 尾音（PTT 松键后无尾帧）触发。
    pub fn drain_inactive(&mut self, now: Instant) -> Vec<SpeechChunk> {
        const SPEAKER_IDLE_EVICT_AFTER_SECS: u64 = 300;

        let mut chunks = Vec::new();
        let mut discard_ids = Vec::new();
        for (client_id, state) in self.speakers.iter_mut() {
            if !state.speaking && state.pcm16_mono_16k.is_empty() {
                continue;
            }
            let idle_ms = now.duration_since(state.last_seen).as_millis() as u64;
            if idle_ms < VAD_SILENCE_MS {
                continue;
            }

            if state.speech_ms >= MIN_CHUNK_MS {
                // 达到最短语音长度：冲刷为完整 utterance
                chunks.push(SpeechChunk {
                    speaker_client_id: *client_id,
                    speaker_name: state.name.clone(),
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

// STT 输入最长 12 秒；64 KiB 已容纳远超正常转写长度的 JSON 元数据。
const MAX_STT_RESPONSE_BYTES: usize = 64 * 1024;
// TTS 按最多 100 字句段请求；8 MiB 可容纳约 43 秒 48 kHz 双声道 PCM WAV。
const MAX_TTS_AUDIO_BYTES: usize = 8 * 1024 * 1024;
const MAX_TTS_BASE64_BYTES: usize = MAX_TTS_AUDIO_BYTES.div_ceil(3) * 4;
const MAX_MIMO_RESPONSE_BYTES: usize = MAX_TTS_BASE64_BYTES + 64 * 1024;

/// 不信任 Content-Length：在每块加入缓冲之前检查累计大小。
/// `api` 只进错误文案：三类语音请求共用这条读取路径，失败时要能分辨是谁
async fn read_speech_response(
    mut response: Response,
    max_bytes: usize,
    api: &str,
) -> Result<Vec<u8>> {
    if !response.status().is_success() {
        anyhow::bail!("{api} request failed: status {}", response.status());
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        anyhow::bail!("{api} response exceeds {max_bytes} byte limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > max_bytes - body.len() {
            anyhow::bail!("{api} response exceeds {max_bytes} byte limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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
        let body = read_speech_response(resp, MAX_STT_RESPONSE_BYTES, "stt").await?;
        let body = String::from_utf8(body).context("STT response is not UTF-8")?;
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

        // MiMo TTS：使用 /chat/completions 的 audio 字段
        if tts.provider == "mimo" {
            return self.synthesize_mimo(text, api_key).await;
        }

        // OpenAI 兼容格式
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
        read_speech_response(resp, MAX_TTS_AUDIO_BYTES, "tts").await
    }

    /// MiMo TTS：使用 /chat/completions 的 messages + audio 字段
    async fn synthesize_mimo(&self, text: &str, api_key: &str) -> Result<Vec<u8>> {
        let tts = &self.config.headless.tts;
        // 解析 URL：优先用 tts.base_url，为空则回退到 llm.base_url
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

        // MiMo TTS 返回含 base64 编码音频数据的 JSON 响应。
        let body = read_speech_response(resp, MAX_MIMO_RESPONSE_BYTES, "mimo tts").await?;
        let resp_json: serde_json::Value =
            serde_json::from_slice(&body).context("Failed to parse MiMo TTS response as JSON")?;

        // 从 choices[0].message.audio.data 提取 base64 音频数据
        let audio_data = resp_json
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("audio"))
            .and_then(|a| a.get("data"))
            .and_then(|d| d.as_str())
            .context("MiMo TTS response missing audio data field")?;

        if audio_data.len() > MAX_TTS_BASE64_BYTES {
            anyhow::bail!("MiMo TTS audio exceeds {MAX_TTS_AUDIO_BYTES} byte limit");
        }
        // 将 base64 解码为原始音频字节。
        let audio_bytes = base64::prelude::BASE64_STANDARD
            .decode(audio_data)
            .context("Failed to decode base64 audio data")?;

        if audio_bytes.len() > MAX_TTS_AUDIO_BYTES {
            anyhow::bail!("MiMo TTS audio exceeds {MAX_TTS_AUDIO_BYTES} byte limit");
        }
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

pub(crate) fn normalized_average_abs(samples: &[i16]) -> f32 {
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
    if base.ends_with("/audio/transcriptions")
        || base.ends_with("/inference")
        || base.ends_with("/asr/transcription")
    {
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

/// STT 文本清洗：归一化 → 剥前导标点 → 最小长度门 → 截断。
/// `wakeword_gate` 为 `[headless.wakeword]` 启用状态：门开着时语音已在音频层过唤醒筛选，
/// 不再按最小长度丢弃短句
pub fn preprocess_stt_text(raw: &str, wakeword_gate: bool) -> Option<String> {
    const STT_TEXT_MAX_LEN: usize = 240;
    const STT_MIN_CJK_LEN: usize = 4;
    let mut text = normalize_text(raw);

    if text.is_empty() {
        debug!(raw = %raw, "STT text is empty after normalization");
        return None;
    }

    text = strip_leading_punct(&text);
    if text.is_empty() {
        debug!("STT text is empty after stripping leading punctuation");
        return None;
    }
    if !wakeword_gate {
        let cjk_count = count_cjk_chars(&text);
        if cjk_count > 0 && cjk_count < STT_MIN_CJK_LEN {
            debug!(
                text = %text,
                cjk_count = cjk_count,
                min_len = STT_MIN_CJK_LEN,
                "STT text too short without wakeword gate"
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

fn is_readable_char(c: char) -> bool {
    c.is_alphanumeric()
}

fn is_unreadable_emoji_or_symbol(c: char) -> bool {
    matches!(
        c,
        '\u{200D}'
            | '\u{2022}'..='\u{2023}'
            | '\u{2043}'
            | '\u{20E3}'
            | '\u{231A}'..='\u{231B}'
            | '\u{23E9}'..='\u{23F3}'
            | '\u{23F8}'..='\u{23FA}'
            | '\u{2500}'..='\u{2BFF}'
            | '\u{FE0E}'..='\u{FE0F}'
            | '\u{1F000}'..='\u{1FAFF}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

fn strip_markdown_line_prefix(raw_line: &str) -> &str {
    let mut line = raw_line.trim_start();
    loop {
        if let Some(rest) = line.strip_prefix('>') {
            line = rest.trim_start();
            continue;
        }
        let hashes = line.bytes().take_while(|b| *b == b'#').count();
        if hashes > 0 {
            let after_hashes = &line[hashes..];
            if after_hashes.is_empty() || after_hashes.starts_with(char::is_whitespace) {
                line = after_hashes.trim_start();
                continue;
            }
        }
        if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("-\t")) {
            line = rest.trim_start();
            continue;
        }
        if let Some(rest) = line.strip_prefix("* ").or_else(|| line.strip_prefix("*\t")) {
            line = rest.trim_start();
            continue;
        }
        if line == "-" || line == "*" {
            return "";
        }
        break;
    }
    line
}

fn is_link_target(target: &str) -> bool {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || trimmed.starts_with('/')
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
        || trimmed.starts_with('#')
        || lower.starts_with("mailto:")
    {
        return true;
    }
    if !trimmed.contains(char::is_whitespace) {
        return true;
    }
    trimmed
        .split_once(char::is_whitespace)
        .is_some_and(|(url_part, title)| {
            !url_part.is_empty() && title.trim_start().starts_with('"')
        })
}

fn strip_markdown_links(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < input.len() {
        let rest = &input[i..];
        let at_image = rest.starts_with("![")
            && (i == 0
                || input[..i]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace));
        let link_start = if at_image {
            Some(i + 1)
        } else if rest.starts_with('[') {
            Some(i)
        } else {
            None
        };

        if let Some(open_bracket) = link_start {
            let after_open = &input[open_bracket + 1..];
            if let Some(close_rel) = after_open.find(']') {
                let label = &after_open[..close_rel];
                let after_bracket = open_bracket + 1 + close_rel + 1;
                if !label.contains('\n') && input[after_bracket..].starts_with('(') {
                    let paren_content_start = after_bracket + 1;
                    let mut depth = 1usize;
                    let mut close_paren = None;
                    for (rel, ch) in input[paren_content_start..].char_indices() {
                        if ch == '\n' {
                            break;
                        }
                        if ch == '(' {
                            depth += 1;
                        } else if ch == ')' {
                            depth -= 1;
                            if depth == 0 {
                                close_paren = Some(paren_content_start + rel);
                                break;
                            }
                        }
                    }
                    if let Some(close_idx) = close_paren {
                        let target = &input[paren_content_start..close_idx];
                        if is_link_target(target) {
                            out.push_str(label);
                            i = close_idx + 1;
                            continue;
                        }
                    }
                }
            }
        }

        let ch = rest.chars().next().expect("non-empty slice");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn find_bare_url_end(slice: &str, scheme_len: usize) -> usize {
    let mut end = scheme_len;
    let mut open_parens = 0usize;
    for (rel, ch) in slice[scheme_len..].char_indices() {
        if ch.is_whitespace()
            || !ch.is_ascii()
            || matches!(ch, '<' | '>' | '"' | '\'' | '`' | '[' | ']' | '{' | '}')
        {
            break;
        }
        if ch == '(' {
            open_parens += 1;
        } else if ch == ')' {
            if open_parens == 0 {
                break;
            }
            open_parens -= 1;
        }
        end = scheme_len + rel + ch.len_utf8();
    }
    while end > scheme_len
        && matches!(
            slice.as_bytes()[end - 1],
            b'.' | b',' | b'!' | b'?' | b';' | b':'
        )
    {
        end -= 1;
    }
    end
}

fn strip_bare_urls(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < input.len() {
        let rest = &input[i..];
        let scheme_len = if rest
            .get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
        {
            Some(8)
        } else if rest
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
        {
            Some(7)
        } else {
            None
        };
        if let Some(slen) = scheme_len {
            let url_end = find_bare_url_end(rest, slen);
            let tail = &rest[url_end..];
            let needs_space = !out.is_empty()
                && !out.ends_with(char::is_whitespace)
                && !tail.is_empty()
                && !tail.starts_with(char::is_whitespace)
                && !tail.starts_with(|c: char| {
                    matches!(
                        c,
                        '.' | ','
                            | '!'
                            | '?'
                            | ';'
                            | ':'
                            | ')'
                            | ']'
                            | '。'
                            | '，'
                            | '！'
                            | '？'
                            | '；'
                            | '：'
                            | '、'
                            | '）'
                            | '】'
                    )
                });
            if needs_space {
                out.push(' ');
            }
            i += url_end;
            continue;
        }
        let ch = rest.chars().next().expect("non-empty slice");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn strip_inline_modifiers_and_emoji(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < input.len() {
        let rest = &input[i..];
        if rest.starts_with("**") || rest.starts_with("__") || rest.starts_with("~~") {
            i += 2;
            continue;
        }
        if rest.starts_with('*') || rest.starts_with('`') {
            i += 1;
            continue;
        }
        let ch = rest.chars().next().expect("non-empty slice");
        if !is_unreadable_emoji_or_symbol(ch) && (!ch.is_control() || ch.is_whitespace()) {
            out.push(ch);
        }
        i += ch.len_utf8();
    }
    remove_empty_brackets(&out)
}

fn remove_empty_brackets(input: &str) -> String {
    let mut s = input.split_whitespace().collect::<Vec<_>>().join(" ");
    loop {
        let next = s
            .replace("()", "")
            .replace("( )", "")
            .replace("（）", "")
            .replace("（ )", "")
            .replace("[]", "")
            .replace("[ ]", "")
            .replace("【】", "")
            .replace("【 】", "")
            .replace("{}", "")
            .replace("{ }", "");
        let collapsed = next.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed == s {
            return collapsed;
        }
        s = collapsed;
    }
}

fn filter_isolated_symbols(line: &str) -> String {
    let kept: Vec<&str> = line
        .split_whitespace()
        .filter(|tok| tok.chars().any(is_readable_char) || matches!(*tok, "+" | "="))
        .collect();
    let joined = kept.join(" ");
    if joined.chars().any(is_readable_char) {
        joined
    } else {
        String::new()
    }
}

/// 清洗送往 TTS 的文本：剥离 Markdown 修饰、链接目标、裸 URL、Emoji 与孤立纯符号。
pub fn sanitize_for_tts(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut cleaned_lines = Vec::new();
    for raw_line in normalized.split('\n') {
        let line = strip_markdown_line_prefix(raw_line);
        if line.is_empty() {
            continue;
        }
        let without_links = strip_markdown_links(line);
        let without_urls = strip_bare_urls(&without_links);
        let without_md = strip_inline_modifiers_and_emoji(&without_urls);
        let filtered = filter_isolated_symbols(&without_md);
        if !filtered.is_empty() {
            cleaned_lines.push(filtered);
        }
    }
    let joined = cleaned_lines.join(" ");
    if joined.chars().any(is_readable_char) {
        joined
    } else {
        String::new()
    }
}

pub fn is_speakable(text: &str) -> bool {
    let cleaned = sanitize_for_tts(text);
    if cleaned.is_empty() || !cleaned.chars().any(is_readable_char) {
        return false;
    }
    if count_cjk_chars(&cleaned) > 0 {
        return true;
    }
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    if tokens.is_empty() {
        return false;
    }
    let all_garbled = tokens.iter().all(|tok| {
        tok.chars().count() > 8
            && tok.chars().any(|c| matches!(c, '+' | '/' | '='))
            && count_cjk_chars(tok) == 0
    });
    !all_garbled
}

#[cfg(test)]
mod tests {
    use super::{
        preprocess_stt_text, resolve_speech_api_key, resolve_stt_url, voicev1, OpusJitterDecoder,
        OpusSttPipeline, SpeakerState, VAD_SILENCE_MS,
    };
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    async fn mock_speech_server(
        status: &str,
        chunk: Vec<u8>,
        count: usize,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let received = socket.read(&mut buffer).await.unwrap();
                assert!(received > 0);
                request.extend_from_slice(&buffer[..received]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let headers = format!(
                "HTTP/1.1 {status}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            for _ in 0..count {
                if socket
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await
                    .is_err()
                    || socket.write_all(&chunk).await.is_err()
                    || socket.write_all(b"\r\n").await.is_err()
                {
                    return;
                }
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        });
        (format!("http://{address}"), server)
    }

    fn speech_provider(base_url: String, provider: &str) -> super::OpenAiSpeechProvider {
        let mut config = crate::config::AppConfig::default();
        config.headless.stt.enabled = true;
        config.headless.stt.base_url = base_url.clone();
        config.headless.tts.enabled = true;
        config.headless.tts.base_url = base_url;
        config.headless.tts.provider = provider.to_string();
        super::OpenAiSpeechProvider {
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            config: std::sync::Arc::new(config),
            tts_style_prompt: "style".to_string(),
        }
    }

    #[tokio::test]
    async fn speech_apis_reject_chunked_responses_over_their_byte_limits() {
        for (api, limit) in [
            ("stt", super::MAX_STT_RESPONSE_BYTES),
            ("openai", super::MAX_TTS_AUDIO_BYTES),
            ("mimo", super::MAX_MIMO_RESPONSE_BYTES),
        ] {
            let chunk = vec![b'x'; 16 * 1024];
            let (url, server) =
                mock_speech_server("200 OK", chunk.clone(), limit / chunk.len() + 1).await;
            let provider = speech_provider(url, if api == "mimo" { "mimo" } else { "openai" });
            let error = if api == "stt" {
                provider.transcribe_wav(vec![0; 44]).await.unwrap_err()
            } else {
                provider.synthesize("hello").await.unwrap_err()
            };
            assert!(error.to_string().contains("byte limit"), "{api}: {error}");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn mimo_rejects_decoded_audio_over_the_limit_at_base64_boundary() {
        use base64::Engine;
        // 上限和上限 + 1 的 base64 长度相同，仍须校验实际解码长度。
        let encoded =
            base64::prelude::BASE64_STANDARD.encode(vec![0u8; super::MAX_TTS_AUDIO_BYTES + 1]);
        assert_eq!(encoded.len(), super::MAX_TTS_BASE64_BYTES);
        let body = serde_json::json!({
            "choices": [{"message": {"audio": {"data": encoded}}}]
        })
        .to_string()
        .into_bytes();
        let (url, server) = mock_speech_server("200 OK", body, 1).await;
        let error = speech_provider(url, "mimo")
            .synthesize("hello")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("audio exceeds"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn speech_api_errors_do_not_expose_response_bodies() {
        for api in ["stt", "openai", "mimo"] {
            let (url, server) = mock_speech_server(
                "401 Unauthorized",
                b"secret-provider-diagnostic".to_vec(),
                1,
            )
            .await;
            let provider = speech_provider(url, if api == "mimo" { "mimo" } else { "openai" });
            let error = if api == "stt" {
                provider.transcribe_wav(vec![0; 44]).await.unwrap_err()
            } else {
                provider.synthesize("hello").await.unwrap_err()
            };
            assert!(error.to_string().contains("401"), "{api}: {error}");
            assert!(!format!("{error:#}").contains("secret-provider-diagnostic"));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn speech_apis_preserve_valid_responses() {
        for api in ["stt", "openai", "mimo"] {
            let body = match api {
                "stt" => br#"{"text":"hello"}"#.to_vec(),
                "mimo" => br#"{"choices":[{"message":{"audio":{"data":"UklGRg=="}}}]}"#.to_vec(),
                _ => b"RIFF".to_vec(),
            };
            let (url, server) = mock_speech_server("200 OK", body, 1).await;
            let provider = speech_provider(url, if api == "mimo" { "mimo" } else { "openai" });
            if api == "stt" {
                assert_eq!(provider.transcribe_wav(vec![0; 44]).await.unwrap(), "hello");
            } else {
                assert_eq!(provider.synthesize("hello").await.unwrap(), b"RIFF");
            }
            server.await.unwrap();
        }
    }

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
    fn stt_url_appends_transcriptions_to_plain_base() {
        assert_eq!(
            resolve_stt_url("https://speech.example/v1", ""),
            "https://speech.example/v1/audio/transcriptions"
        );
    }

    #[test]
    fn stt_url_keeps_whisper_inference_and_openai_paths() {
        assert_eq!(
            resolve_stt_url("http://localhost:9000/inference", ""),
            "http://localhost:9000/inference"
        );
        assert_eq!(
            resolve_stt_url("https://speech.example/v1/audio/transcriptions", ""),
            "https://speech.example/v1/audio/transcriptions"
        );
    }

    #[test]
    fn stt_url_keeps_asr_transcription_path() {
        // 百炼/DashScope 异步转写路径：不得再追加 /audio/transcriptions
        assert_eq!(
            resolve_stt_url(
                "https://ws-demo.cn-beijing.maas.aliyuncs.com/compatible-mode/v1/services/audio/asr/transcription",
                ""
            ),
            "https://ws-demo.cn-beijing.maas.aliyuncs.com/compatible-mode/v1/services/audio/asr/transcription"
        );
        assert_eq!(
            resolve_stt_url(
                "https://dashscope.aliyuncs.com/api/v1/services/audio/asr/transcription/",
                ""
            ),
            "https://dashscope.aliyuncs.com/api/v1/services/audio/asr/transcription"
        );
    }

    fn speaker_state(speech_ms: u64, speaking: bool, buffered: bool) -> SpeakerState {
        SpeakerState {
            decoder: OpusJitterDecoder::new_stereo().unwrap(),
            pcm16_mono_16k: if buffered {
                vec![0i16; 160]
            } else {
                Vec::new()
            },
            speaking,
            speech_ms,
            silence_ms: 0,
            last_seen: Instant::now(),
            name: "speaker-a".to_string(),
        }
    }

    #[test]
    fn drain_inactive_flushes_long_tail_utterance() {
        let mut pipeline = OpusSttPipeline::new();
        pipeline.speakers = HashMap::from([(1, speaker_state(800, true, true))]);
        // 空闲超过断句端点且语音长度达到 400ms 最短阈值 → 冲刷为完整 utterance
        let now = Instant::now() + Duration::from_millis(VAD_SILENCE_MS + 100);

        let chunks = pipeline.drain_inactive(now);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].speaker_client_id, 1);
        assert_eq!(chunks[0].speaker_name, "speaker-a");
    }

    #[test]
    fn drain_inactive_discards_short_burst() {
        let mut pipeline = OpusSttPipeline::new();
        // 200ms 突发低于 400ms 最短语音长度 → 丢弃并复位
        pipeline.speakers = HashMap::from([(1, speaker_state(200, true, true))]);
        let now = Instant::now() + Duration::from_millis(VAD_SILENCE_MS + 100);

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

    /// 单帧产物带出本帧 PCM 与 VAD 结果：唤醒门要在收帧路径上实时喂入
    #[test]
    fn frame_outcome_carries_pcm_and_vad_state() {
        let mut pipeline = OpusSttPipeline::new();

        let silence = pipeline
            .process_audio_frame(&audio_event(0, opus_frame_bytes(0.0)))
            .expect("decode silence")
            .expect("silence frame produces an outcome");
        assert_eq!(silence.mono_16k.len(), 320, "20ms @16k mono");
        assert!(!silence.voiced);
        assert!(!silence.utterance_started);
        assert!(silence.chunk.is_none());

        // 活跃语音开启新 utterance；不足最短长度时不冲刷
        let speech = pipeline
            .process_audio_frame(&audio_event(1, opus_frame_bytes(0.9)))
            .expect("decode speech")
            .expect("speech frame produces an outcome");
        assert!(speech.voiced);
        assert!(speech.utterance_started);
        assert!(speech.chunk.is_none());
        assert!(!pipeline.speakers[&7].pcm16_mono_16k.is_empty());
    }

    #[test]
    fn one_byte_and_zero_byte_packets_skip_decode_and_clear_reorder_buffer() {
        let mut pipeline = OpusSttPipeline::new();
        let t0 = Instant::now();

        // 先送正常语音建立说话人状态
        assert!(pipeline
            .process_audio_frame_at(&audio_event(10, opus_frame_bytes(0.8)), t0)
            .unwrap()
            .is_some());
        // 1 字节包静默跳过（推进 expected_seq 不误触发 PLC）
        assert!(pipeline
            .process_audio_frame_at(&audio_event(11, vec![0x02]), t0 + Duration::from_millis(20),)
            .unwrap()
            .is_none());
        // 先塞入一帧乱序未来包（seq=13），再收到 0 字节结束尾包 -> 清空待重排缓冲、无 PLC
        assert!(pipeline
            .process_audio_frame_at(
                &audio_event(13, opus_frame_bytes(0.8)),
                t0 + Duration::from_millis(40),
            )
            .unwrap()
            .is_none());
        assert_eq!(pipeline.speakers[&7].decoder.pending_len(), 1);

        assert!(pipeline
            .process_audio_frame_at(&audio_event(14, Vec::new()), t0 + Duration::from_millis(60))
            .unwrap()
            .is_none());
        let stats = pipeline.speakers[&7].decoder.stats();
        assert_eq!(pipeline.speakers[&7].decoder.pending_len(), 0);
        assert_eq!(stats.plc_frames, 0);
        assert_eq!(stats.marker_skipped, 1);
        assert_eq!(stats.eos_resets, 1);
    }

    #[test]
    fn jitter_reorder_and_loss_recovery_keep_stt_16k_stream_continuous() {
        let mut pipeline = OpusSttPipeline::new();
        let t0 = Instant::now();
        let pkt = opus_frame_bytes(0.8);

        assert_eq!(
            pipeline
                .process_audio_frame_at(&audio_event(0, pkt.clone()), t0)
                .unwrap()
                .unwrap()
                .mono_16k
                .len(),
            320
        );
        // 丢 1，乱序到 2, 3, 4 -> 一次性输出恢复后的连续 4 帧（1280 样本 @16kHz）
        assert!(pipeline
            .process_audio_frame_at(&audio_event(2, pkt.clone()), t0 + Duration::from_millis(20))
            .unwrap()
            .is_none());
        assert!(pipeline
            .process_audio_frame_at(&audio_event(3, pkt.clone()), t0 + Duration::from_millis(40))
            .unwrap()
            .is_none());
        let recovered = pipeline
            .process_audio_frame_at(&audio_event(4, pkt), t0 + Duration::from_millis(60))
            .unwrap()
            .expect("recovered 4 frames");
        assert_eq!(recovered.mono_16k.len(), 320 * 4);
    }

    fn audio_event(seq: u32, frame: Vec<u8>) -> voicev1::AudioFrameEvent {
        voicev1::AudioFrameEvent {
            from_client_id: 7,
            from_client_name: "speaker-a".to_string(),
            codec: 4,
            frame,
            seq,
        }
    }

    fn opus_frame_bytes(amplitude: f32) -> Vec<u8> {
        let mut encoder =
            crate::adapter::headless::audio_codec::OpusEncoder::new_stereo().expect("opus encoder");
        let samples_per_channel =
            crate::adapter::headless::audio_codec::PCM_FRAME_SAMPLES_PER_CHANNEL;
        // 1kHz 正弦波避开直流信号被不同版本编码器的高通滤波衰减
        let mut pcm = Vec::with_capacity(samples_per_channel * 2);
        for i in 0..samples_per_channel {
            let t = i as f32 / 48_000.0;
            let sample = amplitude * (2.0 * std::f32::consts::PI * 1000.0 * t).sin();
            pcm.push(sample);
            pcm.push(sample);
        }
        let mut out = [0u8; crate::adapter::headless::audio_codec::MAX_VOICE_PACKET_BYTES];
        let encoded = encoder
            .encode_frame(&pcm, &mut out)
            .expect("opus encode frame");
        out[..encoded].to_vec()
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

    #[test]
    fn preprocess_stt_text_normalizes_and_strips_leading_punctuation() {
        let text = preprocess_stt_text("  播放\t周杰伦\n的夜曲 ", false);
        assert_eq!(text, Some("播放 周杰伦 的夜曲".to_string()));

        let text = preprocess_stt_text("!? 播放音乐", false);
        assert_eq!(text, Some("播放音乐".to_string()));
    }

    #[test]
    fn preprocess_stt_text_drops_short_cjk_only_without_wakeword_gate() {
        assert_eq!(preprocess_stt_text("嗯", false), None);
        // 唤醒门启用时语音已在音频层筛选，短句原样放行
        assert_eq!(preprocess_stt_text("嗯", true), Some("嗯".to_string()));
        assert_eq!(
            preprocess_stt_text("播放音乐", false),
            Some("播放音乐".to_string())
        );
    }

    #[test]
    fn preprocess_stt_text_truncates_to_240_chars() {
        let raw = "夜".repeat(300);
        let text = preprocess_stt_text(&raw, false).expect("long text passes");
        assert_eq!(text.chars().count(), 240);
    }

    #[test]
    fn sanitize_for_tts_strips_markdown_links_urls_emoji_and_isolated_symbols() {
        use super::{is_speakable, sanitize_for_tts};

        assert_eq!(
            sanitize_for_tts(
                "### 标题\n- **重点**内容\n* *斜体*与__下划线__\n> ~~删除线~~和`cargo test`"
            ),
            "标题 重点内容 斜体与下划线 删除线和cargo test"
        );
        assert_eq!(
            sanitize_for_tts("详情见[官方文档](https://example.com/docs_(v1))，或者访问 https://example.com/a?x=1 查看。"),
            "详情见官方文档，或者访问 查看。"
        );
        assert_eq!(
            sanitize_for_tts("太棒了👍🎉！ 请看（https://example.com） --- 完毕 | OK"),
            "太棒了！ 请看 完毕 OK"
        );
        assert_eq!(
            sanitize_for_tts("[https://example.com](https://example.com)"),
            ""
        );
        assert_eq!(sanitize_for_tts("--- *** ... ！？ 👍"), "");
        assert!(!is_speakable("--- ... ！？"));
        assert!(!is_speakable("👍🎉✨"));
        assert!(!is_speakable("https://example.com"));
        assert!(is_speakable("3.14"));
        assert!(is_speakable("1 + 2 = 3"));
    }
}

//! TeamSpeak 音频处理模块
//! 
//! 使用外部 ffmpeg 进行 Opus 编解码

#[cfg(feature = "audio")]
use std::io::Write;
#[cfg(feature = "audio")]
use std::path::PathBuf;
#[cfg(feature = "audio")]
use std::process::{Command, Stdio};
#[cfg(feature = "audio")]
use std::sync::Arc;
#[cfg(feature = "audio")]
use tokio::sync::{mpsc, RwLock};
#[cfg(feature = "audio")]
use tracing::{debug, error, info, warn};

/// 音频配置
#[cfg(feature = "audio")]
#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// ffmpeg 可执行文件路径
    pub ffmpeg_path: String,
    /// 采样率 (Hz)
    pub sample_rate: u32,
    /// 声道数
    pub channels: u16,
    /// 帧大小 (ms)
    pub frame_size_ms: u32,
    /// 比特率 (bps)
    pub bitrate: u32,
}

#[cfg(feature = "audio")]
impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            ffmpeg_path: "ffmpeg".to_string(),
            sample_rate: 48000,
            channels: 1,
            frame_size_ms: 20,
            bitrate: 48000,
        }
    }
}

/// 音频帧
#[cfg(feature = "audio")]
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// PCM 数据 (16-bit signed)
    pub pcm: Vec<i16>,
    /// 客户端 ID
    pub client_id: Option<u16>,
    /// 通道 ID
    pub channel_id: Option<u32>,
}

/// 音频错误
#[cfg(feature = "audio")]
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("FFmpeg not found: {0}")]
    FfmpegNotFound(String),
    
    #[error("FFmpeg execution failed: {0}")]
    ExecutionFailed(String),
    
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    
    #[error("Invalid data")]
    InvalidData,
}

/// FFmpeg 编码器
#[cfg(feature = "audio")]
pub struct FfmpegEncoder {
    config: AudioConfig,
}

#[cfg(feature = "audio")]
impl FfmpegEncoder {
    /// 创建新的编码器
    pub fn new(config: AudioConfig) -> Result<Self, AudioError> {
        // 检查 ffmpeg 是否可用
        Self::check_ffmpeg(&config.ffmpeg_path)?;
        Ok(Self { config })
    }

    /// 检查 ffmpeg 是否可用
    fn check_ffmpeg(path: &str) -> Result<(), AudioError> {
        let output = Command::new(path)
            .arg("-version")
            .output()
            .map_err(|e| AudioError::FfmpegNotFound(format!("{path}: {e}")))?;
        
        if !output.status.success() {
            return Err(AudioError::FfmpegNotFound(format!("{path}: command failed")));
        }
        
        Ok(())
    }

    /// 编码 PCM 数据为 Opus
    pub fn encode(&self, pcm: &[i16]) -> Result<Vec<u8>, AudioError> {
        // 将 i16 PCM 转换为字节
        let pcm_bytes: Vec<u8> = pcm.iter()
            .flat_map(|&s| s.to_le_bytes())
            .collect();
        
        let mut child = Command::new(&self.config.ffmpeg_path)
            .args([
                "-f", "s16le",
                "-ar", &self.config.sample_rate.to_string(),
                "-ac", &self.config.channels.to_string(),
                "-i", "pipe:0",
                "-c:a", "libopus",
                "-b:a", &format!("{}k", self.config.bitrate / 1000),
                "-frame_duration", &self.config.frame_size_ms.to_string(),
                "-f", "opus",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| AudioError::ExecutionFailed(format!("Failed to start ffmpeg: {e}")))?;

        // 写入 PCM 数据
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&pcm_bytes)
                .map_err(|e| AudioError::ExecutionFailed(format!("Failed to write to ffmpeg: {e}")))?;
        }

        // 等待完成并获取输出
        let output = child.wait_with_output()
            .map_err(|e| AudioError::ExecutionFailed(format!("Failed to read ffmpeg output: {e}")))?;

        if !output.status.success() {
            return Err(AudioError::ExecutionFailed("FFmpeg encoding failed".into()));
        }

        Ok(output.stdout)
    }

    /// 获取配置
    pub fn config(&self) -> &AudioConfig {
        &self.config
    }
}

/// FFmpeg 解码器
#[cfg(feature = "audio")]
pub struct FfmpegDecoder {
    config: AudioConfig,
}

#[cfg(feature = "audio")]
impl FfmpegDecoder {
    /// 创建新的解码器
    pub fn new(config: AudioConfig) -> Result<Self, AudioError> {
        FfmpegEncoder::check_ffmpeg(&config.ffmpeg_path)?;
        Ok(Self { config })
    }

    /// 解码 Opus 数据为 PCM
    pub fn decode(&self, opus_data: &[u8]) -> Result<Vec<i16>, AudioError> {
        let mut child = Command::new(&self.config.ffmpeg_path)
            .args([
                "-f", "opus",
                "-i", "pipe:0",
                "-f", "s16le",
                "-ar", &self.config.sample_rate.to_string(),
                "-ac", &self.config.channels.to_string(),
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| AudioError::ExecutionFailed(format!("Failed to start ffmpeg: {e}")))?;

        // 写入 Opus 数据
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(opus_data)
                .map_err(|e| AudioError::ExecutionFailed(format!("Failed to write to ffmpeg: {e}")))?;
        }

        // 等待完成并获取输出
        let output = child.wait_with_output()
            .map_err(|e| AudioError::ExecutionFailed(format!("Failed to read ffmpeg output: {e}")))?;

        if !output.status.success() {
            return Err(AudioError::ExecutionFailed("FFmpeg decoding failed".into()));
        }

        // 将字节转换为 i16 PCM
        let pcm: Vec<i16> = output.stdout
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();

        Ok(pcm)
    }

    /// 获取配置
    pub fn config(&self) -> &AudioConfig {
        &self.config
    }
}

/// 音频处理器
#[cfg(feature = "audio")]
pub struct AudioProcessor {
    encoder: Arc<RwLock<FfmpegEncoder>>,
    decoder: Arc<RwLock<FfmpegDecoder>>,
    config: AudioConfig,
}

#[cfg(feature = "audio")]
impl AudioProcessor {
    /// 创建新的音频处理器
    pub fn new(config: AudioConfig) -> Result<Self, AudioError> {
        let encoder = FfmpegEncoder::new(config.clone())?;
        let decoder = FfmpegDecoder::new(config.clone())?;

        Ok(Self {
            encoder: Arc::new(RwLock::new(encoder)),
            decoder: Arc::new(RwLock::new(decoder)),
            config,
        })
    }

    /// 编码 PCM 为 Opus
    pub async fn encode(&self, pcm: &[i16]) -> Result<Vec<u8>, AudioError> {
        let encoder = self.encoder.read().await;
        encoder.encode(pcm)
    }

    /// 解码 Opus 为 PCM
    pub async fn decode(&self, opus_data: &[u8]) -> Result<Vec<i16>, AudioError> {
        let decoder = self.decoder.read().await;
        decoder.decode(opus_data)
    }

    /// 获取配置
    pub fn config(&self) -> &AudioConfig {
        &self.config
    }

    /// 验证 ffmpeg 可用性
    pub fn verify_ffmpeg(&self) -> Result<(), AudioError> {
        FfmpegEncoder::check_ffmpeg(&self.config.ffmpeg_path)
    }
}

/// 音频发送器
#[cfg(feature = "audio")]
pub struct AudioSender {
    processor: Arc<AudioProcessor>,
    sequence: u64,
}

#[cfg(feature = "audio")]
impl AudioSender {
    /// 创建新的音频发送器
    pub fn new(processor: Arc<AudioProcessor>) -> Self {
        Self {
            processor,
            sequence: 0,
        }
    }

    /// 发送音频数据
    pub async fn send(&mut self, pcm: &[i16]) -> Result<Vec<u8>, AudioError> {
        let encoded = self.processor.encode(pcm).await?;
        self.sequence += 1;
        Ok(encoded)
    }

    /// 获取序列号
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}

/// 音频接收器
#[cfg(feature = "audio")]
pub struct AudioReceiver {
    processor: Arc<AudioProcessor>,
}

#[cfg(feature = "audio")]
impl AudioReceiver {
    /// 创建新的音频接收器
    pub fn new(processor: Arc<AudioProcessor>) -> Self {
        Self { processor }
    }

    /// 接收并解码音频数据
    pub async fn receive(&self, opus_data: &[u8]) -> Result<Vec<i16>, AudioError> {
        self.processor.decode(opus_data).await
    }
}

#[cfg(feature = "audio")]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_config_default() {
        let config = AudioConfig::default();
        assert_eq!(config.ffmpeg_path, "ffmpeg");
        assert_eq!(config.sample_rate, 48000);
        assert_eq!(config.channels, 1);
        assert_eq!(config.frame_size_ms, 20);
        assert_eq!(config.bitrate, 48000);
    }
}

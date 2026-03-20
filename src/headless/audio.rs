//! TeamSpeak 音频处理模块
//! 
//! 使用外部 ffmpeg 进行音频解码和编码，通过 Ogg 容器流式传输 Opus 数据。
//! 避免内部链接 libopus，完全依赖外部进程。

use tokio::process::{Command, Child};
use std::process::Stdio;
#[cfg(feature = "audio")]
use tokio::io::AsyncReadExt;
#[cfg(feature = "audio")]
use tokio::sync::mpsc;
#[cfg(feature = "audio")]
use tokio::task;
#[cfg(feature = "audio")]
use tracing::{debug, error, info, warn};
#[cfg(feature = "audio")]
use crate::headless::error::{HeadlessError, Result};

/// 音频配置
#[cfg(feature = "audio")]
#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// ffmpeg 可执行文件路径
    pub ffmpeg_path: String,
    /// 采样率 (Hz) - TeamSpeak 标准为 48000
    pub sample_rate: u32,
    /// 声道数 - TeamSpeak 标准为 1 (Mono) 或 2 (Stereo)
    pub channels: u16,
    /// 比特率 (bps)
    pub bitrate: u32,
    /// 音量 (0.0 - 1.0)
    pub volume: f32,
}

#[cfg(feature = "audio")]
impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            ffmpeg_path: "ffmpeg".to_string(),
            sample_rate: 48000,
            channels: 2, // Stereo
            bitrate: 48000, // 48kbps
            volume: 1.0,
        }
    }
}

/// 音频播放器
/// 
/// 管理 ffmpeg 子进程，解析 Ogg Opus 流，并发送音频帧
#[cfg(feature = "audio")]
pub struct AudioPlayer {
    config: AudioConfig,
    command_tx: mpsc::Sender<PlayerCommand>,
}

#[cfg(feature = "audio")]
enum PlayerCommand {
    Play(String),
    Stop,
    Volume(f32),
}

#[cfg(feature = "audio")]
impl AudioPlayer {
    /// 创建新的音频播放器
    pub fn new(config: AudioConfig, frame_tx: mpsc::Sender<Vec<u8>>) -> Self {
        let (command_tx, command_rx) = mpsc::channel(32);
        
        let player_config = config.clone();
        task::spawn(async move {
            Self::run_player_loop(player_config, command_rx, frame_tx).await;
        });

        Self {
            config,
            command_tx,
        }
    }

    /// 播放 URL 或文件
    pub async fn play(&self, url: String) -> Result<()> {
        self.command_tx.send(PlayerCommand::Play(url)).await
            .map_err(|_| HeadlessError::InternalError("Audio player loop closed".into()))
    }

    /// 停止播放
    pub async fn stop(&self) -> Result<()> {
        self.command_tx.send(PlayerCommand::Stop).await
            .map_err(|_| HeadlessError::InternalError("Audio player loop closed".into()))
    }

    /// 设置音量
    pub async fn set_volume(&self, volume: f32) -> Result<()> {
        self.command_tx.send(PlayerCommand::Volume(volume)).await
            .map_err(|_| HeadlessError::InternalError("Audio player loop closed".into()))
    }

    /// 播放器主循环
    async fn run_player_loop(
        config: AudioConfig,
        mut command_rx: mpsc::Receiver<PlayerCommand>,
        frame_tx: mpsc::Sender<Vec<u8>>,
    ) {
        let mut current_process: Option<Child> = None;
        let mut volume = config.volume;
        // 用于中止当前读取任务
        let mut abort_handle: Option<task::JoinHandle<()>> = None;

        loop {
            tokio::select! {
                cmd = command_rx.recv() => {
                    match cmd {
                        Some(PlayerCommand::Play(url)) => {
                            // 停止当前播放
                            if let Some(handle) = abort_handle.take() {
                                handle.abort();
                            }
                            if let Some(mut child) = current_process.take() {
                                let _ = child.kill().await;
                            }

                            info!("Starting playback: {}", url);
                            match start_ffmpeg(&config, &url, volume) {
                                Ok(mut child) => {
                                    if let Some(stdout) = child.stdout.take() {
                                        let frame_tx_clone = frame_tx.clone();
                                        
                                        // 启动读取任务
                                        abort_handle = Some(task::spawn(async move {
                                            if let Err(e) = process_ffmpeg_output(stdout, frame_tx_clone).await {
                                                error!("Audio processing error: {}", e);
                                            }
                                        }));

                                        current_process = Some(child);
                                    }
                                }
                                Err(e) => error!("Failed to start ffmpeg: {}", e),
                            }
                        }
                        Some(PlayerCommand::Stop) => {
                            if let Some(handle) = abort_handle.take() {
                                handle.abort();
                            }
                            if let Some(mut child) = current_process.take() {
                                let _ = child.kill().await;
                            }
                        }
                        Some(PlayerCommand::Volume(v)) => {
                            volume = v.max(0.0).min(1.0);
                        }
                        None => break, // Channel closed
                    }
                }
            }
        }
    }
}

/// 启动 ffmpeg 进程
#[cfg(feature = "audio")]
fn start_ffmpeg(config: &AudioConfig, url: &str, volume: f32) -> std::io::Result<Child> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-nostats".to_string(),
        "-i".to_string(),
        url.to_string(),
        "-map".to_string(),
        "0:a:0".to_string(),
        "-acodec".to_string(),
        "libopus".to_string(),
        "-b:a".to_string(),
        format!("{}", config.bitrate),
        "-vbr".to_string(),
        "on".to_string(),
        "-compression_level".to_string(),
        "10".to_string(),
        "-frame_duration".to_string(),
        "20".to_string(),
        "-application".to_string(),
        "voip".to_string(),
        "-ar".to_string(),
        format!("{}", config.sample_rate),
        "-ac".to_string(),
        format!("{}", config.channels),
        "-f".to_string(),
        "opus".to_string(), // Ogg Opus format
        "-".to_string(),
    ];

    // 如果需要调整音量
    if (volume - 1.0).abs() > 0.01 {
        // 在 -i 之后插入 filter
        // 这里的插入位置取决于 -i 的位置，上面是固定的
        // -i is at index 2, url is at 3.
        // so filter should be after input.
        // insert at 4
        args.insert(4, "-filter:a".to_string());
        args.insert(5, format!("volume={}", volume));
    }

    debug!("ffmpeg args: {:?}", args);

    Command::new(&config.ffmpeg_path)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .kill_on_drop(true) // Ensure child is killed when handle is dropped
        .spawn()
}

// ============================================================================
// Ogg Opus Demuxer
// ============================================================================

/// Ogg 页面头解析结果
struct OggPage {
    header_len: usize,
    body_len: usize,
    segment_table: Vec<u8>,
}

/// 解析 Ogg 页面头
fn parse_ogg_page(data: &[u8]) -> Option<OggPage> {
    if data.len() < 27 { return None; }
    if &data[0..4] != b"OggS" { return None; }

    let page_segments = data[26] as usize;
    let header_len = 27 + page_segments;
    
    if data.len() < header_len { return None; }

    let segment_table = data[27..header_len].to_vec();
    let body_len: usize = segment_table.iter().map(|&x| x as usize).sum();

    if data.len() < header_len + body_len { return None; }

    Some(OggPage {
        header_len,
        body_len,
        segment_table,
    })
}

/// 从 ffmpeg stdout 读取并解析 Opus 帧
#[cfg(feature = "audio")]
pub async fn process_ffmpeg_output(
    mut stdout: tokio::process::ChildStdout,
    frame_tx: mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let mut buffer = Vec::with_capacity(8192);
    let mut temp_buf = [0u8; 4096];
    let mut packet_buffer = Vec::new();

    // 状态追踪
    let mut headers_skipped = 0; // 0=none, 1=id done, 2=comment done

    loop {
        let n = stdout.read(&mut temp_buf).await
            .map_err(|e| HeadlessError::InternalError(format!("Read ffmpeg failed: {}", e)))?;
        
        if n == 0 { break; }
        buffer.extend_from_slice(&temp_buf[..n]);

        loop {
            // 尝试解析一个 Ogg 页面
            match parse_ogg_page(&buffer) {
                Some(page) => {
                    let page_total_len = page.header_len + page.body_len;
                    let page_body = &buffer[page.header_len..page_total_len];

                    // 遍历 segment table 提取 packet
                    let mut body_offset = 0;
                    for &seg_len in &page.segment_table {
                        let len = seg_len as usize;
                        packet_buffer.extend_from_slice(&page_body[body_offset..body_offset+len]);
                        body_offset += len;

                        if seg_len < 255 {
                            // Packet 结束
                            if headers_skipped < 2 {
                                // 简单检查：OpusHead (8 bytes) / OpusTags (8 bytes)
                                if packet_buffer.starts_with(b"OpusHead") {
                                    headers_skipped += 1;
                                } else if packet_buffer.starts_with(b"OpusTags") {
                                    headers_skipped += 1;
                                } else {
                                    // 如果没有检测到头但我们认为没跳过，强制完成跳过（容错）
                                    // 或者这已经是数据包了
                                    headers_skipped = 2;
                                    if !packet_buffer.is_empty() {
                                        let _ = frame_tx.send(packet_buffer.clone()).await;
                                    }
                                }
                            } else {
                                // 数据包
                                if !packet_buffer.is_empty() {
                                    if frame_tx.send(packet_buffer.clone()).await.is_err() {
                                        return Ok(()); // Receiver dropped
                                    }
                                }
                            }
                            packet_buffer.clear();
                        }
                    }

                    // 移除已处理的页面
                    buffer.drain(0..page_total_len);
                }
                None => {
                    // 数据不足，等待更多读取
                    // 如果 buffer 过大但无法解析，可能是数据损坏，需要处理（此处简化忽略）
                    if buffer.len() > 1024 * 1024 {
                        warn!("Audio buffer too large, clearing");
                        buffer.clear();
                    }
                    break; 
                }
            }
        }
    }
    Ok(())
}


//! TeamSpeak 包处理器
//! 
//! 处理 UDP 包的发送、接收、分片、重组和重传

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::interval;
use tracing::{debug, trace, warn};

use crate::headless::{
    crypto::TsCrypto,
    error::{HeadlessError, Result},
    packet::{Packet, PacketFlags, PacketType, MAX_DATA_SIZE, needs_splitting},
};

/// 包 ID 类型
pub type PacketId = u16;

/// 代数 ID 类型
pub type GenerationId = u32;

/// 最大重试次数
const MAX_RETRIES: u32 = 10;

/// 最小重传间隔
const MIN_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// 最大重传间隔
const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// Ping 间隔
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// 连接超时
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(60);

/// 待确认包信息
#[derive(Debug, Clone)]
struct PendingPacket {
    /// 包数据
    packet: Packet,
    /// 首次发送时间
    first_send: Instant,
    /// 最后发送时间
    last_send: Instant,
    /// 重试次数
    retries: u32,
}

/// 分片信息
#[derive(Debug, Clone)]
struct FragmentInfo {
    /// 分片数据
    fragments: Vec<Vec<u8>>,
    /// 已接收的分片数
    received: usize,
    /// 总分片数
    total: usize,
    /// 创建时间（用于超时清理）
    created: Instant,
}

/// 包处理器配置
#[derive(Debug, Clone)]
pub struct PacketHandlerConfig {
    /// 本地地址
    pub local_addr: SocketAddr,
    /// 远程地址
    pub remote_addr: SocketAddr,
}

/// 包处理器
pub struct PacketHandler {
    /// UDP socket
    socket: Arc<UdpSocket>,
    /// 加密处理器
    crypto: Arc<Mutex<TsCrypto>>,
    /// 包计数器
    packet_counters: Arc<RwLock<[PacketId; 9]>>,
    /// 代数计数器
    generation_counters: Arc<RwLock<[GenerationId; 9]>>,
    /// 待确认包
    pending_acks: Arc<RwLock<HashMap<PacketId, PendingPacket>>>,
    /// 接收窗口（用于去重）
    receive_window: Arc<RwLock<ReceiveWindow>>,
    /// 分片缓冲区
    fragment_buffer: Arc<RwLock<HashMap<PacketId, FragmentInfo>>>,
    /// RTT 估算
    rtt_estimator: Arc<Mutex<RttEstimator>>,
    /// 接收通道
    rx_tx: mpsc::Sender<Packet>,
    /// 关闭信号
    shutdown: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

/// 接收窗口（用于去重）
struct ReceiveWindow {
    /// 已接收的包 ID 位图
    bitmap: Vec<bool>,
    /// 窗口起始位置
    start: PacketId,
    /// 窗口大小
    size: usize,
}

impl ReceiveWindow {
    fn new(size: usize) -> Self {
        Self {
            bitmap: vec![false; size],
            start: 0,
            size,
        }
    }

    /// 检查包是否已接收
    fn is_received(&self, id: PacketId) -> bool {
        let offset = self.offset(id);
        if offset >= self.size {
            return false;
        }
        self.bitmap[offset]
    }

    /// 标记包为已接收
    fn mark_received(&mut self, id: PacketId) {
        let offset = self.offset(id);
        if offset >= self.size {
            // 滑动窗口
            let shift = offset - self.size + 1;
            self.start = self.start.wrapping_add(shift as u16);
            self.bitmap.rotate_left(shift);
            for i in (self.size - shift)..self.size {
                self.bitmap[i] = false;
            }
            self.bitmap[self.size - 1] = true;
        } else {
            self.bitmap[offset] = true;
        }
    }

    fn offset(&self, id: PacketId) -> usize {
        id.wrapping_sub(self.start) as usize
    }
}

/// RTT 估算器
struct RttEstimator {
    /// 平滑的 RTT
    smoothed_rtt: Duration,
    /// RTT 方差
    rtt_var: Duration,
    /// 当前重传超时
    current_rto: Duration,
}

impl RttEstimator {
    fn new() -> Self {
        Self {
            smoothed_rtt: Duration::from_secs(1),
            rtt_var: Duration::ZERO,
            current_rto: MAX_RETRY_INTERVAL,
        }
    }

    /// 更新 RTT 估算
    fn update(&mut self, sample: Duration) {
        const ALPHA: f64 = 0.125;
        const BETA: f64 = 0.25;

        if self.smoothed_rtt == Duration::from_secs(1) {
            // 首次采样
            self.smoothed_rtt = sample;
            self.rtt_var = sample / 2;
        } else {
            let diff = if sample > self.smoothed_rtt {
                sample - self.smoothed_rtt
            } else {
                self.smoothed_rtt - sample
            };
            
            self.rtt_var = Duration::from_secs_f64(
                (1.0 - BETA) * self.rtt_var.as_secs_f64() + BETA * diff.as_secs_f64()
            );
            
            self.smoothed_rtt = Duration::from_secs_f64(
                (1.0 - ALPHA) * self.smoothed_rtt.as_secs_f64() + ALPHA * sample.as_secs_f64()
            );
        }

        self.current_rto = self.smoothed_rtt + 4 * self.rtt_var;
        self.current_rto = self.current_rto.clamp(MIN_RETRY_INTERVAL, MAX_RETRY_INTERVAL);
    }

    /// 获取当前 RTO
    fn rto(&self) -> Duration {
        self.current_rto
    }
}

impl PacketHandler {
    /// 创建新的包处理器
    pub async fn new(
        config: PacketHandlerConfig,
        crypto: TsCrypto,
    ) -> Result<(Self, mpsc::Receiver<Packet>)> {
        let socket = UdpSocket::bind(config.local_addr).await
            .map_err(|e| HeadlessError::ConnectionError(format!("Bind failed: {e}")))?;
        
        socket.connect(config.remote_addr).await
            .map_err(|e| HeadlessError::ConnectionError(format!("Connect failed: {e}")))?;

        let (tx, rx) = mpsc::channel(1024);

        let handler = Self {
            socket: Arc::new(socket),
            crypto: Arc::new(Mutex::new(crypto)),
            packet_counters: Arc::new(RwLock::new([0; 9])),
            generation_counters: Arc::new(RwLock::new([0; 9])),
            pending_acks: Arc::new(RwLock::new(HashMap::new())),
            receive_window: Arc::new(RwLock::new(ReceiveWindow::new(256))),
            fragment_buffer: Arc::new(RwLock::new(HashMap::new())),
            rtt_estimator: Arc::new(Mutex::new(RttEstimator::new())),
            rx_tx: tx,
            shutdown: Arc::new(Mutex::new(None)),
        };

        Ok((handler, rx))
    }

    /// 启动包处理器
    pub async fn start(&self) -> Result<()> {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        *self.shutdown.lock().await = Some(shutdown_tx);

        // 启动接收任务
        let socket = self.socket.clone();
        let crypto = self.crypto.clone();
        let receive_window = self.receive_window.clone();
        let fragment_buffer = self.fragment_buffer.clone();
        let rx_tx = self.rx_tx.clone();
        let pending_acks = self.pending_acks.clone();
        let rtt_estimator = self.rtt_estimator.clone();

        tokio::spawn(async move {
            Self::receive_loop(
                socket,
                crypto,
                receive_window,
                fragment_buffer,
                rx_tx,
                pending_acks,
                rtt_estimator,
                shutdown_rx,
            ).await;
        });

        // 启动重传任务
        let socket = self.socket.clone();
        let crypto = self.crypto.clone();
        let pending_acks = self.pending_acks.clone();
        let rtt_estimator = self.rtt_estimator.clone();

        tokio::spawn(async move {
            Self::resend_loop(socket, crypto, pending_acks, rtt_estimator).await;
        });

        Ok(())
    }

    /// 发送包
    pub async fn send(&self, data: &[u8], packet_type: PacketType) -> Result<()> {
        let needs_split = needs_splitting(data.len());
        
        if needs_split && packet_type != PacketType::Voice && packet_type != PacketType::VoiceWhisper {
            // 尝试压缩
            // TODO: 实现压缩
            
            // 分片发送
            return self.send_fragmented(data, packet_type).await;
        }

        self.send_single(data, packet_type, PacketFlags::NONE).await
    }

    /// 发送单个包
    async fn send_single(
        &self,
        data: &[u8],
        packet_type: PacketType,
        flags: PacketFlags,
    ) -> Result<()> {
        let (packet_id, generation_id) = self.get_counter(packet_type).await;
        self.increment_counter(packet_type).await;

        let mut packet = Packet::new(packet_type, packet_id, generation_id, data.to_vec());
        packet.header.flags = flags;

        // 根据包类型设置标志
        match packet_type {
            PacketType::Command | PacketType::CommandLow => {
                packet.header.flags |= PacketFlags::NEW_PROTOCOL;
            }
            PacketType::Voice | PacketType::VoiceWhisper | PacketType::Ping | PacketType::Pong | PacketType::Init1 => {
                packet.header.flags |= PacketFlags::UNENCRYPTED;
            }
            _ => {}
        }

        // 加密
        let crypto = self.crypto.lock().await;
        crypto.encrypt(&mut packet)?;
        drop(crypto);

        // 发送
        let raw = packet.to_bytes();
        self.socket.send(&raw).await
            .map_err(|e| HeadlessError::ConnectionError(format!("Send failed: {e}")))?;

        // 如果需要确认，添加到待确认列表
        if packet_type.needs_ack() {
            let mut pending = self.pending_acks.write().await;
            pending.insert(packet_id, PendingPacket {
                packet: packet.clone(),
                first_send: Instant::now(),
                last_send: Instant::now(),
                retries: 0,
            });
        }

        trace!("Sent packet: {}", packet);
        Ok(())
    }

    /// 分片发送
    async fn send_fragmented(&self, data: &[u8], packet_type: PacketType) -> Result<()> {
        let chunks: Vec<&[u8]> = data.chunks(MAX_DATA_SIZE).collect();

        for (i, chunk) in chunks.iter().enumerate() {
            let mut flags = PacketFlags::FRAGMENTED;
            if i == 0 {
                flags |= PacketFlags::COMPRESSED; // 第一个分片标记
            }
            
            self.send_single(chunk, packet_type, flags).await?;
        }

        Ok(())
    }

    /// 发送 Ping
    pub async fn send_ping(&self) -> Result<()> {
        let (packet_id, _) = self.get_counter(PacketType::Ping).await;
        let data = packet_id.to_be_bytes().to_vec();
        self.send_single(&data, PacketType::Ping, PacketFlags::UNENCRYPTED).await
    }

    /// 发送 Ack
    pub async fn send_ack(&self, ack_id: PacketId, packet_type: PacketType) -> Result<()> {
        let ack_packet_type = if packet_type == PacketType::CommandLow {
            PacketType::AckLow
        } else {
            PacketType::Ack
        };

        let data = ack_id.to_be_bytes().to_vec();
        self.send_single(&data, ack_packet_type, PacketFlags::NONE).await
    }

    /// 内部发送 Ack（在接收循环中调用，直接使用 socket 发送）
    async fn send_ack_internal(
        socket: &Arc<UdpSocket>,
        crypto: &Arc<Mutex<TsCrypto>>,
        ack_id: PacketId,
        packet_type: PacketType,
    ) -> Result<()> {
        let ack_packet_type = if packet_type == PacketType::CommandLow {
            PacketType::AckLow
        } else {
            PacketType::Ack
        };

        let data = ack_id.to_be_bytes().to_vec();
        let mut packet = Packet::new(ack_packet_type, 0, 0, data);
        packet.header.flags |= PacketFlags::UNENCRYPTED;

        let crypto = crypto.lock().await;
        crypto.encrypt(&mut packet)?;
        drop(crypto);

        let raw = packet.to_bytes();
        socket.send(&raw).await
            .map_err(|e| HeadlessError::ConnectionError(format!("Ack send failed: {e}")))?;

        trace!("Sent Ack for packet {}", ack_id);
        Ok(())
    }

    /// 获取包计数器
    async fn get_counter(&self, packet_type: PacketType) -> (PacketId, GenerationId) {
        let counters = self.packet_counters.read().await;
        let generations = self.generation_counters.read().await;
        let idx = packet_type as usize;
        
        if idx < counters.len() {
            (counters[idx], generations[idx])
        } else {
            (0, 0)
        }
    }

    /// 递增包计数器
    async fn increment_counter(&self, packet_type: PacketType) {
        let mut counters = self.packet_counters.write().await;
        let mut generations = self.generation_counters.write().await;
        let idx = packet_type as usize;
        
        if idx < counters.len() {
            counters[idx] = counters[idx].wrapping_add(1);
            if counters[idx] == 0 {
                generations[idx] = generations[idx].wrapping_add(1);
            }
        }
    }

    /// 接收循环
    async fn receive_loop(
        socket: Arc<UdpSocket>,
        crypto: Arc<Mutex<TsCrypto>>,
        receive_window: Arc<RwLock<ReceiveWindow>>,
        fragment_buffer: Arc<RwLock<HashMap<PacketId, FragmentInfo>>>,
        tx: mpsc::Sender<Packet>,
        pending_acks: Arc<RwLock<HashMap<PacketId, PendingPacket>>>,
        rtt_estimator: Arc<Mutex<RttEstimator>>,
        mut shutdown: tokio::sync::oneshot::Receiver<()>,
    ) {
        let mut buf = vec![0u8; 4096];

        loop {
            tokio::select! {
                result = socket.recv(&mut buf) => {
                    match result {
                        Ok(len) => {
                            if let Err(e) = Self::handle_received(
                                &buf[..len],
                                &socket,
                                &crypto,
                                &receive_window,
                                &fragment_buffer,
                                &tx,
                                &pending_acks,
                                &rtt_estimator,
                            ).await {
                                warn!("Error handling packet: {e}");
                            }
                        }
                        Err(e) => {
                            warn!("Receive error: {e}");
                            break;
                        }
                    }
                }
                _ = &mut shutdown => {
                    debug!("Receive loop shutdown");
                    break;
                }
            }
        }
    }

    /// 处理接收到的包
    async fn handle_received(
        data: &[u8],
        socket: &Arc<UdpSocket>,
        crypto: &Arc<Mutex<TsCrypto>>,
        receive_window: &Arc<RwLock<ReceiveWindow>>,
        fragment_buffer: &Arc<RwLock<HashMap<PacketId, FragmentInfo>>>,
        tx: &mpsc::Sender<Packet>,
        pending_acks: &Arc<RwLock<HashMap<PacketId, PendingPacket>>>,
        rtt_estimator: &Arc<Mutex<RttEstimator>>,
    ) -> Result<()> {
        let mut packet = Packet::from_raw(data)?;
        trace!("Received raw packet: {}", packet);

        // 检查是否重复
        {
            let window = receive_window.read().await;
            if window.is_received(packet.header.packet_id) {
                trace!("Duplicate packet, ignoring: {}", packet.header.packet_id);
                return Ok(());
            }
        }

        // 解密
        {
            let crypto = crypto.lock().await;
            if !crypto.decrypt(&mut packet)? {
                warn!("Failed to decrypt packet");
                return Ok(());
            }
        }

        // 标记为已接收
        {
            let mut window = receive_window.write().await;
            window.mark_received(packet.header.packet_id);
        }

        // 处理 Ack
        match packet.header.packet_type {
            PacketType::Ack | PacketType::AckLow => {
                if packet.data.len() >= 2 {
                    let ack_id = u16::from_be_bytes([packet.data[0], packet.data[1]]);
                    let mut pending = pending_acks.write().await;
                    if let Some(pending_packet) = pending.remove(&ack_id) {
                        let rtt = pending_packet.first_send.elapsed();
                        let mut estimator = rtt_estimator.lock().await;
                        estimator.update(rtt);
                        trace!("Ack received for packet {} (RTT: {:?})", ack_id, rtt);
                    }
                }
                return Ok(());
            }
            PacketType::Pong => {
                if packet.data.len() >= 2 {
                    let ping_id = u16::from_be_bytes([packet.data[0], packet.data[1]]);
                    let mut pending = pending_acks.write().await;
                    if let Some(pending_packet) = pending.remove(&ping_id) {
                        let rtt = pending_packet.first_send.elapsed();
                        let mut estimator = rtt_estimator.lock().await;
                        estimator.update(rtt);
                        trace!("Pong received for ping {} (RTT: {:?})", ping_id, rtt);
                    }
                }
                return Ok(());
            }
            _ => {}
        }

        // 处理分片
        if packet.header.flags.contains(PacketFlags::FRAGMENTED) {
            return Self::handle_fragment(packet, fragment_buffer, tx).await;
        }

        // 发送 Ack（如果需要）
        if packet.needs_ack() {
            if let Err(e) = Self::send_ack_internal(socket, crypto, packet.header.packet_id, packet.header.packet_type).await {
                warn!("Failed to send Ack: {e}");
            }
        }

        // 发送到接收通道
        tx.send(packet).await
            .map_err(|_| HeadlessError::ConnectionError("Channel closed".into()))?;

        Ok(())
    }

    /// 处理分片
    async fn handle_fragment(
        packet: Packet,
        fragment_buffer: &Arc<RwLock<HashMap<PacketId, FragmentInfo>>>,
        tx: &mpsc::Sender<Packet>,
    ) -> Result<()> {
        let packet_id = packet.header.packet_id;
        let is_first = packet.header.flags.contains(PacketFlags::COMPRESSED);

        let mut buffer = fragment_buffer.write().await;

        // 清理超时的分片（超过 30 秒）
        let now = Instant::now();
        buffer.retain(|_, info| now.duration_since(info.created) < Duration::from_secs(30));

        if is_first {
            // 第一个分片，创建新的分片信息
            let info = FragmentInfo {
                fragments: vec![packet.data.clone()],
                received: 1,
                total: 0, // 未知总数
                created: Instant::now(),
            };
            buffer.insert(packet_id, info);
        } else {
            // 后续分片
            if let Some(info) = buffer.get_mut(&packet_id) {
                info.fragments.push(packet.data.clone());
                info.received += 1;

                // 检查是否是最后一个分片（没有 FRAGMENTED 标志）
                if !packet.header.flags.contains(PacketFlags::FRAGMENTED) {
                    info.total = info.received;
                }

                // 如果接收完毕，重组并发送
                if info.total > 0 && info.received >= info.total {
                    let complete_data: Vec<u8> = info.fragments.iter()
                        .flat_map(|f| f.iter())
                        .copied()
                        .collect();

                    let mut complete_packet = packet.clone();
                    complete_packet.data = complete_data;
                    complete_packet.header.flags.remove(PacketFlags::FRAGMENTED);

                    buffer.remove(&packet_id);

                    tx.send(complete_packet).await
                        .map_err(|_| HeadlessError::ConnectionError("Channel closed".into()))?;
                }
            }
        }

        Ok(())
    }

    /// 重传循环
    async fn resend_loop(
        socket: Arc<UdpSocket>,
        _crypto: Arc<Mutex<TsCrypto>>,
        pending_acks: Arc<RwLock<HashMap<PacketId, PendingPacket>>>,
        rtt_estimator: Arc<Mutex<RttEstimator>>,
    ) {
        let mut ticker = interval(Duration::from_millis(50));

        loop {
            ticker.tick().await;

            let now = Instant::now();
            let rto = {
                let estimator = rtt_estimator.lock().await;
                estimator.rto()
            };

            let mut to_resend = Vec::new();
            let mut to_remove = Vec::new();

            {
                let mut pending = pending_acks.write().await;
                for (id, info) in pending.iter_mut() {
                    let elapsed = now.duration_since(info.last_send);
                    
                    if elapsed >= rto {
                        if info.retries >= MAX_RETRIES {
                            to_remove.push(*id);
                            warn!("Packet {} exceeded max retries", id);
                        } else {
                            to_resend.push(info.packet.clone());
                            info.last_send = now;
                            info.retries += 1;
                        }
                    }
                }

                for id in to_remove {
                    pending.remove(&id);
                }
            }

            // 重发包
            for packet in to_resend {
                let raw = packet.to_bytes();
                if let Err(e) = socket.send(&raw).await {
                    warn!("Resend failed: {e}");
                } else {
                    trace!("Resent packet: {}", packet.header.packet_id);
                }
            }
        }
    }

    /// 获取当前 RTT
    pub async fn current_rtt(&self) -> Duration {
        let estimator = self.rtt_estimator.lock().await;
        estimator.rto()
    }

    /// 关闭包处理器
    pub async fn shutdown(&self) {
        if let Some(tx) = self.shutdown.lock().await.take() {
            let _ = tx.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_receive_window() {
        let mut window = ReceiveWindow::new(256);
        
        assert!(!window.is_received(0));
        window.mark_received(0);
        assert!(window.is_received(0));
        
        assert!(!window.is_received(100));
        window.mark_received(100);
        assert!(window.is_received(100));
    }

    #[test]
    fn test_rtt_estimator() {
        let mut estimator = RttEstimator::new();
        
        estimator.update(Duration::from_millis(100));
        assert!(estimator.rto() >= Duration::from_millis(100));
        
        estimator.update(Duration::from_millis(120));
        assert!(estimator.smoothed_rtt > Duration::from_millis(100));
    }
}

//! TeamSpeak 包处理器
//!
//! 当前聚焦“连接握手链路”对齐：
//! - 支持 TS 线格式 [MAC][Header][Data]
//! - Init1/Command/Ack/Ping 的收发与重传
//! - 避免重复启动循环任务

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tokio::time::interval;
use tracing::{debug, error, trace, warn};

use super::crypto::TsCrypto;
use super::error::{HeadlessError, Result};
use super::packet::{
    needs_splitting, Packet, PacketDirection, PacketFlags, PacketType, MAX_DATA_SIZE,
};

pub type PacketId = u16;
pub type GenerationId = u32;

const MIN_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const PING_INTERVAL: Duration = Duration::from_secs(3);
const PACKET_TIMEOUT: Duration = Duration::from_secs(30);

fn compress_zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).unwrap_or_default();
    encoder.finish().unwrap_or_default()
}

#[derive(Debug, Clone)]
struct PendingPacket {
    packet: Packet,
    first_send: Instant,
    last_send: Instant,
    retries: u32,
}

#[derive(Debug, Clone)]
struct FragmentInfo {
    fragments: Vec<Vec<u8>>,
    received: usize,
    total: usize,
    created: Instant,
}

#[derive(Debug, Clone)]
pub struct PacketHandlerConfig {
    pub local_addr: SocketAddr,
    pub remote_addr: SocketAddr,
}

#[derive(Clone)]
pub struct PacketHandler {
    socket: Arc<UdpSocket>,
    crypto: Arc<Mutex<TsCrypto>>,
    packet_counters: Arc<RwLock<[PacketId; 9]>>,
    generation_counters: Arc<RwLock<[GenerationId; 9]>>,
    pending_acks: Arc<RwLock<HashMap<PacketId, PendingPacket>>>,
    receive_window: Arc<RwLock<ReceiveWindow>>,
    fragment_buffer: Arc<RwLock<HashMap<PacketId, FragmentInfo>>>,
    rtt_estimator: Arc<Mutex<RttEstimator>>,
    rx_tx: mpsc::Sender<Packet>,
    shutdown_tx: broadcast::Sender<()>,
    started: Arc<std::sync::atomic::AtomicBool>,
    client_id: Arc<RwLock<u16>>,
    last_activity: Arc<StdMutex<Instant>>,
}

struct ReceiveWindow {
    bitmap: Vec<bool>,
    start: PacketId,
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

    fn is_received(&self, id: PacketId) -> bool {
        let offset = self.offset(id);
        if offset >= self.size {
            return false;
        }
        self.bitmap[offset]
    }

    fn mark_received(&mut self, id: PacketId) {
        let offset = self.offset(id);
        if offset >= self.size {
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

struct RttEstimator {
    smoothed_rtt: Duration,
    rtt_var: Duration,
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

    fn update(&mut self, sample: Duration) {
        const ALPHA: f64 = 0.125;
        const BETA: f64 = 0.25;

        if self.smoothed_rtt == Duration::from_secs(1) {
            self.smoothed_rtt = sample;
            self.rtt_var = sample / 2;
        } else {
            let diff = if sample > self.smoothed_rtt {
                sample - self.smoothed_rtt
            } else {
                self.smoothed_rtt - sample
            };
            self.rtt_var = Duration::from_secs_f64(
                (1.0 - BETA) * self.rtt_var.as_secs_f64() + BETA * diff.as_secs_f64(),
            );
            self.smoothed_rtt = Duration::from_secs_f64(
                (1.0 - ALPHA) * self.smoothed_rtt.as_secs_f64() + ALPHA * sample.as_secs_f64(),
            );
        }

        self.current_rto =
            (self.smoothed_rtt + 4 * self.rtt_var).clamp(MIN_RETRY_INTERVAL, MAX_RETRY_INTERVAL);
    }

    fn rto(&self) -> Duration {
        self.current_rto
    }
}

impl PacketHandler {
    pub async fn new(
        config: PacketHandlerConfig,
        crypto: Arc<Mutex<TsCrypto>>,
    ) -> Result<(Self, mpsc::Receiver<Packet>)> {
        let socket = UdpSocket::bind(config.local_addr)
            .await
            .map_err(|e| HeadlessError::ConnectionError(format!("Bind failed: {e}")))?;

        socket
            .connect(config.remote_addr)
            .await
            .map_err(|e| HeadlessError::ConnectionError(format!("Connect failed: {e}")))?;

        let (tx, rx) = mpsc::channel(1024);
        let (shutdown_tx, _) = broadcast::channel(1);

        Ok((
            Self {
                socket: Arc::new(socket),
                crypto,
                packet_counters: Arc::new(RwLock::new([0; 9])),
                generation_counters: Arc::new(RwLock::new([0; 9])),
                pending_acks: Arc::new(RwLock::new(HashMap::new())),
                receive_window: Arc::new(RwLock::new(ReceiveWindow::new(256))),
                fragment_buffer: Arc::new(RwLock::new(HashMap::new())),
                rtt_estimator: Arc::new(Mutex::new(RttEstimator::new())),
                rx_tx: tx,
                shutdown_tx,
                started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                client_id: Arc::new(RwLock::new(0)),
                last_activity: Arc::new(StdMutex::new(Instant::now())),
            },
            rx,
        ))
    }

    pub async fn start(&self) -> Result<()> {
        if self.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }

        let shutdown_rx1 = self.shutdown_tx.subscribe();
        let shutdown_rx2 = self.shutdown_tx.subscribe();
        let shutdown_rx3 = self.shutdown_tx.subscribe();

        let this = self.clone();
        tokio::spawn(async move {
            this.run_receive_loop(shutdown_rx1).await;
        });

        let this = self.clone();
        tokio::spawn(async move {
            this.run_resend_loop(shutdown_rx2).await;
        });

        let this = self.clone();
        tokio::spawn(async move {
            this.run_ping_loop(shutdown_rx3).await;
        });

        Ok(())
    }

    pub async fn send(&self, data: &[u8], packet_type: PacketType) -> Result<()> {
        let needs_split = needs_splitting(data.len());
        if needs_split
            && packet_type != PacketType::Voice
            && packet_type != PacketType::VoiceWhisper
        {
            let compressed = compress_zlib(data);
            if compressed.len() < data.len() {
                return self.send_fragmented(&compressed, packet_type).await;
            }
            return self.send_fragmented(data, packet_type).await;
        }
        self.send_single(data, packet_type, PacketFlags::NONE).await
    }

    async fn send_single(
        &self,
        data: &[u8],
        packet_type: PacketType,
        flags: PacketFlags,
    ) -> Result<()> {
        let (packet_id, generation_id) = self.get_counter(packet_type).await;
        self.increment_counter(packet_type).await;

        let mut packet = Packet::new(packet_type, packet_id, generation_id, data.to_vec());
        packet.header.client_id = Some(*self.client_id.read().await);
        packet.header.flags = flags;
        match packet_type {
            PacketType::Command | PacketType::CommandLow => {
                packet.header.flags |= PacketFlags::NEW_PROTOCOL;
            }
            PacketType::Voice
            | PacketType::VoiceWhisper
            | PacketType::Ping
            | PacketType::Pong
            | PacketType::Init1
            | PacketType::AckLow => {
                packet.header.flags |= PacketFlags::UNENCRYPTED;
            }
            _ => {}
        }

        {
            let crypto = self.crypto.lock().await;
            crypto.encrypt(&mut packet, PacketDirection::C2S)?;
        }

        let raw = packet.to_bytes_with_direction(PacketDirection::C2S);
        self.socket
            .send(&raw)
            .await
            .map_err(|e| HeadlessError::ConnectionError(format!("Send failed: {e}")))?;

        if packet_type.needs_ack() {
            self.pending_acks.write().await.insert(
                packet_id,
                PendingPacket {
                    packet: packet.clone(),
                    first_send: Instant::now(),
                    last_send: Instant::now(),
                    retries: 0,
                },
            );
        }

        trace!("Sent packet: {}", packet);
        Ok(())
    }

    async fn send_fragmented(&self, data: &[u8], packet_type: PacketType) -> Result<()> {
        let chunks: Vec<&[u8]> = data.chunks(MAX_DATA_SIZE).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            let mut flags = PacketFlags::FRAGMENTED;
            if i == 0 {
                flags |= PacketFlags::COMPRESSED;
            }
            self.send_single(chunk, packet_type, flags).await?;
        }
        Ok(())
    }

    pub async fn send_ping(&self) -> Result<()> {
        let (packet_id, _) = self.get_counter(PacketType::Ping).await;
        let data = packet_id.to_be_bytes().to_vec();
        self.send_single(&data, PacketType::Ping, PacketFlags::UNENCRYPTED)
            .await
    }

    pub async fn send_ack(&self, ack_id: PacketId, packet_type: PacketType) -> Result<()> {
        let ack_packet_type = if packet_type == PacketType::CommandLow {
            PacketType::AckLow
        } else {
            PacketType::Ack
        };
        self.send_single(&ack_id.to_be_bytes(), ack_packet_type, PacketFlags::NONE)
            .await
    }

    async fn get_counter(&self, packet_type: PacketType) -> (PacketId, GenerationId) {
        if packet_type == PacketType::Init1 {
            return (101, 0);
        }
        let idx = packet_type as usize;
        let counters = self.packet_counters.read().await;
        let generations = self.generation_counters.read().await;
        if idx < counters.len() {
            (counters[idx], generations[idx])
        } else {
            (0, 0)
        }
    }

    async fn increment_counter(&self, packet_type: PacketType) {
        let idx = packet_type as usize;
        if idx >= 9 {
            return;
        }
        let mut counters = self.packet_counters.write().await;
        let mut generations = self.generation_counters.write().await;
        counters[idx] = counters[idx].wrapping_add(1);
        if counters[idx] == 0 {
            generations[idx] = generations[idx].wrapping_add(1);
        }
    }

    async fn run_receive_loop(self, mut shutdown: broadcast::Receiver<()>) {
        let mut buf = vec![0u8; 4096];
        loop {
            tokio::select! {
                result = self.socket.recv(&mut buf) => {
                    match result {
                        Ok(len) => {
                            if let Err(e) = self.handle_received(&buf[..len]).await {
                                warn!("Error handling packet: {e}");
                            }
                        }
                        Err(e) => {
                            warn!("Receive error: {e}");
                            break;
                        }
                    }
                }
                _ = shutdown.recv() => {
                    debug!("Receive loop shutdown");
                    break;
                }
            }
        }
    }

    async fn handle_received(&self, data: &[u8]) -> Result<()> {
        let mut packet = Packet::from_raw(data, PacketDirection::S2C)?;
        trace!("Received raw packet: {}", packet);

        // Update last_activity for every received packet, including keepalives
        *self.last_activity.lock().expect("last_activity mutex poisoned") = Instant::now();

        let should_dedupe = matches!(
            packet.header.packet_type,
            PacketType::Voice
                | PacketType::VoiceWhisper
                | PacketType::Command
                | PacketType::CommandLow
        );
        {
            let crypto = self.crypto.lock().await;
            if !crypto.decrypt(&mut packet, PacketDirection::S2C)? {
                warn!("Failed to decrypt packet");
                return Ok(());
            }
        }

        if should_dedupe
            && self
                .receive_window
                .read()
                .await
                .is_received(packet.header.packet_id)
        {
            trace!("Duplicate packet, ignoring: {}", packet.header.packet_id);
            if packet.needs_ack() {
                if let Err(e) = self
                    .send_ack(packet.header.packet_id, packet.header.packet_type)
                    .await
                {
                    warn!("Failed to send Ack for duplicate packet: {e}");
                }
            }
            return Ok(());
        }

        if should_dedupe {
            self.receive_window
                .write()
                .await
                .mark_received(packet.header.packet_id);
        }

        match packet.header.packet_type {
            PacketType::Ack | PacketType::AckLow => {
                if packet.data.len() >= 2 {
                    let ack_id = u16::from_be_bytes([packet.data[0], packet.data[1]]);
                    if let Some(pending_packet) = self.pending_acks.write().await.remove(&ack_id) {
                        let rtt = pending_packet.first_send.elapsed();
                        self.rtt_estimator.lock().await.update(rtt);
                        trace!("Ack received for packet {} (RTT: {:?})", ack_id, rtt);
                    }
                }
                return Ok(());
            }
            PacketType::Pong => {
                if packet.data.len() >= 2 {
                    let ping_id = u16::from_be_bytes([packet.data[0], packet.data[1]]);
                    if let Some(pending_packet) = self.pending_acks.write().await.remove(&ping_id) {
                        let rtt = pending_packet.first_send.elapsed();
                        self.rtt_estimator.lock().await.update(rtt);
                        trace!("Pong received for ping {} (RTT: {:?})", ping_id, rtt);
                    }
                }
                return Ok(());
            }
            PacketType::Ping => {
                if packet.data.len() >= 2 {
                    let ping_id = u16::from_be_bytes([packet.data[0], packet.data[1]]);
                    let _ = self
                        .send_single(
                            &ping_id.to_be_bytes(),
                            PacketType::Pong,
                            PacketFlags::UNENCRYPTED,
                        )
                        .await;
                }
                return Ok(());
            }
            _ => {}
        }

        if packet.needs_ack() {
            if let Err(e) = self
                .send_ack(packet.header.packet_id, packet.header.packet_type)
                .await
            {
                warn!("Failed to send Ack: {e}");
            }
        }

        if packet.header.flags.contains(PacketFlags::FRAGMENTED) {
            return self.handle_fragment(packet).await;
        }

        self.rx_tx
            .send(packet)
            .await
            .map_err(|_| HeadlessError::ConnectionError("Channel closed".into()))?;

        Ok(())
    }

    async fn handle_fragment(&self, packet: Packet) -> Result<()> {
        let packet_id = packet.header.packet_id;
        let is_first = packet.header.flags.contains(PacketFlags::COMPRESSED);

        let mut buffer = self.fragment_buffer.write().await;
        let now = Instant::now();
        buffer.retain(|_, info| now.duration_since(info.created) < Duration::from_secs(30));

        if is_first {
            buffer.insert(
                packet_id,
                FragmentInfo {
                    fragments: vec![packet.data.clone()],
                    received: 1,
                    total: 0,
                    created: Instant::now(),
                },
            );
        } else if let Some(info) = buffer.get_mut(&packet_id) {
            info.fragments.push(packet.data.clone());
            info.received += 1;
            if !packet.header.flags.contains(PacketFlags::FRAGMENTED) {
                info.total = info.received;
            }
            if info.total > 0 && info.received >= info.total {
                let complete_data: Vec<u8> = info
                    .fragments
                    .iter()
                    .flat_map(|f| f.iter())
                    .copied()
                    .collect();
                let mut complete_packet = packet.clone();
                complete_packet.data = complete_data;
                let was_compressed = complete_packet
                    .header
                    .flags
                    .contains(PacketFlags::COMPRESSED);
                complete_packet.header.flags.remove(PacketFlags::FRAGMENTED);
                buffer.remove(&packet_id);

                if was_compressed {
                    complete_packet.data = decompress_zlib(&complete_packet.data)?;
                }

                self.rx_tx
                    .send(complete_packet)
                    .await
                    .map_err(|_| HeadlessError::ConnectionError("Channel closed".into()))?;
            }
        }

        Ok(())
    }

    async fn run_resend_loop(self, mut shutdown: broadcast::Receiver<()>) {
        let mut ticker = interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let now = Instant::now();
                    let rto = self.rtt_estimator.lock().await.rto();

                    let mut to_resend = Vec::new();
                    {
                        let mut pending = self.pending_acks.write().await;
                        for (id, info) in pending.iter_mut() {
                            if now.duration_since(info.first_send) >= PACKET_TIMEOUT {
                                error!(
                                    "Packet {} timed out ({}s), shutting down connection",
                                    id,
                                    PACKET_TIMEOUT.as_secs()
                                );
                                let _ = self.shutdown_tx.send(());
                                break;
                            }

                            if now.duration_since(info.last_send) >= rto {
                                to_resend.push(info.packet.clone());
                                info.last_send = now;
                                info.retries += 1;
                            }
                        }
                    }

                    for packet in to_resend {
                        let raw = packet.to_bytes_with_direction(PacketDirection::C2S);
                        if let Err(e) = self.socket.send(&raw).await {
                            warn!("Resend failed: {e}");
                        } else {
                            trace!("Resent packet: {}", packet.header.packet_id);
                        }
                    }
                }
                _ = shutdown.recv() => {
                    debug!("Resend loop shutdown");
                    break;
                }
            }
        }
    }

    async fn run_ping_loop(self, mut shutdown: broadcast::Receiver<()>) {
        let mut tick = interval(PING_INTERVAL);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = self.send_ping().await {
                        warn!("Failed to send ping: {e}");
                    }
                }
                _ = shutdown.recv() => {
                    debug!("Ping loop shutdown");
                    break;
                }
            }
        }
    }

    pub fn last_activity(&self) -> Instant {
        *self.last_activity.lock().expect("last_activity mutex poisoned")
    }

    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
        self.started
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub async fn set_client_id(&self, client_id: u16) {
        *self.client_id.write().await = client_id;
    }

    pub async fn bump_counter(&self, packet_type: PacketType) {
        self.increment_counter(packet_type).await;
    }
}

fn decompress_zlib(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).map_err(|e| {
        HeadlessError::PacketError(format!("Failed to decompress packet data: {e}"))
    })?;
    Ok(out)
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
    }

    #[test]
    fn test_rtt_estimator() {
        let mut estimator = RttEstimator::new();
        estimator.update(Duration::from_millis(100));
        assert!(estimator.rto() >= Duration::from_millis(100));
    }
}

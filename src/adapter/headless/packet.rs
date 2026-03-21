//! TeamSpeak 包结构
//!
//! 定义网络包的类型、格式和序列化

use bitflags::bitflags;
use std::fmt;

use super::error::{HeadlessError, Result};

/// 包类型枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PacketType {
    /// 初始化握手
    Init1 = 0,
    /// 命令（加密）
    Command = 1,
    /// 低优先级命令
    CommandLow = 2,
    /// 心跳
    Ping = 3,
    /// 心跳响应
    Pong = 4,
    /// 确认
    Ack = 5,
    /// 低优先级确认
    AckLow = 6,
    /// 语音数据
    Voice = 7,
    /// 私语语音
    VoiceWhisper = 8,
}

impl PacketType {
    /// 从字节创建
    pub fn from_u8(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Init1),
            1 => Ok(Self::Command),
            2 => Ok(Self::CommandLow),
            3 => Ok(Self::Ping),
            4 => Ok(Self::Pong),
            5 => Ok(Self::Ack),
            6 => Ok(Self::AckLow),
            7 => Ok(Self::Voice),
            8 => Ok(Self::VoiceWhisper),
            _ => Err(HeadlessError::InvalidPacketType(value)),
        }
    }

    /// 是否需要确认
    pub fn needs_ack(&self) -> bool {
        matches!(self, Self::Command | Self::CommandLow)
    }
}

impl fmt::Display for PacketType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Init1 => write!(f, "Init1"),
            Self::Command => write!(f, "Command"),
            Self::CommandLow => write!(f, "CommandLow"),
            Self::Ping => write!(f, "Ping"),
            Self::Pong => write!(f, "Pong"),
            Self::Ack => write!(f, "Ack"),
            Self::AckLow => write!(f, "AckLow"),
            Self::Voice => write!(f, "Voice"),
            Self::VoiceWhisper => write!(f, "VoiceWhisper"),
        }
    }
}

bitflags! {
    /// 包标志位
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct PacketFlags: u16 {
        /// 无标志
        const NONE = 0x0000;
        /// 未加密
        const UNENCRYPTED = 0x0008;
        /// 压缩
        const COMPRESSED = 0x0010;
        /// 分片
        const FRAGMENTED = 0x0020;
        /// 新协议
        const NEW_PROTOCOL = 0x0040;
    }
}

impl fmt::Display for PacketFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let flags = [
            (Self::UNENCRYPTED, "Unencrypted"),
            (Self::COMPRESSED, "Compressed"),
            (Self::FRAGMENTED, "Fragmented"),
            (Self::NEW_PROTOCOL, "NewProtocol"),
        ];

        let mut first = true;
        for (flag, name) in flags {
            if self.contains(flag) {
                if !first {
                    write!(f, "|")?;
                }
                write!(f, "{name}")?;
                first = false;
            }
        }

        if first {
            write!(f, "None")?;
        }
        Ok(())
    }
}

/// 包头
#[derive(Debug, Clone)]
pub struct PacketHeader {
    /// 包类型
    pub packet_type: PacketType,
    /// 包 ID
    pub packet_id: u16,
    /// 代数 ID
    pub generation_id: u32,
    /// 标志位
    pub flags: PacketFlags,
}

impl PacketHeader {
    /// 创建新的包头
    pub fn new(packet_type: PacketType, packet_id: u16, generation_id: u32) -> Self {
        Self {
            packet_type,
            packet_id,
            generation_id,
            flags: PacketFlags::NONE,
        }
    }

    /// 序列化为字节
    pub fn to_bytes(&self) -> [u8; 9] {
        let mut bytes = [0u8; 9];
        bytes[0] = self.packet_type as u8;
        bytes[1..3].copy_from_slice(&self.packet_id.to_be_bytes());
        bytes[3..7].copy_from_slice(&self.generation_id.to_be_bytes());
        bytes[7..9].copy_from_slice(&self.flags.bits().to_be_bytes());
        bytes
    }

    /// 从字节反序列化
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 9 {
            return Err(HeadlessError::PacketError("Header too short".into()));
        }

        let packet_type = PacketType::from_u8(data[0])?;
        let packet_id = u16::from_be_bytes([data[1], data[2]]);
        let generation_id = u32::from_be_bytes([data[3], data[4], data[5], data[6]]);
        let flags = PacketFlags::from_bits_truncate(u16::from_be_bytes([data[7], data[8]]));

        Ok(Self {
            packet_type,
            packet_id,
            generation_id,
            flags,
        })
    }
}

/// 网络包
#[derive(Debug, Clone)]
pub struct Packet {
    /// 包头
    pub header: PacketHeader,
    /// 包数据
    pub data: Vec<u8>,
    /// MAC 认证标签（8 字节）
    pub mac: [u8; 8],
}

impl Packet {
    /// 创建新包
    pub fn new(packet_type: PacketType, packet_id: u16, generation_id: u32, data: Vec<u8>) -> Self {
        Self {
            header: PacketHeader::new(packet_type, packet_id, generation_id),
            data,
            mac: [0u8; 8],
        }
    }

    /// 从原始字节创建
    pub fn from_raw(data: &[u8]) -> Result<Self> {
        if data.len() < 9 + 8 {
            // 至少需要包头 + MAC
            return Err(HeadlessError::PacketError("Packet too short".into()));
        }

        let header = PacketHeader::from_bytes(data)?;

        // MAC 在最后 8 字节
        let mac_start = data.len() - 8;
        let mut mac = [0u8; 8];
        mac.copy_from_slice(&data[mac_start..]);

        // 数据在包头之后，MAC 之前
        let payload = data[9..mac_start].to_vec();

        Ok(Self {
            header,
            data: payload,
            mac,
        })
    }

    /// 序列化为字节
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut result = Vec::with_capacity(9 + self.data.len() + 8);
        result.extend_from_slice(&self.header.to_bytes());
        result.extend_from_slice(&self.data);
        result.extend_from_slice(&self.mac);
        result
    }

    /// 是否需要确认
    pub fn needs_ack(&self) -> bool {
        self.header.packet_type.needs_ack()
    }
}

impl fmt::Display for Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{} id={} gen={} flags={} data_len={}]",
            self.header.packet_type,
            self.header.packet_id,
            self.header.generation_id,
            self.header.flags,
            self.data.len()
        )
    }
}

/// 最大包大小
pub const MAX_PACKET_SIZE: usize = 500;

/// 最大数据大小（减去包头和 MAC）
pub const MAX_DATA_SIZE: usize = MAX_PACKET_SIZE - 9 - 8;

/// 检查是否需要分片
pub fn needs_splitting(data_len: usize) -> bool {
    data_len + 9 + 8 > MAX_PACKET_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_type_from_u8() {
        assert_eq!(PacketType::from_u8(0).unwrap(), PacketType::Init1);
        assert_eq!(PacketType::from_u8(1).unwrap(), PacketType::Command);
        assert!(PacketType::from_u8(99).is_err());
    }

    #[test]
    fn test_packet_flags() {
        let flags = PacketFlags::UNENCRYPTED | PacketFlags::COMPRESSED;
        assert!(flags.contains(PacketFlags::UNENCRYPTED));
        assert!(flags.contains(PacketFlags::COMPRESSED));
        assert!(!flags.contains(PacketFlags::FRAGMENTED));
    }

    #[test]
    fn test_packet_header_roundtrip() {
        let header = PacketHeader::new(PacketType::Command, 12345, 67890);
        let bytes = header.to_bytes();
        let parsed = PacketHeader::from_bytes(&bytes).unwrap();

        assert_eq!(parsed.packet_type, PacketType::Command);
        assert_eq!(parsed.packet_id, 12345);
        assert_eq!(parsed.generation_id, 67890);
    }

    #[test]
    fn test_packet_roundtrip() {
        let data = vec![1, 2, 3, 4, 5];
        let mut packet = Packet::new(PacketType::Command, 100, 0, data.clone());
        packet.mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22];

        let bytes = packet.to_bytes();
        let parsed = Packet::from_raw(&bytes).unwrap();

        assert_eq!(parsed.header.packet_type, PacketType::Command);
        assert_eq!(parsed.header.packet_id, 100);
        assert_eq!(parsed.data, data);
        assert_eq!(parsed.mac, packet.mac);
    }

    #[test]
    fn test_needs_splitting() {
        assert!(!needs_splitting(100));
        assert!(needs_splitting(1000));
    }
}

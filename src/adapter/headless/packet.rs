//! TeamSpeak 包结构
//!
//! 对齐 TS3AudioBot 的线格式：
//! - Raw = [MAC(8)][Header(3 或 5)][Data]
//! - header 的 type/flags 合并在同一个字节（低 4 位 type，高 4 位 flags）

use bitflags::bitflags;
use std::fmt;

use super::error::{HeadlessError, Result};

/// MAC 长度（TeamSpeak 固定 8）
pub const MAC_LEN: usize = 8;

/// 包类型枚举（与 TS3AudioBot 一致）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PacketType {
    Voice = 0x0,
    VoiceWhisper = 0x1,
    Command = 0x2,
    CommandLow = 0x3,
    Ping = 0x4,
    Pong = 0x5,
    Ack = 0x6,
    AckLow = 0x7,
    Init1 = 0x8,
}

impl PacketType {
    /// 从低 4 位 type 值解析
    pub fn from_u8(value: u8) -> Result<Self> {
        match value & 0x0F {
            0x0 => Ok(Self::Voice),
            0x1 => Ok(Self::VoiceWhisper),
            0x2 => Ok(Self::Command),
            0x3 => Ok(Self::CommandLow),
            0x4 => Ok(Self::Ping),
            0x5 => Ok(Self::Pong),
            0x6 => Ok(Self::Ack),
            0x7 => Ok(Self::AckLow),
            0x8 => Ok(Self::Init1),
            v => Err(HeadlessError::InvalidPacketType(v)),
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
            Self::Voice => write!(f, "Voice"),
            Self::VoiceWhisper => write!(f, "VoiceWhisper"),
            Self::Command => write!(f, "Command"),
            Self::CommandLow => write!(f, "CommandLow"),
            Self::Ping => write!(f, "Ping"),
            Self::Pong => write!(f, "Pong"),
            Self::Ack => write!(f, "Ack"),
            Self::AckLow => write!(f, "AckLow"),
            Self::Init1 => write!(f, "Init1"),
        }
    }
}

bitflags! {
    /// 包标志位（与 TS3AudioBot 一致，放在高 4 位）
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct PacketFlags: u8 {
        const NONE = 0x00;
        const FRAGMENTED = 0x10;
        const NEW_PROTOCOL = 0x20;
        const COMPRESSED = 0x40;
        const UNENCRYPTED = 0x80;
    }
}

impl fmt::Display for PacketFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let flags = [
            (Self::FRAGMENTED, "Fragmented"),
            (Self::NEW_PROTOCOL, "NewProtocol"),
            (Self::COMPRESSED, "Compressed"),
            (Self::UNENCRYPTED, "Unencrypted"),
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

/// 包方向（影响 header 长度）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketDirection {
    /// Client -> Server，header = 5（packet_id + client_id + type_flags）
    C2S,
    /// Server -> Client，header = 3（packet_id + type_flags）
    S2C,
}

impl PacketDirection {
    pub const fn header_len(self) -> usize {
        match self {
            Self::C2S => 5,
            Self::S2C => 3,
        }
    }
}

/// 包头
#[derive(Debug, Clone)]
pub struct PacketHeader {
    /// 包类型
    pub packet_type: PacketType,
    /// 包 ID
    pub packet_id: u16,
    /// 代数 ID（不在线上传输，由接收窗口逻辑推导）
    pub generation_id: u32,
    /// 标志位
    pub flags: PacketFlags,
    /// C2S 时使用（S2C 为 None）
    pub client_id: Option<u16>,
}

impl PacketHeader {
    pub fn new(packet_type: PacketType, packet_id: u16, generation_id: u32) -> Self {
        Self {
            packet_type,
            packet_id,
            generation_id,
            flags: PacketFlags::NONE,
            client_id: None,
        }
    }

    fn type_flags_byte(&self) -> u8 {
        (self.packet_type as u8 & 0x0F) | (self.flags.bits() & 0xF0)
    }

    /// 仅序列化 header（不含 MAC）
    pub fn to_wire_header(&self, direction: PacketDirection) -> [u8; 5] {
        let mut out = [0u8; 5];
        out[0..2].copy_from_slice(&self.packet_id.to_be_bytes());
        match direction {
            PacketDirection::S2C => {
                out[2] = self.type_flags_byte();
            }
            PacketDirection::C2S => {
                let client_id = self.client_id.unwrap_or(0);
                out[2..4].copy_from_slice(&client_id.to_be_bytes());
                out[4] = self.type_flags_byte();
            }
        }
        out
    }

    /// 仅从 header 解析（不含 MAC）
    pub fn from_wire_header(data: &[u8], direction: PacketDirection) -> Result<Self> {
        let header_len = direction.header_len();
        if data.len() < header_len {
            return Err(HeadlessError::PacketError("Header too short".into()));
        }

        let packet_id = u16::from_be_bytes([data[0], data[1]]);
        let type_flags = match direction {
            PacketDirection::S2C => data[2],
            PacketDirection::C2S => data[4],
        };
        let packet_type = PacketType::from_u8(type_flags & 0x0F)?;
        let flags = PacketFlags::from_bits_truncate(type_flags & 0xF0);
        let client_id = match direction {
            PacketDirection::S2C => None,
            PacketDirection::C2S => Some(u16::from_be_bytes([data[2], data[3]])),
        };

        Ok(Self {
            packet_type,
            packet_id,
            generation_id: 0,
            flags,
            client_id,
        })
    }
}

/// 网络包
#[derive(Debug, Clone)]
pub struct Packet {
    pub header: PacketHeader,
    pub data: Vec<u8>,
    pub mac: [u8; MAC_LEN],
}

impl Packet {
    pub fn new(packet_type: PacketType, packet_id: u16, generation_id: u32, data: Vec<u8>) -> Self {
        Self {
            header: PacketHeader::new(packet_type, packet_id, generation_id),
            data,
            mac: [0u8; MAC_LEN],
        }
    }

    /// 按方向反序列化原始包
    pub fn from_raw(data: &[u8], direction: PacketDirection) -> Result<Self> {
        let header_len = direction.header_len();
        if data.len() < MAC_LEN + header_len {
            return Err(HeadlessError::PacketError("Packet too short".into()));
        }

        let mut mac = [0u8; MAC_LEN];
        mac.copy_from_slice(&data[..MAC_LEN]);

        let header =
            PacketHeader::from_wire_header(&data[MAC_LEN..MAC_LEN + header_len], direction)?;
        let payload = data[MAC_LEN + header_len..].to_vec();

        Ok(Self {
            header,
            data: payload,
            mac,
        })
    }

    /// 按方向序列化原始包：Raw = [MAC][Header][Data]
    pub fn to_bytes_with_direction(&self, direction: PacketDirection) -> Vec<u8> {
        let header_len = direction.header_len();
        let mut out = Vec::with_capacity(MAC_LEN + header_len + self.data.len());
        out.extend_from_slice(&self.mac);
        let header = self.header.to_wire_header(direction);
        out.extend_from_slice(&header[..header_len]);
        out.extend_from_slice(&self.data);
        out
    }

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

/// 最大包大小（与 TS3AudioBot 一致）
pub const MAX_PACKET_SIZE: usize = 500;

/// 最大数据大小（按 C2S 方向计算：MAC(8) + Header(5)）
pub const MAX_DATA_SIZE: usize = MAX_PACKET_SIZE - MAC_LEN - PacketDirection::C2S.header_len();

/// 检查是否需要分片
pub fn needs_splitting(data_len: usize) -> bool {
    data_len + MAC_LEN + PacketDirection::C2S.header_len() > MAX_PACKET_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_type_from_u8() {
        assert_eq!(PacketType::from_u8(0x8).unwrap(), PacketType::Init1);
        assert_eq!(PacketType::from_u8(0x2).unwrap(), PacketType::Command);
        assert!(PacketType::from_u8(0xF).is_err());
    }

    #[test]
    fn test_flags_and_type_same_byte() {
        let mut header = PacketHeader::new(PacketType::Command, 123, 0);
        header.flags = PacketFlags::UNENCRYPTED | PacketFlags::NEW_PROTOCOL;
        assert_eq!(header.type_flags_byte(), 0xA2);
    }

    #[test]
    fn test_packet_roundtrip_s2c() {
        let data = vec![1, 2, 3];
        let mut packet = Packet::new(PacketType::Command, 100, 0, data.clone());
        packet.header.flags = PacketFlags::NEW_PROTOCOL;
        packet.mac = [0x11; MAC_LEN];

        let raw = packet.to_bytes_with_direction(PacketDirection::S2C);
        let parsed = Packet::from_raw(&raw, PacketDirection::S2C).unwrap();

        assert_eq!(parsed.header.packet_type, PacketType::Command);
        assert_eq!(parsed.header.packet_id, 100);
        assert_eq!(parsed.header.flags, PacketFlags::NEW_PROTOCOL);
        assert_eq!(parsed.data, data);
        assert_eq!(parsed.mac, [0x11; MAC_LEN]);
    }

    #[test]
    fn test_packet_roundtrip_c2s() {
        let mut packet = Packet::new(PacketType::Ping, 7, 0, vec![9, 9]);
        packet.header.flags = PacketFlags::UNENCRYPTED;
        packet.header.client_id = Some(42);
        packet.mac = [0x22; MAC_LEN];

        let raw = packet.to_bytes_with_direction(PacketDirection::C2S);
        let parsed = Packet::from_raw(&raw, PacketDirection::C2S).unwrap();
        assert_eq!(parsed.header.client_id, Some(42));
        assert_eq!(parsed.header.packet_type, PacketType::Ping);
        assert_eq!(parsed.header.flags, PacketFlags::UNENCRYPTED);
        assert_eq!(parsed.data, vec![9, 9]);
    }
}

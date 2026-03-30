use serde::{Deserialize, Serialize};
use serde_json::Value;

// ─────────────────────────────────────────────
// 消息段（Message Segments）
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Segment {
    Text {
        text: String,
    },
    Image {
        file: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    At {
        qq: String,
    },
    Face {
        id: String,
    },
    Reply {
        id: String,
    },
    Record {
        file: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    Video {
        file: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

impl Segment {
    pub fn text(s: impl Into<String>) -> Self {
        Segment::Text { text: s.into() }
    }

    pub fn at(qq: i64) -> Self {
        Segment::At { qq: qq.to_string() }
    }

    /// 从消息段提取纯文本内容
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Segment::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// 将消息段列表拼合为纯文本（忽略非文本段）
pub fn segments_to_text(segments: &[Segment]) -> String {
    segments
        .iter()
        .filter_map(|s| s.as_text())
        .collect::<Vec<_>>()
        .join("")
}

// ─────────────────────────────────────────────
// Sender 信息
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Sender {
    pub user_id: i64,
    pub nickname: String,
    #[serde(default)]
    pub card: String, // 群名片（私聊为空）
    #[serde(default)]
    pub role: Option<String>, // owner / admin / member
}

// ─────────────────────────────────────────────
// API 请求 / 响应
// ─────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct NcAction {
    pub action: String,
    pub params: Value,
    pub echo: String,
}

#[derive(Debug, Deserialize)]
pub struct NcApiResponse {
    pub status: String, // "ok" | "failed"
    pub retcode: i64,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub echo: String,
    #[serde(default)]
    pub message: Option<String>,
}

impl NcApiResponse {
    pub fn is_ok(&self) -> bool {
        self.status == "ok" && self.retcode == 0
    }
}

use super::types::{Segment, Sender};
use serde::Deserialize;
use serde_json::Value;

// ─────────────────────────────────────────────
// 顶层事件枚举
// ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum NcEvent {
    PrivateMessage(PrivateMessageEvent),
    GroupMessage(GroupMessageEvent),
    FriendRequest(FriendRequestEvent),
    GroupRequest(GroupRequestEvent),
    Notice(NcNoticeEvent),
    Lifecycle(LifecycleEvent),
    Heartbeat,
    Unknown(Value),
}

// ─────────────────────────────────────────────
// 消息事件
// ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PrivateMessageEvent {
    pub message_id: i64,
    pub user_id: i64,
    pub raw_message: String,
    pub message: Vec<Segment>,
    pub sender: Sender,
    pub time: i64,
}

#[derive(Debug, Clone)]
pub struct GroupMessageEvent {
    pub message_id: i64,
    pub group_id: i64,
    pub user_id: i64,
    pub raw_message: String,
    pub message: Vec<Segment>,
    pub sender: Sender,
    pub time: i64,
}

// ─────────────────────────────────────────────
// 请求事件
// ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FriendRequestEvent {
    pub user_id: i64,
    pub comment: String,
    pub flag: String,
    pub time: i64,
}

#[derive(Debug, Clone)]
pub struct GroupRequestEvent {
    pub group_id: i64,
    pub user_id: i64,
    pub sub_type: String, // "add" | "invite"
    pub comment: String,
    pub flag: String,
    pub time: i64,
}

// ─────────────────────────────────────────────
// 通知事件
// ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum NcNoticeEvent {
    GroupMemberIncrease {
        group_id: i64,
        user_id: i64,
        operator_id: i64,
        sub_type: String,
    },
    GroupMemberDecrease {
        group_id: i64,
        user_id: i64,
        operator_id: i64,
        sub_type: String,
    },
    GroupBan {
        group_id: i64,
        user_id: i64,
        operator_id: i64,
        duration: i64,
        sub_type: String,
    },
    GroupRecall {
        group_id: i64,
        user_id: i64,
        operator_id: i64,
        message_id: i64,
    },
    FriendAdd {
        user_id: i64,
    },
    Other(Value),
}

// ─────────────────────────────────────────────
// 元事件
// ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LifecycleEvent {
    pub sub_type: String, // "connect" | "disconnect" | "enable" | "disable"
    pub self_id: i64,
    pub time: i64,
}

// ─────────────────────────────────────────────
// 原始 JSON 结构（供解析使用）
// ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RawEvent {
    post_type: Option<String>,
    meta_event_type: Option<String>,
    message_type: Option<String>,
    notice_type: Option<String>,
    request_type: Option<String>,
    sub_type: Option<String>,

    // 消息字段
    message_id: Option<i64>,
    group_id: Option<i64>,
    user_id: Option<i64>,
    raw_message: Option<String>,
    message: Option<Value>, // 可能是数组也可能是字符串（CQ码）
    sender: Option<Value>,
    time: Option<i64>,

    // 请求事件
    comment: Option<String>,
    flag: Option<String>,

    // 通知事件
    operator_id: Option<i64>,
    duration: Option<i64>,

    // 元事件
    self_id: Option<i64>,

    // 其余字段透传
    #[serde(flatten)]
    extra: Value,
}

/// 将原始 JSON 值解析为 NcEvent
pub fn parse_event(raw: Value) -> NcEvent {
    let ev: RawEvent = match serde_json::from_value(raw.clone()) {
        Ok(v) => v,
        Err(_) => return NcEvent::Unknown(raw),
    };

    match ev.post_type.as_deref() {
        Some("message") => parse_message_event(ev, raw),
        Some("notice") => parse_notice_event(ev, raw),
        Some("request") => parse_request_event(ev),
        Some("meta_event") => parse_meta_event(ev),
        _ => NcEvent::Unknown(raw),
    }
}

fn parse_message_event(ev: RawEvent, raw: Value) -> NcEvent {
    let message_id = ev.message_id.unwrap_or(0);
    let user_id = ev.user_id.unwrap_or(0);
    let time = ev.time.unwrap_or(0);
    let raw_message = ev.raw_message.unwrap_or_default();
    let message = parse_segments(ev.message.as_ref().unwrap_or(&Value::Array(vec![])));
    let sender = parse_sender(ev.sender.as_ref().unwrap_or(&Value::Null), user_id);

    match ev.message_type.as_deref() {
        Some("private") => NcEvent::PrivateMessage(PrivateMessageEvent {
            message_id,
            user_id,
            raw_message,
            message,
            sender,
            time,
        }),
        Some("group") => NcEvent::GroupMessage(GroupMessageEvent {
            message_id,
            group_id: ev.group_id.unwrap_or(0),
            user_id,
            raw_message,
            message,
            sender,
            time,
        }),
        _ => NcEvent::Unknown(raw),
    }
}

fn parse_notice_event(ev: RawEvent, raw: Value) -> NcEvent {
    let notice = match ev.notice_type.as_deref() {
        Some("group_increase") => NcNoticeEvent::GroupMemberIncrease {
            group_id: ev.group_id.unwrap_or(0),
            user_id: ev.user_id.unwrap_or(0),
            operator_id: ev.operator_id.unwrap_or(0),
            sub_type: ev.sub_type.unwrap_or_default(),
        },
        Some("group_decrease") => NcNoticeEvent::GroupMemberDecrease {
            group_id: ev.group_id.unwrap_or(0),
            user_id: ev.user_id.unwrap_or(0),
            operator_id: ev.operator_id.unwrap_or(0),
            sub_type: ev.sub_type.unwrap_or_default(),
        },
        Some("group_ban") => NcNoticeEvent::GroupBan {
            group_id: ev.group_id.unwrap_or(0),
            user_id: ev.user_id.unwrap_or(0),
            operator_id: ev.operator_id.unwrap_or(0),
            duration: ev.duration.unwrap_or(0),
            sub_type: ev.sub_type.unwrap_or_default(),
        },
        Some("group_recall") => NcNoticeEvent::GroupRecall {
            group_id: ev.group_id.unwrap_or(0),
            user_id: ev.user_id.unwrap_or(0),
            operator_id: ev.operator_id.unwrap_or(0),
            message_id: ev.message_id.unwrap_or(0),
        },
        Some("friend_add") => NcNoticeEvent::FriendAdd {
            user_id: ev.user_id.unwrap_or(0),
        },
        _ => NcNoticeEvent::Other(raw),
    };
    NcEvent::Notice(notice)
}

fn parse_request_event(ev: RawEvent) -> NcEvent {
    match ev.request_type.as_deref() {
        Some("friend") => NcEvent::FriendRequest(FriendRequestEvent {
            user_id: ev.user_id.unwrap_or(0),
            comment: ev.comment.unwrap_or_default(),
            flag: ev.flag.unwrap_or_default(),
            time: ev.time.unwrap_or(0),
        }),
        Some("group") => NcEvent::GroupRequest(GroupRequestEvent {
            group_id: ev.group_id.unwrap_or(0),
            user_id: ev.user_id.unwrap_or(0),
            sub_type: ev.sub_type.unwrap_or_default(),
            comment: ev.comment.unwrap_or_default(),
            flag: ev.flag.unwrap_or_default(),
            time: ev.time.unwrap_or(0),
        }),
        _ => NcEvent::Unknown(Value::Null),
    }
}

fn parse_meta_event(ev: RawEvent) -> NcEvent {
    match ev.meta_event_type.as_deref() {
        Some("heartbeat") => NcEvent::Heartbeat,
        Some("lifecycle") => NcEvent::Lifecycle(LifecycleEvent {
            sub_type: ev.sub_type.unwrap_or_default(),
            self_id: ev.self_id.unwrap_or(0),
            time: ev.time.unwrap_or(0),
        }),
        _ => NcEvent::Heartbeat,
    }
}

/// 解析 message 字段（数组格式）
fn parse_segments(val: &Value) -> Vec<Segment> {
    match val {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|seg| serde_json::from_value(seg.clone()).ok())
            .collect(),
        Value::String(s) => {
            // CQ码纯文本回退
            vec![Segment::Text { text: s.clone() }]
        }
        _ => vec![],
    }
}

fn parse_sender(val: &Value, fallback_uid: i64) -> Sender {
    serde_json::from_value(val.clone()).unwrap_or(Sender {
        user_id: fallback_uid,
        nickname: fallback_uid.to_string(),
        card: String::new(),
        role: None,
    })
}

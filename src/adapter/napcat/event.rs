use super::types::{Segment, Sender};
use serde::Deserialize;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub enum NcEvent {
    PrivateMessage(PrivateMessageEvent),
    GroupMessage(GroupMessageEvent),
    Heartbeat,
}

#[derive(Debug, Clone)]
pub struct PrivateMessageEvent {
    pub user_id: i64,
    pub message: Vec<Segment>,
    pub sender: Sender,
    pub timestamp: u64,
}

#[derive(Debug, Clone)]
pub struct GroupMessageEvent {
    pub group_id: i64,
    pub user_id: i64,
    pub message: Vec<Segment>,
    pub sender: Sender,
    pub timestamp: u64,
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    post_type: Option<String>,
    message_type: Option<String>,
    group_id: Option<i64>,
    user_id: Option<i64>,
    message: Option<Value>,
    sender: Option<Value>,
    time: Option<u64>,
}

pub fn parse_event(raw: Value) -> NcEvent {
    let ev: RawEvent = match serde_json::from_value(raw.clone()) {
        Ok(v) => v,
        Err(_) => return NcEvent::Heartbeat,
    };

    match ev.post_type.as_deref() {
        Some("message") => parse_message_event(ev),
        Some("meta_event") => NcEvent::Heartbeat,
        _ => NcEvent::Heartbeat,
    }
}

fn parse_message_event(ev: RawEvent) -> NcEvent {
    let Some(user_id) = ev.user_id else {
        return NcEvent::Heartbeat;
    };
    let message = parse_segments(ev.message.as_ref().unwrap_or(&Value::Array(vec![])));
    let sender = parse_sender(ev.sender.as_ref().unwrap_or(&Value::Null), user_id);
    let timestamp = ev.time.unwrap_or_else(now_unix_seconds);

    match ev.message_type.as_deref() {
        Some("private") => NcEvent::PrivateMessage(PrivateMessageEvent {
            user_id,
            message,
            sender,
            timestamp,
        }),
        Some("group") => {
            let Some(group_id) = ev.group_id else {
                return NcEvent::Heartbeat;
            };
            NcEvent::GroupMessage(GroupMessageEvent {
                group_id,
                user_id,
                message,
                sender,
                timestamp,
            })
        }
        _ => NcEvent::Heartbeat,
    }
}

fn parse_segments(val: &Value) -> Vec<Segment> {
    match val {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|seg| serde_json::from_value(seg.clone()).ok())
            .collect(),
        Value::String(s) => vec![Segment::Text { text: s.clone() }],
        _ => vec![],
    }
}

fn parse_sender(val: &Value, fallback_uid: i64) -> Sender {
    serde_json::from_value(val.clone()).unwrap_or(Sender {
        nickname: fallback_uid.to_string(),
    })
}

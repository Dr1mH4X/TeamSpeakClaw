use super::types::{Segment, Sender};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone)]
pub enum NcEvent {
    PrivateMessage(PrivateMessageEvent),
    GroupMessage(GroupMessageEvent),
    // #[derive(Debug, Clone)]
    // FriendRequest(FriendRequestEvent),
    // GroupRequest(GroupRequestEvent),
    // Notice(NcNoticeEvent),
    // Lifecycle(LifecycleEvent),
    Heartbeat,
    Unknown(Value),
}

#[derive(Debug, Clone)]
pub struct PrivateMessageEvent {
    pub message_id: i64,
    pub user_id: i64,
    pub message: Vec<Segment>,
    pub sender: Sender,
    pub time: i64,
}

#[derive(Debug, Clone)]
pub struct GroupMessageEvent {
    pub message_id: i64,
    pub group_id: i64,
    pub user_id: i64,
    pub message: Vec<Segment>,
    pub sender: Sender,
    pub time: i64,
}

// #[derive(Debug, Clone)]
// pub struct FriendRequestEvent {
//     pub user_id: i64,
//     pub comment: String,
//     pub flag: String,
//     pub time: i64,
// }
//
// #[derive(Debug, Clone)]
// pub struct GroupRequestEvent {
//     pub group_id: i64,
//     pub user_id: i64,
//     pub sub_type: String,
//     pub comment: String,
//     pub flag: String,
//     pub time: i64,
// }
//
// #[derive(Debug, Clone)]
// pub enum NcNoticeEvent {
//     GroupMemberIncrease { group_id: i64, user_id: i64, operator_id: i64, sub_type: String },
//     GroupMemberDecrease { group_id: i64, user_id: i64, operator_id: i64, sub_type: String },
//     GroupBan { group_id: i64, user_id: i64, operator_id: i64, duration: i64, sub_type: String },
//     GroupRecall { group_id: i64, user_id: i64, operator_id: i64, message_id: i64 },
//     FriendAdd { user_id: i64 },
//     Other(Value),
// }
//
// #[derive(Debug, Clone)]
// pub struct LifecycleEvent {
//     pub sub_type: String,
//     pub self_id: i64,
//     pub time: i64,
// }

#[derive(Debug, Deserialize)]
struct RawEvent {
    post_type: Option<String>,
    meta_event_type: Option<String>,
    message_type: Option<String>,
    message_id: Option<i64>,
    group_id: Option<i64>,
    user_id: Option<i64>,
    message: Option<Value>,
    sender: Option<Value>,
    time: Option<i64>,
    self_id: Option<i64>,
    sub_type: Option<String>,
}

pub fn parse_event(raw: Value) -> NcEvent {
    let ev: RawEvent = match serde_json::from_value(raw.clone()) {
        Ok(v) => v,
        Err(_) => return NcEvent::Unknown(raw),
    };

    match ev.post_type.as_deref() {
        Some("message") => parse_message_event(ev),
        Some("meta_event") => parse_meta_event(ev),
        _ => NcEvent::Unknown(raw),
    }
}

fn parse_message_event(ev: RawEvent) -> NcEvent {
    let user_id = ev.user_id.unwrap_or(0);
    let message = parse_segments(ev.message.as_ref().unwrap_or(&Value::Array(vec![])));
    let sender = parse_sender(ev.sender.as_ref().unwrap_or(&Value::Null), user_id);

    match ev.message_type.as_deref() {
        Some("private") => NcEvent::PrivateMessage(PrivateMessageEvent {
            message_id: ev.message_id.unwrap_or(0),
            user_id,
            message,
            sender,
            time: ev.time.unwrap_or(0),
        }),
        Some("group") => NcEvent::GroupMessage(GroupMessageEvent {
            message_id: ev.message_id.unwrap_or(0),
            group_id: ev.group_id.unwrap_or(0),
            user_id,
            message,
            sender,
            time: ev.time.unwrap_or(0),
        }),
        _ => NcEvent::Unknown(Value::Null),
    }
}

fn parse_meta_event(ev: RawEvent) -> NcEvent {
    match ev.meta_event_type.as_deref() {
        Some("heartbeat") => NcEvent::Heartbeat,
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

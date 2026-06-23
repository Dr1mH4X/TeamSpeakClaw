use super::types::{NcAction, Segment};
use serde_json::json;
use uuid::Uuid;

fn new_echo() -> String {
    Uuid::new_v4().to_string()
}

// 使用中的 API

// 消息发送 API

pub fn action_send_private_msg(user_id: i64, message: &[Segment]) -> NcAction {
    NcAction {
        action: "send_private_msg".into(),
        params: json!({
            "user_id": user_id,
            "message": message,
        }),
        echo: new_echo(),
    }
}

pub fn action_send_group_msg(group_id: i64, message: &[Segment]) -> NcAction {
    NcAction {
        action: "send_group_msg".into(),
        params: json!({
            "group_id": group_id,
            "message": message,
        }),
        echo: new_echo(),
    }
}

// 连接验证 API (获取登录信息)

pub fn action_get_login_info() -> NcAction {
    NcAction {
        action: "get_login_info".into(),
        params: json!({}),
        echo: new_echo(),
    }
}



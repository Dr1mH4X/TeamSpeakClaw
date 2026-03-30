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

// 预留 API (暂未使用，保留以备将来扩展)

// #[derive(Debug, Clone)]
// pub struct NapCatApi;
//
// impl NapCatApi {
//     pub fn action_delete_msg(message_id: i64) -> NcAction { ... }
//     pub fn action_get_msg(message_id: i64) -> NcAction { ... }
//     pub fn action_get_friend_list() -> NcAction { ... }
//     pub fn action_get_stranger_info(user_id: i64) -> NcAction { ... }
//     pub fn action_send_like(user_id: i64, times: u32) -> NcAction { ... }
//     pub fn action_get_group_list() -> NcAction { ... }
//     pub fn action_get_group_info(group_id: i64) -> NcAction { ... }
//     pub fn action_get_group_member_list(group_id: i64) -> NcAction { ... }
//     pub fn action_get_group_member_info(group_id: i64, user_id: i64) -> NcAction { ... }
//     pub fn action_set_group_kick(group_id: i64, user_id: i64, reject_add_request: bool) -> NcAction { ... }
//     pub fn action_set_group_ban(group_id: i64, user_id: i64, duration: u64) -> NcAction { ... }
//     pub fn action_set_group_whole_ban(group_id: i64, enable: bool) -> NcAction { ... }
//     pub fn action_set_group_card(group_id: i64, user_id: i64, card: &str) -> NcAction { ... }
//     pub fn action_set_group_admin(group_id: i64, user_id: i64, enable: bool) -> NcAction { ... }
//     pub fn action_set_group_leave(group_id: i64, is_dismiss: bool) -> NcAction { ... }
//     pub fn action_set_friend_add_request(flag: &str, approve: bool, remark: &str) -> NcAction { ... }
//     pub fn action_set_group_add_request(flag: &str, sub_type: &str, approve: bool) -> NcAction { ... }
//     pub fn action_get_version_info() -> NcAction { ... }
// }

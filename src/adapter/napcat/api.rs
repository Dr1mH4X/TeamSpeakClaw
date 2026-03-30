use super::types::{NcAction, Segment};
use serde_json::json;
use uuid::Uuid;

fn new_echo() -> String {
    Uuid::new_v4().to_string()
}

// ─────────────────────────────────────────────
// 消息 API
// ─────────────────────────────────────────────

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

pub fn action_delete_msg(message_id: i64) -> NcAction {
    NcAction {
        action: "delete_msg".into(),
        params: json!({ "message_id": message_id }),
        echo: new_echo(),
    }
}

pub fn action_get_msg(message_id: i64) -> NcAction {
    NcAction {
        action: "get_msg".into(),
        params: json!({ "message_id": message_id }),
        echo: new_echo(),
    }
}

// ─────────────────────────────────────────────
// 好友 / 用户 API
// ─────────────────────────────────────────────

pub fn action_get_friend_list() -> NcAction {
    NcAction {
        action: "get_friend_list".into(),
        params: json!({}),
        echo: new_echo(),
    }
}

pub fn action_get_stranger_info(user_id: i64) -> NcAction {
    NcAction {
        action: "get_stranger_info".into(),
        params: json!({ "user_id": user_id, "no_cache": false }),
        echo: new_echo(),
    }
}

pub fn action_send_like(user_id: i64, times: u32) -> NcAction {
    NcAction {
        action: "send_like".into(),
        params: json!({ "user_id": user_id, "times": times }),
        echo: new_echo(),
    }
}

// ─────────────────────────────────────────────
// 群组 API
// ─────────────────────────────────────────────

pub fn action_get_group_list() -> NcAction {
    NcAction {
        action: "get_group_list".into(),
        params: json!({}),
        echo: new_echo(),
    }
}

pub fn action_get_group_info(group_id: i64) -> NcAction {
    NcAction {
        action: "get_group_info".into(),
        params: json!({ "group_id": group_id, "no_cache": false }),
        echo: new_echo(),
    }
}

pub fn action_get_group_member_list(group_id: i64) -> NcAction {
    NcAction {
        action: "get_group_member_list".into(),
        params: json!({ "group_id": group_id }),
        echo: new_echo(),
    }
}

pub fn action_get_group_member_info(group_id: i64, user_id: i64) -> NcAction {
    NcAction {
        action: "get_group_member_info".into(),
        params: json!({ "group_id": group_id, "user_id": user_id, "no_cache": false }),
        echo: new_echo(),
    }
}

pub fn action_set_group_kick(group_id: i64, user_id: i64, reject_add_request: bool) -> NcAction {
    NcAction {
        action: "set_group_kick".into(),
        params: json!({
            "group_id": group_id,
            "user_id": user_id,
            "reject_add_request": reject_add_request,
        }),
        echo: new_echo(),
    }
}

pub fn action_set_group_ban(group_id: i64, user_id: i64, duration: u64) -> NcAction {
    NcAction {
        action: "set_group_ban".into(),
        params: json!({
            "group_id": group_id,
            "user_id": user_id,
            "duration": duration,
        }),
        echo: new_echo(),
    }
}

pub fn action_set_group_whole_ban(group_id: i64, enable: bool) -> NcAction {
    NcAction {
        action: "set_group_whole_ban".into(),
        params: json!({ "group_id": group_id, "enable": enable }),
        echo: new_echo(),
    }
}

pub fn action_set_group_card(group_id: i64, user_id: i64, card: &str) -> NcAction {
    NcAction {
        action: "set_group_card".into(),
        params: json!({ "group_id": group_id, "user_id": user_id, "card": card }),
        echo: new_echo(),
    }
}

pub fn action_set_group_admin(group_id: i64, user_id: i64, enable: bool) -> NcAction {
    NcAction {
        action: "set_group_admin".into(),
        params: json!({ "group_id": group_id, "user_id": user_id, "enable": enable }),
        echo: new_echo(),
    }
}

pub fn action_set_group_leave(group_id: i64, is_dismiss: bool) -> NcAction {
    NcAction {
        action: "set_group_leave".into(),
        params: json!({ "group_id": group_id, "is_dismiss": is_dismiss }),
        echo: new_echo(),
    }
}

// ─────────────────────────────────────────────
// 请求处理 API
// ─────────────────────────────────────────────

pub fn action_set_friend_add_request(flag: &str, approve: bool, remark: &str) -> NcAction {
    NcAction {
        action: "set_friend_add_request".into(),
        params: json!({ "flag": flag, "approve": approve, "remark": remark }),
        echo: new_echo(),
    }
}

pub fn action_set_group_add_request(flag: &str, sub_type: &str, approve: bool) -> NcAction {
    NcAction {
        action: "set_group_add_request".into(),
        params: json!({ "flag": flag, "sub_type": sub_type, "approve": approve }),
        echo: new_echo(),
    }
}

// ─────────────────────────────────────────────
// 机器人信息 API
// ─────────────────────────────────────────────

pub fn action_get_login_info() -> NcAction {
    NcAction {
        action: "get_login_info".into(),
        params: json!({}),
        echo: new_echo(),
    }
}

pub fn action_get_version_info() -> NcAction {
    NcAction {
        action: "get_version_info".into(),
        params: json!({}),
        echo: new_echo(),
    }
}

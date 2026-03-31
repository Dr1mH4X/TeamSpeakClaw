use serde::{Deserialize, Serialize};

/// NapCat 适配器配置
///
/// 用于配置 QQ 机器人功能，通过 NapCat（OneBot 11 协议实现）连接 QQ。
///
/// # 字段说明
///
/// - `enabled`: 是否启用 NapCat 适配器
/// - `ws_url`: NapCat WebSocket 服务地址（默认 `ws://127.0.0.1:3001`）
/// - `access_token`: 访问令牌（若 NapCat 配置了鉴权则填写）
/// - `listen_groups`: 监听的群 ID 列表，空列表表示监听所有群
/// - `trigger_prefixes`: 群聊触发前缀（私聊无需前缀）
/// - `trusted_groups`: 信任的群 ID 列表，群内所有成员可使用机器人
/// - `trusted_users`: 信任的用户 QQ 号列表，私聊和群聊均可使用
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct NapCatConfig {
    /// 是否启用 NapCat 适配器
    pub enabled: bool,
    /// NapCat WebSocket 服务地址
    pub ws_url: String,
    /// 访问令牌（若 NapCat 配置了鉴权则填写）
    pub access_token: String,
    /// 监听的群 ID 列表，空列表表示监听所有群
    pub listen_groups: Vec<i64>,
    /// 群聊触发前缀（私聊无需前缀）
    pub trigger_prefixes: Vec<String>,
    /// 信任的群 ID 列表，群内所有成员可使用机器人
    pub trusted_groups: Vec<i64>,
    /// 信任的用户 QQ 号列表，私聊和群聊均可使用
    pub trusted_users: Vec<i64>,
}

impl Default for NapCatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ws_url: "ws://127.0.0.1:3001".to_string(),
            access_token: String::new(),
            listen_groups: vec![],
            trigger_prefixes: vec!["!claw".to_string(), "!bot".to_string()],
            trusted_groups: vec![],
            trusted_users: vec![],
        }
    }
}

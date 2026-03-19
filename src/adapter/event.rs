#[derive(Debug, Clone)]
pub enum TsEvent {
    TextMessage(TextMessageEvent),
    ClientEnterView(ClientEnterEvent),
    ClientLeftView(ClientLeftEvent),
    Unknown,
}

#[derive(Debug, Clone)]
pub struct TextMessageEvent {
    pub target_mode: TextMessageTarget,
    pub invoker_name: String,
    #[allow(dead_code)]
    pub invoker_uid: String,
    pub invoker_id: u32, // clid（会话）
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TextMessageTarget {
    Private, // targetmode=1（私聊）
    Channel, // targetmode=2（频道）
    Server,  // targetmode=3（服务器）
}

#[derive(Debug, Clone)]
pub struct ClientEnterEvent {
    pub clid: u32,
    pub cldbid: u32,
    pub client_nickname: String,
    pub client_server_groups: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct ClientLeftEvent {
    pub clid: u32,
}

/// 将一行原始 ServerQuery 通知解析为 TsEvent 列表。
pub fn parse_events(line: &str) -> Vec<TsEvent> {
    if line.starts_with("notifytextmessage") {
        vec![parse_text_message(line)]
    } else if line.starts_with("notifycliententerview") {
        vec![parse_client_enter(line)]
    } else if line.starts_with("notifyclientleftview") {
        vec![parse_client_left(line)]
    } else if line.starts_with("clid=") {
        // 处理 clientlist 响应（使用 '|' 分隔）
        line.split('|').map(parse_client_enter).collect()
    } else {
        vec![]
    }
}

fn kv(line: &str, key: &str) -> Option<String> {
    line.split_whitespace()
        .find(|s| s.starts_with(&format!("{key}=")))
        .map(|s| {
            let v = &s[key.len() + 1..];
            ts_unescape(v)
        })
}

fn ts_unescape(s: &str) -> String {
    s.replace("\\s", " ")
        .replace("\\p", "|")
        .replace("\\n", "\n")
        .replace("\\\\", "\\")
        .replace("\\/", "/")
}

fn parse_text_message(line: &str) -> TsEvent {
    let target_mode = match kv(line, "targetmode").as_deref() {
        Some("1") => TextMessageTarget::Private,
        Some("2") => TextMessageTarget::Channel,
        Some("3") => TextMessageTarget::Server,
        _ => return TsEvent::Unknown,
    };
    let invoker_name = kv(line, "invokername").unwrap_or_default();
    let invoker_uid = kv(line, "invokeruid").unwrap_or_default();
    let invoker_id = kv(line, "invokerid")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let message = kv(line, "msg").unwrap_or_default();

    TsEvent::TextMessage(TextMessageEvent {
        target_mode,
        invoker_name,
        invoker_uid,
        invoker_id,
        message,
    })
}

fn parse_client_enter(line: &str) -> TsEvent {
    let clid = kv(line, "clid").and_then(|v| v.parse().ok()).unwrap_or(0);
    let cldbid = kv(line, "client_database_id")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let client_nickname = kv(line, "client_nickname").unwrap_or_default();
    let groups = kv(line, "client_servergroups")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.parse().ok())
        .collect();

    TsEvent::ClientEnterView(ClientEnterEvent {
        clid,
        cldbid,
        client_nickname,
        client_server_groups: groups,
    })
}

fn parse_client_left(line: &str) -> TsEvent {
    let clid = kv(line, "clid").and_then(|v| v.parse().ok()).unwrap_or(0);
    TsEvent::ClientLeftView(ClientLeftEvent { clid })
}

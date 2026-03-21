use crate::error::{AppError, Result};

/// 所有 ServerQuery 的响应错误都包含 `id=` 字段。
#[allow(dead_code)]
pub fn check_ts_error(response: &str) -> Result<()> {
    let id: u32 = response
        .split_whitespace()
        .find(|s| s.starts_with("error id="))
        .or_else(|| response.split_whitespace().find(|s| s.starts_with("id=")))
        .and_then(|s| {
            let val = if s.starts_with("error id=") {
                &s[9..]
            } else {
                &s[3..]
            };
            val.parse().ok()
        })
        .unwrap_or(0);

    if id == 0 {
        return Ok(());
    }
    let msg = response
        .split_whitespace()
        .find(|s| s.starts_with("msg="))
        .map(|s| ts_unescape(&s[4..]))
        .unwrap_or_else(|| "unknown error".into());
    Err(AppError::TsError {
        code: id,
        message: msg,
    })
}

#[allow(dead_code)]
fn ts_unescape(s: &str) -> String {
    s.replace("\\s", " ")
        .replace("\\p", "|")
        .replace("\\\\", "\\")
}

pub fn ts_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(' ', "\\s")
        .replace('|', "\\p")
        .replace('\n', "\\n")
        .replace('\r', "")
        .replace('/', "\\/")
}

/// 高层命令构建器：返回要发送的原始查询字符串。
pub fn cmd_login(name: &str, pass: &str) -> String {
    let mut s = String::with_capacity(6 + name.len() + pass.len() + 2);
    s.push_str("login ");
    s.push_str(&ts_escape(name));
    s.push(' ');
    s.push_str(&ts_escape(pass));
    s
}
pub fn cmd_use(server_id: u32) -> String {
    format!("use {server_id}")
}
pub fn cmd_whoami() -> String {
    "whoami".into()
}
#[allow(dead_code)]
pub fn cmd_version() -> String {
    "version".into()
}
pub fn cmd_clientupdate_nick(nick: &str) -> String {
    format!("clientupdate client_nickname={}", ts_escape(nick))
}
pub fn cmd_register_event(event: &str) -> String {
    if event == "textchannel" {
        format!("servernotifyregister event={event} id=0")
    } else {
        format!("servernotifyregister event={event}")
    }
}
#[allow(dead_code)]
pub fn cmd_clientlist() -> String {
    "clientlist -groups".into()
}
#[allow(dead_code)]
pub fn cmd_clientfind(pattern: &str) -> String {
    format!("clientfind pattern={}", ts_escape(pattern))
}
#[allow(dead_code)]
pub fn cmd_clientinfo(clid: u32) -> String {
    format!("clientinfo clid={clid}")
}
#[allow(dead_code)]
pub fn cmd_clientdbinfo(cldbid: u32) -> String {
    format!("clientdbinfo cldbid={cldbid}")
}
pub fn cmd_poke(clid: u32, msg: &str) -> String {
    format!("clientpoke clid={clid} msg={}", ts_escape(msg))
}
pub fn cmd_send_text(target_mode: u8, target: u32, msg: &str) -> String {
    format!(
        "sendtextmessage targetmode={target_mode} target={target} msg={}",
        ts_escape(msg)
    )
}
pub fn cmd_kick(clid: u32, reason: &str) -> String {
    format!(
        "clientkick clid={clid} reasonid=5 reasonmsg={}",
        ts_escape(reason)
    )
}
pub fn cmd_ban(clid: u32, time_secs: u64, reason: &str) -> String {
    format!(
        "banclient clid={clid} time={time_secs} banreason={}",
        ts_escape(reason)
    )
}
#[allow(dead_code)]
pub fn cmd_move(clid: u32, channel_id: u32) -> String {
    format!("clientmove clid={clid} cid={channel_id}")
}
#[allow(dead_code)]
pub fn cmd_serverinfo() -> String {
    "serverinfo".into()
}
#[allow(dead_code)]
pub fn cmd_channellist() -> String {
    "channellist".into()
}
pub fn cmd_clientlist_uid_groups() -> String {
    "clientlist -uid -groups".into()
}
pub fn cmd_quit() -> String {
    "quit".into()
}

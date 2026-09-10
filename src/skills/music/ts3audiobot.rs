use super::chat::send_and_await_reply;
use crate::adapter::headless::TextMessageTarget;
use crate::skills::UnifiedExecutionContext;
use anyhow::Result;
use serde_json::Value;

/// TS3AudioBot 网易云插件聊天命令（上游 README「目前的指令」）
/// https://github.com/ZHANGTIANYAO1/TS3AudioBot-NetEaseCloudmusic-plugin
fn build_bot_cmd(action: &str, value: &str) -> Result<String> {
    let cmd = match action {
        "next" => "!yun next".to_string(),
        "stop" => "!yun stop".to_string(),
        "login" => "!yun login".to_string(),
        "play" => format!("!yun play {value}"),
        "add" => format!("!yun add {value}"),
        "gedan" => format!("!yun gedan {value}"),
        "gedanid" => format!("!yun gedanid {value}"),
        "playid" => format!("!yun playid {value}"),
        "addid" => format!("!yun addid {value}"),
        "mode" => format!("!yun mode {value}"),
        other => {
            return Err(anyhow::anyhow!(
                "Action '{}' is not supported by the ts3audiobot backend.",
                other
            ))
        }
    };
    Ok(cmd)
}

fn needs_value(action: &str) -> bool {
    matches!(
        action,
        "play" | "add" | "gedan" | "gedanid" | "playid" | "addid" | "mode"
    )
}

pub(crate) async fn execute(
    action: &str,
    args: &Value,
    ctx: &UnifiedExecutionContext,
) -> Result<Value> {
    if needs_value(action) && args["value"].as_str().unwrap_or("").is_empty() {
        return Err(anyhow::anyhow!(
            "Action '{}' requires a 'value' parameter",
            action
        ));
    }

    let value = args["value"].as_str().unwrap_or("");
    let bot_cmd = build_bot_cmd(action, value)?;

    send_and_await_reply(ctx, &bot_cmd, TextMessageTarget::Private, "TS3AudioBot").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_all_documented_plugin_commands() {
        assert_eq!(build_bot_cmd("next", "").unwrap(), "!yun next");
        assert_eq!(build_bot_cmd("login", "").unwrap(), "!yun login");
        assert_eq!(build_bot_cmd("play", "稻香").unwrap(), "!yun play 稻香");
        assert_eq!(build_bot_cmd("add", "稻香").unwrap(), "!yun add 稻香");
        assert_eq!(build_bot_cmd("gedan", "热歌").unwrap(), "!yun gedan 热歌");
        assert_eq!(
            build_bot_cmd("gedanid", "2139305008").unwrap(),
            "!yun gedanid 2139305008"
        );
        assert_eq!(
            build_bot_cmd("playid", "123").unwrap(),
            "!yun playid 123"
        );
        assert_eq!(build_bot_cmd("addid", "123").unwrap(), "!yun addid 123");
        assert_eq!(build_bot_cmd("mode", "2").unwrap(), "!yun mode 2");
        assert_eq!(build_bot_cmd("stop", "").unwrap(), "!yun stop");
    }

    #[test]
    fn rejects_unknown_action() {
        assert!(build_bot_cmd("pause", "").is_err());
    }

    #[test]
    fn value_required_actions() {
        assert!(needs_value("play"));
        assert!(needs_value("mode"));
        assert!(!needs_value("next"));
        assert!(!needs_value("login"));
    }
}

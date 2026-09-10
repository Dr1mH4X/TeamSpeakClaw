use super::chat::send_and_await_reply;
use crate::adapter::headless::TextMessageTarget;
use crate::skills::UnifiedExecutionContext;
use anyhow::Result;
use serde_json::Value;

/// TSMusicBot 聊天命令（上游 README「TeamSpeak 文字命令」）
/// https://github.com/ZHANGTIANYAO1/teamspeak-music-bot
fn build_bot_cmd(action: &str, value: &str) -> Result<String> {
    let cmd = match action {
        "play" => format!("!play {value}"),
        "add" => format!("!add {value}"),
        "search" => format!("!search {value}"),
        "playlist" => format!("!playlist {value}"),
        "pause" => "!pause".to_string(),
        "resume" => "!resume".to_string(),
        "next" | "skip" => "!next".to_string(),
        "previous" | "prev" => "!prev".to_string(),
        "stop" => "!stop".to_string(),
        "vol" | "volume" => format!("!vol {value}"),
        "mode" => format!("!mode {value}"),
        "queue" => "!queue".to_string(),
        "now" => "!now".to_string(),
        "fm" => "!fm".to_string(),
        other => {
            return Err(anyhow::anyhow!(
                "Action '{}' is not supported by the tsmusicbot backend.",
                other
            ))
        }
    };
    Ok(cmd)
}

fn needs_value(action: &str) -> bool {
    matches!(
        action,
        "play" | "add" | "search" | "playlist" | "vol" | "mode"
    )
}

pub(crate) async fn execute(
    action: &str,
    args: &Value,
    ctx: &UnifiedExecutionContext,
) -> Result<Value> {
    if needs_value(action)
        && args["value"].as_str().unwrap_or("").is_empty()
        && args["keywords"].as_str().unwrap_or("").is_empty()
    {
        return Err(anyhow::anyhow!(
            "Action '{}' requires a 'value' or 'keywords' parameter",
            action
        ));
    }

    let value = args["value"]
        .as_str()
        .or_else(|| args["keywords"].as_str())
        .unwrap_or("");

    let bot_cmd = build_bot_cmd(action, value)?;

    send_and_await_reply(ctx, &bot_cmd, TextMessageTarget::Channel, "TSMusicBot").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_all_documented_chat_commands() {
        assert_eq!(build_bot_cmd("play", "稻香").unwrap(), "!play 稻香");
        assert_eq!(build_bot_cmd("add", "稻香").unwrap(), "!add 稻香");
        assert_eq!(build_bot_cmd("search", "稻香").unwrap(), "!search 稻香");
        assert_eq!(build_bot_cmd("playlist", "热歌").unwrap(), "!playlist 热歌");
        assert_eq!(build_bot_cmd("pause", "").unwrap(), "!pause");
        assert_eq!(build_bot_cmd("resume", "").unwrap(), "!resume");
        assert_eq!(build_bot_cmd("next", "").unwrap(), "!next");
        assert_eq!(build_bot_cmd("skip", "").unwrap(), "!next");
        assert_eq!(build_bot_cmd("prev", "").unwrap(), "!prev");
        assert_eq!(build_bot_cmd("previous", "").unwrap(), "!prev");
        assert_eq!(build_bot_cmd("stop", "").unwrap(), "!stop");
        assert_eq!(build_bot_cmd("vol", "50").unwrap(), "!vol 50");
        assert_eq!(build_bot_cmd("volume", "50").unwrap(), "!vol 50");
        assert_eq!(build_bot_cmd("mode", "loop").unwrap(), "!mode loop");
        assert_eq!(build_bot_cmd("queue", "").unwrap(), "!queue");
        assert_eq!(build_bot_cmd("now", "").unwrap(), "!now");
        assert_eq!(build_bot_cmd("fm", "").unwrap(), "!fm");
    }

    #[test]
    fn rejects_unknown_action() {
        assert!(build_bot_cmd("login", "").is_err());
    }

    #[test]
    fn value_or_keywords_satisfies_needs_value() {
        assert!(needs_value("play"));
        assert!(needs_value("vol"));
        assert!(!needs_value("pause"));
    }
}

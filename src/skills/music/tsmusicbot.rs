use crate::adapter::{TextMessageTarget, TsEvent};
use crate::skills::ExecutionContext;
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info};

static TSMUSICBOT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub(crate) async fn execute(
    action: &str,
    args: &Value,
    ctx: &ExecutionContext<'_>,
) -> Result<Value> {
    let needs_value = matches!(
        action,
        "play" | "add" | "search" | "playlist" | "vol" | "mode"
    );
    if needs_value && args["value"].as_str().unwrap_or("").is_empty() && args["keywords"].as_str().unwrap_or("").is_empty() {
        return Err(anyhow::anyhow!(
            "Action '{}' requires a 'value' or 'keywords' parameter",
            action
        ));
    }

    let value = args["value"].as_str().or_else(|| args["keywords"].as_str()).unwrap_or("");

    let bot_cmd = match action {
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

    let target_name = &ctx.config.music_backend.musicbot_name;
    let clients: Vec<_> = ctx.clients.iter().map(|r| r.value().clone()).collect();
    let audiobot = clients
        .iter()
        .find(|c| c.nickname.to_ascii_lowercase().contains(&target_name.to_ascii_lowercase()))
        .ok_or_else(|| anyhow::anyhow!("Music bot '{}' not found online", target_name))?;

    let mut ts_rx;
    {
        let _guard = TSMUSICBOT_LOCK.lock().await;
        ts_rx = ctx.adapter.subscribe();

        ctx.adapter
            .send_text_message(1, audiobot.clid, &bot_cmd)
            .await?;
    }

    let reply = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ts_rx.recv().await {
                Ok(TsEvent::TextMessage(msg))
                    if msg.invoker_name.eq_ignore_ascii_case(target_name)
                        && msg.target_mode == TextMessageTarget::Channel =>
                {
                    return msg.message;
                }
                Ok(_) => continue,
                Err(e) => {
                    debug!("TS event channel error while waiting for TSMusicBot reply: {e}");
                    return String::new();
                }
            }
        }
    })
    .await;

    match reply {
        Ok(content) if !content.is_empty() => {
            info!("TSMusicBot replied: {content}");
            Ok(content.into())
        }
        Err(_) => {
            debug!("Timed out waiting for TSMusicBot reply");
            Ok(json!({
                "status": "timeout",
                "sent_to": "TSMusicBot",
                "command": bot_cmd
            }))
        }
        _ => Ok(json!({
            "status": "ok",
            "sent_to": "TSMusicBot",
            "command": bot_cmd
        })),
    }
}

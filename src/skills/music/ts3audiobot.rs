use crate::adapter::{TextMessageTarget, TsEvent};
use crate::skills::ExecutionContext;
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info};

static TS3AUDIOBOT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub(crate) async fn execute(action: &str, args: &Value, ctx: &ExecutionContext) -> Result<Value> {
    let needs_value = matches!(
        action,
        "play" | "add" | "gedan" | "gedanid" | "playid" | "addid" | "mode"
    );
    if needs_value && args["value"].as_str().unwrap_or("").is_empty() {
        return Err(anyhow::anyhow!(
            "Action '{}' requires a 'value' parameter",
            action
        ));
    }

    let value = args["value"].as_str().unwrap_or("");

    let bot_cmd = match action {
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

    let target_name = ctx
        .config
        .music_backend
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("MusicControl registered but music_backend is None"))?
        .musicbot_name
        .as_str();
    let clients = ctx.adapter.list_clients().await?;
    let audiobot = clients
        .iter()
        .find(|c| {
            c.nickname
                .to_ascii_lowercase()
                .contains(&target_name.to_ascii_lowercase())
        })
        .ok_or_else(|| anyhow::anyhow!("Music bot '{}' not found online", target_name))?;

    let _guard = TS3AUDIOBOT_LOCK.lock().await;

    let mut ts_rx = ctx.adapter.subscribe();

    ctx.adapter
        .send_text_message(1, audiobot.id as u32, &bot_cmd)
        .await?;

    let reply = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ts_rx.recv().await {
                Ok(TsEvent::TextMessage(msg))
                    if msg
                        .invoker_name
                        .to_ascii_lowercase()
                        .contains(&target_name.to_ascii_lowercase())
                        && msg.target_mode == TextMessageTarget::Private =>
                {
                    return msg.message;
                }
                Ok(_) => continue,
                Err(e) => {
                    debug!("TS event channel error while waiting for TS3AudioBot reply: {e}");
                    return String::new();
                }
            }
        }
    })
    .await;

    drop(_guard);

    match reply {
        Ok(content) if !content.is_empty() => {
            info!("TS3AudioBot replied: {content}");
            Ok(content.into())
        }
        _ => Ok(json!({
            "status": "ok",
            "sent_to": "TS3AudioBot",
            "command": bot_cmd
        })),
    }
}

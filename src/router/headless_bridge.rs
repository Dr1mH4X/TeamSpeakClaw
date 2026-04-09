use std::sync::Arc;

use anyhow::Result;
use serde_json::json;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tracing::{error, info, warn};

use crate::adapter::headless::INTERNAL_GRPC_ADDR;
use crate::config::PromptsConfig;
use crate::llm::LlmEngine;

use crate::adapter::headless::tsbot::voice::v1 as voicev1;
use voicev1::voice_service_client::VoiceServiceClient;

pub struct HeadlessLlmBridge {
    prompts: Arc<PromptsConfig>,
    llm: Arc<LlmEngine>,
}

impl HeadlessLlmBridge {
    pub fn new(prompts: Arc<PromptsConfig>, llm: Arc<LlmEngine>) -> Self {
        Self { prompts, llm }
    }

    pub async fn run(self) -> Result<()> {
        let endpoint = format!("http://{}", INTERNAL_GRPC_ADDR);
        let channel = Channel::from_shared(endpoint.clone())?.connect().await?;
        let mut client = VoiceServiceClient::new(channel);

        let req = tonic::Request::new(voicev1::SubscribeRequest {
            include_chat: true,
            include_playback: false,
            include_log: true,
            include_audio: false,
        });

        let mut stream = client.subscribe_events(req).await?.into_inner();
        info!("Headless LLM bridge subscribed: {}", endpoint);

        while let Some(item) = stream.next().await {
            match item {
                Ok(ev) => {
                    let Some(payload) = ev.payload else {
                        continue;
                    };
                    if let voicev1::event::Payload::Chat(chat) = payload {
                        if !chat.should_trigger_llm {
                            continue;
                        }
                        if let Err(e) = self.handle_chat(&mut client, chat).await {
                            error!("headless bridge chat handling failed: {e}");
                        }
                    }
                }
                Err(e) => {
                    warn!("headless event stream error: {e}");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn handle_chat(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        chat: voicev1::ChatEvent,
    ) -> Result<()> {
        let system_prompt = &self.prompts.system.content;
        let user_ctx = format!(
            "User: {} (uid: {}) [headless-bridge]",
            chat.invoker_name, chat.invoker_unique_id
        );
        let user_msg = chat.message.clone();

        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "set_client_description",
                "description": "Set TeamSpeak client description via headless bridge",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "description": { "type": "string" }
                    },
                    "required": ["description"]
                }
            }
        })];

        let mut messages = vec![
            json!({"role":"system","content":system_prompt}),
            json!({"role":"system","content":user_ctx}),
            json!({"role":"user","content":user_msg}),
        ];

        let response = self.llm.chat(messages.clone(), tools.clone()).await?;
        if response.tool_calls.is_empty() {
            if let Some(content) = response.content {
                self.send_reply(client, &chat, &content).await?;
            }
            return Ok(());
        }

        for call in &response.tool_calls {
            if call.name == "set_client_description" {
                let desc = call.arguments["description"].as_str().unwrap_or("").trim();
                if desc.is_empty() {
                    self.send_reply(client, &chat, "描述不能为空").await?;
                    continue;
                }

                let result = client
                    .set_client_description(tonic::Request::new(
                        voicev1::SetClientDescriptionRequest {
                            description: desc.to_string(),
                        },
                    ))
                    .await?
                    .into_inner();

                let msg = if result.ok {
                    "已尝试更新机器人描述"
                } else {
                    "更新描述失败"
                };
                self.send_reply(client, &chat, msg).await?;

                messages.push(json!({
                    "role":"assistant",
                    "tool_calls":[{"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments.to_string()}}]
                }));
                messages.push(json!({
                    "role":"tool",
                    "tool_call_id":call.id,
                    "name":call.name,
                    "content":result.message
                }));
            }
        }
        Ok(())
    }

    async fn send_reply(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        chat: &voicev1::ChatEvent,
        text: &str,
    ) -> Result<()> {
        let req = voicev1::NoticeRequest {
            message: text.to_string(),
            target_mode: chat.reply_target_mode,
            target_client_id: chat.reply_target_client_id,
        };
        let _ = client.send_notice(tonic::Request::new(req)).await?;
        Ok(())
    }
}

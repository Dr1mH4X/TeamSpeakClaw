use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Tool definition for LLM (OpenAI format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub r#type: String,
    pub function: Function,
}

impl Tool {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            r#type: "function".to_string(),
            function: Function {
                name: name.into(),
                description: Some(description.into()),
                parameters,
            },
        }
    }
}

/// Function definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value, // JSON Schema object
}

/// Tool call from LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub r#type: String, // "function"
    pub function: ToolCallFunction,
}

/// Function call details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String, // JSON string
}

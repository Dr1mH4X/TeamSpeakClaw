use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("TeamSpeak error {code}: {message}")]
    TsError { code: u32, message: String },

    #[error("Permission denied: {reason}")]
    PermissionDenied { reason: String },

    #[error("Target is protected")]
    TargetProtected,

    #[error("LLM error: {0}")]
    LlmError(#[from] anyhow::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error("Config error: {0}")]
    ConfigError(String),
}

pub type Result<T> = std::result::Result<T, AppError>;

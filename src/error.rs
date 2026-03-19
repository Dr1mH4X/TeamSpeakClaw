use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("TeamSpeak error {code}: {message}")]
    TsError { code: u32, message: String },

    #[error("Permission denied: {reason}")]
    PermissionDenied { reason: String },

    #[error("Rate limited")]
    RateLimited,

    #[error("Target not found: {name}")]
    TargetNotFound { name: String },

    #[error("Target is protected")]
    TargetProtected,

    #[error("LLM error: {0}")]
    LlmError(#[from] anyhow::Error),
}

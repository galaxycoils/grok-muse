use thiserror::Error;

#[derive(Debug, Error)]
pub enum GmError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("IPC protocol error: {0}")]
    Ipc(String),

    #[error("Platform error: {0}")]
    Platform(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Safety violation: {0}")]
    SafetyViolation(String),

    #[error("LLM Provider error: {0}")]
    Provider(String),

    #[error("Budget exceeded: {0}")]
    BudgetExceeded(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("Configuration error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, GmError>;

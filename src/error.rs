use thiserror::Error;

#[derive(Debug, Error)]
pub enum KexshError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Server error: {0}")]
    Server(String),
    #[error("IPC error: {0}")]
    Ipc(String),
    #[error("Config error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, KexshError>;

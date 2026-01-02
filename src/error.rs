use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[cfg(windows)]
    #[error("Windows error: {0}")]
    Windows(#[from] windows::core::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Job not found: {0}")]
    JobNotFound(u32),

    #[error("Job already exists: {0}")]
    JobExists(u32),

    #[error("Invalid state transition: {0}")]
    InvalidStateTransition(String),

    #[error("Authorization failed: {0}")]
    AuthorizationFailed(String),

    #[error("Process launch failed: {0}")]
    ProcessLaunchFailed(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Service error: {0}")]
    Service(String),
}

pub type Result<T> = std::result::Result<T, Error>;

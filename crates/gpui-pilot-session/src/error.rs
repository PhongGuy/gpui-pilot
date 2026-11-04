use gpui_pilot_protocol::{ErrorCode, PilotError};

/// Every way the session can fail; each variant maps to the protocol code of the same name.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("{tool} is missing: {why}")]
    EnvironmentMissing { tool: String, why: String },
    #[error("isolation breach: {address} accepted a connection")]
    IsolationBreach { address: String },
    #[error("stage {stage}: {message}")]
    LaunchFailed {
        stage: &'static str,
        message: String,
    },
    #[error("the session keymap cannot type {0:?}")]
    UnsupportedCharacter(char),
    #[error("invalid keystroke {0:?}")]
    InvalidKeystroke(String),
    #[error("{0}")]
    InvalidArgument(String),
    #[error("{0}")]
    Timeout(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Internal(String),
}

impl SessionError {
    /// The protocol error code this failure reports as.
    pub fn code(&self) -> ErrorCode {
        match self {
            SessionError::EnvironmentMissing { .. } => ErrorCode::EnvironmentMissing,
            SessionError::IsolationBreach { .. } => ErrorCode::IsolationBreach,
            SessionError::LaunchFailed { .. } => ErrorCode::LaunchFailed,
            SessionError::UnsupportedCharacter(_) => ErrorCode::UnsupportedCharacter,
            SessionError::InvalidKeystroke(_) => ErrorCode::InvalidKeystroke,
            SessionError::InvalidArgument(_) => ErrorCode::InvalidArgument,
            SessionError::Timeout(_) => ErrorCode::Timeout,
            SessionError::Unsupported(_) => ErrorCode::Unsupported,
            SessionError::Internal(_) => ErrorCode::Internal,
        }
    }
}

impl From<SessionError> for PilotError {
    fn from(error: SessionError) -> PilotError {
        PilotError::new(error.code(), error.to_string())
    }
}

impl From<std::io::Error> for SessionError {
    fn from(error: std::io::Error) -> SessionError {
        SessionError::Internal(error.to_string())
    }
}

impl From<zbus::Error> for SessionError {
    fn from(error: zbus::Error) -> SessionError {
        SessionError::Internal(error.to_string())
    }
}

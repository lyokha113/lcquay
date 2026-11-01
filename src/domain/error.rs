use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Error)]
pub enum RegistryError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("not found")]
    NotFound,
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("rate limited")]
    RateLimited,
    #[error("untrusted realm: {0}")]
    UntrustedRealm(String),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("repeated pagination cursor: {0}")]
    RepeatedCursor(String),
    #[error("response too large")]
    TooLarge,
    #[error("network error: {0}")]
    Network(String),
    #[error("AWS credentials: {0}")]
    AwsCredentials(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl RegistryError {
    /// A short name of the error class, without its text (safe for logs and summaries).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unauthorized | Self::AwsCredentials(_) => "credentials",
            Self::Forbidden(_) => "permission denied",
            Self::NotFound => "not found",
            Self::Unsupported(_) => "unsupported",
            Self::RateLimited => "rate limited",
            Self::UntrustedRealm(_) => "untrusted realm",
            Self::Timeout(_) => "timeout",
            Self::RepeatedCursor(_) => "repeated cursor",
            Self::TooLarge => "too large",
            Self::Network(_) => "network",
            Self::Provider(_) => "registry error",
            Self::Internal(_) => "internal",
        }
    }
}

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::domain::RegistryError;

pub mod basic;
pub mod docker_config;
pub mod ecr;
pub mod helper;

/// Holds a secret value; `Debug`/`Display` never reveal it.
#[derive(Clone)]
pub struct Secret<T>(pub T);

impl<T> Secret<T> {
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[derive(Clone, Debug)]
pub enum Credential {
    Anonymous,
    Basic {
        username: String,
        password: Secret<String>,
    },
    /// Registry HTTP only; never given to `docker login` or the Engine.
    Bearer(Secret<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destination {
    Http,
    Login,
    Engine,
}

#[derive(Clone, Debug)]
pub struct Issued {
    pub credential: Credential,
    /// Known for ECR tokens; `None` otherwise.
    pub expires_at: Option<OffsetDateTime>,
}

#[async_trait]
pub trait CredentialSource: Send + Sync {
    /// `Login`/`Engine` never return `Bearer`.
    async fn credential(&self, dest: Destination) -> Result<Issued, RegistryError>;
    /// The credential can be given to `docker login`.
    fn can_login(&self) -> bool;
}

pub struct AnonymousSource;

#[async_trait]
impl CredentialSource for AnonymousSource {
    async fn credential(&self, _dest: Destination) -> Result<Issued, RegistryError> {
        Ok(Issued {
            credential: Credential::Anonymous,
            expires_at: None,
        })
    }
    fn can_login(&self) -> bool {
        false
    }
}

const CREDENTIAL_TIMEOUT: Duration = Duration::from_secs(30);

/// `CredentialSource::credential` bounded to 30 s (covers the AWS credential chain).
pub async fn with_credential_timeout(
    src: &dyn CredentialSource,
    dest: Destination,
) -> Result<Issued, RegistryError> {
    tokio::time::timeout(CREDENTIAL_TIMEOUT, src.credential(dest))
        .await
        .unwrap_or_else(|_| Err(RegistryError::Timeout("credentials".into())))
}

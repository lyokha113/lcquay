use async_trait::async_trait;

use super::{Credential, CredentialSource, Destination, Issued, Secret};
use crate::domain::RegistryError;

/// Prefix of the error text when the password variable is unset or empty.
pub const BASIC_ENV_UNSET: &str = "environment variable ";

/// Fixed username; the password is read from an environment variable on every call (no cache).
pub struct BasicSource {
    username: String,
    password_env: String,
    env: fn(&str) -> Option<String>,
}

fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

impl BasicSource {
    pub fn new(username: String, password_env: String) -> Self {
        Self::with_env(username, password_env, process_env)
    }

    /// Tests inject the lookup; `std::env::set_var` is unsound with parallel test threads.
    pub(crate) fn with_env(
        username: String,
        password_env: String,
        env: fn(&str) -> Option<String>,
    ) -> Self {
        Self {
            username,
            password_env,
            env,
        }
    }
}

#[async_trait]
impl CredentialSource for BasicSource {
    async fn credential(&self, _dest: Destination) -> Result<Issued, RegistryError> {
        let password = (self.env)(&self.password_env)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                RegistryError::Provider(format!(
                    "{BASIC_ENV_UNSET}{} is not set or empty",
                    self.password_env
                ))
            })?;
        Ok(Issued {
            credential: Credential::Basic {
                username: self.username.clone(),
                password: Secret(password),
            },
            expires_at: None,
        })
    }
    fn can_login(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(env: fn(&str) -> Option<String>) -> BasicSource {
        BasicSource::with_env("ci".into(), "PW".into(), env)
    }
    fn set(n: &str) -> Option<String> {
        (n == "PW").then(|| "pw".into())
    }

    #[tokio::test]
    async fn basic_returns_same_credential_for_every_destination() {
        let s = src(set);
        for d in [Destination::Http, Destination::Login, Destination::Engine] {
            let i = s.credential(d).await.unwrap();
            assert!(i.expires_at.is_none());
            let Credential::Basic { username, password } = i.credential else {
                panic!("expected Basic");
            };
            assert_eq!(
                (username.as_str(), password.expose().as_str()),
                ("ci", "pw")
            );
        }
    }

    #[tokio::test]
    async fn basic_missing_env_names_var() {
        let e = src(|_| None)
            .credential(Destination::Http)
            .await
            .unwrap_err();
        let RegistryError::Provider(m) = e else {
            panic!("expected Provider, got {e:?}");
        };
        assert!(m.starts_with(BASIC_ENV_UNSET) && m.contains("PW"), "{m}");
    }

    #[tokio::test]
    async fn basic_empty_env_names_var() {
        let e = src(|_| Some(String::new()))
            .credential(Destination::Http)
            .await
            .unwrap_err();
        let RegistryError::Provider(m) = e else {
            panic!("expected Provider, got {e:?}");
        };
        assert!(m.starts_with(BASIC_ENV_UNSET) && m.contains("PW"), "{m}");
    }

    #[test]
    fn basic_can_login() {
        assert!(src(set).can_login());
    }

    #[tokio::test]
    async fn basic_debug_hides_password() {
        let i = src(set).credential(Destination::Http).await.unwrap();
        assert!(!format!("{i:?}").contains("pw"));
    }
}

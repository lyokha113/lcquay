use std::path::PathBuf;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::Value;

use super::helper::{IDENTITY_TOKEN_MSG, NOT_FOUND, run_helper};
use super::{Credential, CredentialSource, Destination, Issued, Secret};
use crate::domain::RegistryError;

const HELPER_BUDGET: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a successful lookup is reused (an outside `docker login` takes effect after this).
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// `$DOCKER_CONFIG` or `~/.docker`.
pub fn default_config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("DOCKER_CONFIG").filter(|d| !d.is_empty()) {
        return d.into();
    }
    directories::BaseDirs::new()
        .map(|b| b.home_dir().join(".docker"))
        .unwrap_or_else(|| PathBuf::from(".docker"))
}

/// Strips scheme and trailing `/`; Docker Hub aliases (and `/v1`) collapse to `docker.io`.
fn normalize(host: &str) -> String {
    let h = host
        .strip_prefix("https://")
        .or_else(|| host.strip_prefix("http://"))
        .unwrap_or(host)
        .trim_end_matches('/');
    let first = h.split('/').next().unwrap_or(h);
    if matches!(
        first,
        "docker.io" | "index.docker.io" | "registry-1.docker.io"
    ) {
        return "docker.io".into();
    }
    h.to_string()
}

fn lookup<'a>(map: Option<&'a Value>, host: &str) -> Option<&'a Value> {
    map?.as_object()?
        .iter()
        .find(|(k, _)| normalize(k) == host)
        .map(|(_, v)| v)
}

pub struct DockerConfigSource {
    config_dir: PathBuf,
    host: String,
    /// Held across the fetch so concurrent callers share one config read / helper run.
    cache: tokio::sync::Mutex<Option<(Issued, Instant)>>,
}

impl DockerConfigSource {
    pub fn new(config_dir: PathBuf, host: String) -> Self {
        Self {
            config_dir,
            host,
            cache: Default::default(),
        }
    }

    async fn read_config(&self) -> Result<Value, RegistryError> {
        let path = self.config_dir.join("config.json");
        let bytes = match tokio::time::timeout(READ_TIMEOUT, tokio::fs::read(&path)).await {
            Err(_) => return Err(RegistryError::Timeout("reading docker config".into())),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Value::Null),
            Ok(Err(e)) => {
                return Err(RegistryError::Provider(format!(
                    "reading docker config: {e}"
                )));
            }
            Ok(Ok(b)) => b,
        };
        serde_json::from_slice(&bytes)
            .map_err(|e| RegistryError::Provider(format!("invalid docker config: {e}")))
    }

    async fn helper_credential(&self, name: &str) -> Result<Credential, RegistryError> {
        let c = run_helper(
            &format!("docker-credential-{name}"),
            &self.host,
            HELPER_BUDGET,
        )
        .await?;
        Ok(Credential::Basic {
            username: c.username,
            password: c.secret,
        })
    }
}

#[async_trait]
impl CredentialSource for DockerConfigSource {
    async fn credential(&self, _dest: Destination) -> Result<Issued, RegistryError> {
        let mut cache = self.cache.lock().await;
        if let Some((i, _)) = cache.as_ref().filter(|(_, at)| at.elapsed() < CACHE_TTL) {
            return Ok(i.clone());
        }
        let issued = self.fetch().await?;
        *cache = Some((issued.clone(), Instant::now()));
        Ok(issued)
    }

    fn can_login(&self) -> bool {
        false
    }
}

impl DockerConfigSource {
    async fn fetch(&self) -> Result<Issued, RegistryError> {
        let cfg = self.read_config().await?;
        let host = normalize(&self.host);
        let issued = |credential| Issued {
            credential,
            expires_at: None,
        };

        let helper = lookup(cfg.get("credHelpers"), &host)
            .or_else(|| cfg.get("credsStore"))
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty());
        if let Some(name) = helper {
            match self.helper_credential(name).await {
                Ok(c) => return Ok(issued(c)),
                // Helpers report an unknown host as an error; that means "no entry", not failure.
                Err(RegistryError::Provider(m)) if m.contains(NOT_FOUND) => {}
                Err(e) => return Err(e),
            }
        }

        let Some(entry) = lookup(cfg.get("auths"), &host) else {
            return Ok(issued(Credential::Anonymous));
        };
        let field = |k: &str| {
            entry
                .get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        if field("identitytoken").is_some() {
            return Err(RegistryError::Unsupported(IDENTITY_TOKEN_MSG.into()));
        }
        let Some(auth) = field("auth") else {
            return Ok(issued(Credential::Anonymous));
        };
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(auth)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or_else(|| RegistryError::Provider("invalid auth entry in docker config".into()))?;
        let (username, password) = decoded
            .split_once(':')
            .ok_or_else(|| RegistryError::Provider("invalid auth entry in docker config".into()))?;
        Ok(issued(Credential::Basic {
            username: username.to_string(),
            password: Secret(password.to_string()),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(json: &str, host: &str) -> (tempfile::TempDir, DockerConfigSource) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.json"), json).unwrap();
        let s = DockerConfigSource::new(d.path().to_path_buf(), host.to_string());
        (d, s)
    }
    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    #[tokio::test]
    async fn auths_entry_basic() {
        let j = format!(
            r#"{{"auths":{{"registry.example.com":{{"auth":"{}"}}}}}}"#,
            b64("u:p:q")
        );
        let (_d, s) = src(&j, "registry.example.com");
        match s.credential(Destination::Http).await.unwrap().credential {
            Credential::Basic { username, password } => {
                assert_eq!(username, "u");
                assert_eq!(password.expose(), "p:q");
            }
            o => panic!("{o:?}"),
        }
    }

    #[tokio::test]
    async fn identity_token_rejected() {
        let j = r#"{"auths":{"registry.example.com":{"identitytoken":"x"}}}"#;
        let (_d, s) = src(j, "registry.example.com");
        let e = s.credential(Destination::Http).await.unwrap_err();
        assert!(
            matches!(e, RegistryError::Unsupported(m) if m.contains("identity tokens are not supported"))
        );
    }

    #[tokio::test]
    async fn no_entry_is_anonymous() {
        let (_d, s) = src(
            r#"{"auths":{"other":{"auth":"eDp5"}}}"#,
            "registry.example.com",
        );
        assert!(matches!(
            s.credential(Destination::Http).await.unwrap().credential,
            Credential::Anonymous
        ));
        // missing config.json is anonymous too
        let d = tempfile::tempdir().unwrap();
        let s = DockerConfigSource::new(d.path().to_path_buf(), "h".into());
        assert!(matches!(
            s.credential(Destination::Http).await.unwrap().credential,
            Credential::Anonymous
        ));
    }

    #[tokio::test]
    async fn docker_hub_aliases_match() {
        let j = format!(
            r#"{{"auths":{{"https://index.docker.io/v1/":{{"auth":"{}"}}}}}}"#,
            b64("u:p")
        );
        for host in ["registry-1.docker.io", "docker.io", "index.docker.io"] {
            let (_d, s) = src(&j, host);
            assert!(
                matches!(
                    s.credential(Destination::Http).await.unwrap().credential,
                    Credential::Basic { .. }
                ),
                "{host}"
            );
        }
    }

    #[tokio::test]
    async fn host_match_strips_scheme_and_slash() {
        let j = format!(
            r#"{{"auths":{{"https://reg.example.com/":{{"auth":"{}"}}}}}}"#,
            b64("u:p")
        );
        let (_d, s) = src(&j, "reg.example.com");
        assert!(matches!(
            s.credential(Destination::Http).await.unwrap().credential,
            Credential::Basic { .. }
        ));
    }

    #[test]
    fn secret_debug_redacted() {
        assert_eq!(format!("{:?}", Secret("pw".to_string())), "***");
        assert_eq!(format!("{}", Secret("pw".to_string())), "***");
    }
}

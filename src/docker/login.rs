//! `docker login --password-stdin` through the docker CLI.

use std::time::Duration;

use crate::auth::Secret;
use crate::domain::RegistryError;
use crate::process::{self, RunError};

/// Prefix of every `docker login` failure message.
pub const LOGIN_FAILED_PREFIX: &str = "docker login failed";

const LOGIN_BUDGET: Duration = Duration::from_secs(30);

pub async fn docker_login(
    program: &str,
    host: &str,
    username: &str,
    password: &Secret<String>,
) -> Result<(), RegistryError> {
    docker_login_with_budget(program, host, username, password, LOGIN_BUDGET).await
}

#[doc(hidden)]
pub async fn docker_login_with_budget(
    program: &str,
    host: &str,
    username: &str,
    password: &Secret<String>,
    budget: Duration,
) -> Result<(), RegistryError> {
    let failed = |m: &str| RegistryError::Provider(format!("{LOGIN_FAILED_PREFIX}: {m}"));
    let pw = password.expose();
    // stdin cannot carry a line break, and one-line passwords make the redaction below exact.
    if pw.contains(['\n', '\r']) {
        return Err(failed("password contains a line break"));
    }
    tracing::info!(host, "docker login");
    let args = ["login", "--username", username, "--password-stdin", host];
    let out = process::run_piped(program, &args, pw.as_bytes(), budget)
        .await
        .map_err(|e| match e {
            RunError::Spawn(e) if e.kind() == std::io::ErrorKind::NotFound => {
                failed("docker CLI not found")
            }
            RunError::Spawn(e) | RunError::Io(e) => failed(&e.kind().to_string()),
            RunError::Timeout => RegistryError::Timeout("docker login".into()),
        })?;
    if out.status.success() {
        return Ok(());
    }
    // Redact the password from all output before picking a line, so no echo can leak it.
    let text = |b: &[u8]| {
        let s = String::from_utf8_lossy(b);
        if pw.is_empty() {
            s.into_owned()
        } else {
            s.replace(pw.as_str(), "***")
        }
    };
    let (err, o) = (text(&out.stderr), text(&out.stdout));
    let line = [&err, &o]
        .into_iter()
        .find_map(|s| s.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("");
    Err(failed(&line.chars().take(200).collect::<String>()))
}

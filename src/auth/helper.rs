use std::time::Duration;

use serde::Deserialize;

use super::Secret;
use crate::domain::RegistryError;
use crate::process::{self, Piped, RunError};

pub const IDENTITY_TOKEN_MSG: &str =
    "identity tokens are not supported; log in with a password or personal access token";

/// Prefix of every credential-helper failure message (see `run_helper`).
pub const HELPER_FAILED_PREFIX: &str = "credential helper";

/// What credential helpers print for an unknown host.
pub const NOT_FOUND: &str = "credentials not found";

#[derive(Debug)]
pub struct HelperCreds {
    pub username: String,
    pub secret: Secret<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Output {
    username: String,
    secret: String,
}

/// Runs `<program> get` with `host` on stdin. One `budget` covers spawn, stdin,
/// draining stdout and stderr concurrently, and `wait`; on expiry the child is
/// killed and reaped.
pub async fn run_helper(
    program: &str,
    host: &str,
    budget: Duration,
) -> Result<HelperCreds, RegistryError> {
    let name = std::path::Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program);
    let name = name.strip_prefix("docker-credential-").unwrap_or(name);
    let failed =
        |m: &str| RegistryError::Provider(format!("{HELPER_FAILED_PREFIX} {name} failed: {m}"));

    let Piped {
        status,
        stdout: out,
        stderr: err,
    } = process::run_piped(program, &["get"], host.as_bytes(), budget)
        .await
        .map_err(|e| match e {
            RunError::Spawn(e) | RunError::Io(e) => failed(&e.to_string()),
            RunError::Timeout => RegistryError::Timeout(format!("{HELPER_FAILED_PREFIX} {name}")),
        })?;

    if !status.success() {
        // docker-credential-helpers print errors on stdout, so look there too. A "not found"
        // line from either stream wins (DockerConfigSource treats it as "no entry"); otherwise
        // the first stderr line, or the first stdout line when stderr is empty.
        let (err, out) = (String::from_utf8_lossy(&err), String::from_utf8_lossy(&out));
        let first = |s: &str| s.lines().next().unwrap_or("").trim().to_string();
        let not_found = err
            .lines()
            .chain(out.lines())
            .find(|l| l.contains(NOT_FOUND));
        let msg = match not_found {
            Some(l) => l.trim().to_string(),
            None if !err.trim().is_empty() => first(&err),
            None => first(&out),
        };
        return Err(failed(&msg));
    }
    let o: Output = serde_json::from_slice(&out).map_err(|_| {
        RegistryError::Provider(format!(
            "{HELPER_FAILED_PREFIX} {name} returned invalid output"
        ))
    })?;
    if o.username == "<token>" {
        return Err(RegistryError::Unsupported(IDENTITY_TOKEN_MSG.into()));
    }
    Ok(HelperCreds {
        username: o.username,
        secret: Secret(o.secret),
    })
}

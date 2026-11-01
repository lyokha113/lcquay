use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use lcquay::auth::docker_config::DockerConfigSource;
use lcquay::auth::helper::run_helper;
use lcquay::auth::{Credential, CredentialSource, Destination};
use lcquay::domain::RegistryError;
use tokio::sync::Mutex;

// Serializes tests that touch PATH.
static PATH_LOCK: Mutex<()> = Mutex::const_new(());

/// Prepends `dir` to PATH while alive; restores PATH on drop (also on panic).
struct PathGuard {
    old: std::ffi::OsString,
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

impl PathGuard {
    async fn prepend(dir: &Path) -> Self {
        let _lock = PATH_LOCK.lock().await;
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![dir.to_path_buf()];
        paths.extend(std::env::split_paths(&old));
        // SAFETY: PATH edits are serialized by PATH_LOCK and restored in Drop.
        unsafe { std::env::set_var("PATH", std::env::join_paths(paths).unwrap()) };
        Self { old, _lock }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        // SAFETY: still holding PATH_LOCK (field drops after this body).
        unsafe { std::env::set_var("PATH", &self.old) };
    }
}

fn script(dir: &Path, name: &str, body: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.to_str().unwrap().to_string()
}

const B: Duration = Duration::from_secs(10);

#[tokio::test]
async fn helper_ok() {
    let d = tempfile::tempdir().unwrap();
    let p = script(
        d.path(),
        "ok",
        r#"cat >/dev/null; echo '{"ServerURL":"h","Username":"u","Secret":"s"}'"#,
    );
    let c = run_helper(&p, "h", B).await.unwrap();
    assert_eq!(c.username, "u");
    assert_eq!(c.secret.expose(), "s");
}

#[tokio::test]
async fn helper_receives_host_on_stdin() {
    let d = tempfile::tempdir().unwrap();
    let p = script(
        d.path(),
        "echo-host",
        r#"h=$(cat); printf '{"ServerURL":"%s","Username":"%s","Secret":"s"}' "$h" "$h""#,
    );
    let c = run_helper(&p, "reg.example.com", B).await.unwrap();
    assert_eq!(c.username, "reg.example.com");
}

#[tokio::test]
async fn helper_token_username_rejected() {
    let d = tempfile::tempdir().unwrap();
    let p = script(
        d.path(),
        "tok",
        r#"echo '{"ServerURL":"h","Username":"<token>","Secret":"s"}'"#,
    );
    let e = run_helper(&p, "h", B).await.unwrap_err();
    assert!(
        matches!(e, RegistryError::Unsupported(m) if m.contains("identity tokens are not supported"))
    );
}

#[tokio::test]
async fn helper_hang_is_killed_and_reaped() {
    let d = tempfile::tempdir().unwrap();
    let pidfile = d.path().join("pid");
    let p = script(
        d.path(),
        "hang",
        &format!(
            "[ \"$1\" = warm ] && exit 0\necho $$ > {}\nexec sleep 1000",
            pidfile.display()
        ),
    );
    // First exec of a fresh script can be slow (macOS scanning); warm it so the 1 s budget is fair.
    std::process::Command::new(&p).arg("warm").status().unwrap();
    let t = Instant::now();
    let e = run_helper(&p, "h", Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(matches!(e, RegistryError::Timeout(m) if m == "credential helper hang"));
    assert!(t.elapsed() < Duration::from_secs(3));
    let pid = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .to_string();
    let alive = std::process::Command::new("kill")
        .args(["-0", &pid])
        .status()
        .unwrap()
        .success();
    assert!(!alive, "helper process {pid} still exists");
}

#[tokio::test]
async fn helper_flooding_stderr_does_not_block() {
    let d = tempfile::tempdir().unwrap();
    let p = script(
        d.path(),
        "flood",
        r#"head -c 5242880 /dev/zero | tr '\0' 'x' >&2; echo '{"ServerURL":"h","Username":"u","Secret":"s"}'"#,
    );
    let c = run_helper(&p, "h", Duration::from_secs(5)).await.unwrap();
    assert_eq!(c.username, "u");
}

#[tokio::test]
async fn helper_failure_message() {
    let d = tempfile::tempdir().unwrap();
    let p = script(
        d.path(),
        "fail",
        "cat >/dev/null; printf 'boom\\nmore\\n' >&2; exit 1",
    );
    let e = run_helper(&p, "h", B).await.unwrap_err();
    let RegistryError::Provider(m) = e else {
        panic!("{e:?}")
    };
    assert!(m.contains("boom") && !m.contains("more"), "{m}");
    assert!(m.contains("credential helper fail failed"), "{m}");
}

const HELPER_OK: &str = r#"cat >/dev/null; echo '{"ServerURL":"h","Username":"hu","Secret":"hs"}'"#;
const AUTH_AU: &str = "YXU6YXA="; // base64("au:ap")

fn config(dir: &Path, json: &str) {
    std::fs::write(dir.join("config.json"), json).unwrap();
}

async fn cred(dir: &Path, host: &str) -> Result<Credential, RegistryError> {
    let src = DockerConfigSource::new(dir.to_path_buf(), host.to_string());
    src.credential(Destination::Http)
        .await
        .map(|i| i.credential)
}

fn basic(c: Credential) -> (String, String) {
    match c {
        Credential::Basic { username, password } => (username, password.expose().clone()),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn helper_not_found_on_stdout_is_not_found_message() {
    let d = tempfile::tempdir().unwrap();
    let p = script(
        d.path(),
        "nf",
        "cat >/dev/null; echo 'credentials not found in native keychain'; exit 1",
    );
    let e = run_helper(&p, "h", B).await.unwrap_err();
    let RegistryError::Provider(m) = e else {
        panic!("{e:?}")
    };
    assert!(m.contains("credentials not found"), "{m}");
}

#[tokio::test]
async fn helper_stdout_only_error_message() {
    let d = tempfile::tempdir().unwrap();
    let p = script(
        d.path(),
        "so",
        "cat >/dev/null; printf 'keychain locked\nmore\n'; exit 1",
    );
    let e = run_helper(&p, "h", B).await.unwrap_err();
    let RegistryError::Provider(m) = e else {
        panic!("{e:?}")
    };
    assert!(m.contains("keychain locked") && !m.contains("more"), "{m}");
}

#[tokio::test]
async fn creds_store_not_found_on_stdout_falls_through() {
    let d = tempfile::tempdir().unwrap();
    script(
        d.path(),
        "docker-credential-nf",
        "cat >/dev/null; echo 'credentials not found in native keychain'; exit 1",
    );
    let _g = PathGuard::prepend(d.path()).await;
    // no auths entry -> Anonymous
    config(d.path(), r#"{"credsStore":"nf"}"#);
    assert!(matches!(
        cred(d.path(), "reg.example.com").await,
        Ok(Credential::Anonymous)
    ));
    // auths entry -> Basic
    config(
        d.path(),
        &format!(r#"{{"credsStore":"nf","auths":{{"reg.example.com":{{"auth":"{AUTH_AU}"}}}}}}"#),
    );
    let c = cred(d.path(), "reg.example.com").await.unwrap();
    assert_eq!(basic(c), ("au".into(), "ap".into()));
}

#[tokio::test]
async fn creds_store_other_stdout_error_is_reported() {
    let d = tempfile::tempdir().unwrap();
    script(
        d.path(),
        "docker-credential-bad",
        "cat >/dev/null; echo 'keychain locked'; exit 1",
    );
    let _g = PathGuard::prepend(d.path()).await;
    config(d.path(), r#"{"credsStore":"bad"}"#);
    let e = cred(d.path(), "reg.example.com").await.unwrap_err();
    assert!(matches!(e, RegistryError::Provider(m) if m.contains("keychain locked")));
}

#[tokio::test]
async fn docker_config_uses_cred_helper() {
    let d = tempfile::tempdir().unwrap();
    script(d.path(), "docker-credential-fake", HELPER_OK);
    let _g = PathGuard::prepend(d.path()).await;
    config(d.path(), r#"{"credHelpers":{"reg.example.com":"fake"}}"#);
    let c = cred(d.path(), "reg.example.com").await.unwrap();
    assert_eq!(basic(c), ("hu".into(), "hs".into()));
}

#[tokio::test]
async fn creds_store_only() {
    let d = tempfile::tempdir().unwrap();
    script(d.path(), "docker-credential-store", HELPER_OK);
    let _g = PathGuard::prepend(d.path()).await;
    config(d.path(), r#"{"credsStore":"store"}"#);
    let c = cred(d.path(), "reg.example.com").await.unwrap();
    assert_eq!(basic(c), ("hu".into(), "hs".into()));
}

#[tokio::test]
async fn precedence_cred_helpers_then_creds_store_then_auths() {
    let d = tempfile::tempdir().unwrap();
    script(
        d.path(),
        "docker-credential-per",
        r#"cat >/dev/null; echo '{"ServerURL":"h","Username":"per","Secret":"s"}'"#,
    );
    script(
        d.path(),
        "docker-credential-store",
        r#"cat >/dev/null; echo '{"ServerURL":"h","Username":"store","Secret":"s"}'"#,
    );
    let _g = PathGuard::prepend(d.path()).await;
    let auths = format!(r#""auths":{{"reg.example.com":{{"auth":"{AUTH_AU}"}}}}"#);

    // credHelpers[host] beats credsStore and auths
    config(
        d.path(),
        &format!(r#"{{"credHelpers":{{"reg.example.com":"per"}},"credsStore":"store",{auths}}}"#),
    );
    assert_eq!(
        basic(cred(d.path(), "reg.example.com").await.unwrap()).0,
        "per"
    );

    // credsStore beats auths (credHelpers has no entry for this host)
    config(
        d.path(),
        &format!(r#"{{"credHelpers":{{"other.example.com":"per"}},"credsStore":"store",{auths}}}"#),
    );
    assert_eq!(
        basic(cred(d.path(), "reg.example.com").await.unwrap()).0,
        "store"
    );

    // auths alone
    config(d.path(), &format!("{{{auths}}}"));
    assert_eq!(
        basic(cred(d.path(), "reg.example.com").await.unwrap()).0,
        "au"
    );
}

#[tokio::test]
async fn helper_token_username_rejected_through_docker_config() {
    let d = tempfile::tempdir().unwrap();
    script(
        d.path(),
        "docker-credential-tok",
        r#"cat >/dev/null; echo '{"ServerURL":"h","Username":"<token>","Secret":"s"}'"#,
    );
    let _g = PathGuard::prepend(d.path()).await;
    config(d.path(), r#"{"credsStore":"tok"}"#);
    let e = cred(d.path(), "reg.example.com").await.unwrap_err();
    assert!(
        matches!(e, RegistryError::Unsupported(m) if m.contains("identity tokens are not supported"))
    );
}

#[tokio::test]
async fn credential_is_cached_and_single_flight() {
    let d = tempfile::tempdir().unwrap();
    let count = d.path().join("count");
    script(
        d.path(),
        "docker-credential-cnt",
        &format!(
            r#"echo x >> {}; sleep 0.2; cat >/dev/null; echo '{{"ServerURL":"h","Username":"hu","Secret":"hs"}}'"#,
            count.display()
        ),
    );
    let _g = PathGuard::prepend(d.path()).await;
    config(d.path(), r#"{"credsStore":"cnt"}"#);
    let src = std::sync::Arc::new(DockerConfigSource::new(
        d.path().to_path_buf(),
        "reg.example.com".into(),
    ));
    let hs: Vec<_> = (0..8)
        .map(|_| {
            let s = src.clone();
            tokio::spawn(async move { s.credential(Destination::Http).await.unwrap() })
        })
        .collect();
    for h in hs {
        h.await.unwrap();
    }
    for _ in 0..3 {
        src.credential(Destination::Http).await.unwrap();
    }
    assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 1);
}

#[tokio::test]
async fn failed_credential_is_not_cached() {
    let d = tempfile::tempdir().unwrap();
    let count = d.path().join("count");
    script(
        d.path(),
        "docker-credential-bad",
        &format!(
            "echo x >> {}; cat >/dev/null; echo boom >&2; exit 1",
            count.display()
        ),
    );
    let _g = PathGuard::prepend(d.path()).await;
    config(d.path(), r#"{"credsStore":"bad"}"#);
    let src = DockerConfigSource::new(d.path().to_path_buf(), "reg.example.com".into());
    assert!(src.credential(Destination::Http).await.is_err());
    assert!(src.credential(Destination::Http).await.is_err());
    assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 2);
}

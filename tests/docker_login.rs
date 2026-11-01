use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use lcquay::auth::Secret;
use lcquay::docker::login::{docker_login, docker_login_with_budget};
use lcquay::domain::RegistryError;

fn script(dir: &Path, name: &str, body: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.to_str().unwrap().to_string()
}

fn pw(s: &str) -> Secret<String> {
    Secret(s.to_string())
}

async fn fail_msg(body: &str, password: &str) -> String {
    let d = tempfile::tempdir().unwrap();
    let p = script(d.path(), "docker", body);
    match docker_login(&p, "h", "u", &pw(password)).await {
        Err(RegistryError::Provider(m)) => m,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn login_password_only_on_stdin() {
    let d = tempfile::tempdir().unwrap();
    let p = script(
        d.path(),
        "docker",
        &format!(
            "echo \"$@\" | tr -d '\\n' > {0}/args.txt\ncat > {0}/stdin.txt",
            d.path().display()
        ),
    );
    docker_login(&p, "public.ecr.aws", "AWS", &pw("s3cr3t"))
        .await
        .unwrap();
    let args = std::fs::read_to_string(d.path().join("args.txt")).unwrap();
    assert_eq!(args, "login --username AWS --password-stdin public.ecr.aws");
    assert!(!args.contains("s3cr3t"));
    assert_eq!(
        std::fs::read_to_string(d.path().join("stdin.txt")).unwrap(),
        "s3cr3t"
    );
}

#[tokio::test]
async fn login_failure_reports_first_stderr_line() {
    let m = fail_msg(
        "cat >/dev/null; printf 'Error response from daemon: unauthorized\\nmore\\n' >&2; exit 1",
        "s3cr3t",
    )
    .await;
    assert_eq!(
        m,
        "docker login failed: Error response from daemon: unauthorized"
    );
}

#[tokio::test]
async fn login_failure_falls_back_to_stdout_and_truncates() {
    let m = fail_msg("cat >/dev/null; echo '  denied  '; exit 1", "s3cr3t").await;
    assert_eq!(m, "docker login failed: denied");
    let m = fail_msg(
        "cat >/dev/null; head -c 500 /dev/zero | tr '\\0' x >&2; exit 1",
        "s3cr3t",
    )
    .await;
    assert_eq!(m, format!("docker login failed: {}", "x".repeat(200)));
}

#[tokio::test]
async fn login_error_never_contains_password() {
    // password echoed back on the first line
    let m = fail_msg("cat >&2; exit 1", "s3cr3t").await;
    assert!(!m.contains("s3cr3t") && m.contains("***"), "{m}");
    // CRLF-terminated prefix line, password on the next line
    let m = fail_msg("printf 'prefix\\r\\n' >&2; cat >&2; exit 1", "s3cr3t").await;
    assert!(!m.contains("s3cr3t"), "{m}");
    assert_eq!(m, "docker login failed: prefix");
    // password twice on one line
    let m = fail_msg("p=$(cat); echo \"$p and $p\" >&2; exit 1", "s3cr3t").await;
    assert!(!m.contains("s3cr3t") && m.contains("*** and ***"), "{m}");
    // password only on stdout
    let m = fail_msg("cat; exit 1", "s3cr3t").await;
    assert!(!m.contains("s3cr3t") && m.contains("***"), "{m}");
}

#[tokio::test]
async fn login_rejects_password_with_line_break() {
    let d = tempfile::tempdir().unwrap();
    let marker = d.path().join("ran");
    let p = script(d.path(), "docker", &format!("touch {}", marker.display()));
    for bad in ["a\nb", "a\rb"] {
        let e = docker_login(&p, "h", "u", &pw(bad)).await.unwrap_err();
        assert!(
            matches!(&e, RegistryError::Provider(m) if m == "docker login failed: password contains a line break"),
            "{e:?}"
        );
    }
    assert!(!marker.exists());
}

#[tokio::test]
async fn login_missing_program() {
    let e = docker_login("/nonexistent/docker", "h", "u", &pw("x"))
        .await
        .unwrap_err();
    assert!(
        matches!(e, RegistryError::Provider(m) if m == "docker login failed: docker CLI not found")
    );
}

#[tokio::test]
async fn login_hang_times_out_and_reaps() {
    let d = tempfile::tempdir().unwrap();
    let pidfile = d.path().join("pid");
    let p = script(
        d.path(),
        "docker",
        &format!(
            "[ \"$1\" = warm ] && exit 0\necho $$ > {}\nexec sleep 1000",
            pidfile.display()
        ),
    );
    // First exec of a fresh script can be slow (macOS scanning); warm it so the 1 s budget is fair.
    std::process::Command::new(&p).arg("warm").status().unwrap();
    let t = Instant::now();
    let e = docker_login_with_budget(&p, "h", "u", &pw("x"), Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(matches!(e, RegistryError::Timeout(m) if m == "docker login"));
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
    assert!(!alive, "docker process {pid} still exists");
}

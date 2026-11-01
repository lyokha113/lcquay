//! The registry form's connection test (`Ctrl-T`). Step 0 (`config valid`) runs in `app`; this
//! runs steps 1..=4 on a throwaway `Registry` built from the form, stopping at the first failure.
//! Details never hold a secret: env vars are named, never read out, and errors are
//! `RegistryError` texts (query-free by construction).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use reqwest::Method;

use crate::auth::ecr::sdk_call;
use crate::auth::{Credential, Destination, with_credential_timeout};
use crate::config::{AuthCfg, ProviderCfg, RegistryCfg};
use crate::domain::RegistryError;
use crate::providers::oci::LISTING_UNAVAILABLE;
use crate::providers::{Registry, build_registry, ecr};
use crate::registry::{build_http_client, http_error};
use crate::reqlog::{RegLog, RequestLog};

pub const STEPS: [&str; 5] = [
    "config valid",
    "credentials",
    "endpoint reachable",
    "registry login",
    "list repositories",
];

const STEP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq)]
pub enum CheckStatus {
    Ok,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub status: CheckStatus,
    pub detail: String,
    pub ms: u64,
}

impl Check {
    pub fn skipped() -> Check {
        Check {
            status: CheckStatus::Skipped,
            detail: String::new(),
            ms: 0,
        }
    }
}

pub struct Deps {
    pub registry: Registry,
    /// For `ecr` / `ecr-public`: the caller identity of the form's profile.
    pub sts: Option<aws_sdk_sts::Client>,
    pub docker_config_dir: PathBuf,
    /// Where the plain `GET /v2/` of step 2 is recorded.
    pub log: RegLog,
}

/// The throwaway registry (and STS client) for `cfg`; no network calls.
pub async fn deps(
    cfg: &RegistryCfg,
    log: &RequestLog,
    docker_config_dir: &Path,
) -> Result<Deps, RegistryError> {
    let sts = match &cfg.provider {
        ProviderCfg::Ecr {
            region, profile, ..
        } => Some(ecr::sts_client(region, profile.as_deref()).await?),
        ProviderCfg::EcrPublic { profile, .. } => {
            Some(ecr::sts_client("us-east-1", profile.as_deref()).await?)
        }
        ProviderCfg::Oci => None,
    };
    Ok(Deps {
        registry: build_registry(cfg, log, docker_config_dir).await?,
        sts,
        docker_config_dir: docker_config_dir.to_path_buf(),
        log: log.scoped(&cfg.id.0),
    })
}

/// `deps`, then `run`; a `deps` failure is a failed `credentials` step.
pub async fn test(
    cfg: &RegistryCfg,
    log: &RequestLog,
    docker_config_dir: &Path,
    report: &(dyn Fn(usize, Check) + Sync),
) {
    match deps(cfg, log, docker_config_dir).await {
        Ok(d) => run(cfg, &d, report).await,
        Err(e) => {
            report(
                1,
                Check {
                    status: CheckStatus::Failed,
                    detail: e.to_string(),
                    ms: 0,
                },
            );
            (2..STEPS.len()).for_each(|i| report(i, Check::skipped()));
        }
    }
}

/// Steps 1..=4, each bounded to 30 s; after the first failure the rest are `Skipped`.
pub async fn run(cfg: &RegistryCfg, deps: &Deps, report: &(dyn Fn(usize, Check) + Sync)) {
    let mut failed = false;
    for i in 1..STEPS.len() {
        if failed {
            report(i, Check::skipped());
            continue;
        }
        let t = Instant::now();
        let r = tokio::time::timeout(STEP_TIMEOUT, step(i, cfg, deps))
            .await
            .unwrap_or_else(|_| Err("timed out".into()));
        failed = r.is_err();
        let (status, detail) = match r {
            Ok(d) => (CheckStatus::Ok, d),
            Err(d) => (CheckStatus::Failed, d),
        };
        report(
            i,
            Check {
                status,
                detail,
                ms: t.elapsed().as_millis() as u64,
            },
        );
    }
}

async fn step(i: usize, cfg: &RegistryCfg, d: &Deps) -> Result<String, String> {
    let reg = &d.registry;
    match i {
        1 => credentials(cfg, d).await,
        2 => {
            let url = cfg
                .endpoint
                .join("/v2/")
                .map_err(|_| "invalid endpoint".to_string())?;
            let t = Instant::now();
            let r = build_http_client()
                .get(url.clone())
                .send()
                .await
                .map_err(http_error);
            let status = r.as_ref().map(|r| r.status().as_u16());
            d.log
                .http(&Method::GET, &url, &cfg.endpoint, status, t, None);
            match status.map_err(|e| e.to_string())? {
                s @ (200 | 401) => Ok(format!("HTTP {s}")),
                s => Err(format!("HTTP {s}")),
            }
        }
        3 => {
            let r = reg
                .oci
                .get_raw("/v2/", None, &[])
                .await
                .map_err(|e| e.to_string())?;
            let mut detail = format!("HTTP {}", r.status.as_u16());
            // Cached by the source after the login above; known for ECR tokens.
            if let Ok(i) = with_credential_timeout(&*reg.creds, Destination::Http).await
                && let Some(at) = i.expires_at.map(|at| at.to_offset(time::UtcOffset::UTC))
            {
                detail += &format!(" · token expires {:02}:{:02} UTC", at.hour(), at.minute());
            }
            Ok(detail)
        }
        _ => match reg.provider.repositories(&|_| {}).await {
            Ok(v) => Ok(format!("{} repos", v.len())),
            Err(RegistryError::Unsupported(m)) if m.starts_with(LISTING_UNAVAILABLE) => {
                Err("no catalog — add Repos".into())
            }
            Err(e) => Err(e.to_string()),
        },
    }
}

async fn credentials(cfg: &RegistryCfg, d: &Deps) -> Result<String, String> {
    if let Some(sts) = &d.sts {
        let id = sdk_call(
            &d.log,
            "sts:GetCallerIdentity",
            sts.get_caller_identity().send(),
        )
        .await
        .map_err(|e| e.to_string())?;
        return Ok(id.arn().unwrap_or_default().to_string());
    }
    let cred = with_credential_timeout(&*d.registry.creds, Destination::Http).await;
    match &cfg.auth {
        // Only whether the variable is usable; its value never leaves the source.
        AuthCfg::Basic { password_env, .. } => match cred {
            Ok(_) => Ok(format!("${password_env} set")),
            Err(_) => Err(format!("${password_env} not set")),
        },
        AuthCfg::DockerConfig => match cred.map_err(|e| e.to_string())?.credential {
            Credential::Anonymous => Ok("none — anonymous".into()),
            _ => Ok("docker config entry found".into()),
        },
        AuthCfg::Anonymous => Ok("none needed".into()),
        AuthCfg::Ecr => Err("no STS client".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_edit::{AuthKind, Kind, RegistryForm, validate};
    use crate::reqlog::RequestLog;
    use CheckStatus::*;
    use aws_sdk_sts::error::ErrorMetadata;
    use aws_sdk_sts::operation::get_caller_identity::{
        GetCallerIdentityError, GetCallerIdentityOutput,
    };
    use aws_smithy_mocks::{mock, mock_client};
    use std::sync::Mutex;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn form(endpoint: &str, auth: AuthKind, env: &str) -> RegistryForm {
        RegistryForm {
            id: "t".into(),
            kind: Kind::Oci,
            endpoint: endpoint.into(),
            auth,
            username: if env.is_empty() { "" } else { "u" }.into(),
            password_env: env.into(),
            ..RegistryForm::new_default()
        }
    }

    fn cfg(endpoint: &str, auth: AuthKind, env: &str) -> RegistryCfg {
        validate(&form(endpoint, auth, env), &[]).unwrap()
    }

    /// A registry answering `/v2/` with 200 and `_catalog` with `catalog`.
    async fn server(catalog: ResponseTemplate) -> MockServer {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/_catalog"))
            .respond_with(catalog)
            .mount(&s)
            .await;
        s
    }

    fn two_repos() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({"repositories": ["a", "b"]}))
    }

    /// Runs steps 1..=4 with deps built for `cfg` (docker config in a tempdir).
    async fn checks(cfg: &RegistryCfg) -> Vec<(CheckStatus, String)> {
        let dir = tempfile::tempdir().unwrap();
        let d = deps(cfg, &RequestLog::default(), dir.path()).await.unwrap();
        run_with(cfg, &d).await
    }

    async fn run_with(cfg: &RegistryCfg, d: &Deps) -> Vec<(CheckStatus, String)> {
        let got = Mutex::new(vec![None; STEPS.len()]);
        run(cfg, d, &|i, c| got.lock().unwrap()[i] = Some(c)).await;
        let got = got.into_inner().unwrap();
        assert!(got[0].is_none(), "step 0 is the app's");
        got[1..]
            .iter()
            .map(|c| {
                let c = c.clone().expect("every step reported");
                (c.status, c.detail)
            })
            .collect()
    }

    fn skipped() -> (CheckStatus, String) {
        (Skipped, String::new())
    }

    #[tokio::test]
    async fn all_steps_ok_anonymous() {
        let s = server(two_repos()).await;
        let got = checks(&cfg(&s.uri(), AuthKind::Anonymous, "")).await;
        assert_eq!(
            got,
            [
                (Ok, "none needed".into()),
                (Ok, "HTTP 200".into()),
                (Ok, "HTTP 200".into()),
                (Ok, "2 repos".into()),
            ]
        );
    }

    #[tokio::test]
    async fn basic_env_unset_stops() {
        let s = server(two_repos()).await;
        let unset = "LCQUAY_TEST_SURELY_UNSET_7F3A";
        let got = checks(&cfg(&s.uri(), AuthKind::Basic, unset)).await;
        assert_eq!(
            got,
            [
                (Failed, format!("${unset} not set")),
                skipped(),
                skipped(),
                skipped()
            ]
        );
        let got = checks(&cfg(&s.uri(), AuthKind::Basic, "PATH")).await;
        assert_eq!(got[0], (Ok, "$PATH set".into()));
    }

    #[tokio::test]
    async fn docker_config_without_entry_is_anonymous() {
        let s = server(two_repos()).await;
        let got = checks(&cfg(&s.uri(), AuthKind::DockerConfig, "")).await;
        assert_eq!(got[0], (Ok, "none — anonymous".into()));
    }

    #[tokio::test]
    async fn unreachable_endpoint_fails_step2() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let got = checks(&cfg(
            &format!("http://127.0.0.1:{port}"),
            AuthKind::Anonymous,
            "",
        ))
        .await;
        assert_eq!(got[0], (Ok, "none needed".into()));
        assert_eq!(got[1].0, Failed, "{got:?}");
        assert!(got[1].1.starts_with("network error"), "{got:?}");
        assert_eq!(got[2..], [skipped(), skipped()]);
    }

    #[tokio::test]
    async fn endpoint_401_is_reachable_other_status_fails() {
        let s = MockServer::start().await;
        Mock::given(path("/v2/"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        let got = checks(&cfg(&s.uri(), AuthKind::Anonymous, "")).await;
        assert_eq!(got[1], (Failed, "HTTP 404".into()));

        let s = MockServer::start().await;
        Mock::given(path("/v2/"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&s)
            .await;
        let got = checks(&cfg(&s.uri(), AuthKind::Anonymous, "")).await;
        assert_eq!(got[1], (Ok, "HTTP 401".into()));
        assert_eq!(got[2].0, Failed, "anonymous login refused: {got:?}");
        assert_eq!(got[3], skipped());
    }

    #[tokio::test]
    async fn no_catalog() {
        let s = server(ResponseTemplate::new(404)).await;
        let got = checks(&cfg(&s.uri(), AuthKind::Anonymous, "")).await;
        assert_eq!(got[2], (Ok, "HTTP 200".into()));
        assert_eq!(got[3], (Failed, "no catalog — add Repos".into()));
    }

    /// An ECR config whose credentials step uses a mocked STS client.
    async fn aws_checks(sts: aws_sdk_sts::Client) -> Vec<(CheckStatus, String)> {
        let s = server(two_repos()).await;
        let dir = tempfile::tempdir().unwrap();
        let ecr = validate(
            &RegistryForm {
                id: "e".into(),
                kind: Kind::Ecr,
                account_id: "123456789012".into(),
                region: "us-west-2".into(),
                endpoint: s.uri(),
                ..RegistryForm::new_default()
            },
            &[],
        )
        .unwrap();
        // The registry part is the wiremock one; only the credentials step differs.
        let oci = cfg(&s.uri(), AuthKind::Anonymous, "");
        let mut d = deps(&oci, &RequestLog::default(), dir.path())
            .await
            .unwrap();
        d.sts = Some(sts);
        run_with(&ecr, &d).await
    }

    #[tokio::test]
    async fn aws_credentials_from_sts() {
        let arn = "arn:aws:iam::123456789012:user/me";
        let ok = mock!(aws_sdk_sts::Client::get_caller_identity).then_output(move || {
            GetCallerIdentityOutput::builder()
                .account("123456789012")
                .arn(arn)
                .build()
        });
        let got = aws_checks(mock_client!(aws_sdk_sts, [&ok])).await;
        assert_eq!(got[0], (Ok, arn.into()));
        assert_eq!(got[3], (Ok, "2 repos".into()));

        let expired = mock!(aws_sdk_sts::Client::get_caller_identity).then_error(|| {
            GetCallerIdentityError::generic(ErrorMetadata::builder().code("ExpiredToken").build())
        });
        let got = aws_checks(mock_client!(aws_sdk_sts, [&expired])).await;
        assert_eq!(got[0].0, Failed, "{got:?}");
        assert!(got[0].1.starts_with("AWS credentials"), "{got:?}");
        assert_eq!(got[1..], [skipped(), skipped(), skipped()]);
    }

    #[tokio::test]
    async fn no_secret_in_details() {
        let s = server(two_repos()).await;
        let log = RequestLog::default();
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(&s.uri(), AuthKind::Basic, "PATH");
        let d = deps(&c, &log, dir.path()).await.unwrap();
        let got = run_with(&c, &d).await;
        assert!(got.iter().all(|(st, _)| *st == Ok), "{got:?}");
        let secret = std::env::var("PATH").unwrap();
        for (_, detail) in &got {
            assert!(!detail.contains(&secret), "{detail}");
        }
        let entries = log.snapshot();
        assert!(entries.iter().any(|e| e.path == "/v2/"), "step 2 is logged");
        for e in entries {
            assert!(!format!("{e:?}").contains(&secret));
        }
    }
}

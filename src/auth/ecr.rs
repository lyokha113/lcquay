use std::future::Future;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures::future::BoxFuture;
use time::OffsetDateTime;
use tokio::sync::Mutex;

use super::{Credential, CredentialSource, Destination, Issued, Secret};
use crate::domain::RegistryError;
use crate::reqlog::RegLog;

const REFRESH_MARGIN: time::Duration = time::Duration::minutes(5);
const CREDENTIAL_PROCESS_MSG: &str = "credential_process profiles are not supported by lcquay v1; use SSO, environment or static credentials";

/// ECR tokens are base64 of `AWS:<password>`; split on the first `:`, never inspect the password.
pub fn decode_token(b64: &str, expires_at: OffsetDateTime) -> Result<Issued, RegistryError> {
    let malformed = || RegistryError::Provider("malformed ECR authorization token".into());
    let raw = STANDARD.decode(b64.trim()).map_err(|_| malformed())?;
    let raw = String::from_utf8(raw).map_err(|_| malformed())?;
    let (username, password) = raw.split_once(':').ok_or_else(malformed)?;
    Ok(Issued {
        credential: Credential::Basic {
            username: username.to_string(),
            password: Secret(password.to_string()),
        },
        expires_at: Some(expires_at),
    })
}

/// Rejects a profile that sets `credential_process` (the SDK is built without it).
pub fn check_no_credential_process(config_text: &str, profile: &str) -> Result<(), RegistryError> {
    let in_profile = |section: &str| {
        let section = section.trim();
        if profile == "default" {
            section == "default" || section == "profile default"
        } else {
            section
                .strip_prefix("profile")
                .is_some_and(|n| n.starts_with(char::is_whitespace) && n.trim() == profile)
        }
    };
    let mut active = false;
    for line in config_text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[') {
            active = in_profile(section.split(']').next().unwrap_or(""));
        } else if active
            && line
                .split_once('=')
                .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case("credential_process"))
        {
            return Err(RegistryError::AwsCredentials(CREDENTIAL_PROCESS_MSG.into()));
        }
    }
    Ok(())
}

pub async fn load_sdk_config(
    region: &str,
    profile: Option<&str>,
) -> Result<aws_config::SdkConfig, RegistryError> {
    let effective = profile
        .map(str::to_string)
        .or_else(|| std::env::var("AWS_PROFILE").ok().filter(|p| !p.is_empty()))
        .unwrap_or_else(|| "default".into());
    let path = std::env::var_os("AWS_CONFIG_FILE")
        .map(std::path::PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().join(".aws/config")));
    if let Some(path) = path {
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => check_no_credential_process(&text, &effective)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(RegistryError::AwsCredentials(format!(
                    "cannot read AWS config: {}",
                    e.kind()
                )));
            }
        }
    }
    let timeouts = aws_config::timeout::TimeoutConfig::builder()
        .operation_attempt_timeout(Duration::from_secs(30))
        .operation_timeout(Duration::from_secs(90))
        .build();
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_string()))
        .timeout_config(timeouts);
    if let Some(p) = profile {
        loader = loader.profile_name(p);
    }
    Ok(loader.load().await)
}

/// Cache + refresh logic, independent of AWS. The lock is held across the fetch
/// so concurrent callers share one refresh (single flight).
pub(crate) struct TokenCache<F> {
    fetch: F,
    slot: Mutex<Option<Issued>>,
}

impl<F, Fut> TokenCache<F>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = Result<Issued, RegistryError>> + Send,
{
    pub(crate) fn new(fetch: F) -> Self {
        Self {
            fetch,
            slot: Mutex::new(None),
        }
    }

    pub(crate) async fn get(&self) -> Result<Issued, RegistryError> {
        let mut slot = self.slot.lock().await;
        if let Some(cached) = slot.as_ref()
            && cached
                .expires_at
                .is_some_and(|at| OffsetDateTime::now_utc() < at - REFRESH_MARGIN)
        {
            return Ok(cached.clone());
        }
        let fresh = (self.fetch)().await?;
        *slot = Some(fresh.clone());
        Ok(fresh)
    }
}

type BoxedFetch = Box<dyn Fn() -> BoxFuture<'static, Result<Issued, RegistryError>> + Send + Sync>;

pub struct EcrSource {
    client: aws_sdk_ecr::Client,
    cache: TokenCache<BoxedFetch>,
}

impl EcrSource {
    pub fn new(client: aws_sdk_ecr::Client) -> Self {
        Self::logged_to(client, RegLog::default())
    }

    pub fn with_log(self, log: RegLog) -> Self {
        Self::logged_to(self.client, log)
    }

    fn logged_to(client: aws_sdk_ecr::Client, log: RegLog) -> Self {
        let c = client.clone();
        let fetch: BoxedFetch = Box::new(move || {
            let (client, log) = (c.clone(), log.clone());
            Box::pin(async move { fetch_token(&client, &log).await })
        });
        Self {
            client,
            cache: TokenCache::new(fetch),
        }
    }
}

#[async_trait]
impl CredentialSource for EcrSource {
    /// The same Basic credential for every destination.
    async fn credential(&self, _dest: Destination) -> Result<Issued, RegistryError> {
        self.cache.get().await
    }

    fn can_login(&self) -> bool {
        true
    }
}

async fn fetch_token(client: &aws_sdk_ecr::Client, log: &RegLog) -> Result<Issued, RegistryError> {
    let call = client.get_authorization_token().send();
    let out = sdk_call(log, "ecr:GetAuthorizationToken", call).await?;
    let data = out
        .authorization_data()
        .first()
        .ok_or_else(|| RegistryError::Provider("ECR returned no authorization data".into()))?;
    let token = data
        .authorization_token()
        .ok_or_else(|| RegistryError::Provider("ECR returned no authorization token".into()))?;
    // A missing expiry is treated as already expired: refetch on every call.
    let expires_at = data
        .expires_at()
        .and_then(|t| OffsetDateTime::from_unix_timestamp_nanos(t.as_nanos()).ok())
        .unwrap_or_else(OffsetDateTime::now_utc);
    decode_token(token, expires_at)
}

pub struct EcrPublicSource {
    client: aws_sdk_ecrpublic::Client,
    cache: TokenCache<BoxedFetch>,
}

impl EcrPublicSource {
    pub fn new(client: aws_sdk_ecrpublic::Client) -> Self {
        Self::logged_to(client, RegLog::default())
    }

    pub fn with_log(self, log: RegLog) -> Self {
        Self::logged_to(self.client, log)
    }

    fn logged_to(client: aws_sdk_ecrpublic::Client, log: RegLog) -> Self {
        let c = client.clone();
        let fetch: BoxedFetch = Box::new(move || {
            let (client, log) = (c.clone(), log.clone());
            Box::pin(async move { fetch_public_token(&client, &log).await })
        });
        Self {
            client,
            cache: TokenCache::new(fetch),
        }
    }
}

#[async_trait]
impl CredentialSource for EcrPublicSource {
    /// The same Basic credential for every destination; `Http` is answered through
    /// the `public.ecr.aws/token/` challenge (spec §13).
    async fn credential(&self, _dest: Destination) -> Result<Issued, RegistryError> {
        self.cache.get().await
    }

    fn can_login(&self) -> bool {
        true
    }
}

async fn fetch_public_token(
    client: &aws_sdk_ecrpublic::Client,
    log: &RegLog,
) -> Result<Issued, RegistryError> {
    let call = client.get_authorization_token().send();
    let out = sdk_call(log, "ecr-public:GetAuthorizationToken", call).await?;
    // Unlike ECR, the output carries a single `authorization_data`, not a list.
    let data = out.authorization_data().ok_or_else(|| {
        RegistryError::Provider("ECR Public returned no authorization data".into())
    })?;
    let token = data.authorization_token().ok_or_else(|| {
        RegistryError::Provider("ECR Public returned no authorization token".into())
    })?;
    let expires_at = data
        .expires_at()
        .and_then(|t| OffsetDateTime::from_unix_timestamp_nanos(t.as_nanos()).ok())
        .unwrap_or_else(OffsetDateTime::now_utc);
    decode_token(token, expires_at)
}

/// Whether `e`'s source chain holds a credential-provider failure. The SDK reports one as a
/// `DispatchFailure` although it happens before anything is sent.
pub(crate) fn credentials_failed(e: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(e), |e| e.source())
        .any(|e| e.is::<aws_credential_types::provider::error::CredentialsError>())
}

/// Awaits one SDK call of the qualified operation `op`, records it in `log` and maps its error.
pub(crate) async fn sdk_call<T, E>(
    log: &RegLog,
    op: &str,
    call: impl Future<Output = Result<T, aws_sdk_ecr::error::SdkError<E>>>,
) -> Result<T, RegistryError>
where
    E: std::error::Error + aws_sdk_ecr::error::ProvideErrorMetadata + Send + Sync + 'static,
{
    let t = Instant::now();
    let r = call.await.map_err(|e| map_sdk_error(op, e));
    log.call(op, "", t, r.as_ref().map(|_| ()));
    r
}

/// Maps an SDK failure of the qualified operation `op` (`ecr:…`, `ecr-public:…` or `sts:…`).
/// The SDKs share one `SdkError`, so one mapper serves every provider call.
pub(crate) fn map_sdk_error<E>(op: &str, err: aws_sdk_ecr::error::SdkError<E>) -> RegistryError
where
    E: std::error::Error + aws_sdk_ecr::error::ProvideErrorMetadata + Send + Sync + 'static,
{
    use aws_sdk_ecr::error::SdkError;
    // Messages come from SDK error chains, which never contain the token.
    let detail = aws_sdk_ecr::error::DisplayErrorContext(&err).to_string();
    if credentials_failed(&err) {
        return RegistryError::AwsCredentials(detail);
    }
    match &err {
        SdkError::ServiceError(se) => match se.err().code() {
            Some(c) if c.starts_with("AccessDenied") => RegistryError::Forbidden(op.to_string()),
            Some("RepositoryNotFoundException") => RegistryError::NotFound,
            Some(
                "ThrottlingException"
                | "TooManyRequestsException"
                | "RequestLimitExceeded"
                | "Throttling"
                | "ThrottledException",
            ) => RegistryError::RateLimited,
            Some(
                "ExpiredTokenException"
                | "ExpiredToken"
                | "SignatureDoesNotMatch"
                | "IncompleteSignature"
                | "UnrecognizedClientException"
                | "InvalidSignatureException"
                | "InvalidClientTokenId",
            ) => RegistryError::AwsCredentials(detail),
            _ => RegistryError::Provider(detail),
        },
        SdkError::TimeoutError(_) => RegistryError::Timeout(op.to_string()),
        SdkError::DispatchFailure(_) => {
            let reason = crate::registry::http::root_cause(&err);
            RegistryError::Network(reason.unwrap_or_else(|| op.to_string()))
        }
        _ if detail.to_lowercase().contains("credential") => RegistryError::AwsCredentials(detail),
        _ => RegistryError::Provider(detail),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn b64(raw: &[u8]) -> String {
        STANDARD.encode(raw)
    }

    fn in_(d: time::Duration) -> OffsetDateTime {
        OffsetDateTime::now_utc() + d
    }

    fn issued(expires_at: OffsetDateTime) -> Issued {
        decode_token(&b64(b"AWS:tok"), expires_at).unwrap()
    }

    #[test]
    fn decode_splits_user_and_password() {
        let i = decode_token(&b64(b"AWS:pa:ss"), in_(time::Duration::hours(12))).unwrap();
        assert!(
            matches!(i.credential, Credential::Basic{ref username, ref password} if username=="AWS" && password.expose()=="pa:ss")
        );
    }

    #[test]
    fn decode_rejects_malformed_without_leaking() {
        for bad in ["!!!not base64", &b64(b"nocolon")] {
            let e = decode_token(bad, in_(time::Duration::hours(1))).unwrap_err();
            assert!(matches!(e, RegistryError::Provider(_)));
            assert!(!e.to_string().contains("nocolon"));
        }
    }

    #[test]
    fn credential_process_rejected() {
        let cfg = "[profile work]\nregion = us-west-2\ncredential_process = /bin/x\n";
        assert!(check_no_credential_process(cfg, "work").is_err());
        assert!(check_no_credential_process(cfg, "default").is_ok());
    }

    #[test]
    fn credential_process_error_message() {
        let cfg = "[default]\ncredential_process=/bin/x\n";
        assert_eq!(
            check_no_credential_process(cfg, "default"),
            Err(RegistryError::AwsCredentials(CREDENTIAL_PROCESS_MSG.into()))
        );
    }

    #[test]
    fn credential_process_parsing_edge_cases() {
        let cfg = "# credential_process = x\n[default]\n; credential_process = y\nregion = a\n\n[profile  spaced]\ncredential_process = z\n[profile other]\nregion = b\n[sso-session work]\ncredential_process = q\n";
        assert!(check_no_credential_process(cfg, "default").is_ok());
        assert!(check_no_credential_process(cfg, "spaced").is_err());
        assert!(check_no_credential_process(cfg, "other").is_ok());
        assert!(check_no_credential_process(cfg, "work").is_ok());
        assert!(
            check_no_credential_process("[profile default]\ncredential_process = x\n", "default")
                .is_err()
        );
    }

    fn counting(
        calls: Arc<AtomicUsize>,
        lifetimes: Vec<time::Duration>,
        delay: Duration,
    ) -> TokenCache<BoxedFetch> {
        TokenCache::new(Box::new(move || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            let life = lifetimes[n.min(lifetimes.len() - 1)];
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                Ok(issued(in_(life)))
            })
        }))
    }

    #[test]
    fn dispatch_failure_is_network() {
        use aws_sdk_ecr::error::{ConnectorError, SdkError};
        use aws_sdk_ecr::operation::get_authorization_token::GetAuthorizationTokenError as E;
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "Connection refused");
        let err: SdkError<E, aws_sdk_ecr::config::http::HttpResponse> =
            SdkError::dispatch_failure(ConnectorError::io(Box::new(io)));
        let e = map_sdk_error("ecr:GetAuthorizationToken", err);
        assert!(
            matches!(&e, RegistryError::Network(m) if m.contains("refused")),
            "{e:?}"
        );
    }

    #[test]
    fn credentials_dispatch_failure_is_aws_credentials() {
        use aws_credential_types::provider::error::CredentialsError;
        use aws_sdk_ecr::error::{ConnectorError, SdkError};
        use aws_sdk_ecr::operation::get_authorization_token::GetAuthorizationTokenError as E;
        let cause = Box::new(CredentialsError::not_loaded("no profile"));
        let err: SdkError<E, aws_sdk_ecr::config::http::HttpResponse> =
            SdkError::dispatch_failure(ConnectorError::other(cause, None));
        let e = map_sdk_error("ecr:GetAuthorizationToken", err);
        assert!(matches!(e, RegistryError::AwsCredentials(_)), "{e:?}");
    }

    #[tokio::test]
    async fn sts_credential_codes_are_aws_credentials() {
        use aws_sdk_sts::operation::get_caller_identity::GetCallerIdentityError;
        for code in [
            "ExpiredToken",
            "SignatureDoesNotMatch",
            "IncompleteSignature",
        ] {
            let r = aws_smithy_mocks::mock!(aws_sdk_sts::Client::get_caller_identity).then_error(
                move || {
                    GetCallerIdentityError::generic(
                        aws_sdk_ecr::error::ErrorMetadata::builder()
                            .code(code)
                            .build(),
                    )
                },
            );
            let sts = aws_smithy_mocks::mock_client!(aws_sdk_sts, [&r]);
            let err = sts.get_caller_identity().send().await.unwrap_err();
            let e = map_sdk_error("sts:GetCallerIdentity", err);
            assert!(
                matches!(e, RegistryError::AwsCredentials(_)),
                "{code}: {e:?}"
            );
        }
    }

    #[tokio::test]
    async fn token_cache_refreshes_5min_before_expiry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cache = counting(
            calls.clone(),
            vec![time::Duration::minutes(4), time::Duration::hours(1)],
            Duration::ZERO,
        );
        cache.get().await.unwrap();
        cache.get().await.unwrap(); // 4 min left < 5 min margin: refetch
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        cache.get().await.unwrap(); // 1 h token reused
        cache.get().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn token_cache_single_flight() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cache = Arc::new(counting(
            calls.clone(),
            vec![time::Duration::hours(1)],
            Duration::from_millis(100),
        ));
        let tasks: Vec<_> = (0..10)
            .map(|_| {
                let c = cache.clone();
                tokio::spawn(async move { c.get().await.unwrap() })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn token_cache_failure_is_not_cached() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        let cache = TokenCache::new(move || {
            let n = c2.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(RegistryError::Provider("boom".into()))
                } else {
                    Ok(issued(in_(time::Duration::hours(1))))
                }
            }
        });
        assert!(cache.get().await.is_err());
        assert!(cache.get().await.is_ok());
    }

    #[tokio::test]
    async fn no_expiry_always_refetches() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        let cache = TokenCache::new(move || {
            c2.fetch_add(1, Ordering::SeqCst);
            async {
                Ok(Issued {
                    credential: Credential::Anonymous,
                    expires_at: None,
                })
            }
        });
        cache.get().await.unwrap();
        cache.get().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    fn mock_source(rule: &aws_smithy_mocks::Rule) -> EcrSource {
        EcrSource::new(aws_smithy_mocks::mock_client!(aws_sdk_ecr, [rule]))
    }

    #[tokio::test]
    async fn ecr_source_uses_first_authorization_data_for_every_destination() {
        use aws_sdk_ecr::primitives::DateTime;
        use aws_sdk_ecr::types::AuthorizationData;
        let exp = DateTime::from_secs(
            (OffsetDateTime::now_utc() + time::Duration::hours(12)).unix_timestamp(),
        );
        let rule = aws_smithy_mocks::mock!(aws_sdk_ecr::Client::get_authorization_token).then_output(move || {
            aws_sdk_ecr::operation::get_authorization_token::GetAuthorizationTokenOutput::builder()
                .authorization_data(
                    AuthorizationData::builder()
                        .authorization_token(STANDARD.encode(b"AWS:s3cret"))
                        .expires_at(exp)
                        .build(),
                )
                .build()
        });
        let src = mock_source(&rule);
        for d in [Destination::Http, Destination::Login, Destination::Engine] {
            let i = src.credential(d).await.unwrap();
            assert!(
                matches!(i.credential, Credential::Basic{ref username, ref password} if username=="AWS" && password.expose()=="s3cret")
            );
            assert!(i.expires_at.is_some());
        }
        assert_eq!(rule.num_calls(), 1);
        assert!(src.can_login());
    }

    #[tokio::test]
    async fn ecr_source_maps_access_denied() {
        use aws_sdk_ecr::operation::get_authorization_token::GetAuthorizationTokenError;
        let rule = aws_smithy_mocks::mock!(aws_sdk_ecr::Client::get_authorization_token)
            .then_error(|| {
                GetAuthorizationTokenError::generic(
                    aws_sdk_ecr::error::ErrorMetadata::builder()
                        .code("AccessDeniedException")
                        .message("nope")
                        .build(),
                )
            });
        let e = mock_source(&rule)
            .credential(Destination::Http)
            .await
            .unwrap_err();
        assert_eq!(
            e,
            RegistryError::Forbidden("ecr:GetAuthorizationToken".into())
        );
    }

    fn public_source(rule: &aws_smithy_mocks::Rule) -> EcrPublicSource {
        EcrPublicSource::new(aws_smithy_mocks::mock_client!(aws_sdk_ecrpublic, [rule]))
    }

    #[tokio::test]
    async fn ecr_public_source_basic_for_every_destination() {
        use aws_sdk_ecrpublic::operation::get_authorization_token::GetAuthorizationTokenOutput;
        use aws_sdk_ecrpublic::primitives::DateTime;
        use aws_sdk_ecrpublic::types::AuthorizationData;
        let at = (OffsetDateTime::now_utc() + time::Duration::hours(12)).unix_timestamp();
        let rule = aws_smithy_mocks::mock!(aws_sdk_ecrpublic::Client::get_authorization_token)
            .then_output(move || {
                GetAuthorizationTokenOutput::builder()
                    .authorization_data(
                        AuthorizationData::builder()
                            .authorization_token(STANDARD.encode(b"AWS:pw"))
                            .expires_at(DateTime::from_secs(at))
                            .build(),
                    )
                    .build()
            });
        let src = public_source(&rule);
        for d in [Destination::Http, Destination::Login, Destination::Engine] {
            let i = src.credential(d).await.unwrap();
            assert!(
                matches!(i.credential, Credential::Basic{ref username, ref password} if username=="AWS" && password.expose()=="pw")
            );
            assert_eq!(i.expires_at.unwrap().unix_timestamp(), at);
        }
        assert_eq!(rule.num_calls(), 1);
        assert!(src.can_login());
    }

    #[tokio::test]
    async fn ecr_public_source_access_denied() {
        use aws_sdk_ecrpublic::operation::get_authorization_token::GetAuthorizationTokenError;
        let rule = aws_smithy_mocks::mock!(aws_sdk_ecrpublic::Client::get_authorization_token)
            .then_error(|| {
                GetAuthorizationTokenError::generic(
                    aws_sdk_ecrpublic::error::ErrorMetadata::builder()
                        .code("AccessDeniedException")
                        .message("nope")
                        .build(),
                )
            });
        let e = public_source(&rule)
            .credential(Destination::Http)
            .await
            .unwrap_err();
        assert_eq!(
            e,
            RegistryError::Forbidden("ecr-public:GetAuthorizationToken".into())
        );
    }

    #[tokio::test]
    async fn ecr_public_source_missing_data_is_provider_error() {
        use aws_sdk_ecrpublic::operation::get_authorization_token::GetAuthorizationTokenOutput;
        let rule = aws_smithy_mocks::mock!(aws_sdk_ecrpublic::Client::get_authorization_token)
            .then_output(|| GetAuthorizationTokenOutput::builder().build());
        let e = public_source(&rule)
            .credential(Destination::Http)
            .await
            .unwrap_err();
        assert_eq!(
            e,
            RegistryError::Provider("ECR Public returned no authorization data".into())
        );
    }
}

//! Status-line wording for failed loads (spec §9).

use super::state::{CacheKey, RegistryView};
use crate::auth::helper::HELPER_FAILED_PREFIX;
use crate::config::AuthCfg;
use crate::domain::RegistryError;
use crate::providers::oci::LISTING_UNAVAILABLE;

/// `what` names the thing being loaded (see `describe`); used by the timeout and cursor rows.
pub fn user_message(err: &RegistryError, reg: &RegistryView, what: &str) -> String {
    use RegistryError as E;
    let id = &reg.id;
    match err {
        // Specific, already user-facing texts (credential_process, AWS config read errors).
        E::AwsCredentials(m) if m.contains("credential_process") => m.clone(),
        E::AwsCredentials(_) => format!("AWS creds invalid/expired for {id} — check profile/env"),
        E::Forbidden(action) => format!("No permission on {id}: {action}"),
        E::Unauthorized => {
            let hint = match &reg.auth {
                AuthCfg::Ecr => "check AWS creds".to_owned(),
                AuthCfg::DockerConfig => format!("run `docker login {}`", reg.ref_host),
                AuthCfg::Anonymous => "registry requires credentials".to_owned(),
                AuthCfg::Basic {
                    username,
                    password_env,
                } => format!("check username {username} and ${password_env}"),
            };
            format!("auth failed for {id} — {hint}")
        }
        E::UntrustedRealm(host) => {
            format!("untrusted auth realm {host} for {id} — add it to trusted_realms if expected")
        }
        E::RateLimited => format!("rate limited by {id} — press r to retry later"),
        E::Unsupported(m) if m.starts_with(LISTING_UNAVAILABLE) => {
            format!("repository listing not available on {id} — add repositories = [...] in config")
        }
        // Identity tokens and other unsupported setups carry their own wording.
        E::Unsupported(m) => m.clone(),
        E::Provider(m) if m.starts_with(HELPER_FAILED_PREFIX) => m.clone(),
        E::Provider(m) => format!("{id}: {m}"),
        E::Network(m) => format!("network error: {m} — press r to retry"),
        E::Timeout(_) => format!("timed out loading {what} from {id} — press r to retry"),
        E::RepeatedCursor(_) => format!("{id} returned a repeating page cursor for {what}"),
        E::NotFound => format!("{what} not found on {id}"),
        E::TooLarge => format!("{what} from {id} is too large (over 4 MiB)"),
        E::Internal(m) => format!("internal error: {m}"),
    }
}

/// What a key loads, for messages: "images of app", "manifest app@e4498f74aaaa", …
pub fn describe(key: &CacheKey) -> String {
    match key {
        CacheKey::Repos(_) => "repositories".to_owned(),
        CacheKey::Inventory(_, p) => format!("images of {p}"),
        CacheKey::Manifest(_, p, d) => format!("manifest {p}@{}", d.short()),
        CacheKey::Config(_, p, d) => format!("config of {p}@{}", d.short()),
        CacheKey::Referrers(_, p, d) => format!("referrers of {p}@{}", d.short()),
        CacheKey::TagPolicy(_, p, t) => format!("tag policy of {p}:{t}"),
        CacheKey::RegistryDetails(_) => "registry details".to_owned(),
        CacheKey::RepoDetails(_, p) => format!("details of {p}"),
        CacheKey::LocalImages => "local images".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::basic::BASIC_ENV_UNSET;
    use crate::auth::helper::IDENTITY_TOKEN_MSG;
    use crate::domain::{Caps, Inventory, RegistryId};

    fn reg(id: &str, auth: AuthCfg) -> RegistryView {
        RegistryView {
            id: RegistryId(id.into()),
            ref_host: "h.example".into(),
            endpoint: "https://h.example".parse().unwrap(),
            provider: "oci",
            auth,
            caps: Caps {
                inventory: Inventory::TaggedOnly,
                pushed_at: false,
                last_pulled: false,
                delete_tag: false,
                delete_manifest: false,
            },
            auth_failed: false,
            can_login: false,
            login: None,
            pinned: vec![],
        }
    }

    #[test]
    fn user_messages() {
        use RegistryError as E;
        let ecr = reg("ecr", AuthCfg::Ecr);
        let m = |e: E| user_message(&e, &ecr, "images of app");

        assert_eq!(
            m(E::AwsCredentials("token expired".into())),
            "AWS creds invalid/expired for ecr — check profile/env"
        );
        let cp = "credential_process profiles are not supported by lcquay v1; use SSO, environment or static credentials";
        assert_eq!(m(E::AwsCredentials(cp.into())), cp);
        assert_eq!(
            m(E::Forbidden("ecr:DescribeImages".into())),
            "No permission on ecr: ecr:DescribeImages"
        );
        assert_eq!(m(E::Unauthorized), "auth failed for ecr — check AWS creds");
        assert_eq!(
            user_message(&E::Unauthorized, &reg("hub", AuthCfg::DockerConfig), ""),
            "auth failed for hub — run `docker login h.example`"
        );
        assert_eq!(
            user_message(&E::Unauthorized, &reg("pub", AuthCfg::Anonymous), ""),
            "auth failed for pub — registry requires credentials"
        );
        assert_eq!(
            m(E::Unsupported(IDENTITY_TOKEN_MSG.into())),
            IDENTITY_TOKEN_MSG
        );
        assert_eq!(
            m(E::UntrustedRealm("evil.example".into())),
            "untrusted auth realm evil.example for ecr — add it to trusted_realms if expected"
        );
        assert_eq!(
            m(E::RateLimited),
            "rate limited by ecr — press r to retry later"
        );
        assert_eq!(
            m(E::Unsupported(
                "repository listing not available — add repositories = [...] in config".into()
            )),
            "repository listing not available on ecr — add repositories = [...] in config"
        );
        // Docker unavailable: local-images failures show this text verbatim (see update).
        assert_eq!(
            crate::docker::UNAVAILABLE,
            "Docker not available — push and local check disabled"
        );
        assert_eq!(
            m(E::Provider(
                "credential helper pass failed: gpg: no secret key".into()
            )),
            "credential helper pass failed: gpg: no secret key"
        );
        assert_eq!(
            m(E::Network("connection refused".into())),
            "network error: connection refused — press r to retry"
        );
        assert_eq!(
            m(E::Timeout("request".into())),
            "timed out loading images of app from ecr — press r to retry"
        );
        assert_eq!(
            user_message(
                &E::RepeatedCursor("/v2/_catalog".into()),
                &ecr,
                "repositories"
            ),
            "ecr returned a repeating page cursor for repositories"
        );
    }

    fn harbor() -> RegistryView {
        reg(
            "harbor",
            AuthCfg::Basic {
                username: "ci".into(),
                password_env: "HARBOR_PW".into(),
            },
        )
    }

    #[test]
    fn unauthorized_hint_basic() {
        assert_eq!(
            user_message(&RegistryError::Unauthorized, &harbor(), ""),
            "auth failed for harbor — check username ci and $HARBOR_PW"
        );
    }

    #[test]
    fn basic_env_unset_message() {
        let e = RegistryError::Provider(format!("{}X is not set or empty", BASIC_ENV_UNSET));
        assert_eq!(
            user_message(&e, &harbor(), ""),
            "harbor: environment variable X is not set or empty"
        );
    }

    #[test]
    fn describe_keys() {
        let d = crate::domain::Digest::parse(&format!("sha256:{}", "e".repeat(64))).unwrap();
        let (r, p) = (
            RegistryId("ecr".into()),
            crate::domain::RepoPath("app".into()),
        );
        assert_eq!(describe(&CacheKey::Repos(r.clone())), "repositories");
        assert_eq!(
            describe(&CacheKey::Inventory(r.clone(), p.clone())),
            "images of app"
        );
        assert_eq!(
            describe(&CacheKey::Manifest(r, p, d)),
            "manifest app@eeeeeeeeeeee"
        );
        assert_eq!(
            describe(&CacheKey::TagPolicy(
                RegistryId("ecr".into()),
                crate::domain::RepoPath("app".into()),
                "1.0".into()
            )),
            "tag policy of app:1.0"
        );
        let (r, p) = (
            RegistryId("ecr".into()),
            crate::domain::RepoPath("app".into()),
        );
        assert_eq!(
            describe(&CacheKey::RegistryDetails(r.clone())),
            "registry details"
        );
        assert_eq!(describe(&CacheKey::RepoDetails(r, p)), "details of app");
    }
}

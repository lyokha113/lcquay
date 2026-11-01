//! `WWW-Authenticate` parsing, Bearer realm trust and token lifetime (spec §7.2).

use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Challenge {
    Basic,
    Bearer {
        realm: Url,
        service: Option<String>,
        scope: Option<String>,
    },
}

/// Parses one challenge. Unknown schemes, and Bearer without a valid absolute
/// `realm` URL, yield `None`.
pub fn parse_www_authenticate(v: &str) -> Option<Challenge> {
    let v = v.trim();
    let (scheme, rest) = v.split_once(char::is_whitespace).unwrap_or((v, ""));
    if scheme.eq_ignore_ascii_case("basic") {
        return Some(Challenge::Basic);
    }
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let params = parse_params(rest);
    let get = |k: &str| {
        params
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.clone())
    };
    Some(Challenge::Bearer {
        realm: Url::parse(&get("realm")?).ok()?,
        service: get("service"),
        scope: get("scope"),
    })
}

/// `name=value` pairs separated by commas; values may be quoted (with `\`
/// escapes) and then contain commas.
fn parse_params(mut s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    loop {
        s = s.trim_start_matches(|c: char| c == ',' || c.is_whitespace());
        let Some((name, rest)) = s.split_once('=') else {
            return out;
        };
        let rest = rest.trim_start();
        let value = if let Some(q) = rest.strip_prefix('"') {
            let mut v = String::new();
            let mut end = q.len(); // unterminated quote: take the rest
            let mut escaped = false;
            for (i, c) in q.char_indices() {
                match c {
                    _ if escaped => {
                        v.push(c);
                        escaped = false;
                    }
                    '\\' => escaped = true,
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    _ => v.push(c),
                }
            }
            s = &q[end..];
            v
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            s = &rest[end..];
            rest[..end].trim().to_owned()
        };
        out.push((name.trim().to_owned(), value));
    }
}

/// HTTPS realm on the endpoint host or a `trusted` host; an `http` realm only
/// when the endpoint is `http` and on the same host (local development).
pub fn realm_trusted(realm: &Url, endpoint: &Url, trusted: &[String]) -> bool {
    let Some(host) = realm.host_str() else {
        return false;
    };
    let same_host = endpoint
        .host_str()
        .is_some_and(|h| h.eq_ignore_ascii_case(host));
    match realm.scheme() {
        "https" => same_host || trusted.iter().any(|t| t.eq_ignore_ascii_case(host)),
        "http" => endpoint.scheme() == "http" && same_host,
        _ => false,
    }
}

/// Token endpoint body. No `Debug`: it carries the token.
#[derive(Default, Deserialize)]
pub struct TokenResponse {
    pub token: Option<String>,
    pub access_token: Option<String>,
    pub expires_in: Option<u64>,
    pub issued_at: Option<String>,
}

/// `issued_at` (RFC 3339) if present and valid, else `received`, plus
/// `expires_in` (default 60 s).
pub fn token_expiry(resp: &TokenResponse, received: OffsetDateTime) -> OffsetDateTime {
    let start = resp
        .issued_at
        .as_deref()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .unwrap_or(received);
    let secs = i64::try_from(resp.expires_in.unwrap_or(60)).unwrap_or(i64::MAX);
    start.saturating_add(Duration::seconds(secs))
}

/// Tokens are renewed 10 s before expiry.
pub fn needs_renewal(expiry: OffsetDateTime, now: OffsetDateTime) -> bool {
    now >= expiry.saturating_sub(Duration::seconds(10))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;
    use url::Url;

    #[test]
    fn parse_ecr_basic() {
        assert_eq!(
            parse_www_authenticate(r#"Basic realm="https://x",service="ecr.amazonaws.com""#),
            Some(Challenge::Basic)
        );
        assert_eq!(parse_www_authenticate("basic"), Some(Challenge::Basic));
    }

    #[test]
    fn parse_public_ecr_bearer() {
        let c = parse_www_authenticate(
            r#"Bearer realm="https://public.ecr.aws/token/",service="public.ecr.aws",scope="aws""#,
        )
        .unwrap();
        assert!(matches!(c, Challenge::Bearer{ref scope, ..} if scope.as_deref()==Some("aws")));
        assert_eq!(
            c,
            Challenge::Bearer {
                realm: Url::parse("https://public.ecr.aws/token/").unwrap(),
                service: Some("public.ecr.aws".into()),
                scope: Some("aws".into()),
            }
        );
    }

    #[test]
    fn parse_scope_with_comma() {
        let c = parse_www_authenticate(
            r#"bEaReR realm="https://h/token", scope="repository:a/b:pull,push", service=h"#,
        )
        .unwrap();
        assert_eq!(
            c,
            Challenge::Bearer {
                realm: Url::parse("https://h/token").unwrap(),
                service: Some("h".into()),
                scope: Some("repository:a/b:pull,push".into()),
            }
        );
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(parse_www_authenticate(""), None);
        assert_eq!(parse_www_authenticate("Negotiate abc"), None);
        assert_eq!(parse_www_authenticate(r#"Bearer service="x""#), None);
        assert_eq!(parse_www_authenticate(r#"Bearer realm="not a url""#), None);
    }

    #[test]
    fn realm_rules() {
        let ep = Url::parse("https://harbor.example.com").unwrap();
        assert!(realm_trusted(
            &Url::parse("https://harbor.example.com/service/token").unwrap(),
            &ep,
            &[]
        ));
        assert!(!realm_trusted(
            &Url::parse("https://evil.example").unwrap(),
            &ep,
            &[]
        ));
        assert!(realm_trusted(
            &Url::parse("https://auth.example.com/t").unwrap(),
            &ep,
            &["auth.example.com".into()]
        ));
        assert!(!realm_trusted(
            &Url::parse("http://harbor.example.com/t").unwrap(),
            &ep,
            &[]
        ));
        let local = Url::parse("http://localhost:5000").unwrap();
        assert!(realm_trusted(
            &Url::parse("http://localhost:5000/auth").unwrap(),
            &local,
            &[]
        ));
        // http realm on a trusted (non-endpoint) host is still refused
        assert!(!realm_trusted(
            &Url::parse("http://auth.example.com/t").unwrap(),
            &local,
            &["auth.example.com".into()]
        ));
        assert!(!realm_trusted(
            &Url::parse("ftp://harbor.example.com/t").unwrap(),
            &ep,
            &[]
        ));
    }

    #[test]
    fn expiry_uses_issued_at() {
        let received = datetime!(2026-10-29 12:00:00 UTC);
        let r = TokenResponse {
            issued_at: Some("2026-10-29T11:59:00Z".into()),
            expires_in: Some(300),
            ..Default::default()
        };
        assert_eq!(
            token_expiry(&r, received),
            datetime!(2026-10-29 12:04:00 UTC)
        );
        let r = TokenResponse {
            expires_in: Some(300),
            ..Default::default()
        };
        assert_eq!(
            token_expiry(&r, received),
            datetime!(2026-10-29 12:05:00 UTC)
        );
        let r = TokenResponse::default();
        assert_eq!(
            token_expiry(&r, received),
            datetime!(2026-10-29 12:01:00 UTC)
        );
        let r = TokenResponse {
            expires_in: Some(u64::MAX),
            ..Default::default()
        };
        assert!(token_expiry(&r, received) > received);
    }

    #[test]
    fn renewal_ten_seconds_early() {
        let exp = datetime!(2026-10-29 12:01:00 UTC);
        assert!(!needs_renewal(exp, datetime!(2026-10-29 12:00:49 UTC)));
        assert!(needs_renewal(exp, datetime!(2026-10-29 12:00:50 UTC)));
        assert!(needs_renewal(exp, datetime!(2026-10-29 12:02:00 UTC)));
    }

    #[test]
    fn token_response_deserializes() {
        let r: TokenResponse =
            serde_json::from_str(r#"{"access_token":"t","expires_in":30}"#).unwrap();
        assert_eq!(r.access_token.as_deref(), Some("t"));
        assert_eq!(r.expires_in, Some(30));
    }
}

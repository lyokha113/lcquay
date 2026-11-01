//! The request log: a ring of the last `LOG_CAP` registry, SDK and Docker calls, shown by the
//! requests overlay. An entry holds only the method, a query-free path, the outcome, the time
//! and a size: never a header, a body, a query string or a credential.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use reqwest::Method;
use time::OffsetDateTime;
use url::Url;

use crate::domain::RegistryError;

pub const LOG_CAP: usize = 500;

#[derive(Clone, Debug, PartialEq)]
pub struct RequestEntry {
    pub at: OffsetDateTime,
    pub registry: String,
    pub op: String,
    pub path: String,
    /// The HTTP status, `ok` for SDK and Docker calls, else `RegistryError::kind()`.
    pub outcome: String,
    pub ms: u64,
    pub bytes: Option<u64>,
}

/// Shared by every client; the entries plus a generation bumped on each push.
#[derive(Clone, Default)]
pub struct RequestLog(Arc<Mutex<(VecDeque<RequestEntry>, u64)>>);

impl RequestLog {
    fn lock(&self) -> MutexGuard<'_, (VecDeque<RequestEntry>, u64)> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Drops the oldest entry beyond `LOG_CAP`.
    pub fn push(&self, e: RequestEntry) {
        let mut g = self.lock();
        if g.0.len() == LOG_CAP {
            g.0.pop_front();
        }
        g.0.push_back(e);
        g.1 += 1;
    }

    /// Oldest first.
    pub fn snapshot(&self) -> Vec<RequestEntry> {
        self.lock().0.iter().cloned().collect()
    }

    pub fn generation(&self) -> u64 {
        self.lock().1
    }

    pub fn scoped(&self, registry: &str) -> RegLog {
        RegLog {
            log: self.clone(),
            registry: registry.to_string(),
        }
    }
}

/// The log as one registry (or `local`) writes to it.
#[derive(Clone, Default)]
pub struct RegLog {
    log: RequestLog,
    registry: String,
}

impl RegLog {
    /// One HTTP request. The path is `url.path()` for registry API paths (`/v2/…`) on the
    /// endpoint origin, else `host[:port]` + path (token realms, other origins); never a query.
    pub fn http(
        &self,
        method: &Method,
        url: &Url,
        endpoint: &Url,
        status: Result<u16, &RegistryError>,
        started: Instant,
        bytes: Option<u64>,
    ) {
        let path = if url.origin() == endpoint.origin() && url.path().starts_with("/v2/") {
            url.path().to_string()
        } else {
            let host = url.host_str().unwrap_or_default();
            match url.port() {
                Some(p) => format!("{host}:{p}{}", url.path()),
                None => format!("{host}{}", url.path()),
            }
        };
        let outcome = match status {
            Ok(s) => s.to_string(),
            Err(e) => e.kind().to_string(),
        };
        self.push(op_label(method, url.path()), path, outcome, started, bytes);
    }

    /// One SDK or Docker call (`op` such as `ecr:DescribeImages` or `docker pull`).
    pub fn call(&self, op: &str, path: &str, started: Instant, result: Result<(), &RegistryError>) {
        let outcome = result.map_or_else(|e| e.kind(), |()| "ok");
        self.push(op.into(), path.into(), outcome.into(), started, None);
    }

    fn push(
        &self,
        op: String,
        path: String,
        outcome: String,
        started: Instant,
        bytes: Option<u64>,
    ) {
        self.log.push(RequestEntry {
            at: OffsetDateTime::now_utc(),
            registry: self.registry.clone(),
            op,
            path,
            outcome,
            ms: started.elapsed().as_millis() as u64,
            bytes,
        });
    }
}

/// `<METHOD> <what>` from a URL path; anything outside `/v2/` is a token realm.
pub fn op_label(method: &Method, path: &str) -> String {
    let what = match path.strip_prefix("/v2/") {
        None => "token",
        Some("") => "ping",
        Some("_catalog") => "catalog",
        Some(p) if p.contains("/blobs/uploads/") => "upload",
        Some(p) if p.contains("/manifests/") => "manifest",
        Some(p) if p.contains("/blobs/") => "blob",
        Some(p) if p.ends_with("/tags/list") => "tags",
        Some(p) if p.contains("/referrers/") => "referrers",
        Some(_) => "other",
    };
    format!("{method} {what}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(i: usize) -> RequestEntry {
        RequestEntry {
            at: OffsetDateTime::UNIX_EPOCH,
            registry: "r".into(),
            op: format!("op{i}"),
            path: String::new(),
            outcome: "ok".into(),
            ms: 0,
            bytes: None,
        }
    }

    #[test]
    fn ring_keeps_last_500() {
        let log = RequestLog::default();
        for i in 1..=501 {
            log.push(entry(i));
        }
        let all = log.snapshot();
        assert_eq!(all.len(), 500);
        assert_eq!(all[0], entry(2));
        assert_eq!(all[499], entry(501));
        assert_eq!(log.generation(), 501);
    }

    #[test]
    fn op_labels() {
        for (m, p, want) in [
            (Method::GET, "/v2/a/manifests/x", "GET manifest"),
            (Method::HEAD, "/v2/a/blobs/sha256:..", "HEAD blob"),
            (Method::POST, "/v2/a/blobs/uploads/", "POST upload"),
            (Method::PATCH, "/v2/a/blobs/uploads/u1", "PATCH upload"),
            (Method::GET, "/v2/a/tags/list", "GET tags"),
            (Method::GET, "/v2/_catalog", "GET catalog"),
            (Method::GET, "/v2/a/referrers/d", "GET referrers"),
            (Method::GET, "/v2/", "GET ping"),
            (Method::GET, "/token", "GET token"),
        ] {
            assert_eq!(op_label(&m, p), want, "{m} {p}");
        }
    }
}

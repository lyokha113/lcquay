//! Registry HTTP client with the §7.2 auth flow: registry/scoped hints,
//! Basic and Bearer challenges, token cache, realm trust.
//!
//! Credentials (Basic, Bearer, tokens) are only ever attached to requests for
//! the endpoint origin. A request for another origin (e.g. a cross-origin
//! `Link` cursor) is sent without credentials, and its challenges are never
//! answered. Redirects are followed by reqwest, which drops `Authorization`
//! when the origin changes; write requests never follow one.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use bytes::Bytes;
use reqwest::header::{ACCEPT, CONTENT_RANGE, CONTENT_TYPE, HeaderMap, WWW_AUTHENTICATE};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::sync::OnceCell;
use url::Url;

use super::next_link;
use super::{
    BODY_CAP, Challenge, CursorGuard, Gate, GatePermit, IDLE_TIMEOUT, TokenResponse, WRITE_TIMEOUT,
    build_http_client, build_stream_client, build_write_client, http_error, needs_renewal,
    parse_www_authenticate, read_capped, realm_trusted, token_expiry,
};
use crate::auth::{Credential, CredentialSource, Destination, Secret, with_credential_timeout};
use crate::domain::{
    Descriptor, Digest, HistoryEntry, ImageConfig, ManifestDocument, ManifestKind, Platform,
    Reference, RegistryError, RepoPath,
};
use crate::reqlog::RegLog;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpClass {
    Pull,
    Push,
    Delete,
}

/// (realm, service, scope)
type TokenKey = (Url, Option<String>, Option<String>);
/// Per-key async mutex: concurrent misses share one token request.
type TokenSlot = Arc<tokio::sync::Mutex<Option<(Secret<String>, OffsetDateTime)>>>;

/// A response whose body was read (capped at `BODY_CAP`) while the request's
/// gate permit was held; a 2xx in `Mode::Stream` is left unread in `stream`.
pub struct RawResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// Final URL after redirects; may carry a signed query string, never log it.
    pub url: Url,
    pub body: Bytes,
    stream: Option<BlobStream>,
}

/// The body of a write request, sent as is (and once more only to answer a
/// 401 challenge).
#[derive(Clone, Debug)]
pub struct WriteBody {
    pub bytes: Bytes,
    pub content_type: Option<String>,
    pub content_range: Option<String>,
}

/// An unread blob response; holds its gate permit until dropped.
pub struct BlobStream {
    resp: reqwest::Response,
    _permit: GatePermit,
}

impl BlobStream {
    /// `Content-Length`, if the registry sent one.
    #[allow(clippy::len_without_is_empty)] // a byte count for progress, not a collection
    pub fn len(&self) -> Option<u64> {
        self.resp.content_length()
    }

    /// The next piece of the body, `None` at the end. Fails with `Timeout`
    /// after `IDLE_TIMEOUT` without data.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, RegistryError> {
        self.resp.chunk().await.map_err(http_error)
    }
}

/// Which client sends a request and how its answer is read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// 30 s client, body capped at `BODY_CAP`.
    Capped,
    /// Idle-timeout client; a 2xx body is left unread.
    Stream,
    /// Non-redirecting client, `WRITE_TIMEOUT`, body capped.
    Write,
}

/// Status and path only: headers, body and query may hold secrets.
impl fmt::Debug for RawResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawResponse")
            .field("status", &self.status)
            .field("path", &self.url.path())
            .finish_non_exhaustive()
    }
}

/// A failed delete, by whether it may have reached the server.
#[derive(Clone, Debug, PartialEq)]
pub enum DeleteError {
    /// Failed before the DELETE was sent (credentials, probe, token, auth).
    NotSent(RegistryError),
    /// HTTP 405, or 400 with error code `UNSUPPORTED`.
    Unsupported,
    /// Any other status below 500: refused, not applied.
    Rejected(String),
    /// HTTP 5xx, or a transport error after dispatch: may have been applied.
    Uncertain(String),
}

pub struct OciClient {
    endpoint: Url,
    trusted_realms: Vec<String>,
    creds: Arc<dyn CredentialSource>,
    gate: Arc<Gate>,
    http: reqwest::Client,
    stream: reqwest::Client,
    write: reqwest::Client,
    /// Scoped hints. `None` repo = registry-level paths such as `_catalog`.
    hints: Mutex<HashMap<(Option<RepoPath>, OpClass), Challenge>>,
    registry_hint: OnceCell<Option<Challenge>>,
    tokens: Mutex<HashMap<TokenKey, TokenSlot>>,
    log: RegLog,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl OciClient {
    pub fn new(
        endpoint: Url,
        trusted_realms: Vec<String>,
        creds: Arc<dyn CredentialSource>,
        gate: Arc<Gate>,
    ) -> Self {
        OciClient {
            endpoint,
            trusted_realms,
            creds,
            gate,
            http: build_http_client(),
            stream: build_stream_client(IDLE_TIMEOUT),
            write: build_write_client(),
            hints: Mutex::default(),
            registry_hint: OnceCell::new(),
            tokens: Mutex::default(),
            log: RegLog::default(),
        }
    }

    pub fn with_log(mut self, log: RegLog) -> Self {
        self.log = log;
        self
    }

    /// `GET` with the pull auth flow; non-success statuses become errors.
    pub async fn get_raw(
        &self,
        path: &str,
        repo: Option<&RepoPath>,
        accept: &[&str],
    ) -> Result<RawResponse, RegistryError> {
        self.send(Method::GET, path, repo, OpClass::Pull, accept)
            .await
    }

    /// `path` is resolved against the endpoint (absolute URLs are taken as is).
    pub(crate) async fn send(
        &self,
        method: Method,
        path: &str,
        repo: Option<&RepoPath>,
        op: OpClass,
        accept: &[&str],
    ) -> Result<RawResponse, RegistryError> {
        let url = self.url(path)?;
        let resp = self
            .send_inner(&method, &url, repo, op, accept, None, Mode::Capped)
            .await
            .map_err(|(e, _)| e)?;
        check(resp, &method, &url)
    }

    fn url(&self, path: &str) -> Result<Url, RegistryError> {
        self.endpoint
            .join(path)
            .map_err(|_| RegistryError::Internal("invalid request path".into()))
    }

    /// The auth flow, any status returned as is. The error's bool: the request
    /// to `url` may have been sent (true only for errors of that request).
    /// A 401 is answered once, with the same `body`.
    #[allow(clippy::too_many_arguments)]
    async fn send_inner(
        &self,
        method: &Method,
        url: &Url,
        repo: Option<&RepoPath>,
        op: OpClass,
        accept: &[&str],
        body: Option<&WriteBody>,
        mode: Mode,
    ) -> Result<RawResponse, (RegistryError, bool)> {
        let sent = |e| (e, true);
        let not_sent = |e| (e, false);
        let none = Credential::Anonymous;
        if url.origin() != self.endpoint.origin() {
            return self
                .request(method, url, accept, &none, body, mode)
                .await
                .map_err(sent);
        }

        let cred = with_credential_timeout(&*self.creds, Destination::Http)
            .await
            .map_err(not_sent)?
            .credential;
        if let Credential::Bearer(_) = cred {
            return self
                .request(method, url, accept, &cred, body, mode)
                .await
                .map_err(sent);
        }

        let key = (repo.cloned(), op);
        let scoped = lock(&self.hints).get(&key).cloned();
        let auth = match (scoped, self.registry_hint().await.map_err(not_sent)?) {
            (Some(Challenge::Basic), _) | (None, Some(Challenge::Basic)) => basic_only(&cred),
            (
                Some(Challenge::Bearer {
                    realm,
                    service,
                    scope,
                }),
                _,
            ) => Credential::Bearer(
                self.token(realm, service, scope, &cred, None)
                    .await
                    .map_err(not_sent)?,
            ),
            // registry Bearer hint alone: go unauthenticated to learn the scope
            _ => Credential::Anonymous,
        };

        let resp = self
            .request(method, url, accept, &auth, body, mode)
            .await
            .map_err(sent)?;
        if resp.status != StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        let challenge = self
            .challenge(&resp)
            .ok_or((RegistryError::Unauthorized, false))?;
        lock(&self.hints).insert(key, challenge.clone());
        let retry = match challenge {
            Challenge::Basic => match (&auth, basic_only(&cred)) {
                (Credential::Basic { .. }, _) | (_, Credential::Anonymous) => {
                    return Err((RegistryError::Unauthorized, false));
                }
                (_, b) => b,
            },
            Challenge::Bearer {
                realm,
                service,
                scope,
            } => {
                let rejected = match &auth {
                    Credential::Bearer(t) => Some(t.expose().as_str()),
                    _ => None,
                };
                Credential::Bearer(
                    self.token(realm, service, scope, &cred, rejected)
                        .await
                        .map_err(not_sent)?,
                )
            }
        };
        self.request(method, url, accept, &retry, body, mode)
            .await
            .map_err(sent)
    }

    /// `DELETE /v2/<repo>/manifests/<digest>`, sent once (the only resend
    /// answers a 401 challenge).
    pub async fn delete_manifest(&self, repo: &RepoPath, d: &Digest) -> Result<(), DeleteError> {
        let url = self
            .url(&format!("/v2/{repo}/manifests/{d}"))
            .map_err(DeleteError::NotSent)?;
        let resp = self
            .send_inner(
                &Method::DELETE,
                &url,
                Some(repo),
                OpClass::Delete,
                &[],
                None,
                Mode::Capped,
            )
            .await
            .map_err(write_error)?;
        // e.g. a 303 turns the DELETE into a GET, whose 2xx says nothing
        if resp.url != url {
            return Err(DeleteError::Uncertain(
                "delete was redirected; outcome unknown".into(),
            ));
        }
        if resp.status.is_success() {
            return Ok(());
        }
        let errors = registry_errors(&resp.body);
        let s = resp.status.as_u16();
        if s == 405 || (s == 400 && errors.iter().any(|e| e.code == "UNSUPPORTED")) {
            return Err(DeleteError::Unsupported);
        }
        Err(status_error(s, &errors))
    }

    /// One write request (push scope, non-redirecting client), sent once; the
    /// only resend answers a 401 challenge with the same body. `target` is a
    /// path or an absolute URL (an upload `Location`, whose query is a secret).
    pub async fn write(
        &self,
        method: Method,
        target: &str,
        repo: &RepoPath,
        body: Option<WriteBody>,
    ) -> Result<RawResponse, DeleteError> {
        let url = self.url(target).map_err(DeleteError::NotSent)?;
        let resp = self
            .send_inner(
                &method,
                &url,
                Some(repo),
                OpClass::Push,
                &[],
                body.as_ref(),
                Mode::Write,
            )
            .await
            .map_err(write_error)?;
        let s = resp.status.as_u16();
        match s {
            _ if resp.status.is_success() => Ok(resp),
            300..=399 => Err(DeleteError::Rejected(format!(
                "HTTP {s}: redirect not followed"
            ))),
            _ => Err(status_error(s, &registry_errors(&resp.body))),
        }
    }

    /// Whether the blob exists (`HEAD`: 200 → true, 404 → false).
    pub async fn head_blob(&self, repo: &RepoPath, d: &Digest) -> Result<bool, RegistryError> {
        let path = format!("/v2/{repo}/blobs/{d}");
        match self
            .send(Method::HEAD, &path, Some(repo), OpClass::Pull, &[])
            .await
        {
            Ok(r) if r.status.is_success() => Ok(true),
            Ok(r) => Err(RegistryError::Provider(format!(
                "HTTP {} for HEAD {path}",
                r.status.as_u16()
            ))),
            Err(RegistryError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The manifest exactly as served, with the `Content-Type` media type.
    pub async fn manifest_bytes(
        &self,
        repo: &RepoPath,
        d: &Digest,
    ) -> Result<(Bytes, String), RegistryError> {
        let resp = self
            .get_raw(
                &format!("/v2/{repo}/manifests/{d}"),
                Some(repo),
                &MANIFEST_ACCEPT,
            )
            .await?;
        let media_type = content_type(&resp.headers).unwrap_or_default();
        Ok((resp.body, media_type))
    }

    /// The blob body, unread and uncapped, following redirects (credentials
    /// are dropped when the origin changes).
    pub async fn blob_stream(
        &self,
        repo: &RepoPath,
        d: &Digest,
    ) -> Result<BlobStream, RegistryError> {
        let url = self.url(&format!("/v2/{repo}/blobs/{d}"))?;
        let resp = self
            .send_inner(
                &Method::GET,
                &url,
                Some(repo),
                OpClass::Pull,
                &[],
                None,
                Mode::Stream,
            )
            .await
            .map_err(|(e, _)| e)?;
        let resp = check(resp, &Method::GET, &url)?;
        resp.stream.ok_or_else(|| {
            RegistryError::Provider(format!(
                "HTTP {} for GET {}",
                resp.status.as_u16(),
                url.path()
            ))
        })
    }

    pub async fn manifest(
        &self,
        repo: &RepoPath,
        r: &Reference,
    ) -> Result<ManifestDocument, RegistryError> {
        let resp = self
            .get_raw(
                &format!("/v2/{repo}/manifests/{}", ref_str(r)),
                Some(repo),
                &MANIFEST_ACCEPT,
            )
            .await?;
        parse_manifest(&resp.body, content_type(&resp.headers))
    }

    /// Digest and media type from a `HEAD` (`Docker-Content-Digest`).
    pub async fn head_manifest(
        &self,
        repo: &RepoPath,
        r: &Reference,
    ) -> Result<(Digest, String), RegistryError> {
        let resp = self
            .send(
                Method::HEAD,
                &format!("/v2/{repo}/manifests/{}", ref_str(r)),
                Some(repo),
                OpClass::Pull,
                &MANIFEST_ACCEPT,
            )
            .await?;
        let digest = resp
            .headers
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| Digest::parse(v).ok())
            .ok_or_else(|| {
                RegistryError::Provider("registry did not return Docker-Content-Digest".into())
            })?;
        Ok((digest, content_type(&resp.headers).unwrap_or_default()))
    }

    /// `None` when the descriptor is not an image config.
    pub async fn image_config(
        &self,
        repo: &RepoPath,
        desc: &Descriptor,
    ) -> Result<Option<ImageConfig>, RegistryError> {
        if !CONFIG_TYPES.contains(&desc.media_type.as_str()) {
            return Ok(None);
        }
        let resp = self
            .get_raw(
                &format!("/v2/{repo}/blobs/{}", desc.digest),
                Some(repo),
                &[],
            )
            .await?;
        parse_config(&resp.body).map(Some)
    }

    pub async fn tags(
        &self,
        repo: &RepoPath,
        progress: &(dyn Fn(usize) + Sync),
    ) -> Result<Vec<String>, RegistryError> {
        let done = AtomicUsize::new(0);
        let counted = |n| {
            done.store(n, Ordering::Relaxed);
            progress(n)
        };
        let first = self
            .tags_from(
                repo,
                format!("/v2/{repo}/tags/list?n={PAGE}"),
                true,
                &counted,
            )
            .await;
        // A registry that rejects or mishandles `n` on the very first request
        // gets one plain, unpaginated request (Link still followed).
        match first {
            Err(RegistryError::Provider(_)) if done.load(Ordering::Relaxed) == 0 => {
                match self
                    .tags_from(repo, format!("/v2/{repo}/tags/list"), false, progress)
                    .await
                {
                    Ok(t) => Ok(t),
                    Err(_) => first,
                }
            }
            r => r,
        }
    }

    /// `paginate`: without `Link`, a full page continues with `last`.
    async fn tags_from(
        &self,
        repo: &RepoPath,
        first: String,
        paginate: bool,
        progress: &(dyn Fn(usize) + Sync),
    ) -> Result<Vec<String>, RegistryError> {
        #[derive(Deserialize)]
        struct List {
            tags: Option<Vec<String>>,
        }
        let (mut all, mut seen) = (Vec::new(), HashSet::new());
        self.pages(first, Some(repo), &[], progress, |body| {
            let page = serde_json::from_slice::<List>(body)
                .map_err(|e| invalid("tag list", e))?
                .tags
                .unwrap_or_default();
            let (count, last) = (page.len(), page.last().cloned());
            let before = all.len();
            for t in page {
                if seen.insert(t.clone()) {
                    all.push(t);
                }
            }
            let new = all.len() - before;
            // No Link: a full page may have more; a page of nothing new means the
            // registry ignores `last`, so stop with what we have.
            let more = last
                .filter(|_| paginate && count == PAGE && new > 0)
                .map(|l| format!("/v2/{repo}/tags/list?n={PAGE}&last={}", quote(&l)));
            Ok((new, more))
        })
        .await?;
        Ok(all)
    }

    pub async fn catalog(
        &self,
        progress: &(dyn Fn(usize) + Sync),
    ) -> Result<Vec<RepoPath>, RegistryError> {
        #[derive(Deserialize)]
        struct List {
            repositories: Option<Vec<String>>,
        }
        let mut all = Vec::new();
        self.pages(
            format!("/v2/_catalog?n={PAGE}"),
            None,
            &[],
            progress,
            |body| {
                let page = serde_json::from_slice::<List>(body)
                    .map_err(|e| invalid("catalog", e))?
                    .repositories
                    .unwrap_or_default();
                let new = page.len();
                all.extend(page.into_iter().map(RepoPath));
                Ok((new, None))
            },
        )
        .await?;
        Ok(all)
    }

    /// `None` = unknown (listing failed other than 404).
    pub async fn referrers(
        &self,
        repo: &RepoPath,
        d: &Digest,
    ) -> Result<Option<Vec<Descriptor>>, RegistryError> {
        #[derive(Deserialize)]
        struct Index {
            manifests: Option<Vec<RawDescriptor>>,
        }
        let mut all = Vec::new();
        let listed = self
            .pages(
                format!("/v2/{repo}/referrers/{d}"),
                Some(repo),
                &["application/vnd.oci.image.index.v1+json"],
                &|_| {},
                |body| {
                    let page = serde_json::from_slice::<Index>(body)
                        .map_err(|e| invalid("referrers", e))?
                        .manifests
                        .unwrap_or_default();
                    let new = page.len();
                    all.extend(descriptors(page)?);
                    Ok((new, None))
                },
            )
            .await;
        match listed {
            Ok(()) => return Ok(Some(all)),
            Err(RegistryError::NotFound) => {}
            Err(_) => return Ok(None),
        }
        if d.algorithm() != "sha256" {
            return Ok(None);
        }
        let tag = Reference::Tag(format!("sha256-{}", d.hex()));
        match self.manifest(repo, &tag).await {
            Ok(ManifestDocument {
                kind: ManifestKind::Index { manifests },
                ..
            }) => Ok(Some(manifests)),
            Ok(_) => Ok(Some(vec![])),
            Err(RegistryError::NotFound) => Ok(Some(vec![])),
            Err(_) => Ok(None),
        }
    }

    /// Fetches pages from `first`, following `Link` (else the closure's next
    /// path). `page` returns (new items, next path without `Link`).
    async fn pages(
        &self,
        first: String,
        repo: Option<&RepoPath>,
        accept: &[&str],
        progress: &(dyn Fn(usize) + Sync),
        mut page: impl FnMut(&[u8]) -> Result<(usize, Option<String>), RegistryError>,
    ) -> Result<(), RegistryError> {
        let (mut path, mut guard, mut n) = (first, CursorGuard::default(), 0);
        loop {
            let resp = self.get_raw(&path, repo, accept).await?;
            let (new, fallback) = page(&resp.body)?;
            let link = next_link(&resp.headers, &resp.url).map(String::from);
            guard.check(&path, new, link.is_some())?;
            n += 1;
            progress(n);
            match link.or(fallback) {
                Some(next) => path = next,
                None => return Ok(()),
            }
        }
    }

    /// The single-flight `GET /v2/` probe; transport errors leave it unset so a
    /// later request probes again.
    async fn registry_hint(&self) -> Result<Option<Challenge>, RegistryError> {
        self.registry_hint
            .get_or_try_init(|| async {
                let url = self
                    .endpoint
                    .join("/v2/")
                    .map_err(|_| RegistryError::Internal("invalid endpoint".into()))?;
                let resp = self
                    .request(
                        &Method::GET,
                        &url,
                        &[],
                        &Credential::Anonymous,
                        None,
                        Mode::Capped,
                    )
                    .await?;
                Ok(if resp.status == StatusCode::UNAUTHORIZED {
                    self.challenge(&resp)
                } else {
                    None
                })
            })
            .await
            .cloned()
    }

    /// The challenge of a 401, only if it came from the endpoint origin.
    fn challenge(&self, resp: &RawResponse) -> Option<Challenge> {
        if resp.url.origin() != self.endpoint.origin() {
            return None;
        }
        resp.headers
            .get_all(WWW_AUTHENTICATE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(parse_www_authenticate)
    }

    /// Cached token for the key, fetched if missing, expiring, or equal to
    /// `rejected` (the token a 401 just refused). A fetched token is returned
    /// as is, even if already due for renewal, so its request uses it.
    async fn token(
        &self,
        realm: Url,
        service: Option<String>,
        scope: Option<String>,
        cred: &Credential,
        rejected: Option<&str>,
    ) -> Result<Secret<String>, RegistryError> {
        if !realm_trusted(&realm, &self.endpoint, &self.trusted_realms) {
            let host = realm.host_str().unwrap_or_default().to_owned();
            return Err(RegistryError::UntrustedRealm(host));
        }
        let mut url = realm.clone();
        for (k, v) in [("service", &service), ("scope", &scope)] {
            if let Some(v) = v {
                url.query_pairs_mut().append_pair(k, v);
            }
        }
        let slot = lock(&self.tokens)
            .entry((realm, service, scope))
            .or_default()
            .clone();
        let mut slot = slot.lock().await;
        if let Some((t, exp)) = &*slot
            && !needs_renewal(*exp, OffsetDateTime::now_utc())
            && Some(t.expose().as_str()) != rejected
        {
            return Ok(t.clone());
        }

        let received = OffsetDateTime::now_utc();
        let resp = self
            .request(
                &Method::GET,
                &url,
                &[],
                &basic_only(cred),
                None,
                Mode::Capped,
            )
            .await?;
        let body = check(resp, &Method::GET, &url)?.body;
        // Never echo the body or serde's message: either may hold the token.
        let bad = || RegistryError::Provider("invalid token response".into());
        let tr: TokenResponse = serde_json::from_slice(&body).map_err(|_| bad())?;
        let exp = token_expiry(&tr, received);
        let token = Secret(
            tr.token
                .or(tr.access_token)
                .filter(|t| !t.is_empty())
                .ok_or_else(bad)?,
        );
        *slot = Some((token.clone(), exp));
        Ok(token)
    }

    /// One HTTP request, response body included (except a streamed 2xx),
    /// under one gate permit; logged once its outcome is known.
    async fn request(
        &self,
        method: &Method,
        url: &Url,
        accept: &[&str],
        auth: &Credential,
        body: Option<&WriteBody>,
        mode: Mode,
    ) -> Result<RawResponse, RegistryError> {
        let client = match mode {
            Mode::Capped => &self.http,
            Mode::Stream => &self.stream,
            Mode::Write => &self.write,
        };
        let mut rb = client.request(method.clone(), url.clone());
        if mode == Mode::Write {
            rb = rb.timeout(WRITE_TIMEOUT);
        }
        if let Some(b) = body {
            rb = rb.body(b.bytes.clone());
            if let Some(t) = &b.content_type {
                rb = rb.header(CONTENT_TYPE, t);
            }
            if let Some(r) = &b.content_range {
                rb = rb.header(CONTENT_RANGE, r);
            }
        }
        if !accept.is_empty() {
            rb = rb.header(ACCEPT, accept.join(", "));
        }
        rb = match auth {
            Credential::Anonymous => rb,
            Credential::Basic { username, password } => {
                rb.basic_auth(username, Some(password.expose()))
            }
            Credential::Bearer(t) => rb.bearer_auth(t.expose()),
        };
        let permit = self.gate.acquire().await;
        let start = Instant::now();
        let res = async {
            let resp = rb.send().await.map_err(|e| {
                if e.is_timeout() {
                    RegistryError::Timeout("request".into())
                } else {
                    http_error(e)
                }
            })?;
            let (status, headers, url) =
                (resp.status(), resp.headers().clone(), resp.url().clone());
            let (body, stream) = if mode == Mode::Stream && status.is_success() {
                let stream = BlobStream {
                    resp,
                    _permit: permit,
                };
                (Bytes::new(), Some(stream))
            } else {
                (read_capped(resp, BODY_CAP).await?, None)
            };
            Ok(RawResponse {
                status,
                headers,
                url,
                body,
                stream,
            })
        }
        .await;
        tracing::debug!(
            host = self.endpoint.host_str().unwrap_or_default(),
            method = %method,
            path = url.path(),
            status = res.as_ref().map_or(0, |r| r.status.as_u16()),
            ms = start.elapsed().as_millis() as u64,
        );
        let status = res.as_ref().map(|r| r.status.as_u16());
        let bytes = res.as_ref().ok().and_then(|r| match (mode, &r.stream) {
            (Mode::Write, _) => Some(body.map_or(0, |b| b.bytes.len() as u64)),
            (_, Some(s)) => s.len(),
            _ => Some(r.body.len() as u64),
        });
        self.log
            .http(method, url, &self.endpoint, status, start, bytes);
        res
    }
}

/// Delete error texts are cut to this many bytes.
const ERROR_TEXT_CAP: usize = 200;

pub(crate) fn cut(mut s: String) -> String {
    s.truncate(s.floor_char_boundary(ERROR_TEXT_CAP));
    s
}

/// The `errors` array of a registry error body (empty if unparsable).
fn registry_errors(body: &[u8]) -> Vec<ErrorEntry> {
    #[derive(Deserialize)]
    struct Body {
        errors: Vec<ErrorEntry>,
    }
    serde_json::from_slice::<Body>(body)
        .map(|b| b.errors)
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct ErrorEntry {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

/// `HTTP s[: code: message]`, cut; 5xx may have been applied.
fn status_error(s: u16, errors: &[ErrorEntry]) -> DeleteError {
    let text = cut(match errors.first() {
        Some(e) => format!("HTTP {s}: {}: {}", e.code, e.message),
        None => format!("HTTP {s}"),
    });
    match s {
        500.. => DeleteError::Uncertain(text),
        _ => DeleteError::Rejected(text),
    }
}

/// A `send_inner` failure: after dispatch it may have been applied.
fn write_error((e, sent): (RegistryError, bool)) -> DeleteError {
    match sent {
        true => DeleteError::Uncertain(cut(e.to_string())),
        false => DeleteError::NotSent(e),
    }
}

/// Page size for `tags` and `catalog`.
const PAGE: usize = 1000;

const MANIFEST_ACCEPT: [&str; 4] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

const CONFIG_TYPES: [&str; 2] = [
    "application/vnd.oci.image.config.v1+json",
    "application/vnd.docker.container.image.v1+json",
];

fn ref_str(r: &Reference) -> String {
    match r {
        Reference::Tag(t) => t.clone(),
        Reference::Digest(d) => d.to_string(),
    }
}

/// Content-Type without parameters.
fn content_type(h: &HeaderMap) -> Option<String> {
    let v = h.get(CONTENT_TYPE)?.to_str().ok()?;
    Some(v.split(';').next().unwrap_or(v).trim().to_owned())
}

/// Never echo serde's message: type errors quote the offending value, which
/// may be a secret. Category and position only.
fn invalid(what: &str, e: serde_json::Error) -> RegistryError {
    RegistryError::Provider(format!(
        "invalid {what}: {:?} error at line {} column {}",
        e.classify(),
        e.line(),
        e.column()
    ))
}

fn utf8(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

fn rfc3339(s: Option<String>) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(&s?, &time::format_description::well_known::Rfc3339).ok()
}

#[derive(Deserialize)]
struct RawPlatform {
    os: String,
    architecture: String,
    variant: Option<String>,
}

impl From<RawPlatform> for Platform {
    fn from(p: RawPlatform) -> Self {
        Platform {
            os: p.os,
            architecture: p.architecture,
            variant: p.variant.filter(|v| !v.is_empty()),
        }
    }
}

#[derive(Deserialize)]
struct RawDescriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    platform: Option<RawPlatform>,
    #[serde(rename = "artifactType")]
    artifact_type: Option<String>,
    annotations: Option<BTreeMap<String, String>>,
}

impl RawDescriptor {
    fn convert(self) -> Result<Descriptor, RegistryError> {
        Ok(Descriptor {
            media_type: self.media_type,
            digest: Digest::parse(&self.digest)
                .map_err(|_| RegistryError::Provider("invalid manifest: bad digest".into()))?,
            size: self.size,
            platform: self.platform.map(Into::into),
            artifact_type: self.artifact_type,
            annotations: self.annotations.unwrap_or_default(),
        })
    }
}

fn descriptors(v: Vec<RawDescriptor>) -> Result<Vec<Descriptor>, RegistryError> {
    v.into_iter().map(RawDescriptor::convert).collect()
}

#[derive(Deserialize)]
struct RawManifest {
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
    config: Option<RawDescriptor>,
    layers: Option<Vec<RawDescriptor>>,
    manifests: Option<Vec<RawDescriptor>>,
    subject: Option<RawDescriptor>,
    #[serde(rename = "artifactType")]
    artifact_type: Option<String>,
    annotations: Option<BTreeMap<String, String>>,
}

pub(crate) fn parse_manifest(
    body: &[u8],
    header_type: Option<String>,
) -> Result<ManifestDocument, RegistryError> {
    let m: RawManifest = serde_json::from_slice(body).map_err(|e| invalid("manifest", e))?;
    let bad = |s: &str| RegistryError::Provider(format!("invalid manifest: {s}"));
    let media_type = m
        .media_type
        .or(header_type)
        .ok_or_else(|| bad("no media type"))?;
    let kind = match (m.manifests, m.config) {
        (Some(manifests), _) => ManifestKind::Index {
            manifests: descriptors(manifests)?,
        },
        (None, Some(config)) => ManifestKind::Image {
            config: config.convert()?,
            layers: descriptors(m.layers.unwrap_or_default())?,
        },
        _ => return Err(bad("neither config nor manifests")),
    };
    Ok(ManifestDocument {
        media_type,
        raw: utf8(body),
        kind,
        subject: m.subject.map(RawDescriptor::convert).transpose()?,
        artifact_type: m.artifact_type,
        annotations: m.annotations.unwrap_or_default(),
    })
}

#[derive(Deserialize)]
struct RawConfig {
    created: Option<String>,
    os: Option<String>,
    architecture: Option<String>,
    variant: Option<String>,
    config: Option<RawRuntime>,
    history: Option<Vec<RawHistory>>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawRuntime {
    entrypoint: Option<Vec<String>>,
    cmd: Option<Vec<String>>,
    working_dir: Option<String>,
    user: Option<String>,
    exposed_ports: Option<BTreeMap<String, serde_json::Value>>,
    volumes: Option<BTreeMap<String, serde_json::Value>>,
    labels: Option<BTreeMap<String, String>>,
    env: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawHistory {
    created_by: Option<String>,
    created: Option<String>,
    empty_layer: Option<bool>,
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.is_empty())
}

fn parse_config(body: &[u8]) -> Result<ImageConfig, RegistryError> {
    let c: RawConfig = serde_json::from_slice(body).map_err(|e| invalid("image config", e))?;
    let rt = c.config.unwrap_or_default();
    let keys = |m: Option<BTreeMap<String, serde_json::Value>>| {
        m.map(|m| m.into_keys().collect()).unwrap_or_default()
    };
    Ok(ImageConfig {
        raw: utf8(body),
        created: rfc3339(c.created),
        platform: match (c.os, c.architecture) {
            (Some(os), Some(architecture)) => Some(Platform {
                os,
                architecture,
                variant: non_empty(c.variant),
            }),
            _ => None,
        },
        entrypoint: rt.entrypoint.unwrap_or_default(),
        cmd: rt.cmd.unwrap_or_default(),
        working_dir: non_empty(rt.working_dir),
        user: non_empty(rt.user),
        exposed_ports: keys(rt.exposed_ports),
        volumes: keys(rt.volumes),
        labels: rt.labels.unwrap_or_default(),
        env: rt
            .env
            .unwrap_or_default()
            .into_iter()
            .map(|e| match e.split_once('=') {
                Some((k, v)) => (k.to_owned(), v.to_owned()),
                None => (e, String::new()),
            })
            .collect(),
        history: c
            .history
            .unwrap_or_default()
            .into_iter()
            .map(|h| HistoryEntry {
                created_by: h.created_by.unwrap_or_default(),
                created: rfc3339(h.created),
                empty_layer: h.empty_layer.unwrap_or(false),
            })
            .collect(),
    })
}

fn quote(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// The credential if it is Basic, else none.
fn basic_only(cred: &Credential) -> Credential {
    match cred {
        Credential::Basic { .. } => cred.clone(),
        _ => Credential::Anonymous,
    }
}

fn check(resp: RawResponse, method: &Method, url: &Url) -> Result<RawResponse, RegistryError> {
    let status = resp.status;
    match status.as_u16() {
        _ if status.is_success() || status.is_redirection() => Ok(resp),
        401 => Err(RegistryError::Unauthorized),
        403 => Err(RegistryError::Forbidden(format!("{method} {}", url.path()))),
        404 => Err(RegistryError::NotFound),
        429 => Err(RegistryError::RateLimited),
        s => Err(RegistryError::Provider(format!(
            "HTTP {s} for {method} {}",
            url.path()
        ))),
    }
}

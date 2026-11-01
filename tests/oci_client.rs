use std::io;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lcquay::auth::{Credential, CredentialSource, Destination, Issued, Secret};
use lcquay::domain::{RegistryError, RepoPath};
use lcquay::registry::{Gate, OciClient, RawResponse};
use url::Url;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

struct Fixed(Credential);

#[async_trait]
impl CredentialSource for Fixed {
    async fn credential(&self, _dest: Destination) -> Result<Issued, RegistryError> {
        Ok(Issued {
            credential: self.0.clone(),
            expires_at: None,
        })
    }
    fn can_login(&self) -> bool {
        false
    }
}

/// base64("user:pass")
const BASIC: &str = "Basic dXNlcjpwYXNz";

fn basic() -> Credential {
    Credential::Basic {
        username: "user".into(),
        password: Secret("pass".into()),
    }
}

fn client(server: &MockServer, cred: Credential) -> OciClient {
    OciClient::new(
        Url::parse(&server.uri()).unwrap(),
        vec![],
        Arc::new(Fixed(cred)),
        Arc::new(Gate::new(8)),
    )
}

async fn get(c: &OciClient, p: &str, repo: &str) -> Result<RawResponse, RegistryError> {
    c.get_raw(p, Some(&RepoPath(repo.into())), &[]).await
}

fn auth_of(r: &Request) -> Option<String> {
    r.headers
        .get("authorization")
        .map(|v| v.to_str().unwrap().to_owned())
}

/// Requests received on `p`.
async fn received(server: &MockServer, p: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == p)
        .collect()
}

fn challenge(v: &str) -> ResponseTemplate {
    ResponseTemplate::new(401).insert_header("www-authenticate", v)
}

fn bearer_challenge(server: &MockServer, extra: &str) -> String {
    format!(r#"Bearer realm="{}/token"{extra}"#, server.uri())
}

async fn probe(server: &MockServer, resp: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(resp)
        .expect(1)
        .mount(server)
        .await;
}

const MANIFEST: &str = "/v2/r/manifests/t";

#[tokio::test]
async fn probe_200_then_anonymous_get() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(path(MANIFEST))
        .respond_with(ResponseTemplate::new(200).set_body_string("m"))
        .expect(2)
        .mount(&s)
        .await;
    let c = client(&s, basic());
    for _ in 0..2 {
        let r = get(&c, MANIFEST, "r").await.unwrap();
        assert_eq!(&r.body[..], b"m");
    }
    assert!(
        received(&s, MANIFEST)
            .await
            .iter()
            .all(|r| auth_of(r).is_none())
    );
}

#[tokio::test]
async fn basic_challenge_then_upfront() {
    let s = MockServer::start().await;
    probe(&s, challenge(r#"Basic realm="x""#)).await;
    Mock::given(path(MANIFEST))
        .and(header("authorization", BASIC))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&s)
        .await;
    Mock::given(path(MANIFEST))
        .respond_with(challenge("Basic"))
        .expect(0)
        .mount(&s)
        .await;
    let c = client(&s, basic());
    get(&c, MANIFEST, "r").await.unwrap();
    get(&c, MANIFEST, "r").await.unwrap();
}

#[tokio::test]
async fn bearer_challenge_anonymous_token() {
    let s = MockServer::start().await;
    probe(&s, challenge(&bearer_challenge(&s, r#",service="reg""#))).await;
    Mock::given(path(MANIFEST))
        .and(header("authorization", "Bearer t"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&s)
        .await;
    Mock::given(path(MANIFEST))
        .respond_with(challenge(&bearer_challenge(
            &s,
            r#",service="reg",scope="repository:r:pull""#,
        )))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path("/token"))
        .and(query_param("service", "reg"))
        .and(query_param("scope", "repository:r:pull"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"token":"t","expires_in":300}"#),
        )
        .expect(1)
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    get(&c, MANIFEST, "r").await.unwrap();
    get(&c, MANIFEST, "r").await.unwrap();
    assert!(auth_of(&received(&s, "/token").await[0]).is_none());
}

#[tokio::test]
async fn bearer_token_with_basic_creds_and_access_token_field() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(path(MANIFEST))
        .and(header("authorization", "Bearer t2"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path(MANIFEST))
        .respond_with(challenge(&bearer_challenge(
            &s,
            r#",scope="repository:r:pull""#,
        )))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path("/token"))
        .and(header("authorization", BASIC))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"access_token":"t2"}"#))
        .expect(1)
        .mount(&s)
        .await;
    get(&client(&s, basic()), MANIFEST, "r").await.unwrap();
}

#[tokio::test]
async fn scope_used_verbatim() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(path(MANIFEST))
        .and(header("authorization", "Bearer t"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    Mock::given(path(MANIFEST))
        .respond_with(challenge(&bearer_challenge(&s, r#",scope="aws""#)))
        .mount(&s)
        .await;
    Mock::given(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"t"}"#))
        .expect(1)
        .mount(&s)
        .await;
    get(&client(&s, Credential::Anonymous), MANIFEST, "r")
        .await
        .unwrap();
    let tok = &received(&s, "/token").await[0];
    assert_eq!(tok.url.query(), Some("scope=aws"));
}

#[tokio::test]
async fn scoped_hint_overrides_registry_hint() {
    let s = MockServer::start().await;
    probe(&s, challenge("Basic")).await;
    Mock::given(path(MANIFEST))
        .and(header("authorization", "Bearer t"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&s)
        .await;
    // registry hint says Basic; the repo answers Basic with a Bearer challenge
    Mock::given(path(MANIFEST))
        .and(header("authorization", BASIC))
        .respond_with(challenge(&bearer_challenge(
            &s,
            r#",scope="repository:r:pull""#,
        )))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"t"}"#))
        .expect(1)
        .mount(&s)
        .await;
    let c = client(&s, basic());
    get(&c, MANIFEST, "r").await.unwrap();
    get(&c, MANIFEST, "r").await.unwrap();
}

#[tokio::test]
async fn basic_on_repo_a_not_applied_to_repo_b() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(path("/v2/a/manifests/t"))
        .and(header("authorization", BASIC))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    Mock::given(path("/v2/a/manifests/t"))
        .respond_with(challenge("Basic"))
        .mount(&s)
        .await;
    Mock::given(path("/v2/b/manifests/t"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    let c = client(&s, basic());
    get(&c, "/v2/a/manifests/t", "a").await.unwrap();
    get(&c, "/v2/b/manifests/t", "b").await.unwrap();
    let b = received(&s, "/v2/b/manifests/t").await;
    assert_eq!(b.len(), 1);
    assert!(auth_of(&b[0]).is_none());
}

#[tokio::test]
async fn no_www_authenticate_is_unauthorized_no_retry() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(path(MANIFEST))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&s)
        .await;
    let r = get(&client(&s, basic()), MANIFEST, "r").await;
    assert_eq!(r.unwrap_err(), RegistryError::Unauthorized);
}

#[tokio::test]
async fn second_401_is_unauthorized() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(path(MANIFEST))
        .respond_with(challenge("Basic"))
        .expect(2)
        .mount(&s)
        .await;
    let r = get(&client(&s, basic()), MANIFEST, "r").await;
    assert_eq!(r.unwrap_err(), RegistryError::Unauthorized);
}

#[tokio::test]
async fn direct_bearer_bypasses_probe() {
    let s = MockServer::start().await;
    probe_never(&s).await;
    Mock::given(path(MANIFEST))
        .and(header("authorization", "Bearer x"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path("/v2/r/manifests/denied"))
        .respond_with(challenge("Basic"))
        .expect(1)
        .mount(&s)
        .await;
    let c = client(&s, Credential::Bearer(Secret("x".into())));
    get(&c, MANIFEST, "r").await.unwrap();
    let r = get(&c, "/v2/r/manifests/denied", "r").await;
    assert_eq!(r.unwrap_err(), RegistryError::Unauthorized);
}

async fn probe_never(s: &MockServer) {
    Mock::given(path("/v2/"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(s)
        .await;
}

#[tokio::test]
async fn untrusted_realm_rejected_no_creds_sent() {
    let s = MockServer::start().await;
    let realm = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    // `localhost` is not the endpoint host (127.0.0.1) and not trusted
    let port = realm.address().port();
    Mock::given(path(MANIFEST))
        .respond_with(challenge(&format!(
            r#"Bearer realm="http://localhost:{port}/token",scope="repository:r:pull""#
        )))
        .expect(1)
        .mount(&s)
        .await;
    let r = get(&client(&s, basic()), MANIFEST, "r").await;
    assert!(matches!(r, Err(RegistryError::UntrustedRealm(h)) if h == "localhost"));
    assert!(realm.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn redirect_cross_origin_drops_authorization() {
    let s = MockServer::start().await;
    let cdn = MockServer::start().await;
    probe(&s, challenge("Basic")).await;
    Mock::given(path("/v2/r/blobs/b"))
        .and(header("authorization", BASIC))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/blob?sig=secret", cdn.uri())),
        )
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path("/blob"))
        .respond_with(ResponseTemplate::new(200).set_body_string("data"))
        .expect(1)
        .mount(&cdn)
        .await;
    let r = get(&client(&s, basic()), "/v2/r/blobs/b", "r")
        .await
        .unwrap();
    assert_eq!(&r.body[..], b"data");
    assert!(auth_of(&received(&cdn, "/blob").await[0]).is_none());
}

#[tokio::test]
async fn challenge_after_cross_origin_redirect_not_answered() {
    let s = MockServer::start().await;
    let cdn = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(path("/v2/r/blobs/b"))
        .respond_with(
            ResponseTemplate::new(307).insert_header("location", format!("{}/blob", cdn.uri())),
        )
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path("/blob"))
        .respond_with(challenge("Basic"))
        .expect(1)
        .mount(&cdn)
        .await;
    let r = get(&client(&s, basic()), "/v2/r/blobs/b", "r").await;
    assert_eq!(r.unwrap_err(), RegistryError::Unauthorized);
}

#[tokio::test]
async fn cross_origin_path_sent_without_credentials_and_challenge_ignored() {
    let s = MockServer::start().await;
    let other = MockServer::start().await;
    probe_never(&s).await;
    Mock::given(path("/v2/_catalog"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&other)
        .await;
    Mock::given(path("/v2/r/tags/list"))
        .respond_with(challenge("Basic"))
        .expect(1)
        .mount(&other)
        .await;
    let c = client(&s, basic());
    // e.g. a `Link: rel="next"` pointing at another origin
    c.get_raw(&format!("{}/v2/_catalog?last=a", other.uri()), None, &[])
        .await
        .unwrap();
    let r = get(&c, &format!("{}/v2/r/tags/list", other.uri()), "r").await;
    assert_eq!(r.unwrap_err(), RegistryError::Unauthorized);
    let all = other.received_requests().await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|r| auth_of(r).is_none()));
}

#[tokio::test]
async fn short_lived_token_used_then_renewed() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(path(MANIFEST))
        .and(header("authorization", "Bearer t"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&s)
        .await;
    Mock::given(path(MANIFEST))
        .respond_with(challenge(&bearer_challenge(
            &s,
            r#",scope="repository:r:pull""#,
        )))
        .expect(1)
        .mount(&s)
        .await;
    // expires_in < 10 s: needs renewal at once, but is still used for its request
    Mock::given(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"t","expires_in":5}"#))
        .expect(2)
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    get(&c, MANIFEST, "r").await.unwrap();
    get(&c, MANIFEST, "r").await.unwrap();
}

#[tokio::test]
async fn concurrent_token_misses_share_one_request() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(path(MANIFEST))
        .and(header("authorization", "Bearer t"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    Mock::given(path(MANIFEST))
        .respond_with(challenge(&bearer_challenge(
            &s,
            r#",scope="repository:r:pull""#,
        )))
        .mount(&s)
        .await;
    Mock::given(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"token":"t"}"#)
                .set_delay(std::time::Duration::from_millis(100)),
        )
        .expect(1)
        .mount(&s)
        .await;
    let c = Arc::new(client(&s, Credential::Anonymous));
    // spawned: also proves the request future is `Send`
    let tasks: Vec<_> = (0..2)
        .map(|_| {
            let c = c.clone();
            tokio::spawn(async move { get(&c, MANIFEST, "r").await.map(|_| ()) })
        })
        .collect();
    for t in tasks {
        t.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn status_mapping() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    for (p, code) in [("/v2/r/a", 403), ("/v2/r/b", 404), ("/v2/r/c", 429)] {
        Mock::given(path(p))
            .respond_with(ResponseTemplate::new(code))
            .mount(&s)
            .await;
    }
    let c = client(&s, Credential::Anonymous);
    assert!(matches!(
        get(&c, "/v2/r/a", "r").await,
        Err(RegistryError::Forbidden(_))
    ));
    assert_eq!(
        get(&c, "/v2/r/b", "r").await.unwrap_err(),
        RegistryError::NotFound
    );
    assert_eq!(
        get(&c, "/v2/r/c", "r").await.unwrap_err(),
        RegistryError::RateLimited
    );
}

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn logs_contain_no_secrets() {
    let buf = Buf::default();
    let w = buf.clone();
    let sub = tracing_subscriber::fmt()
        .with_env_filter("lcquay=debug")
        .with_writer(move || w.clone())
        .with_ansi(false)
        .finish();
    // Global, not `set_default`: a thread-local subscriber misses callsites
    // whose interest other test threads cached first. Only this test sets one.
    tracing::subscriber::set_global_default(sub).unwrap();

    let s = MockServer::start().await;
    probe(&s, challenge("Basic")).await;
    Mock::given(path(MANIFEST))
        .and(header("authorization", BASIC))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    Mock::given(path("/v2/b/manifests/t"))
        .and(header("authorization", "Bearer s3cr3ttok"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    Mock::given(path("/v2/b/manifests/t"))
        .respond_with(challenge(&bearer_challenge(
            &s,
            r#",service="reg",scope="repository:b:pull""#,
        )))
        .mount(&s)
        .await;
    Mock::given(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"s3cr3ttok"}"#))
        .mount(&s)
        .await;
    let c = client(&s, basic());
    c.get_raw(&format!("{MANIFEST}?q=1"), Some(&RepoPath("r".into())), &[])
        .await
        .unwrap();
    get(&c, "/v2/b/manifests/t", "b").await.unwrap();

    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("/v2/b/manifests/t") && log.contains("/token"),
        "{log}"
    );
    for bad in ["pass", "dXNlcjpwYXNz", "s3cr3ttok", "?", "scope=", "q=1"] {
        assert!(!log.contains(bad), "log contains {bad:?}:\n{log}");
    }
}

/// Gate of 1: a second request may start only once the first one's slow body
/// has been fully read. (wiremock's `set_delay` delays headers too, so this
/// uses a raw server that sends headers at once and the body later.)
#[tokio::test]
async fn slow_body_keeps_the_permit() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let events = Arc::new(Mutex::new(Vec::<&str>::new()));
    let ev = events.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let ev = ev.clone();
            tokio::spawn(async move {
                let mut req = Vec::new();
                let mut chunk = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = sock.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    req.extend_from_slice(&chunk[..n]);
                }
                let slow = req.starts_with(b"GET /slow ");
                if !slow {
                    ev.lock().unwrap().push("fast request");
                }
                sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
                if slow {
                    sock.flush().await.unwrap();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    ev.lock().unwrap().push("slow body done");
                }
                sock.write_all(b"body").await.unwrap();
            });
        }
    });

    let c = Arc::new(OciClient::new(
        Url::parse(&format!("http://{addr}")).unwrap(),
        vec![],
        Arc::new(Fixed(Credential::Bearer(Secret("x".into())))),
        Arc::new(Gate::new(1)),
    ));
    let c2 = c.clone();
    let slow = tokio::spawn(async move { c2.get_raw("/slow", None, &[]).await.map(|r| r.body) });
    tokio::time::sleep(Duration::from_millis(100)).await; // slow headers are in
    let fast = c.get_raw("/fast", None, &[]).await.unwrap();
    assert_eq!(&slow.await.unwrap().unwrap()[..], b"body");
    assert_eq!(&fast.body[..], b"body");
    assert_eq!(*events.lock().unwrap(), ["slow body done", "fast request"]);
}

// ---- content and listings ----

use lcquay::domain::{Descriptor, Digest, ManifestKind, Reference};
use serde_json::json;

fn dg(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

fn repo(s: &str) -> RepoPath {
    RepoPath(s.into())
}

fn json_resp(v: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(v.to_string())
}

fn no_progress() -> impl Fn(usize) + Sync {
    |_| {}
}

fn tags_body(names: impl IntoIterator<Item = String>) -> serde_json::Value {
    json!({"name": "r", "tags": names.into_iter().collect::<Vec<_>>()})
}

#[tokio::test]
async fn manifest_image_and_index_parse() {
    let s = MockServer::start().await;
    Mock::given(path("/v2/r/manifests/img"))
        .and(|r: &Request| {
            r.headers.get("accept").unwrap().to_str().unwrap()
                == "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json"
        })
        .respond_with(json_resp(json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": "application/vnd.example+type",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": dg('c'), "size": 10},
            "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": dg('1'), "size": 99}],
            "subject": {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": dg('5'), "size": 1},
            "annotations": {"k": "v"}
        })))
        .mount(&s)
        .await;
    // no body mediaType: falls back to Content-Type
    Mock::given(path("/v2/r/manifests/idx"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                json!({"manifests": [{
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": dg('7'), "size": 5,
                    "platform": {"os": "linux", "architecture": "arm64", "variant": "v8"}
                }]})
                .to_string(),
                "application/vnd.oci.image.index.v1+json; charset=utf-8",
            ),
        )
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);

    let m = c
        .manifest(&repo("r"), &Reference::Tag("img".into()))
        .await
        .unwrap();
    assert_eq!(m.media_type, "application/vnd.oci.image.manifest.v1+json");
    assert_eq!(
        m.artifact_type.as_deref(),
        Some("application/vnd.example+type")
    );
    assert_eq!(m.annotations["k"], "v");
    assert_eq!(m.subject.unwrap().digest, Digest::parse(&dg('5')).unwrap());
    assert!(m.raw.contains("schemaVersion"));
    let ManifestKind::Image { config, layers } = m.kind else {
        panic!("expected image")
    };
    assert_eq!(config.size, 10);
    assert_eq!(layers[0].size, 99);

    let m = c
        .manifest(&repo("r"), &Reference::Tag("idx".into()))
        .await
        .unwrap();
    assert_eq!(m.media_type, "application/vnd.oci.image.index.v1+json");
    let ManifestKind::Index { manifests } = m.kind else {
        panic!("expected index")
    };
    assert_eq!(
        manifests[0].platform.as_ref().unwrap().to_string(),
        "linux/arm64/v8"
    );

    // invalid JSON: short error, no body
    Mock::given(path("/v2/r/manifests/bad"))
        .respond_with(ResponseTemplate::new(200).set_body_string("SECRETBODY{"))
        .mount(&s)
        .await;
    let e = c
        .manifest(&repo("r"), &Reference::Tag("bad".into()))
        .await
        .unwrap_err();
    assert!(matches!(&e, RegistryError::Provider(m) if m.starts_with("invalid manifest")));
    assert!(!e.to_string().contains("SECRETBODY"));
}

fn desc(media_type: &str) -> Descriptor {
    Descriptor {
        media_type: media_type.into(),
        digest: Digest::parse(&dg('c')).unwrap(),
        size: 1,
        platform: None,
        artifact_type: None,
        annotations: Default::default(),
    }
}

#[tokio::test]
async fn non_image_config_is_none() {
    let s = MockServer::start().await;
    let c = client(&s, Credential::Anonymous);
    let d = desc("application/vnd.cncf.helm.config.v1+json");
    assert_eq!(c.image_config(&repo("r"), &d).await.unwrap(), None);
    assert!(s.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn config_parses_env_labels_history() {
    let s = MockServer::start().await;
    Mock::given(path(format!("/v2/r/blobs/{}", dg('c'))))
        .respond_with(json_resp(json!({
            "created": "2026-01-02T03:04:05Z",
            "os": "linux", "architecture": "amd64",
            "config": {
                "Entrypoint": ["/app"], "Cmd": ["--x"], "WorkingDir": "/w", "User": "1000",
                "ExposedPorts": {"8080/tcp": {}}, "Volumes": {"/data": {}},
                "Labels": {"a": "b"}, "Env": ["A=1", "SECRET_KEY=x=y", "NOEQ"]
            },
            "history": [
                {"created_by": "ADD . /", "created": "2026-01-02T03:04:05Z"},
                {"created_by": "ENV A=1", "empty_layer": true},
                {"created_by": "CMD"}
            ]
        })))
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    let d = desc("application/vnd.docker.container.image.v1+json");
    let cfg = c.image_config(&repo("r"), &d).await.unwrap().unwrap();
    assert_eq!(cfg.platform.unwrap().to_string(), "linux/amd64");
    assert_eq!(cfg.created.unwrap().year(), 2026);
    assert_eq!(cfg.entrypoint, ["/app"]);
    assert_eq!(cfg.cmd, ["--x"]);
    assert_eq!(cfg.working_dir.as_deref(), Some("/w"));
    assert_eq!(cfg.user.as_deref(), Some("1000"));
    assert_eq!(cfg.exposed_ports, ["8080/tcp"]);
    assert_eq!(cfg.volumes, ["/data"]);
    assert_eq!(cfg.labels["a"], "b");
    assert_eq!(
        cfg.env,
        [
            ("A".to_owned(), "1".to_owned()),
            ("SECRET_KEY".to_owned(), "x=y".to_owned()),
            ("NOEQ".to_owned(), String::new()),
        ]
    );
    assert_eq!(cfg.history.len(), 3);
    assert_eq!(cfg.history.iter().filter(|h| h.empty_layer).count(), 1);
    assert!(cfg.history[0].created.is_some() && cfg.history[1].created.is_none());
    assert!(cfg.raw.contains("SECRET_KEY"));
}

#[tokio::test]
async fn tags_follow_link() {
    let s = MockServer::start().await;
    Mock::given(path("/v2/r/tags/list"))
        .and(query_param("n", "1000"))
        .and(|r: &Request| !r.url.query_pairs().any(|(k, _)| k == "last"))
        .respond_with(
            json_resp(tags_body(["a".into(), "b".into()]))
                .insert_header("link", r#"</v2/r/tags/list?n=1000&last=b&x=1>; rel="next""#),
        )
        .mount(&s)
        .await;
    Mock::given(path("/v2/r/tags/list"))
        .and(query_param("x", "1"))
        .respond_with(json_resp(tags_body(["c".into()])))
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    let pages = Mutex::new(vec![]);
    let t = c
        .tags(&repo("r"), &|n| pages.lock().unwrap().push(n))
        .await
        .unwrap();
    assert_eq!(t, ["a", "b", "c"]);
    assert_eq!(*pages.lock().unwrap(), [1, 2]);
}

#[tokio::test]
async fn tags_exactly_n_without_link_follows_last() {
    let s = MockServer::start().await;
    let first: Vec<String> = (0..1000).map(|i| format!("t{i:04}")).collect();
    Mock::given(path("/v2/r/tags/list"))
        .and(query_param("last", "t0999"))
        .respond_with(json_resp(tags_body(["t1000".into()])))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path("/v2/r/tags/list"))
        .respond_with(json_resp(tags_body(first)))
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    let t = c.tags(&repo("r"), &no_progress()).await.unwrap();
    assert_eq!(t.len(), 1001);
    assert_eq!(t[1000], "t1000");
}

#[tokio::test]
async fn tags_registry_ignoring_last_ends_cleanly() {
    let s = MockServer::start().await;
    let all: Vec<String> = (0..1000).map(|i| format!("t{i:04}")).collect();
    Mock::given(path("/v2/r/tags/list"))
        .respond_with(json_resp(tags_body(all)))
        .expect(2)
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    let t = c.tags(&repo("r"), &no_progress()).await.unwrap();
    assert_eq!(t.len(), 1000);
}

#[tokio::test]
async fn catalog_no_link_ends() {
    let s = MockServer::start().await;
    Mock::given(path("/v2/_catalog"))
        .and(query_param("n", "1000"))
        .respond_with(json_resp(json!({"repositories": ["a", "b/c"]})))
        .expect(1)
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    let r = c.catalog(&no_progress()).await.unwrap();
    assert_eq!(r, [repo("a"), repo("b/c")]);
}

#[tokio::test]
async fn repeated_link_cursor_errors() {
    let s = MockServer::start().await;
    Mock::given(path("/v2/_catalog"))
        .respond_with(
            json_resp(json!({"repositories": ["a"]}))
                .insert_header("link", r#"</v2/_catalog?last=a&n=1000>; rel="next""#),
        )
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    let e = c.catalog(&no_progress()).await.unwrap_err();
    assert!(
        matches!(&e, RegistryError::RepeatedCursor(m) if !m.contains('?')),
        "{e:?}"
    );
}

#[tokio::test]
async fn referrers_404_falls_back_to_tag_schema() {
    let s = MockServer::start().await;
    let subject = Digest::parse(&dg('a')).unwrap();
    Mock::given(path(format!("/v2/r/referrers/{subject}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&s)
        .await;
    Mock::given(path(format!("/v2/r/manifests/sha256-{}", "a".repeat(64))))
        .respond_with(json_resp(json!({
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json",
                           "digest": dg('b'), "size": 3, "artifactType": "application/sig"}]
        })))
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    let r = c.referrers(&repo("r"), &subject).await.unwrap().unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].artifact_type.as_deref(), Some("application/sig"));

    // fallback tag also 404 -> known empty
    let other = Digest::parse(&dg('d')).unwrap();
    Mock::given(path(format!("/v2/r/referrers/{other}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&s)
        .await;
    assert_eq!(c.referrers(&repo("r"), &other).await.unwrap(), Some(vec![]));

    // non-sha256 digests have no fallback
    let sha512 = Digest::parse(&format!("sha512:{}", "e".repeat(128))).unwrap();
    Mock::given(path(format!("/v2/r/referrers/{sha512}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&s)
        .await;
    assert_eq!(c.referrers(&repo("r"), &sha512).await.unwrap(), None);
}

#[tokio::test]
async fn referrers_500_is_unknown() {
    let s = MockServer::start().await;
    Mock::given(path_regex_referrers())
        .respond_with(ResponseTemplate::new(500))
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    let d = Digest::parse(&dg('a')).unwrap();
    assert_eq!(c.referrers(&repo("r"), &d).await.unwrap(), None);
}

fn path_regex_referrers() -> impl wiremock::Match {
    wiremock::matchers::path_regex(r"^/v2/r/referrers/")
}

#[tokio::test]
async fn head_manifest_digest() {
    let s = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/v2/r/manifests/t"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", dg('a').as_str())
                .insert_header("content-type", "application/vnd.oci.image.index.v1+json"),
        )
        .mount(&s)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/r/manifests/nodigest"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    let (d, mt) = c
        .head_manifest(&repo("r"), &Reference::Tag("t".into()))
        .await
        .unwrap();
    assert_eq!(d.to_string(), dg('a'));
    assert_eq!(mt, "application/vnd.oci.image.index.v1+json");
    let e = c
        .head_manifest(&repo("r"), &Reference::Tag("nodigest".into()))
        .await
        .unwrap_err();
    assert!(matches!(e, RegistryError::Provider(_)));
}

#[tokio::test]
async fn invalid_json_errors_never_quote_values() {
    let s = MockServer::start().await;
    Mock::given(path(format!("/v2/r/blobs/{}", dg('c'))))
        .respond_with(json_resp(json!({
            "config": {"Env": "DB_PASS=hunter2", "Labels": {"k": 5}}
        })))
        .mount(&s)
        .await;
    Mock::given(path("/v2/r/manifests/m"))
        .respond_with(json_resp(json!({
            "mediaType": "x", "config": "DB_PASS=hunter2"
        })))
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    let d = desc("application/vnd.oci.image.config.v1+json");
    let e = c.image_config(&repo("r"), &d).await.unwrap_err();
    let m = c
        .manifest(&repo("r"), &Reference::Tag("m".into()))
        .await
        .unwrap_err();
    for e in [e, m] {
        let t = e.to_string();
        assert!(t.contains("invalid") && t.contains("line"), "{t}");
        assert!(!t.contains("hunter2") && !t.contains("DB_PASS"), "{t}");
    }
    // non-string label value alone
    Mock::given(path(format!("/v2/r/blobs/{}", dg('d'))))
        .respond_with(json_resp(
            json!({"config": {"Labels": {"k": "ok", "t": 5}}}),
        ))
        .mount(&s)
        .await;
    let mut d = desc("application/vnd.oci.image.config.v1+json");
    d.digest = Digest::parse(&dg('d')).unwrap();
    assert!(c.image_config(&repo("r"), &d).await.is_err());
}

#[tokio::test]
async fn tags_first_page_rejected_falls_back_to_plain_request() {
    let s = MockServer::start().await;
    Mock::given(path("/v2/r/tags/list"))
        .and(query_param("n", "1000"))
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path("/v2/r/tags/list"))
        .respond_with(json_resp(tags_body(["a".into(), "b".into()])))
        .expect(1)
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    assert_eq!(
        c.tags(&repo("r"), &no_progress()).await.unwrap(),
        ["a", "b"]
    );
}

#[tokio::test]
async fn tags_both_requests_fail_returns_original_error() {
    let s = MockServer::start().await;
    Mock::given(path("/v2/r/tags/list"))
        .and(query_param("n", "1000"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&s)
        .await;
    Mock::given(path("/v2/r/tags/list"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    let e = c.tags(&repo("r"), &no_progress()).await.unwrap_err();
    assert!(
        matches!(&e, RegistryError::Provider(m) if m.contains("400")),
        "{e:?}"
    );
}

#[tokio::test]
async fn tags_auth_errors_do_not_fall_back() {
    let s = MockServer::start().await;
    Mock::given(path("/v2/r/tags/list"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&s)
        .await;
    let c = client(&s, Credential::Anonymous);
    assert_eq!(
        c.tags(&repo("r"), &no_progress()).await.unwrap_err(),
        RegistryError::NotFound
    );
}

// ---- delete ----

use lcquay::registry::DeleteError;

fn app_digest() -> Digest {
    Digest::parse(&dg('a')).unwrap()
}

async fn delete(c: &OciClient) -> Result<(), DeleteError> {
    c.delete_manifest(&repo("app"), &app_digest()).await
}

/// DELETEs received on the app manifest.
async fn deletes(s: &MockServer) -> Vec<Request> {
    received(s, &format!("/v2/app/manifests/{}", dg('a')))
        .await
        .into_iter()
        .filter(|r| r.method.as_str() == "DELETE")
        .collect()
}

/// Probe 200 (anonymous), one DELETE answered with `resp`.
async fn delete_with(resp: ResponseTemplate) -> (MockServer, Result<(), DeleteError>) {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(method("DELETE"))
        .respond_with(resp)
        .mount(&s)
        .await;
    let r = delete(&client(&s, basic())).await;
    (s, r)
}

fn errors_body(code: &str, message: &str) -> String {
    json!({"errors": [{"code": code, "message": message}]}).to_string()
}

#[tokio::test]
async fn delete_202_ok() {
    let (s, r) = delete_with(ResponseTemplate::new(202)).await;
    assert_eq!(r, Ok(()));
    assert_eq!(deletes(&s).await.len(), 1);
}

#[tokio::test]
async fn delete_405_unsupported() {
    let (_, r) = delete_with(ResponseTemplate::new(405)).await;
    assert_eq!(r, Err(DeleteError::Unsupported));
}

#[tokio::test]
async fn delete_400_unsupported_code() {
    let body = errors_body("UNSUPPORTED", "The operation is unsupported.");
    let (_, r) = delete_with(ResponseTemplate::new(400).set_body_string(body)).await;
    assert_eq!(r, Err(DeleteError::Unsupported));
}

#[tokio::test]
async fn delete_400_other_rejected() {
    let body = errors_body("DIGEST_INVALID", "bad digest");
    let (_, r) = delete_with(ResponseTemplate::new(400).set_body_string(body)).await;
    assert!(
        matches!(&r, Err(DeleteError::Rejected(m)) if m.contains("DIGEST_INVALID")),
        "{r:?}"
    );
}

#[tokio::test]
async fn delete_500_uncertain_not_retried() {
    let (s, r) = delete_with(ResponseTemplate::new(500)).await;
    assert!(matches!(r, Err(DeleteError::Uncertain(_))), "{r:?}");
    assert_eq!(deletes(&s).await.len(), 1);
}

#[tokio::test]
async fn delete_403_rejected() {
    let (_, r) = delete_with(ResponseTemplate::new(403)).await;
    assert!(matches!(r, Err(DeleteError::Rejected(_))), "{r:?}");
}

#[tokio::test]
async fn delete_401_challenge_resent_once() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(method("DELETE"))
        .and(header("authorization", BASIC))
        .respond_with(ResponseTemplate::new(202))
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(challenge(r#"Basic realm="x""#))
        .mount(&s)
        .await;
    assert_eq!(delete(&client(&s, basic())).await, Ok(()));
    let d = deletes(&s).await;
    assert_eq!(d.len(), 2);
    assert_eq!(auth_of(&d[0]), None);
    assert_eq!(auth_of(&d[1]).as_deref(), Some(BASIC));
}

struct Failing;

#[async_trait]
impl CredentialSource for Failing {
    async fn credential(&self, _dest: Destination) -> Result<Issued, RegistryError> {
        Err(RegistryError::AwsCredentials("expired".into()))
    }
    fn can_login(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn delete_credential_failure_not_sent() {
    let s = MockServer::start().await;
    let c = OciClient::new(
        Url::parse(&s.uri()).unwrap(),
        vec![],
        Arc::new(Failing),
        Arc::new(Gate::new(8)),
    );
    let r = delete(&c).await;
    assert!(
        matches!(
            r,
            Err(DeleteError::NotSent(RegistryError::AwsCredentials(_)))
        ),
        "{r:?}"
    );
    assert!(s.received_requests().await.unwrap().is_empty());
}

fn client_at(addr: std::net::SocketAddr, cred: Credential) -> OciClient {
    OciClient::new(
        Url::parse(&format!("http://{addr}")).unwrap(),
        vec![],
        Arc::new(Fixed(cred)),
        Arc::new(Gate::new(8)),
    )
}

#[tokio::test]
async fn delete_probe_failure_not_sent() {
    let addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    }; // dropped: the port is closed
    let r = delete(&client_at(addr, basic())).await;
    assert!(
        matches!(r, Err(DeleteError::NotSent(RegistryError::Network(_)))),
        "{r:?}"
    );
}

#[tokio::test]
async fn delete_transport_error_after_send_uncertain() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            drop(sock);
        }
    });
    let r = delete(&client_at(addr, Credential::Bearer(Secret("x".into())))).await;
    assert!(matches!(r, Err(DeleteError::Uncertain(_))), "{r:?}");
}

#[tokio::test]
async fn delete_error_text_bounded() {
    let body = errors_body("DENIED", &"x".repeat(1000));
    let (_, r) = delete_with(ResponseTemplate::new(400).set_body_string(body)).await;
    assert!(
        matches!(&r, Err(DeleteError::Rejected(m)) if m.len() <= 200 && m.starts_with("HTTP 400: DENIED: x")),
        "{r:?}"
    );
}

#[tokio::test]
async fn delete_redirect_uncertain() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(303).insert_header("location", "/elsewhere"))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path("/elsewhere"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    let r = delete(&client(&s, basic())).await;
    assert!(matches!(r, Err(DeleteError::Uncertain(_))), "{r:?}");
}

#[tokio::test]
async fn delete_token_failure_not_sent() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(method("DELETE"))
        .respond_with(challenge(&bearer_challenge(&s, "")))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path("/token"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&s)
        .await;
    let r = delete(&client(&s, basic())).await;
    assert!(matches!(r, Err(DeleteError::NotSent(_))), "{r:?}");
}

#[tokio::test]
async fn delete_401_without_challenge_not_sent() {
    let (_, r) = delete_with(ResponseTemplate::new(401)).await;
    assert_eq!(r, Err(DeleteError::NotSent(RegistryError::Unauthorized)));
}

// ---- request log ----

use lcquay::reqlog::RequestLog;

#[tokio::test]
async fn request_log_entries() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    let p = format!("/v2/r/manifests/{}", dg('a'));
    Mock::given(path(p.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_string("manifest"))
        .mount(&s)
        .await;
    let log = RequestLog::default();
    let c = client(&s, Credential::Anonymous).with_log(log.scoped("it"));
    get(&c, &p, "r").await.unwrap();
    let all = log.snapshot();
    let e = all.iter().find(|e| e.op == "GET manifest").expect("entry");
    assert_eq!(
        (e.registry.as_str(), e.path.as_str(), e.outcome.as_str()),
        ("it", p.as_str(), "200")
    );
    assert_eq!(e.bytes, Some("manifest".len() as u64));
    assert!(all.iter().any(|e| e.op == "GET ping"));
}

#[tokio::test]
async fn request_log_never_holds_secrets() {
    let s = MockServer::start().await;
    let cdn = MockServer::start().await;
    let bearer = bearer_challenge(&s, r#",service="reg",scope="repository:r:pull""#);
    probe(&s, challenge(&bearer)).await;
    Mock::given(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"s3cr3t-token"}"#))
        .mount(&s)
        .await;
    let authed = || header("authorization", "Bearer s3cr3t-token");
    Mock::given(path("/v2/r/tags/list"))
        .and(authed())
        .and(query_param("last", "secret-cursor"))
        .respond_with(json_resp(tags_body(["b".into()])))
        .mount(&s)
        .await;
    Mock::given(path("/v2/r/tags/list"))
        .and(authed())
        .respond_with(json_resp(tags_body(["a".into()])).insert_header(
            "link",
            r#"</v2/r/tags/list?n=1&last=secret-cursor>; rel="next""#,
        ))
        .mount(&s)
        .await;
    let blob = format!("/v2/r/blobs/{}", dg('b'));
    Mock::given(path(blob.as_str()))
        .and(authed())
        .respond_with(ResponseTemplate::new(307).insert_header(
            "location",
            format!("{}/blob?X-Amz-Signature=sig-secret", cdn.uri()),
        ))
        .mount(&s)
        .await;
    Mock::given(wiremock::matchers::any())
        .respond_with(challenge(&bearer))
        .mount(&s)
        .await;
    Mock::given(path("/blob"))
        .respond_with(ResponseTemplate::new(200).set_body_string("data"))
        .mount(&cdn)
        .await;

    let log = RequestLog::default();
    let cred = Credential::Basic {
        username: "u".into(),
        password: Secret("pw-secret".into()),
    };
    let c = client(&s, cred).with_log(log.scoped("it"));
    assert_eq!(
        c.tags(&repo("r"), &no_progress()).await.unwrap(),
        ["a", "b"]
    );
    let r = get(&c, &blob, "r").await.unwrap();
    assert_eq!(&r.body[..], b"data");

    let all = log.snapshot();
    let text = format!("{all:?}");
    for secret in [
        "?",
        "s3cr3t-token",
        "pw-secret",
        "secret-cursor",
        "scope=",
        "sig-secret",
        "Authorization",
    ] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
    let token = all
        .iter()
        .find(|e| e.op == "GET token")
        .expect("token entry");
    assert_eq!(
        token.path,
        format!("127.0.0.1:{}/token", s.address().port())
    );
    assert!(all.iter().any(|e| e.op == "GET tags"));
    assert!(all.iter().any(|e| e.op == "GET blob"));
}

// ---- writes, blob reads ----

use bytes::Bytes;
use lcquay::registry::WriteBody;
use reqwest::Method;

const UPLOAD: &str = "/v2/r/blobs/uploads/u1";

fn layer() -> WriteBody {
    WriteBody {
        bytes: Bytes::from_static(b"layer-bytes"),
        content_type: Some("application/octet-stream".into()),
        content_range: Some("0-10".into()),
    }
}

async fn put_upload(c: &OciClient) -> Result<RawResponse, DeleteError> {
    c.write(
        Method::PUT,
        &format!("{UPLOAD}?_state=s3cr3t"),
        &repo("r"),
        Some(layer()),
    )
    .await
}

/// Probe 200 (anonymous), every PUT answered with `resp`.
async fn put_with(resp: ResponseTemplate) -> (MockServer, Result<RawResponse, DeleteError>) {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(method("PUT"))
        .respond_with(resp)
        .mount(&s)
        .await;
    let r = put_upload(&client(&s, basic())).await;
    (s, r)
}

#[tokio::test]
async fn write_sends_body_once() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(method("PUT"))
        .and(path(UPLOAD))
        .respond_with(ResponseTemplate::new(201))
        .mount(&s)
        .await;
    let log = RequestLog::default();
    let c = client(&s, basic()).with_log(log.scoped("it"));
    assert_eq!(put_upload(&c).await.unwrap().status, 201);
    let reqs = received(&s, UPLOAD).await;
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(r.method.as_str(), "PUT");
    assert_eq!(r.url.query(), Some("_state=s3cr3t"));
    assert_eq!(&r.body[..], b"layer-bytes");
    let h = |n: &str| r.headers.get(n).map(|v| v.to_str().unwrap().to_owned());
    assert_eq!(
        h("content-type").as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(h("content-range").as_deref(), Some("0-10"));

    let all = log.snapshot();
    let puts: Vec<_> = all.iter().filter(|e| e.op == "PUT upload").collect();
    assert_eq!(puts.len(), 1, "{all:?}");
    assert_eq!(puts[0].path, UPLOAD);
    assert_eq!(puts[0].bytes, Some("layer-bytes".len() as u64));
    assert!(!format!("{all:?}").contains("s3cr3t"));
}

#[tokio::test]
async fn write_401_resends_once_with_body() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(method("PUT"))
        .and(header("authorization", BASIC))
        .respond_with(ResponseTemplate::new(201))
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .respond_with(challenge(r#"Basic realm="x""#))
        .mount(&s)
        .await;
    assert_eq!(put_upload(&client(&s, basic())).await.unwrap().status, 201);
    let reqs = received(&s, UPLOAD).await;
    assert_eq!(reqs.len(), 2);
    assert_eq!(auth_of(&reqs[0]), None);
    assert_eq!(auth_of(&reqs[1]).as_deref(), Some(BASIC));
    assert!(reqs.iter().all(|r| &r.body[..] == b"layer-bytes"));

    let (s, r) = put_with(challenge(r#"Basic realm="x""#)).await;
    assert!(
        matches!(&r, Err(DeleteError::Rejected(m)) if m.starts_with("HTTP 401")),
        "{r:?}"
    );
    assert_eq!(received(&s, UPLOAD).await.len(), 2);
}

#[tokio::test]
async fn write_5xx_is_uncertain() {
    let (s, r) = put_with(ResponseTemplate::new(503)).await;
    assert!(
        matches!(&r, Err(DeleteError::Uncertain(m)) if m.starts_with("HTTP 503")),
        "{r:?}"
    );
    assert_eq!(received(&s, UPLOAD).await.len(), 1);
}

#[tokio::test]
async fn write_4xx_rejected_with_code() {
    let body = errors_body("DIGEST_INVALID", "bad");
    let (_, r) = put_with(ResponseTemplate::new(400).set_body_string(body)).await;
    assert_eq!(
        r.unwrap_err(),
        DeleteError::Rejected("HTTP 400: DIGEST_INVALID: bad".into())
    );
}

#[tokio::test]
async fn write_redirect_not_followed() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(method("PUT"))
        .and(path(UPLOAD))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "/elsewhere"))
        .mount(&s)
        .await;
    Mock::given(path("/elsewhere"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&s)
        .await;
    let r = put_upload(&client(&s, basic())).await;
    assert_eq!(
        r.unwrap_err(),
        DeleteError::Rejected("HTTP 307: redirect not followed".into())
    );
    assert_eq!(received(&s, UPLOAD).await.len(), 1);
    assert!(received(&s, "/elsewhere").await.is_empty());
}

#[tokio::test]
async fn write_uses_push_scope() {
    let s = MockServer::start().await;
    probe(&s, challenge(&bearer_challenge(&s, r#",service="reg""#))).await;
    let scope = |sc: &str| bearer_challenge(&s, &format!(r#",service="reg",scope="{sc}""#));
    for (sc, tok) in [
        ("repository:r:pull", "pull-tok"),
        ("repository:r:pull,push", "push-tok"),
    ] {
        Mock::given(path("/token"))
            .and(query_param("scope", sc))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(format!(r#"{{"token":"{tok}"}}"#)),
            )
            .mount(&s)
            .await;
    }
    Mock::given(method("GET"))
        .and(path(MANIFEST))
        .and(header("authorization", "Bearer pull-tok"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(MANIFEST))
        .respond_with(challenge(&scope("repository:r:pull")))
        .mount(&s)
        .await;
    let uploads = "/v2/r/blobs/uploads/";
    Mock::given(method("POST"))
        .and(path(uploads))
        .and(header("authorization", "Bearer push-tok"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path(uploads))
        .respond_with(challenge(&scope("repository:r:pull,push")))
        .mount(&s)
        .await;

    let c = client(&s, Credential::Anonymous);
    get(&c, MANIFEST, "r").await.unwrap();
    let r = c.write(Method::POST, uploads, &repo("r"), None).await;
    assert_eq!(r.unwrap().status, 202);
    get(&c, MANIFEST, "r").await.unwrap();

    let scopes: Vec<_> = received(&s, "/token")
        .await
        .iter()
        .map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "scope")
                .unwrap()
                .1
                .into_owned()
        })
        .collect();
    assert_eq!(scopes, ["repository:r:pull", "repository:r:pull,push"]);
    let gets = received(&s, MANIFEST).await;
    assert_eq!(
        gets.len(),
        3,
        "the last GET goes with its pull token upfront"
    );
    assert_eq!(auth_of(&gets[2]).as_deref(), Some("Bearer pull-tok"));
    assert_eq!(received(&s, uploads).await.len(), 2);
}

#[tokio::test]
async fn head_blob_true_false() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/r/blobs/{}", dg('a'))))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&s)
        .await;
    let c = client(&s, basic());
    let d = |ch| Digest::parse(&dg(ch)).unwrap();
    assert_eq!(c.head_blob(&repo("r"), &d('a')).await, Ok(true));
    assert_eq!(c.head_blob(&repo("r"), &d('b')).await, Ok(false));
}

#[tokio::test]
async fn manifest_bytes_exact() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    let body = "{\n   \"schemaVersion\" : 2,\n\t\"layers\": [] }\n\n";
    let mt = "application/vnd.oci.image.manifest.v1+json";
    Mock::given(method("GET"))
        .and(path(format!("/v2/r/manifests/{}", dg('a'))))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, mt))
        .mount(&s)
        .await;
    let (b, t) = client(&s, basic())
        .manifest_bytes(&repo("r"), &app_digest())
        .await
        .unwrap();
    assert_eq!(&b[..], body.as_bytes());
    assert_eq!(t, mt);
}

async fn drain(mut st: lcquay::registry::BlobStream) -> Vec<u8> {
    let mut all = Vec::new();
    while let Some(c) = st.chunk().await.unwrap() {
        all.extend_from_slice(&c);
    }
    all
}

#[tokio::test]
async fn blob_stream_follows_redirect_without_auth() {
    let s = MockServer::start().await;
    let cdn = MockServer::start().await;
    probe(&s, challenge(r#"Basic realm="x""#)).await;
    let blob = format!("/v2/r/blobs/{}", dg('a'));
    Mock::given(path(blob.as_str()))
        .and(header("authorization", BASIC))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/blob?sig=x", cdn.uri())),
        )
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(path("/blob"))
        .respond_with(ResponseTemplate::new(200).set_body_string("blob-data"))
        .mount(&cdn)
        .await;
    let st = client(&s, basic())
        .blob_stream(&repo("r"), &app_digest())
        .await
        .unwrap();
    assert_eq!(st.len(), Some(9));
    assert_eq!(drain(st).await, b"blob-data");
    let cdn_reqs = cdn.received_requests().await.unwrap();
    assert_eq!(cdn_reqs.len(), 1);
    assert_eq!(auth_of(&cdn_reqs[0]), None);
}

#[tokio::test]
async fn blob_stream_has_no_cap() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    let size = 5 * 1024 * 1024;
    let blob = format!("/v2/r/blobs/{}", dg('a'));
    Mock::given(path(blob.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7u8; size]))
        .mount(&s)
        .await;
    let log = RequestLog::default();
    let c = client(&s, basic()).with_log(log.scoped("it"));
    let st = c.blob_stream(&repo("r"), &app_digest()).await.unwrap();
    assert_eq!(st.len(), Some(size as u64));
    let all = log.snapshot();
    let gets: Vec<_> = all.iter().filter(|e| e.op == "GET blob").collect();
    assert_eq!(gets.len(), 1, "logged once the headers arrive");
    assert_eq!(gets[0].bytes, Some(size as u64));
    let data = drain(st).await;
    assert_eq!(data.len(), size);
    assert!(data.iter().all(|&b| b == 7));
    assert_eq!(
        log.snapshot().len(),
        all.len(),
        "the body read logs nothing"
    );
}

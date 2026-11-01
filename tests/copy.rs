//! Copy engine (`registry::copy`) against wiremock registries.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use lcquay::auth::{Credential, CredentialSource, Destination, Issued, Secret};
use lcquay::domain::{Digest, ItemStep, RegistryError, RepoPath, TransferStep};
use lcquay::registry::copy::{CopyError, CopyJob, copy_image};
use lcquay::registry::{DigestCheck, Gate, OciClient};
use lcquay::reqlog::RequestLog;
use serde_json::json;
use url::Url;
use wiremock::matchers::{method, path, path_regex, query_param};
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

fn client(s: &MockServer, cred: Credential) -> OciClient {
    OciClient::new(
        Url::parse(&s.uri()).unwrap(),
        vec![],
        Arc::new(Fixed(cred)),
        Arc::new(Gate::new(8)),
    )
}

fn anon(s: &MockServer) -> OciClient {
    client(s, Credential::Anonymous)
}

fn sha(b: &[u8]) -> Digest {
    let mut c = DigestCheck::new();
    c.update(b);
    c.finish()
}

fn repo(s: &str) -> RepoPath {
    RepoPath(s.into())
}

const MT_IMG: &str = "application/vnd.oci.image.manifest.v1+json";
const MT_IDX: &str = "application/vnd.oci.image.index.v1+json";
const CONFIG: &[u8] = br#"{"architecture":"amd64","os":"linux"}"#;
const TAG: &str = "v1";

struct Image {
    layer: Vec<u8>,
    bytes: Vec<u8>,
    digest: Digest,
}

fn image(layer: &[u8]) -> Image {
    let bytes = serde_json::to_vec_pretty(&json!({
        "schemaVersion": 2,
        "mediaType": MT_IMG,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                   "digest": sha(CONFIG).to_string(), "size": CONFIG.len()},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": sha(layer).to_string(), "size": layer.len()}],
    }))
    .unwrap();
    Image {
        layer: layer.to_vec(),
        digest: sha(&bytes),
        bytes,
    }
}

async fn probe(s: &MockServer, resp: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(resp)
        .mount(s)
        .await;
}

async fn serve_manifest(s: &MockServer, repo: &str, d: &Digest, body: &[u8], mt: Option<&str>) {
    let resp = match mt {
        Some(mt) => ResponseTemplate::new(200).set_body_raw(body.to_vec(), mt),
        None => ResponseTemplate::new(200).set_body_bytes(body.to_vec()),
    };
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/manifests/{d}")))
        .respond_with(resp)
        .mount(s)
        .await;
}

async fn serve_blob(s: &MockServer, repo: &str, d: &Digest, body: &[u8]) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/blobs/{d}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
        .mount(s)
        .await;
}

/// The source image (manifest, config and layer) in `repo`.
async fn serve_image(s: &MockServer, repo: &str, img: &Image) {
    serve_manifest(s, repo, &img.digest, &img.bytes, Some(MT_IMG)).await;
    serve_blob(s, repo, &sha(CONFIG), CONFIG).await;
    serve_blob(s, repo, &sha(&img.layer), &img.layer).await;
}

/// `PATCH …/uN` answers with `Location: …/u(N+1)?_state=sN+1`.
fn next_location(r: &Request) -> ResponseTemplate {
    let p = r.url.path();
    let (base, n) = p.rsplit_once("/u").unwrap();
    let n: u32 = n.parse().unwrap();
    ResponseTemplate::new(202)
        .insert_header("location", format!("{base}/u{}?_state=s{}", n + 1, n + 1))
}

/// Default destination behaviour in `dst` (lower priority than a test's own mocks): blobs are
/// absent, uploads start at `u1?_state=x`, every PUT succeeds, the tag resolves to `want`.
async fn dst_ok(s: &MockServer, dst: &str, want: &Digest) {
    let mocks = [
        Mock::given(method("HEAD"))
            .and(path_regex(format!("^/v2/{dst}/blobs/")))
            .respond_with(ResponseTemplate::new(404)),
        Mock::given(method("POST"))
            .and(path(format!("/v2/{dst}/blobs/uploads/")))
            .respond_with(
                ResponseTemplate::new(202)
                    .insert_header("location", format!("/v2/{dst}/blobs/uploads/u1?_state=x")),
            ),
        Mock::given(method("PATCH")).respond_with(next_location),
        Mock::given(method("PUT")).respond_with(ResponseTemplate::new(201)),
        Mock::given(method("HEAD"))
            .and(path(format!("/v2/{dst}/manifests/{TAG}")))
            .respond_with(
                ResponseTemplate::new(200).insert_header("docker-content-digest", want.to_string()),
            ),
    ];
    for m in mocks {
        m.with_priority(10).mount(s).await;
    }
}

/// HEAD on every blob of `dst` answers 200.
async fn dst_has_blobs(s: &MockServer, dst: &str) {
    Mock::given(method("HEAD"))
        .and(path_regex(format!("^/v2/{dst}/blobs/")))
        .respond_with(ResponseTemplate::new(200))
        .mount(s)
        .await;
}

/// `METHOD path[?query]` of every request but the `/v2/` probe, in order.
async fn calls(s: &MockServer) -> Vec<String> {
    s.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() != "/v2/")
        .map(|r| match r.url.query() {
            Some(q) => format!("{} {}?{q}", r.method, r.url.path()),
            None => format!("{} {}", r.method, r.url.path()),
        })
        .collect()
}

async fn requests(s: &MockServer, m: &str) -> Vec<Request> {
    s.received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == m)
        .collect()
}

fn header(r: &Request, n: &str) -> Option<String> {
    r.headers.get(n).map(|v| v.to_str().unwrap().to_owned())
}

#[derive(Default)]
struct Steps(Mutex<Vec<TransferStep>>);

impl Steps {
    fn push(&self, s: TransferStep) {
        self.0.lock().unwrap().push(s);
    }
    fn all(&self) -> Vec<TransferStep> {
        self.0.lock().unwrap().clone()
    }
    fn has(&self, d: &Digest, step: ItemStep) -> bool {
        self.all().contains(&TransferStep {
            id: d.short(),
            step,
        })
    }
}

struct Run<'a> {
    src: &'a OciClient,
    dst: &'a OciClient,
    src_repo: &'a str,
    dst_repo: &'a str,
    same_registry: bool,
    chunk: usize,
    flag: &'a AtomicBool,
}

impl Run<'_> {
    async fn go(&self, d: &Digest, steps: &Steps) -> Result<(), CopyError> {
        let (src_repo, dst_repo) = (repo(self.src_repo), repo(self.dst_repo));
        let job = CopyJob {
            src: self.src,
            src_repo: &src_repo,
            dst: self.dst,
            dst_repo: &dst_repo,
            digest: d,
            tag: TAG,
            same_registry: self.same_registry,
            chunk: self.chunk,
            manifest_sent: self.flag,
        };
        copy_image(&job, &|s| steps.push(s)).await
    }
}

fn run<'a>(src: &'a OciClient, dst: &'a OciClient, flag: &'a AtomicBool) -> Run<'a> {
    Run {
        src,
        dst,
        src_repo: "src",
        dst_repo: "dst",
        same_registry: false,
        chunk: 4,
        flag,
    }
}

#[tokio::test]
async fn copy_skips_existing_blobs() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    let img = image(b"layer-1");
    serve_image(&s, "src", &img).await;
    dst_ok(&s, "dst", &img.digest).await;
    dst_has_blobs(&s, "dst").await;
    let (c, flag, steps) = (anon(&s), AtomicBool::new(false), Steps::default());

    run(&c, &c, &flag).go(&img.digest, &steps).await.unwrap();

    let (cfg, layer) = (sha(CONFIG), sha(&img.layer));
    assert_eq!(
        calls(&s).await,
        [
            format!("GET /v2/src/manifests/{}", img.digest),
            format!("HEAD /v2/dst/blobs/{cfg}"),
            format!("HEAD /v2/dst/blobs/{layer}"),
            format!("PUT /v2/dst/manifests/{TAG}"),
            format!("HEAD /v2/dst/manifests/{TAG}"),
        ]
    );
    let all = steps.all();
    assert_eq!(
        all[..2],
        [
            TransferStep {
                id: cfg.short(),
                step: ItemStep::Waiting {
                    total: Some(CONFIG.len() as u64)
                }
            },
            TransferStep {
                id: layer.short(),
                step: ItemStep::Waiting { total: Some(7) }
            },
        ]
    );
    assert!(
        steps.has(&cfg, ItemStep::Exists) && steps.has(&layer, ItemStep::Exists),
        "{all:?}"
    );
    assert!(flag.load(Ordering::SeqCst));
}

#[tokio::test]
async fn copy_mounts_same_registry() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    let img = image(b"layer-1");
    serve_image(&s, "src", &img).await;
    dst_ok(&s, "dst", &img.digest).await;
    Mock::given(method("POST"))
        .and(path("/v2/dst/blobs/uploads/"))
        .and(query_param("from", "src"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&s)
        .await;
    let (c, flag, steps) = (anon(&s), AtomicBool::new(false), Steps::default());
    let mut r = run(&c, &c, &flag);
    r.same_registry = true;

    r.go(&img.digest, &steps).await.unwrap();

    let (cfg, layer) = (sha(CONFIG), sha(&img.layer));
    assert_eq!(
        calls(&s).await,
        [
            format!("GET /v2/src/manifests/{}", img.digest),
            format!("HEAD /v2/dst/blobs/{cfg}"),
            format!("POST /v2/dst/blobs/uploads/?mount={cfg}&from=src"),
            format!("HEAD /v2/dst/blobs/{layer}"),
            format!("POST /v2/dst/blobs/uploads/?mount={layer}&from=src"),
            format!("PUT /v2/dst/manifests/{TAG}"),
            format!("HEAD /v2/dst/manifests/{TAG}"),
        ]
    );
    assert!(steps.has(&cfg, ItemStep::Mounted) && steps.has(&layer, ItemStep::Mounted));
}

/// Same registry, the mount answers 202 with an upload `Location`; returns the server and log.
async fn mount_202() -> (MockServer, Image, RequestLog) {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    let img = image(b"lay");
    serve_image(&s, "src", &img).await;
    dst_ok(&s, "dst", &img.digest).await;
    let log = RequestLog::default();
    let c = anon(&s).with_log(log.scoped("it"));
    let flag = AtomicBool::new(false);
    let mut r = run(&c, &c, &flag);
    (r.same_registry, r.chunk) = (true, 1024);
    r.go(&img.digest, &Steps::default()).await.unwrap();
    (s, img, log)
}

#[tokio::test]
async fn copy_mount_202_uploads_to_given_location() {
    let (s, img, _) = mount_202().await;
    let (cfg, layer) = (sha(CONFIG), sha(&img.layer));
    assert_eq!(
        calls(&s).await,
        [
            format!("GET /v2/src/manifests/{}", img.digest),
            format!("HEAD /v2/dst/blobs/{cfg}"),
            format!("POST /v2/dst/blobs/uploads/?mount={cfg}&from=src"),
            format!("GET /v2/src/blobs/{cfg}"),
            format!("PUT /v2/dst/blobs/uploads/u1?_state=x&digest={cfg}"),
            format!("HEAD /v2/dst/blobs/{layer}"),
            format!("POST /v2/dst/blobs/uploads/?mount={layer}&from=src"),
            format!("GET /v2/src/blobs/{layer}"),
            format!("PUT /v2/dst/blobs/uploads/u1?_state=x&digest={layer}"),
            format!("PUT /v2/dst/manifests/{TAG}"),
            format!("HEAD /v2/dst/manifests/{TAG}"),
        ]
    );
}

#[tokio::test]
async fn copy_log_has_no_upload_state() {
    let (_, _, log) = mount_202().await;
    let all = log.snapshot();
    assert!(all.iter().any(|e| e.op == "PUT upload"), "{all:?}");
    for e in &all {
        let text = format!("{e:?}");
        assert!(!text.contains("_state") && !text.contains('?'), "{e:?}");
    }
}

/// Destination already has the config; only the layer is uploaded.
async fn upload_layer(layer: &[u8], served: &[u8]) -> (MockServer, Digest, Result<(), CopyError>) {
    upload(MockServer::start().await, layer, served).await
}

async fn upload(
    s: MockServer,
    layer: &[u8],
    served: &[u8],
) -> (MockServer, Digest, Result<(), CopyError>) {
    probe(&s, ResponseTemplate::new(200)).await;
    let img = image(layer);
    serve_manifest(&s, "src", &img.digest, &img.bytes, Some(MT_IMG)).await;
    serve_blob(&s, "src", &sha(layer), served).await;
    dst_ok(&s, "dst", &img.digest).await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/dst/blobs/{}", sha(CONFIG))))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    let (c, flag) = (anon(&s), AtomicBool::new(false));
    let r = run(&c, &c, &flag).go(&img.digest, &Steps::default()).await;
    (s, sha(layer), r)
}

#[tokio::test]
async fn copy_chunked_upload() {
    let (s, d, r) = upload_layer(b"0123456789", b"0123456789").await;
    r.unwrap();
    let c = calls(&s).await;
    assert_eq!(
        c[3..9],
        [
            "POST /v2/dst/blobs/uploads/".to_string(),
            format!("GET /v2/src/blobs/{d}"),
            "PATCH /v2/dst/blobs/uploads/u1?_state=x".into(),
            "PATCH /v2/dst/blobs/uploads/u2?_state=s2".into(),
            "PATCH /v2/dst/blobs/uploads/u3?_state=s3".into(),
            format!("PUT /v2/dst/blobs/uploads/u4?_state=s4&digest={d}"),
        ],
        "{c:#?}"
    );
    let patches = requests(&s, "PATCH").await;
    let parts: Vec<_> = patches
        .iter()
        .map(|r| (header(r, "content-range").unwrap(), r.body.clone()))
        .collect();
    assert_eq!(
        parts,
        [
            ("0-3".to_string(), b"0123".to_vec()),
            ("4-7".into(), b"4567".to_vec()),
            ("8-9".into(), b"89".to_vec()),
        ]
    );
    let puts = requests(&s, "PUT").await;
    assert!(puts[0].url.path().contains("/blobs/uploads/"));
    assert!(puts[0].body.is_empty());
}

#[tokio::test]
async fn copy_small_blob_monolithic() {
    let (s, d, r) = upload_layer(b"abc", b"abc").await;
    r.unwrap();
    let c = calls(&s).await;
    assert_eq!(
        c[3..6],
        [
            "POST /v2/dst/blobs/uploads/".to_string(),
            format!("GET /v2/src/blobs/{d}"),
            format!("PUT /v2/dst/blobs/uploads/u1?_state=x&digest={d}"),
        ],
        "{c:#?}"
    );
    assert!(requests(&s, "PATCH").await.is_empty());
    assert_eq!(requests(&s, "PUT").await[0].body, b"abc");
}

#[tokio::test]
async fn copy_chunk_raised_to_registry_minimum() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", "/v2/dst/blobs/uploads/u1")
                .insert_header("oci-chunk-min-length", "8"),
        )
        .mount(&s)
        .await;
    let (s2, _, r) = upload(s, b"0123456789", b"0123456789").await;
    r.unwrap();
    let ranges: Vec<_> = requests(&s2, "PATCH")
        .await
        .iter()
        .map(|r| header(r, "content-range").unwrap())
        .collect();
    assert_eq!(ranges, ["0-7", "8-9"]);
}

#[tokio::test]
async fn copy_source_blob_digest_mismatch_no_put() {
    for (layer, served, patches) in [
        (&b"abc"[..], &b"abX"[..], 0),
        (b"0123456789", b"012345678X", 2),
    ] {
        let (s, d, r) = upload_layer(layer, served).await;
        assert_eq!(
            r,
            Err(CopyError::Failed(format!(
                "source blob {} digest mismatch",
                d.short()
            )))
        );
        assert_eq!(requests(&s, "PATCH").await.len(), patches);
        assert!(
            requests(&s, "PUT").await.is_empty(),
            "{:#?}",
            calls(&s).await
        );
    }
}

#[tokio::test]
async fn copy_puts_children_then_tag() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    let (a, b) = (image(b"amd64-layer"), image(b"arm64-layer"));
    let child = |i: &Image| json!({"mediaType": MT_IMG, "digest": i.digest.to_string(), "size": i.bytes.len()});
    let index = serde_json::to_vec(&json!({
        "schemaVersion": 2, "mediaType": MT_IDX,
        "manifests": [child(&a), child(&b), child(&a)],
    }))
    .unwrap();
    let want = sha(&index);
    serve_manifest(&s, "src", &want, &index, Some(MT_IDX)).await;
    serve_manifest(&s, "src", &a.digest, &a.bytes, Some(MT_IMG)).await;
    // no Content-Type: the media type comes from the body
    serve_manifest(&s, "src", &b.digest, &b.bytes, None).await;
    dst_ok(&s, "dst", &want).await;
    dst_has_blobs(&s, "dst").await;
    let (c, flag, steps) = (anon(&s), AtomicBool::new(false), Steps::default());

    run(&c, &c, &flag).go(&want, &steps).await.unwrap();

    let puts: Vec<_> = requests(&s, "PUT")
        .await
        .into_iter()
        .map(|r| (r.url.path().to_string(), header(&r, "content-type"), r.body))
        .collect();
    let mt = |m: &str| Some(m.to_string());
    assert_eq!(
        puts,
        [
            (
                format!("/v2/dst/manifests/{}", a.digest),
                mt(MT_IMG),
                a.bytes
            ),
            (
                format!("/v2/dst/manifests/{}", b.digest),
                mt(MT_IMG),
                b.bytes
            ),
            (format!("/v2/dst/manifests/{TAG}"), mt(MT_IDX), index),
        ]
    );
    // config shared by both children: one blob, checked once
    let heads = calls(&s).await;
    let cfg_heads = heads
        .iter()
        .filter(|c| c.ends_with(&sha(CONFIG).to_string()))
        .count();
    assert_eq!(cfg_heads, 1, "{heads:#?}");
}

#[tokio::test]
async fn copy_retag_puts_only_tag() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    let img = image(b"layer-1");
    serve_image(&s, "r", &img).await;
    dst_ok(&s, "r", &img.digest).await;
    let flag = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (f, sn) = (flag.clone(), seen.clone());
    Mock::given(method("PUT"))
        .respond_with(move |_: &Request| {
            sn.lock().unwrap().push(f.load(Ordering::SeqCst));
            ResponseTemplate::new(201)
        })
        .mount(&s)
        .await;
    let c = anon(&s);
    let mut r = run(&c, &c, &flag);
    (r.src_repo, r.dst_repo, r.same_registry) = ("r", "r", true);

    let steps = Steps::default();
    r.go(&img.digest, &steps).await.unwrap();

    assert_eq!(
        calls(&s).await,
        [
            format!("GET /v2/r/manifests/{}", img.digest),
            format!("PUT /v2/r/manifests/{TAG}"),
            format!("HEAD /v2/r/manifests/{TAG}"),
        ]
    );
    assert_eq!(*seen.lock().unwrap(), [true], "flag set before the tag PUT");
    assert!(steps.all().is_empty(), "no blob progress in a retag");
}

#[tokio::test]
async fn copy_verify_mismatch() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    let img = image(b"layer-1");
    serve_image(&s, "src", &img).await;
    dst_has_blobs(&s, "dst").await;
    let other = sha(b"other");
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/dst/manifests/{TAG}")))
        .respond_with(
            ResponseTemplate::new(200).insert_header("docker-content-digest", other.to_string()),
        )
        .mount(&s)
        .await;
    dst_ok(&s, "dst", &img.digest).await;
    let (c, flag) = (anon(&s), AtomicBool::new(false));
    let r = run(&c, &c, &flag).go(&img.digest, &Steps::default()).await;
    assert_eq!(
        r,
        Err(CopyError::DigestDiffers {
            got: other,
            want: img.digest
        })
    );
}

#[tokio::test]
async fn copy_5xx_on_tag_put_is_uncertain_and_stops() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    let img = image(b"layer-1");
    serve_image(&s, "src", &img).await;
    dst_has_blobs(&s, "dst").await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/dst/manifests/{TAG}")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&s)
        .await;
    dst_ok(&s, "dst", &img.digest).await;
    let (c, flag) = (anon(&s), AtomicBool::new(false));
    let r = run(&c, &c, &flag).go(&img.digest, &Steps::default()).await;
    assert!(
        matches!(&r, Err(CopyError::Uncertain(m)) if m.starts_with("HTTP 500")),
        "{r:?}"
    );
    let c = calls(&s).await;
    assert_eq!(
        c.last().unwrap(),
        &format!("PUT /v2/dst/manifests/{TAG}"),
        "{c:#?}"
    );
    assert_eq!(requests(&s, "PUT").await.len(), 1, "never retried");
}

#[tokio::test]
async fn copy_cross_registry_credentials_stay_on_origin() {
    let (src, dst) = (MockServer::start().await, MockServer::start().await);
    probe(
        &src,
        ResponseTemplate::new(401).insert_header("www-authenticate", "Basic"),
    )
    .await;
    probe(
        &dst,
        ResponseTemplate::new(401).insert_header("www-authenticate", "Basic"),
    )
    .await;
    let img = image(b"abc");
    serve_image(&src, "src", &img).await;
    dst_ok(&dst, "dst", &img.digest).await;
    let basic = |u: &str, p: &str| Credential::Basic {
        username: u.into(),
        password: Secret(p.into()),
    };
    let (sc, dc) = (
        client(&src, basic("a", "pa")),
        client(&dst, basic("b", "pb")),
    );
    let flag = AtomicBool::new(false);
    let mut r = run(&sc, &dc, &flag);
    r.chunk = 1024;

    r.go(&img.digest, &Steps::default()).await.unwrap();

    // base64("a:pa"), base64("b:pb")
    let (a, b) = ("Basic YTpwYQ==", "Basic YjpwYg==");
    for (s, want, n) in [(&src, a, 3), (&dst, b, 8)] {
        let reqs: Vec<_> = s
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() != "/v2/")
            .collect();
        assert_eq!(reqs.len(), n, "{:#?}", calls(s).await);
        for r in &reqs {
            assert_eq!(
                header(r, "authorization").as_deref(),
                Some(want),
                "{} {}",
                r.method,
                r.url
            );
        }
    }
}

#[tokio::test]
async fn copy_sets_manifest_flag_before_first_put() {
    let s = MockServer::start().await;
    probe(&s, ResponseTemplate::new(200)).await;
    let img = image(b"abc");
    serve_image(&s, "src", &img).await;
    let flag = Arc::new(AtomicBool::new(false));
    let blob_seen = Arc::new(Mutex::new(Vec::new()));
    let (f, seen) = (flag.clone(), blob_seen.clone());
    Mock::given(path_regex("^/v2/dst/blobs/"))
        .respond_with(move |r: &Request| {
            seen.lock().unwrap().push(f.load(Ordering::SeqCst));
            match r.method.as_str() {
                "HEAD" => ResponseTemplate::new(404),
                "POST" => ResponseTemplate::new(202)
                    .insert_header("location", "/v2/dst/blobs/uploads/u1?_state=x"),
                _ => ResponseTemplate::new(201),
            }
        })
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/dst/manifests/{TAG}")))
        .respond_with(ResponseTemplate::new(201).set_delay(Duration::from_millis(300)))
        .mount(&s)
        .await;
    dst_ok(&s, "dst", &img.digest).await;
    let c = anon(&s);
    let mut r = run(&c, &c, &flag);
    r.chunk = 1024;

    let steps = Steps::default();
    let watch = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if calls(&s)
                .await
                .iter()
                .any(|c| c.starts_with("PUT /v2/dst/manifests/"))
            {
                return flag.load(Ordering::SeqCst);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let (res, during_put) = tokio::join!(r.go(&img.digest, &steps), watch);
    res.unwrap();
    let during_put = during_put.expect("no manifest PUT seen");
    assert!(during_put, "flag is set while the manifest PUT is pending");
    let seen = blob_seen.lock().unwrap().clone();
    // HEAD, POST, PUT for each of config and layer
    assert_eq!(seen, [false; 6]);
}

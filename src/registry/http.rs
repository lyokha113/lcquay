//! HTTP client and capped body reads (spec §8 timeouts).

use std::fmt::Write as _;
use std::time::Duration;

use bytes::{Bytes, BytesMut};

use crate::domain::{Digest, RegistryError};

/// Manifests and config blobs larger than this are refused.
pub const BODY_CAP: usize = 4 * 1024 * 1024;

/// 30 s per request including the body; rustls; reqwest's default redirect
/// policy (strips `Authorization` on cross-origin redirects); no automatic
/// retries, so a DELETE is never resent behind our back (e.g. on HTTP/2
/// `REFUSED_STREAM`).
pub fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .retry(reqwest::retry::never())
        .build()
        .expect("reqwest client builds with static config")
}

/// Longest silence while streaming a blob (spec §13).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Whole-request timeout of each write request.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(300);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// For blob downloads: no whole-request timeout, but each read may wait at
/// most `idle`. Default redirect policy, never retries.
pub fn build_stream_client(idle: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .read_timeout(idle)
        .connect_timeout(CONNECT_TIMEOUT)
        .retry(reqwest::retry::never())
        .build()
        .expect("reqwest client builds with static config")
}

/// For write requests: never follows a redirect (reqwest would resend the
/// body on a 307/308) and never retries. Each request sets `WRITE_TIMEOUT`.
pub fn build_write_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .retry(reqwest::retry::never())
        .build()
        .expect("reqwest client builds with static config")
}

/// SHA-256 of streamed bytes, as a `sha256:<hex>` digest.
#[derive(Default)]
pub struct DigestCheck(sha2::Sha256);

impl DigestCheck {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, b: &[u8]) {
        sha2::Digest::update(&mut self.0, b);
    }

    pub fn finish(self) -> Digest {
        let mut s = String::from("sha256:");
        for b in sha2::Digest::finalize(self.0) {
            let _ = write!(s, "{b:02x}");
        }
        Digest::parse(&s).expect("sha256 hex is a valid digest")
    }
}

/// Reads the body, failing with `TooLarge` once it exceeds `cap` bytes
/// (checked while streaming, so a missing or lying `Content-Length` is fine).
pub async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Result<Bytes, RegistryError> {
    let mut buf = BytesMut::new();
    while let Some(chunk) = resp.chunk().await.map_err(http_error)? {
        if buf.len() + chunk.len() > cap {
            return Err(RegistryError::TooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

/// Text of the innermost `source()` (reqwest's Display omits it), cut at any `?` so a
/// URL query can never leak. `None` when there is no source.
pub fn root_cause(e: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut cur = e.source()?;
    while let Some(next) = cur.source() {
        cur = next;
    }
    Some(cur.to_string().split('?').next().unwrap_or("").to_string())
}

/// Maps a transport error, dropping the URL so query strings never reach
/// messages or logs.
pub fn http_error(e: reqwest::Error) -> RegistryError {
    let timeout = e.is_timeout();
    let e = e.without_url();
    let msg = match root_cause(&e) {
        Some(r) => format!("{e}: {r}"),
        None => e.to_string(),
    };
    if timeout {
        RegistryError::Timeout(msg)
    } else {
        RegistryError::Network(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RegistryError;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn serve(len: usize) -> reqwest::Response {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; len]))
            .mount(&server)
            .await;
        build_http_client().get(server.uri()).send().await.unwrap()
    }

    #[tokio::test]
    async fn connect_error_has_reason_and_no_query() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let e = build_http_client()
            .get(format!("http://127.0.0.1:{port}/x?secret=1"))
            .send()
            .await
            .unwrap_err();
        let RegistryError::Network(m) = http_error(e) else {
            panic!("not Network")
        };
        assert!(m.to_lowercase().contains("refused"), "{m}");
        assert!(!m.contains('?') && !m.contains("secret"), "{m}");
    }

    #[tokio::test]
    async fn read_capped_rejects_large() {
        let r = read_capped(serve(BODY_CAP + 1).await, BODY_CAP).await;
        assert_eq!(r, Err(RegistryError::TooLarge));
    }

    #[tokio::test]
    async fn read_capped_accepts_at_cap() {
        let b = read_capped(serve(BODY_CAP).await, BODY_CAP).await.unwrap();
        assert_eq!(b.len(), BODY_CAP);
    }

    #[test]
    fn digest_check_vectors() {
        let hash = |parts: &[&str]| {
            let mut d = DigestCheck::new();
            parts.iter().for_each(|p| d.update(p.as_bytes()));
            d.finish().to_string()
        };
        assert_eq!(
            hash(&[]),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let abc = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(hash(&["abc"]), abc);
        assert_eq!(hash(&["a", "", "bc"]), abc);
    }

    /// Raw HTTP/1.1 server (real time): `/steady` sends 10 body bytes one per
    /// 100 ms; `/gap` sends one byte, waits 300 ms, then the rest.
    fn trickle_server() -> std::net::SocketAddr {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for sock in l.incoming() {
                let mut sock = sock.unwrap();
                sock.set_nodelay(true).unwrap();
                std::thread::spawn(move || {
                    let (mut req, mut buf) = (Vec::new(), [0u8; 1024]);
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = sock.read(&mut buf).unwrap();
                        if n == 0 {
                            return;
                        }
                        req.extend_from_slice(&buf[..n]);
                    }
                    let gap = req.starts_with(b"GET /gap ");
                    let head = "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n";
                    sock.write_all(head.as_bytes()).unwrap();
                    for i in 0..10 {
                        let pause = if gap && i == 1 { 1500 } else { 50 };
                        if i > 0 {
                            std::thread::sleep(Duration::from_millis(pause));
                        }
                        // the client may already have given up
                        if sock.write_all(b"x").is_err() {
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn stream_client_idle_timeout() {
        let addr = trickle_server();
        let client = build_stream_client(Duration::from_millis(600));
        let read_all = |p: &'static str| {
            let rb = client.get(format!("http://{addr}{p}"));
            async move {
                let mut resp = rb.send().await.map_err(http_error)?;
                let mut n = 0;
                while let Some(c) = resp.chunk().await.map_err(http_error)? {
                    n += c.len();
                }
                Ok::<_, RegistryError>(n)
            }
        };
        assert_eq!(read_all("/steady").await, Ok(10));
        let r = read_all("/gap").await;
        assert!(matches!(r, Err(RegistryError::Timeout(_))), "{r:?}");
    }
}

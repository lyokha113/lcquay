//! Copies (or retags) one image between repositories: blobs first (HEAD skip,
//! cross-repo mount, chunked upload), then child manifests, then the tag, then
//! a verify `HEAD`. Every write is sent once; the first error stops the copy.
//! Upload `Location`s carry secret `_state` queries: they go to `write` only,
//! never into progress or error text.

use std::sync::atomic::{AtomicBool, Ordering};

use bytes::{Bytes, BytesMut};
use reqwest::Method;
use url::Url;

use super::{DeleteError, DigestCheck, OciClient, RawResponse, WriteBody, parse_manifest};
use crate::domain::{
    Descriptor, Digest, ItemStep, ManifestKind, Reference, RegistryError, RepoPath, TransferStep,
};

/// Upload part size; a blob of at most this many bytes goes up in one `PUT`.
pub const CHUNK: usize = 16 * 1024 * 1024;

pub struct CopyJob<'a> {
    pub src: &'a OciClient,
    pub src_repo: &'a RepoPath,
    pub dst: &'a OciClient,
    pub dst_repo: &'a RepoPath,
    pub digest: &'a Digest,
    pub tag: &'a str,
    pub same_registry: bool,
    pub chunk: usize,
    /// Set just before the first manifest `PUT` is dispatched.
    pub manifest_sent: &'a AtomicBool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CopyError {
    Failed(String),
    Uncertain(String),
    DigestDiffers { got: Digest, want: Digest },
}

impl From<DeleteError> for CopyError {
    fn from(e: DeleteError) -> Self {
        match e {
            DeleteError::NotSent(e) => CopyError::Failed(e.to_string()),
            DeleteError::Rejected(m) => CopyError::Failed(m),
            DeleteError::Unsupported => CopyError::Failed("unsupported by the registry".into()),
            DeleteError::Uncertain(m) => CopyError::Uncertain(m),
        }
    }
}

impl From<RegistryError> for CopyError {
    fn from(e: RegistryError) -> Self {
        CopyError::Failed(e.to_string())
    }
}

/// A source manifest exactly as served.
struct Manifest {
    digest: Digest,
    bytes: Bytes,
    media_type: String,
}

pub async fn copy_image(
    job: &CopyJob<'_>,
    progress: &(dyn Fn(TransferStep) + Sync),
) -> Result<(), CopyError> {
    let retag = job.same_registry && job.src_repo == job.dst_repo;
    let mut manifests = Vec::new();
    let mut blobs = Vec::new();
    // a retag needs only the top manifest: children and blobs are already there
    visit(job, job.digest, !retag, &mut manifests, &mut blobs).await?;

    if !retag {
        for b in &blobs {
            progress(TransferStep {
                id: b.digest.short(),
                step: ItemStep::Waiting {
                    total: Some(b.size),
                },
            });
        }
        for b in &blobs {
            copy_blob(job, b, progress).await?;
        }
    }

    job.manifest_sent.store(true, Ordering::SeqCst);
    let (top, children) = manifests
        .split_last()
        .expect("visit pushes the top manifest");
    if !retag {
        for m in children {
            put_manifest(job, &m.digest.to_string(), m).await?;
        }
    }
    put_manifest(job, job.tag, top).await?;

    let (got, _) = job
        .dst
        .head_manifest(job.dst_repo, &Reference::Tag(job.tag.into()))
        .await
        // the tag PUT succeeded: the image is most likely there
        .map_err(|e| CopyError::Uncertain(format!("copied, but the check failed: {e}")))?;
    if &got != job.digest {
        return Err(CopyError::DigestDiffers {
            got,
            want: job.digest.clone(),
        });
    }
    Ok(())
}

/// Pushes `d` after its children (when `children`); collects the distinct
/// blobs of every image manifest in first-seen order.
async fn visit(
    job: &CopyJob<'_>,
    d: &Digest,
    children: bool,
    out: &mut Vec<Manifest>,
    blobs: &mut Vec<Descriptor>,
) -> Result<(), CopyError> {
    let (bytes, mt) = job.src.manifest_bytes(job.src_repo, d).await?;
    let mut check = DigestCheck::new();
    check.update(&bytes);
    if &check.finish() != d {
        return Err(CopyError::Failed("source manifest digest mismatch".into()));
    }
    let doc = parse_manifest(&bytes, Some(mt.clone()).filter(|t| !t.is_empty()))?;
    match doc.kind {
        ManifestKind::Index { manifests } if children => {
            for c in manifests {
                if !out.iter().any(|m| m.digest == c.digest) {
                    Box::pin(visit(job, &c.digest, true, out, blobs)).await?;
                }
            }
        }
        ManifestKind::Image { config, layers } => {
            for b in std::iter::once(config).chain(layers) {
                if !blobs.iter().any(|x| x.digest == b.digest) {
                    blobs.push(b);
                }
            }
        }
        ManifestKind::Index { .. } => {}
    }
    let media_type = if mt.is_empty() { doc.media_type } else { mt };
    out.push(Manifest {
        digest: d.clone(),
        bytes,
        media_type,
    });
    Ok(())
}

async fn put_manifest(job: &CopyJob<'_>, reference: &str, m: &Manifest) -> Result<(), CopyError> {
    let body = WriteBody {
        bytes: m.bytes.clone(),
        content_type: Some(m.media_type.clone()),
        content_range: None,
    };
    let path = format!("/v2/{}/manifests/{reference}", job.dst_repo);
    job.dst
        .write(Method::PUT, &path, job.dst_repo, Some(body))
        .await?;
    Ok(())
}

async fn copy_blob(
    job: &CopyJob<'_>,
    b: &Descriptor,
    progress: &(dyn Fn(TransferStep) + Sync),
) -> Result<(), CopyError> {
    let short = b.digest.short();
    let step = |step| {
        progress(TransferStep {
            id: short.clone(),
            step,
        })
    };
    let (dst, repo) = (job.dst, job.dst_repo);
    if dst.head_blob(repo, &b.digest).await? {
        step(ItemStep::Exists);
        return Ok(());
    }

    let uploads = format!("/v2/{repo}/blobs/uploads/");
    let resp = if job.same_registry {
        let target = format!("{uploads}?mount={}&from={}", b.digest, job.src_repo);
        let resp = dst
            .write(Method::POST, &target, repo, Some(empty()))
            .await?;
        if resp.status.as_u16() == 201 {
            step(ItemStep::Mounted);
            return Ok(());
        }
        resp // 202: the registry opened an upload session instead
    } else {
        dst.write(Method::POST, &uploads, repo, Some(empty()))
            .await?
    };
    let min = resp
        .headers
        .get("oci-chunk-min-length")
        .and_then(|v| v.to_str().ok()?.parse().ok())
        .unwrap_or(0);
    let chunk = job.chunk.max(min).max(1);
    let mut loc = location(&resp)?;

    let mut stream = job.src.blob_stream(job.src_repo, &b.digest).await?;
    let mut hash = DigestCheck::new();
    let mut buf = BytesMut::new();
    let mut off = 0usize;
    let monolithic = b.size <= chunk as u64;
    while let Some(c) = stream.chunk().await? {
        hash.update(&c);
        buf.extend_from_slice(&c);
        step(ItemStep::Bytes {
            current: (off + buf.len()) as u64,
            total: b.size,
        });
        if monolithic && buf.len() > chunk {
            return Err(CopyError::Failed(format!(
                "source blob {short} is larger than its descriptor"
            )));
        }
        while !monolithic && buf.len() >= chunk {
            let part = buf.split_to(chunk).freeze();
            loc = patch(dst, repo, &loc, off, part).await?;
            off += chunk;
        }
    }
    drop(stream); // frees its gate permit
    if hash.finish() != b.digest {
        return Err(CopyError::Failed(format!(
            "source blob {short} digest mismatch"
        )));
    }

    let body = if monolithic {
        WriteBody {
            bytes: buf.freeze(),
            content_type: Some(OCTET.into()),
            content_range: None,
        }
    } else {
        if !buf.is_empty() {
            loc = patch(dst, repo, &loc, off, buf.freeze()).await?;
        }
        empty()
    };
    let mut url = loc;
    let q = match url.query() {
        Some(q) => format!("{q}&digest={}", b.digest),
        None => format!("digest={}", b.digest),
    };
    url.set_query(Some(&q));
    dst.write(Method::PUT, url.as_str(), repo, Some(body))
        .await?;
    step(ItemStep::Done);
    Ok(())
}

const OCTET: &str = "application/octet-stream";

fn empty() -> WriteBody {
    WriteBody {
        bytes: Bytes::new(),
        content_type: None,
        content_range: None,
    }
}

/// `PATCH` `part` at byte `off`; returns the next upload location.
async fn patch(
    dst: &OciClient,
    repo: &RepoPath,
    loc: &Url,
    off: usize,
    part: Bytes,
) -> Result<Url, CopyError> {
    let body = WriteBody {
        content_range: Some(format!("{off}-{}", off + part.len() - 1)),
        content_type: Some(OCTET.into()),
        bytes: part,
    };
    let resp = dst
        .write(Method::PATCH, loc.as_str(), repo, Some(body))
        .await?;
    location(&resp)
}

/// The response's `Location`, resolved against its URL. Never put in a message.
fn location(resp: &RawResponse) -> Result<Url, CopyError> {
    resp.headers
        .get("location")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| resp.url.join(v).ok())
        .ok_or_else(|| CopyError::Failed("registry returned no upload location".into()))
}

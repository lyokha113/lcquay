//! Export one image to an OCI image layout tarball (`X`): `oci-layout`, the blobs, `index.json`
//! and, for a single platform, a `docker save`-style `manifest.json` so the classic image store
//! can `docker load` it. Each blob is streamed through a digest check straight into
//! `<path>.part`, which is renamed to `<path>` only after an `fsync`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use serde_json::json;

use crate::domain::{
    Descriptor, Digest, ItemStep, ManifestDocument, ManifestKind, RegistryError, RepoPath,
    TransferStep,
};
use crate::registry::{DigestCheck, OciClient, parse_manifest};

/// The largest entry the 11 octal digits of a ustar size field hold (8 GiB - 1).
const MAX_SIZE: u64 = 0o77_777_777_777;
const MT_INDEX: &str = "application/vnd.oci.image.index.v1+json";

pub struct ExportJob<'a> {
    pub oci: &'a OciClient,
    pub repo: &'a RepoPath,
    /// The selected (top-level) manifest.
    pub top: &'a Digest,
    /// The index child to export; `None` = the image itself, or every platform of an index.
    pub platform: Option<&'a Digest>,
    pub tag: Option<&'a str>,
    /// `<ref_host>/<repo>:<tag>` or `@<digest>`.
    pub full_ref: &'a str,
    pub path: &'a Path,
    /// An existing `<path>` may be replaced.
    pub overwrite: bool,
}

#[derive(Debug, PartialEq)]
pub enum ExportError {
    /// `<path>` exists and `overwrite` is off; nothing was requested or written.
    Exists,
    Failed(String),
}

impl From<RegistryError> for ExportError {
    fn from(e: RegistryError) -> Self {
        ExportError::Failed(e.to_string())
    }
}

/// A `blobs/` entry in archive order: a manifest held in memory, or a blob to stream.
struct Entry {
    digest: Digest,
    size: u64,
    manifest: Option<Bytes>,
}

/// Writes the archive; returns its size in bytes. On any error `<path>.part` is removed and an
/// existing `<path>` is left as it was.
pub async fn export(
    job: &ExportJob<'_>,
    progress: &(dyn Fn(TransferStep) + Sync),
) -> Result<u64, ExportError> {
    if job.path.exists() && !job.overwrite {
        return Err(ExportError::Exists);
    }
    let part = part_path(job.path);
    let r = write_part(job, &part, progress).await;
    if r.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    r
}

async fn write_part(
    job: &ExportJob<'_>,
    part: &Path,
    progress: &(dyn Fn(TransferStep) + Sync),
) -> Result<u64, ExportError> {
    let top = job.platform.unwrap_or(job.top);
    let mut entries = vec![];
    let (media_type, doc, size) = visit(job, top, &mut entries).await?;
    for e in &entries {
        if e.size > MAX_SIZE {
            let m = format!(
                "blob {} is larger than 8 GiB; export unsupported",
                e.digest.short()
            );
            return Err(ExportError::Failed(m));
        }
    }
    for e in entries.iter().filter(|e| e.manifest.is_none()) {
        progress(TransferStep {
            id: e.digest.short(),
            step: ItemStep::Waiting {
                total: Some(e.size),
            },
        });
    }

    // Plain blocking file calls: an abort lands only on an `.await`, so nothing touches the file
    // once the worker is gone and the supervisor's `.part` cleanup is final.
    // ponytail: writes and the fsync block a runtime thread; spawn_blocking if that ever shows.
    let io = |e| io_error(part, e);
    let mut tar = Tar {
        out: BufWriter::new(File::create(part).map_err(io)?),
        len: 0,
        part,
    };
    tar.entry("oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    for e in &entries {
        let name = blob_name(&e.digest);
        match &e.manifest {
            Some(bytes) => tar.entry(&name, bytes)?,
            None => stream_blob(job, &mut tar, &name, e, progress).await?,
        }
    }

    let mut annotations = serde_json::Map::new();
    if let Some(t) = job.tag {
        annotations.insert("org.opencontainers.image.ref.name".into(), t.into());
    }
    annotations.insert("io.containerd.image.name".into(), job.full_ref.into());
    let index = json!({
        "schemaVersion": 2,
        "mediaType": MT_INDEX,
        "manifests": [{"mediaType": media_type, "digest": top.to_string(), "size": size,
                       "annotations": annotations}],
    });
    tar.entry("index.json", index.to_string().as_bytes())?;
    if let ManifestKind::Image { config, layers } = &doc.kind {
        // `docker load` rejects a digest reference in RepoTags.
        let tags: Vec<&str> = job.tag.map(|_| job.full_ref).into_iter().collect();
        let layers: Vec<String> = layers.iter().map(|l| blob_name(&l.digest)).collect();
        let m = json!([{"Config": blob_name(&config.digest), "RepoTags": tags, "Layers": layers}]);
        tar.entry("manifest.json", m.to_string().as_bytes())?;
    }
    tar.write(&[0; 1024])?;
    let file = tar.out.into_inner().map_err(|e| io(e.into_error()))?;
    file.sync_all().map_err(io)?;
    drop(file);
    std::fs::rename(part, job.path).map_err(|e| io_error(job.path, e))?;
    Ok(tar.len)
}

/// Appends the blobs of `d` (an image: config and layers; an index: each child with its blobs),
/// then `d` itself, skipping digests already in `out`. Returns `d`'s media type, document and
/// size.
async fn visit(
    job: &ExportJob<'_>,
    d: &Digest,
    out: &mut Vec<Entry>,
) -> Result<(String, ManifestDocument, u64), ExportError> {
    let (bytes, mt) = job.oci.manifest_bytes(job.repo, d).await?;
    let mut check = DigestCheck::new();
    check.update(&bytes);
    if &check.finish() != d {
        return Err(mismatch(d));
    }
    let doc = parse_manifest(&bytes, Some(mt.clone()).filter(|t| !t.is_empty()))?;
    let seen = |out: &Vec<Entry>, d: &Digest| out.iter().any(|e| &e.digest == d);
    match &doc.kind {
        ManifestKind::Index { manifests } => {
            for c in manifests {
                if !seen(out, &c.digest) {
                    Box::pin(visit(job, &c.digest, out)).await?;
                }
            }
        }
        ManifestKind::Image { config, layers } => {
            for b in std::iter::once(config).chain(layers) {
                if !seen(out, &b.digest) {
                    out.push(blob(b));
                }
            }
        }
    }
    let size = bytes.len() as u64;
    if !seen(out, d) {
        out.push(Entry {
            digest: d.clone(),
            size,
            manifest: Some(bytes),
        });
    }
    let media_type = if mt.is_empty() {
        doc.media_type.clone()
    } else {
        mt
    };
    Ok((media_type, doc, size))
}

fn blob(b: &Descriptor) -> Entry {
    Entry {
        digest: b.digest.clone(),
        size: b.size,
        manifest: None,
    }
}

fn mismatch(d: &Digest) -> ExportError {
    ExportError::Failed(format!("digest mismatch for {}", d.short()))
}

fn blob_name(d: &Digest) -> String {
    format!("blobs/{}/{}", d.algorithm(), d.hex())
}

/// Streams `e` into its entry; its bytes must hash to its digest and match its size.
async fn stream_blob(
    job: &ExportJob<'_>,
    tar: &mut Tar<'_>,
    name: &str,
    e: &Entry,
    progress: &(dyn Fn(TransferStep) + Sync),
) -> Result<(), ExportError> {
    let step = |step| {
        progress(TransferStep {
            id: e.digest.short(),
            step,
        })
    };
    tar.header(name, e.size)?;
    let mut stream = job.oci.blob_stream(job.repo, &e.digest).await?;
    let mut hash = DigestCheck::new();
    let mut n = 0u64;
    while let Some(c) = stream.chunk().await? {
        n += c.len() as u64;
        // More bytes than the header announced would corrupt the archive.
        if n > e.size {
            return Err(mismatch(&e.digest));
        }
        hash.update(&c);
        tar.write(&c)?;
        step(ItemStep::Bytes {
            current: n,
            total: e.size,
        });
    }
    drop(stream); // frees its gate permit
    if n != e.size || hash.finish() != e.digest {
        return Err(mismatch(&e.digest));
    }
    tar.pad()?;
    step(ItemStep::Done);
    Ok(())
}

fn io_error(part: &Path, e: std::io::Error) -> ExportError {
    ExportError::Failed(format!("cannot write {}: {e}", part.display()))
}

/// A ustar stream into `part`: 512-byte headers, data padded to 512.
struct Tar<'a> {
    out: BufWriter<File>,
    len: u64,
    part: &'a Path,
}

impl Tar<'_> {
    fn write(&mut self, b: &[u8]) -> Result<(), ExportError> {
        self.out.write_all(b).map_err(|e| io_error(self.part, e))?;
        self.len += b.len() as u64;
        Ok(())
    }

    fn header(&mut self, name: &str, size: u64) -> Result<(), ExportError> {
        let h = ustar_header(name, size).map_err(ExportError::Failed)?;
        self.write(&h)
    }

    fn pad(&mut self) -> Result<(), ExportError> {
        let rest = (512 - self.len % 512) % 512;
        self.write(&[0; 512][..rest as usize])
    }

    fn entry(&mut self, name: &str, data: &[u8]) -> Result<(), ExportError> {
        self.header(name, data.len() as u64)?;
        self.write(data)?;
        self.pad()
    }
}

/// A ustar header of a regular file: mode 0644, uid/gid 0, mtime 0.
pub fn ustar_header(name: &str, size: u64) -> Result<[u8; 512], String> {
    if name.len() > 100 {
        return Err(format!("tar entry name longer than 100 bytes: {name}"));
    }
    if size > MAX_SIZE {
        return Err(format!("tar entry {name} is larger than 8 GiB"));
    }
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..108].copy_from_slice(b"0000644\0");
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    h[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    h[136..148].copy_from_slice(b"00000000000\0");
    h[148..156].fill(b' '); // the checksum counts its own field as spaces
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    Ok(h)
}

/// `<path>.part`.
pub fn part_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// `~/x` → `$HOME/x`; anything else as is.
pub fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), directories::BaseDirs::new()) {
        (Some(rest), Some(d)) => d.home_dir().join(rest),
        _ => PathBuf::from(p),
    }
}

/// `~/Downloads/eng-api-fo_1.0.tar`, untagged `…_<digest.short()>.tar`.
pub fn default_path(repo: &RepoPath, tag: Option<&str>, digest: &Digest) -> String {
    let name = repo.0.replace('/', "-");
    let suffix = tag.map_or_else(|| digest.short(), str::to_owned);
    format!("~/Downloads/{name}_{suffix}.tar")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};
    use url::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::AnonymousSource;
    use crate::domain::ItemStep;
    use crate::registry::{DigestCheck, Gate};

    const MT_IMG: &str = "application/vnd.oci.image.manifest.v1+json";
    const MT_IDX: &str = "application/vnd.oci.image.index.v1+json";
    const REF: &str = "h.example/app:1.0";

    fn sha(b: &[u8]) -> Digest {
        let mut c = DigestCheck::new();
        c.update(b);
        c.finish()
    }

    fn blob_name(d: &Digest) -> String {
        format!("blobs/sha256/{}", d.hex())
    }

    /// `(name, data)` of each entry; asserts the two zero end blocks.
    fn read_tar(b: &[u8]) -> Vec<(String, Vec<u8>)> {
        assert_eq!(b.len() % 512, 0);
        let mut v = vec![];
        let mut off = 0;
        while b[off..off + 512].iter().any(|x| *x != 0) {
            let h = &b[off..off + 512];
            let name = h[..100].split(|c| *c == 0).next().unwrap();
            let size = std::str::from_utf8(&h[124..135]).unwrap();
            let size = usize::from_str_radix(size, 8).unwrap();
            off += 512;
            v.push((
                String::from_utf8(name.to_vec()).unwrap(),
                b[off..off + size].to_vec(),
            ));
            off += size.div_ceil(512) * 512;
        }
        assert_eq!(&b[off..], &[0u8; 1024][..], "two zero end blocks");
        v
    }

    fn names(entries: &[(String, Vec<u8>)]) -> Vec<&str> {
        entries.iter().map(|(n, _)| n.as_str()).collect()
    }

    fn json_of(entries: &[(String, Vec<u8>)], name: &str) -> Option<Value> {
        let (_, d) = entries.iter().find(|(n, _)| n == name)?;
        Some(serde_json::from_slice(d).unwrap())
    }

    struct Image {
        config: Vec<u8>,
        layer: Vec<u8>,
        bytes: Vec<u8>,
        digest: Digest,
    }

    fn image(arch: &str, layer: &[u8]) -> Image {
        let config = format!(r#"{{"architecture":"{arch}","os":"linux"}}"#).into_bytes();
        let bytes = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": MT_IMG,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": sha(&config).to_string(), "size": config.len()},
            "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                        "digest": sha(layer).to_string(), "size": layer.len()}],
        }))
        .unwrap();
        Image {
            digest: sha(&bytes),
            config,
            layer: layer.to_vec(),
            bytes,
        }
    }

    /// An index over `children` (amd64, arm64): bytes and digest.
    fn index(children: &[&Image]) -> (Vec<u8>, Digest) {
        let ms: Vec<Value> = children
            .iter()
            .zip(["amd64", "arm64"])
            .map(|(c, arch)| {
                json!({"mediaType": MT_IMG, "digest": c.digest.to_string(),
                       "size": c.bytes.len(), "platform": {"os": "linux", "architecture": arch}})
            })
            .collect();
        let bytes =
            serde_json::to_vec(&json!({"schemaVersion": 2, "mediaType": MT_IDX, "manifests": ms}))
                .unwrap();
        let d = sha(&bytes);
        (bytes, d)
    }

    async fn server() -> MockServer {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&s)
            .await;
        s
    }

    async fn serve(s: &MockServer, p: String, body: &[u8], mt: Option<&str>) {
        let resp = match mt {
            Some(mt) => ResponseTemplate::new(200).set_body_raw(body.to_vec(), mt),
            None => ResponseTemplate::new(200).set_body_bytes(body.to_vec()),
        };
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(resp)
            .mount(s)
            .await;
    }

    /// Manifest, config and layer of `img` in repo `app`.
    async fn serve_image(s: &MockServer, img: &Image) {
        let m = format!("/v2/app/manifests/{}", img.digest);
        serve(s, m, &img.bytes, Some(MT_IMG)).await;
        serve_blob(s, &sha(&img.config), &img.config).await;
        serve_blob(s, &sha(&img.layer), &img.layer).await;
    }

    async fn serve_blob(s: &MockServer, d: &Digest, body: &[u8]) {
        serve(s, format!("/v2/app/blobs/{d}"), body, None).await;
    }

    fn client(s: &MockServer) -> OciClient {
        OciClient::new(
            Url::parse(&s.uri()).unwrap(),
            vec![],
            Arc::new(AnonymousSource),
            Arc::new(Gate::new(8)),
        )
    }

    struct Run {
        oci: OciClient,
        repo: RepoPath,
        dir: tempfile::TempDir,
    }

    impl Run {
        fn new(s: &MockServer) -> Self {
            Run {
                oci: client(s),
                repo: RepoPath("app".into()),
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn path(&self) -> PathBuf {
            self.dir.path().join("out.tar")
        }

        async fn go(
            &self,
            top: &Digest,
            platform: Option<&Digest>,
            overwrite: bool,
        ) -> (Result<u64, ExportError>, Vec<TransferStep>) {
            let path = self.path();
            let job = ExportJob {
                oci: &self.oci,
                repo: &self.repo,
                top,
                platform,
                tag: Some("1.0"),
                full_ref: REF,
                path: &path,
                overwrite,
            };
            let steps = Mutex::new(vec![]);
            let r = export(&job, &|s| steps.lock().unwrap().push(s)).await;
            (r, steps.into_inner().unwrap())
        }

        fn entries(&self) -> Vec<(String, Vec<u8>)> {
            read_tar(&std::fs::read(self.path()).unwrap())
        }
    }

    /// Every blob's bytes hash to its name.
    fn assert_blobs_hash(entries: &[(String, Vec<u8>)]) {
        for (n, d) in entries {
            if let Some(hex) = n.strip_prefix("blobs/sha256/") {
                assert_eq!(sha(d).hex(), hex, "{n}");
            }
        }
    }

    fn index_entry(entries: &[(String, Vec<u8>)]) -> Value {
        let v = json_of(entries, "index.json").unwrap();
        assert_eq!(v["schemaVersion"], 2);
        assert_eq!(v["mediaType"], MT_IDX);
        assert_eq!(v["manifests"].as_array().unwrap().len(), 1);
        v["manifests"][0].clone()
    }

    #[test]
    fn ustar_header_fields() {
        let h = ustar_header("blobs/sha256/abc", 10).unwrap();
        assert_eq!(&h[..16], b"blobs/sha256/abc");
        assert!(h[16..100].iter().all(|b| *b == 0));
        assert_eq!(&h[124..136], b"00000000012\0");
        assert_eq!(&h[257..263], b"ustar\0");
        assert_eq!(&h[263..265], b"00");
        // The checksum counts its own field as spaces.
        let sum: u32 = h
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    *b as u32
                }
            })
            .sum();
        let field = std::str::from_utf8(&h[148..154]).unwrap();
        assert_eq!(u32::from_str_radix(field, 8).unwrap(), sum);
        assert_eq!(&h[154..156], b"\0 ");

        assert!(ustar_header(&"x".repeat(101), 1).is_err());
        assert!(ustar_header(&"x".repeat(100), 1).is_ok());
        assert!(ustar_header("x", 8 << 30).is_err());
        assert!(ustar_header("x", (8 << 30) - 1).is_ok());
    }

    #[test]
    fn default_path_forms() {
        let repo = RepoPath("eng/api/fo".into());
        let d = sha(b"x");
        assert_eq!(
            default_path(&repo, Some("1.0"), &d),
            "~/Downloads/eng-api-fo_1.0.tar"
        );
        assert_eq!(
            default_path(&repo, None, &d),
            format!("~/Downloads/eng-api-fo_{}.tar", d.short())
        );
        assert_eq!(d.short().len(), 12);
    }

    #[test]
    fn paths() {
        assert_eq!(
            part_path(Path::new("/t/a.tar")),
            PathBuf::from("/t/a.tar.part")
        );
        assert_eq!(expand_home("/t/a.tar"), PathBuf::from("/t/a.tar"));
        assert_eq!(expand_home("a~/b"), PathBuf::from("a~/b"));
        let home = directories::BaseDirs::new().unwrap().home_dir().to_owned();
        assert_eq!(expand_home("~/x/y.tar"), home.join("x/y.tar"));
    }

    #[tokio::test]
    async fn export_single_platform_layout() {
        let s = server().await;
        let img = image("amd64", b"layer-1");
        serve_image(&s, &img).await;
        let run = Run::new(&s);
        let (r, steps) = run.go(&img.digest, None, false).await;
        let size = r.unwrap();
        assert_eq!(size, std::fs::metadata(run.path()).unwrap().len());
        assert!(!part_path(&run.path()).exists());

        let e = run.entries();
        let (cfg, layer) = (sha(&img.config), sha(&img.layer));
        assert_eq!(
            names(&e),
            [
                "oci-layout".to_owned(),
                blob_name(&cfg),
                blob_name(&layer),
                blob_name(&img.digest),
                "index.json".into(),
                "manifest.json".into(),
            ]
        );
        assert_blobs_hash(&e);
        assert_eq!(
            json_of(&e, "oci-layout").unwrap(),
            json!({"imageLayoutVersion": "1.0.0"})
        );
        assert_eq!(
            index_entry(&e),
            json!({"mediaType": MT_IMG, "digest": img.digest.to_string(),
                   "size": img.bytes.len(),
                   "annotations": {"org.opencontainers.image.ref.name": "1.0",
                                   "io.containerd.image.name": REF}})
        );
        assert_eq!(
            json_of(&e, "manifest.json").unwrap(),
            json!([{"Config": blob_name(&cfg), "RepoTags": [REF],
                    "Layers": [blob_name(&layer)]}])
        );
        // Each blob: waiting with its size, bytes, done.
        let short = layer.short();
        let mine: Vec<_> = steps.iter().filter(|s| s.id == short).collect();
        assert_eq!(mine[0].step, ItemStep::Waiting { total: Some(7) });
        assert!(mine.contains(&&TransferStep {
            id: short.clone(),
            step: ItemStep::Bytes {
                current: 7,
                total: 7
            }
        }));
        assert_eq!(mine.last().unwrap().step, ItemStep::Done);
    }

    #[tokio::test]
    async fn export_untagged_has_no_ref_name() {
        let s = server().await;
        let img = image("amd64", b"layer-1");
        serve_image(&s, &img).await;
        let run = Run::new(&s);
        let path = run.path();
        let full = format!("h.example/app@{}", img.digest);
        let job = ExportJob {
            oci: &run.oci,
            repo: &run.repo,
            top: &img.digest,
            platform: None,
            tag: None,
            full_ref: &full,
            path: &path,
            overwrite: false,
        };
        export(&job, &|_| {}).await.unwrap();
        let e = run.entries();
        assert_eq!(
            index_entry(&e)["annotations"],
            json!({"io.containerd.image.name": full})
        );
        // `docker load` rejects a digest reference as a tag.
        assert_eq!(
            json_of(&e, "manifest.json").unwrap()[0]["RepoTags"],
            json!([])
        );
    }

    #[tokio::test]
    async fn export_index_all_platforms() {
        let s = server().await;
        let (a, b) = (image("amd64", b"layer-a"), image("arm64", b"layer-b"));
        let (idx, idx_d) = index(&[&a, &b]);
        serve(&s, format!("/v2/app/manifests/{idx_d}"), &idx, Some(MT_IDX)).await;
        serve_image(&s, &a).await;
        serve_image(&s, &b).await;
        let run = Run::new(&s);
        run.go(&idx_d, None, false).await.0.unwrap();

        let e = run.entries();
        let mut want = vec!["oci-layout".to_owned()];
        for i in [&a, &b] {
            want.extend([sha(&i.config), sha(&i.layer), i.digest.clone()].map(|d| blob_name(&d)));
        }
        want.extend([blob_name(&idx_d), "index.json".into()]);
        assert_eq!(names(&e), want);
        assert_blobs_hash(&e);
        let top = index_entry(&e);
        assert_eq!(
            (&top["mediaType"], &top["digest"], &top["size"]),
            (&json!(MT_IDX), &json!(idx_d.to_string()), &json!(idx.len()))
        );
    }

    #[tokio::test]
    async fn export_index_one_platform() {
        let s = server().await;
        let (a, b) = (image("amd64", b"layer-a"), image("arm64", b"layer-b"));
        let (idx, idx_d) = index(&[&a, &b]);
        serve(&s, format!("/v2/app/manifests/{idx_d}"), &idx, Some(MT_IDX)).await;
        serve_image(&s, &a).await;
        serve_image(&s, &b).await;
        let run = Run::new(&s);
        run.go(&idx_d, Some(&b.digest), false).await.0.unwrap();

        let e = run.entries();
        assert_eq!(
            names(&e),
            [
                "oci-layout".to_owned(),
                blob_name(&sha(&b.config)),
                blob_name(&sha(&b.layer)),
                blob_name(&b.digest),
                "index.json".into(),
                "manifest.json".into(),
            ]
        );
        let top = index_entry(&e);
        assert_eq!(top["digest"], json!(b.digest.to_string()));
        assert_eq!(top["mediaType"], json!(MT_IMG));
    }

    #[tokio::test]
    async fn export_digest_mismatch_removes_part() {
        let s = server().await;
        let img = image("amd64", b"layer-1");
        let m = format!("/v2/app/manifests/{}", img.digest);
        serve(&s, m, &img.bytes, Some(MT_IMG)).await;
        serve_blob(&s, &sha(&img.config), &img.config).await;
        // Same length, other bytes.
        serve_blob(&s, &sha(&img.layer), b"LAYER-1").await;
        let run = Run::new(&s);
        let (r, _) = run.go(&img.digest, None, false).await;
        let want = format!("digest mismatch for {}", sha(&img.layer).short());
        assert_eq!(r, Err(ExportError::Failed(want)));
        assert!(!part_path(&run.path()).exists());
        assert!(!run.path().exists());
    }

    #[tokio::test]
    async fn export_exists_needs_overwrite() {
        let s = server().await;
        let img = image("amd64", b"layer-1");
        let run = Run::new(&s);
        std::fs::write(run.path(), b"old").unwrap();
        // Nothing is served yet: any request would fail the export.
        let (r, _) = run.go(&img.digest, None, false).await;
        assert_eq!(r, Err(ExportError::Exists));
        assert_eq!(std::fs::read(run.path()).unwrap(), b"old");
        let got = s.received_requests().await.unwrap();
        assert!(got.is_empty(), "{got:?}");

        serve_image(&s, &img).await;
        run.go(&img.digest, None, true).await.0.unwrap();
        assert_eq!(names(&run.entries())[0], "oci-layout");
        assert!(!part_path(&run.path()).exists());
    }

    #[tokio::test]
    async fn export_stale_part_does_not_block() {
        let s = server().await;
        let img = image("amd64", b"layer-1");
        serve_image(&s, &img).await;
        let run = Run::new(&s);
        // Longer than the archive: a truncated reuse would leave garbage behind.
        std::fs::write(part_path(&run.path()), vec![7u8; 64 * 1024]).unwrap();
        run.go(&img.digest, None, false).await.0.unwrap();
        assert_eq!(run.entries().len(), 6);
        assert!(!part_path(&run.path()).exists());
    }
}

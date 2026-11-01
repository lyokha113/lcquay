pub mod engine;
pub mod login;

use std::time::Duration;

use async_trait::async_trait;
use bollard::Docker;
use bollard::models::{ImageManifestSummaryKindEnum, ImageSummary};
use bollard::query_parameters::{
    CreateImageOptions, ListImagesOptionsBuilder, PushImageOptions, TagImageOptions,
};
use futures::StreamExt;
use futures::stream::BoxStream;
use time::OffsetDateTime;

use crate::auth::{Credential, Secret};
use crate::domain::{Digest, LocalImage, RegistryError, RepoPath, TransferStep};

const DOCKER_TIMEOUT: Duration = Duration::from_secs(30);
pub const UNAVAILABLE: &str = "Docker not available — push and local check disabled";

#[async_trait]
pub trait LocalImages: Send + Sync {
    async fn list(&self) -> Result<Vec<LocalImage>, RegistryError>;
}

/// Basic credentials for the Engine push (`X-Registry-Auth`). `Debug` is safe: `Secret` prints `***`.
#[derive(Clone, Debug)]
pub struct EngineAuth {
    pub username: String,
    pub password: Secret<String>,
    pub server: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PushStep {
    Status(String),
    Bytes { current: u64, total: u64 },
    LayerDone,
    Digest(Digest),
}

#[async_trait]
pub trait Engine: LocalImages {
    /// `docker tag <image_id> <target>:<tag>`; 30 s budget.
    async fn tag(&self, image_id: &str, target: &str, tag: &str) -> Result<(), RegistryError>;
    /// Push exactly `<target>:<tag>`. An `errorDetail` entry ends the stream with `Err`.
    fn push(
        &self,
        target: &str,
        tag: &str,
        auth: Option<EngineAuth>,
    ) -> BoxStream<'static, Result<PushStep, RegistryError>>;
    /// Pull `image` (`<name>:<tag>` or `<name>@<digest>`), for `platform` (`os/arch[/variant]`)
    /// when given, as per-layer steps. An `errorDetail` entry ends the stream with `Err`.
    fn pull(
        &self,
        image: &str,
        platform: Option<&str>,
        auth: Option<EngineAuth>,
    ) -> BoxStream<'static, Result<TransferStep, RegistryError>>;
}

/// `create_image` parameters: a digest reference goes whole in `fromImage`; a tag reference is
/// split into name and tag (an empty tag would make the daemon pull every tag). An empty
/// `platform` lets the daemon pick its own.
fn pull_params(image: &str, platform: Option<&str>) -> CreateImageOptions {
    let (from, tag) = match image.rsplit_once(':') {
        Some((name, tag)) if !image.contains('@') && !tag.contains('/') => (name, Some(tag)),
        _ => (image, None),
    };
    CreateImageOptions {
        from_image: Some(from.to_string()),
        tag: tag.map(str::to_string),
        platform: platform.unwrap_or_default().to_string(),
        ..Default::default()
    }
}

/// Spec §4.1: the Engine gets Basic only; Anonymous pushes without credentials.
pub fn engine_auth(issued: &Credential, server: &str) -> Result<Option<EngineAuth>, RegistryError> {
    match issued {
        Credential::Basic { username, password } => Ok(Some(EngineAuth {
            username: username.clone(),
            password: password.clone(),
            server: server.to_string(),
        })),
        Credential::Anonymous => Ok(None),
        Credential::Bearer(_) => Err(RegistryError::Internal(
            "bearer credential cannot be used for the Docker Engine".into(),
        )),
    }
}

/// An empty tag makes the daemon push every local tag of the repository.
fn require_tag(tag: &str) -> Result<(), RegistryError> {
    if tag.trim().is_empty() {
        return Err(RegistryError::Internal(
            "push requires an explicit tag".into(),
        ));
    }
    Ok(())
}

pub struct BollardEngine {
    docker: Docker,
}

impl BollardEngine {
    /// Lazy: no network I/O until the first call.
    pub fn connect() -> Result<Self, RegistryError> {
        let docker = Docker::connect_with_local_defaults()
            .map_err(|_| RegistryError::Provider(UNAVAILABLE.into()))?;
        Ok(Self { docker })
    }
}

#[async_trait]
impl LocalImages for BollardEngine {
    async fn list(&self) -> Result<Vec<LocalImage>, RegistryError> {
        // `manifests` (API 1.47+) lists an index's platform manifests; older daemons ignore it.
        let opts = ListImagesOptionsBuilder::new()
            .digests(true)
            .manifests(true)
            .build();
        let fut = self.docker.list_images(Some(opts));
        let images = tokio::time::timeout(DOCKER_TIMEOUT, fut)
            .await
            .map_err(|_| RegistryError::Timeout("docker".into()))?
            .map_err(|_| RegistryError::Provider(UNAVAILABLE.into()))?;
        Ok(images.into_iter().map(to_local).collect())
    }
}

#[async_trait]
impl Engine for BollardEngine {
    async fn tag(&self, image_id: &str, target: &str, tag: &str) -> Result<(), RegistryError> {
        require_tag(tag)?;
        let opts = TagImageOptions {
            repo: Some(target.to_string()),
            tag: Some(tag.to_string()),
        };
        tokio::time::timeout(DOCKER_TIMEOUT, self.docker.tag_image(image_id, Some(opts)))
            .await
            .map_err(|_| RegistryError::Timeout("docker tag".into()))?
            .map_err(engine::tag_error)
    }

    fn push(
        &self,
        target: &str,
        tag: &str,
        auth: Option<EngineAuth>,
    ) -> BoxStream<'static, Result<PushStep, RegistryError>> {
        if let Err(e) = require_tag(tag) {
            return futures::stream::once(std::future::ready(Err(e))).boxed();
        }
        let opts = PushImageOptions {
            tag: Some(tag.to_string()),
            platform: None,
        };
        // bollard's stream owns its request, so it outlives these borrows.
        let stream =
            self.docker
                .push_image(target, Some(opts), auth.map(engine::to_docker_credentials));
        let tag = tag.to_string();
        stream
            .filter_map(move |item| {
                let step = match item {
                    Ok(info) => engine::map_push_info(&info, &tag).transpose(),
                    Err(e) => Some(Err(engine::push_error(e))),
                };
                std::future::ready(step)
            })
            .boxed()
    }

    fn pull(
        &self,
        image: &str,
        platform: Option<&str>,
        auth: Option<EngineAuth>,
    ) -> BoxStream<'static, Result<TransferStep, RegistryError>> {
        let stream = self.docker.create_image(
            Some(pull_params(image, platform)),
            None,
            auth.map(engine::to_docker_credentials),
        );
        stream
            .filter_map(|item| {
                let step = match item {
                    Ok(info) => engine::map_pull_info(&info).transpose(),
                    Err(e) => Some(Err(engine::pull_error(e))),
                };
                std::future::ready(step)
            })
            .boxed()
    }
}

/// Docker reports untagged / undigested images with `<none>` placeholder entries.
fn real(entries: Vec<String>) -> Vec<String> {
    entries
        .into_iter()
        .filter(|e| e != "<none>:<none>" && e != "<none>@<none>")
        .collect()
}

fn to_local(s: ImageSummary) -> LocalImage {
    LocalImage {
        id: s.id,
        repo_tags: real(s.repo_tags),
        repo_digests: real(s.repo_digests),
        manifests: s
            .manifests
            .unwrap_or_default()
            .into_iter()
            .filter(|m| m.available && m.kind == Some(ImageManifestSummaryKindEnum::IMAGE))
            .map(|m| m.id)
            .collect(),
        created: OffsetDateTime::from_unix_timestamp(s.created).ok(),
        size: u64::try_from(s.size).unwrap_or(0),
    }
}

pub fn local_match<'a>(
    images: &'a [LocalImage],
    ref_host: &str,
    repo: &RepoPath,
    digest: &Digest,
) -> Option<&'a LocalImage> {
    let name = format!("{ref_host}/{repo}");
    let want = format!("{name}@{digest}");
    let digest = digest.to_string();
    // A tag's suffix after `name:` never contains `/` or `:`.
    let tagged = |t: &String| {
        t.strip_prefix(&name)
            .and_then(|r| r.strip_prefix(':'))
            .is_some_and(|tag| !tag.contains(['/', ':']))
    };
    let named = |i: &LocalImage| {
        i.repo_tags.iter().any(tagged)
            || i.repo_digests
                .iter()
                .any(|d| d.strip_prefix(&name).is_some_and(|r| r.starts_with('@')))
    };
    // Exact repo digest, or (multi-platform store) a platform manifest of an image of this repo:
    // pushing one platform of a local index uploads that platform's manifest, not the index.
    images
        .iter()
        .find(|i| i.repo_digests.contains(&want))
        .or_else(|| {
            images
                .iter()
                .find(|i| i.manifests.contains(&digest) && named(i))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(id: &str, digests: Vec<String>) -> LocalImage {
        LocalImage {
            id: id.into(),
            repo_tags: vec![],
            repo_digests: digests,
            manifests: vec![],
            created: None,
            size: 0,
        }
    }

    #[test]
    fn require_tag_rejects_blank() {
        for t in ["", "  ", "\t"] {
            assert!(
                matches!(require_tag(t), Err(RegistryError::Internal(_))),
                "{t:?}"
            );
        }
        assert!(require_tag("1.0.1").is_ok());
    }

    #[test]
    fn pull_params_platform() {
        assert_eq!(
            pull_params("h/a:1", Some("linux/arm64")).platform,
            "linux/arm64"
        );
        assert_eq!(pull_params("h/a:1", None).platform, "");
    }

    #[test]
    fn pull_params_split_tag_keep_digest() {
        let p = |i: &str| {
            let o = pull_params(i, None);
            (o.from_image.unwrap(), o.tag)
        };
        assert_eq!(
            p("h:5000/a/b:1.0"),
            ("h:5000/a/b".into(), Some("1.0".into()))
        );
        let d = format!("h:5000/a@sha256:{}", "a".repeat(64));
        assert_eq!(p(&d), (d.clone(), None));
        assert_eq!(p("h:5000/a"), ("h:5000/a".into(), None));
    }

    #[test]
    fn local_match_by_repo_digest() {
        let a = Digest::parse(&format!("sha256:{}", "a".repeat(64))).unwrap();
        let b = Digest::parse(&format!("sha256:{}", "b".repeat(64))).unwrap();
        let images = vec![
            img("one", vec![format!("h/api-svc@{a}")]),
            img("two", vec![format!("h/other@{b}")]),
        ];
        let repo = RepoPath("api-svc".into());
        assert_eq!(
            local_match(&images, "h", &repo, &a).map(|i| i.id.as_str()),
            Some("one")
        );
        assert!(local_match(&images, "h", &RepoPath("other".into()), &a).is_none());
        assert!(local_match(&images, "x", &repo, &a).is_none());
        assert!(local_match(&images, "h", &repo, &b).is_none());
    }

    #[test]
    fn local_match_by_platform_manifest() {
        let idx = Digest::parse(&format!("sha256:{}", "1".repeat(64))).unwrap();
        let a = Digest::parse(&format!("sha256:{}", "a".repeat(64))).unwrap();
        let mut tagged = img("one", vec![format!("h:5100/api-svc@{idx}")]);
        tagged.repo_tags = vec!["h:5100/api-svc:1.0".into()];
        tagged.manifests = vec![a.to_string()];
        let mut by_tag_only = tagged.clone();
        by_tag_only.repo_digests.clear();
        let mut elsewhere = tagged.clone();
        elsewhere.repo_tags = vec!["h:5100/api-svc-old:1.0".into()];
        elsewhere.repo_digests = vec![format!("h:5100/api-svc/sub@{idx}")];
        let repo = RepoPath("api-svc".into());
        for i in [&tagged, &by_tag_only] {
            let found = local_match(std::slice::from_ref(i), "h:5100", &repo, &a);
            assert_eq!(found.map(|i| i.id.as_str()), Some("one"));
        }
        assert!(local_match(&[elsewhere], "h:5100", &repo, &a).is_none());
        let other = RepoPath("other".into());
        assert!(local_match(std::slice::from_ref(&tagged), "h:5100", &other, &a).is_none());
        assert!(local_match(std::slice::from_ref(&tagged), "h:5100", &repo, &idx).is_some());
    }

    #[test]
    fn to_local_keeps_available_image_manifests() {
        use ImageManifestSummaryKindEnum as K;
        use bollard::models::ImageManifestSummary;
        let m = |id: &str, kind: K, available: bool| ImageManifestSummary {
            id: id.into(),
            kind: Some(kind),
            available,
            ..Default::default()
        };
        let s = ImageSummary {
            manifests: Some(vec![
                m("sha256:img", K::IMAGE, true),
                m("sha256:gone", K::IMAGE, false),
                m("sha256:att", K::ATTESTATION, true),
            ]),
            ..Default::default()
        };
        assert_eq!(to_local(s).manifests, vec!["sha256:img"]);
    }

    #[test]
    fn to_local_maps_and_drops_placeholders() {
        let s = ImageSummary {
            id: "sha256:abc".into(),
            repo_tags: vec!["<none>:<none>".into()],
            repo_digests: vec!["<none>@<none>".into(), "h/r@sha256:x".into()],
            created: 1_700_000_000,
            size: -5,
            ..Default::default()
        };
        let l = to_local(s);
        assert!(l.repo_tags.is_empty());
        assert_eq!(l.repo_digests, vec!["h/r@sha256:x"]);
        assert_eq!(l.created.unwrap().unix_timestamp(), 1_700_000_000);
        assert_eq!(l.size, 0);
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon"]
    async fn lists_real_images() {
        BollardEngine::connect().unwrap().list().await.unwrap();
    }
}

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};

use super::{DeleteResult, Provider, run_batches};

/// Start of the `Unsupported` message when a registry cannot list repositories.
pub const LISTING_UNAVAILABLE: &str = "repository listing not available";

use crate::domain::{
    ArtifactStatus, ArtifactSummary, Caps, DeleteOp, Digest, Inventory, OpOutcome, Reference,
    RegistryError, RepoPath, RepoSummary,
};
use crate::registry::OciClient;

/// Generic OCI Distribution provider (Harbor, registry:2, ...).
pub struct OciProvider {
    oci: Arc<OciClient>,
    configured: Vec<RepoPath>,
}

impl OciProvider {
    pub fn new(oci: Arc<OciClient>, configured: Vec<RepoPath>) -> Self {
        Self { oci, configured }
    }
}

#[async_trait]
impl Provider for OciProvider {
    fn caps(&self) -> Caps {
        Caps {
            inventory: Inventory::TaggedOnly,
            pushed_at: false,
            last_pulled: false,
            delete_tag: false,
            delete_manifest: true,
        }
    }

    async fn repositories(
        &self,
        progress: &(dyn Fn(usize) + Sync),
    ) -> Result<Vec<RepoSummary>, RegistryError> {
        let mut repos = match self.oci.catalog(progress).await {
            Ok(r) => r,
            Err(
                RegistryError::NotFound
                | RegistryError::Unauthorized
                | RegistryError::Forbidden(_)
                | RegistryError::Unsupported(_),
            ) => {
                if self.configured.is_empty() {
                    return Err(RegistryError::Unsupported(format!(
                        "{LISTING_UNAVAILABLE} — add repositories = [...] in config"
                    )));
                }
                Vec::new()
            }
            Err(e) => return Err(e),
        };
        repos.extend(self.configured.iter().cloned());
        repos.sort();
        repos.dedup();
        Ok(repos
            .into_iter()
            .map(|path| RepoSummary {
                path,
                created_at: None,
            })
            .collect())
    }

    async fn artifacts(
        &self,
        repo: &RepoPath,
        progress: &(dyn Fn(usize) + Sync),
    ) -> Result<Vec<ArtifactSummary>, RegistryError> {
        let tags = self.oci.tags(repo, progress).await?;
        let heads: Vec<_> = stream::iter(tags)
            .map(|t| async move {
                let r = self
                    .oci
                    .head_manifest(repo, &Reference::Tag(t.clone()))
                    .await;
                (t, r)
            })
            .buffer_unordered(8)
            .collect()
            .await;
        let mut by_digest: BTreeMap<String, (Digest, String, Vec<String>)> = BTreeMap::new();
        for (tag, head) in heads {
            match head {
                Ok((digest, media_type)) => {
                    by_digest
                        .entry(digest.to_string())
                        .or_insert_with(|| (digest, media_type, Vec::new()))
                        .2
                        .push(tag);
                }
                // Deleted between list and HEAD.
                Err(RegistryError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(by_digest
            .into_values()
            .map(|(digest, media_type, mut tags)| {
                tags.sort();
                ArtifactSummary {
                    digest,
                    tags,
                    media_type: (!media_type.is_empty()).then_some(media_type),
                    artifact_type: None,
                    subject: None,
                    size: None,
                    pushed_at: None,
                    last_pulled: None,
                    status: ArtifactStatus::Active,
                }
            })
            .collect())
    }

    async fn delete(
        &self,
        repo: &RepoPath,
        ops: &[DeleteOp],
    ) -> Result<DeleteResult, RegistryError> {
        // Size 1: each chunk is a single op.
        run_batches(ops, 1, |chunk| async move {
            let op = &chunk[0];
            let o = match op {
                // `delete_tag` is false; the planner never sends one.
                DeleteOp::RemoveTag { .. } => {
                    OpOutcome::Failed("tag removal is not supported on this registry".into())
                }
                DeleteOp::DeleteManifest { digest } => {
                    self.oci.delete_manifest(repo, digest).await?;
                    OpOutcome::Done
                }
            };
            Ok(vec![(op.clone(), o)])
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AnonymousSource;
    use crate::domain::TagPolicy;
    use crate::providers::DELETION_DISABLED;
    use crate::registry::Gate;
    use url::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const D2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    fn provider(server: &MockServer, configured: &[&str]) -> OciProvider {
        let oci = OciClient::new(
            Url::parse(&server.uri()).unwrap(),
            vec![],
            Arc::new(AnonymousSource),
            Arc::new(Gate::new(8)),
        );
        OciProvider::new(
            Arc::new(oci),
            configured.iter().map(|s| RepoPath((*s).into())).collect(),
        )
    }

    fn names(r: Vec<RepoSummary>) -> Vec<String> {
        r.into_iter().map(|r| r.path.0).collect()
    }

    async fn mock_catalog(server: &MockServer, repos: &[&str]) {
        Mock::given(method("GET"))
            .and(path("/v2/_catalog"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "repositories": repos })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn repos_merge_catalog_and_config() {
        let s = MockServer::start().await;
        mock_catalog(&s, &["b", "a"]).await;
        let r = provider(&s, &["c", "a"])
            .repositories(&|_| {})
            .await
            .unwrap();
        assert_eq!(names(r), ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn catalog_404_falls_back_to_config() {
        let s = MockServer::start().await;
        Mock::given(path("/v2/_catalog"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        let r = provider(&s, &["team/api"])
            .repositories(&|_| {})
            .await
            .unwrap();
        assert_eq!(names(r), ["team/api"]);
    }

    #[tokio::test]
    async fn no_catalog_no_config_is_unsupported() {
        let s = MockServer::start().await;
        Mock::given(path("/v2/_catalog"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        let e = provider(&s, &[]).repositories(&|_| {}).await.unwrap_err();
        assert!(matches!(e, RegistryError::Unsupported(_)), "{e:?}");
    }

    async fn mock_head(server: &MockServer, tag: &str, digest: &str) {
        Mock::given(method("HEAD"))
            .and(path(format!("/v2/app/manifests/{tag}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Docker-Content-Digest", digest)
                    .insert_header("Content-Type", "application/vnd.oci.image.manifest.v1+json"),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn tags_grouped_by_digest() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/app/tags/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "name": "app", "tags": ["b", "a", "c", "gone"] }),
            ))
            .mount(&s)
            .await;
        mock_head(&s, "a", D1).await;
        mock_head(&s, "b", D1).await;
        mock_head(&s, "c", D2).await;
        Mock::given(method("HEAD"))
            .and(path("/v2/app/manifests/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        let rows = provider(&s, &[])
            .artifacts(&RepoPath("app".into()), &|_| {})
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].tags, ["a", "b"]);
        assert_eq!(rows[1].tags, ["c"]);
        assert_eq!(rows[0].status, ArtifactStatus::Active);
    }

    #[tokio::test]
    async fn no_tags_is_empty() {
        let s = MockServer::start().await;
        Mock::given(path("/v2/app/tags/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "name": "app", "tags": null })),
            )
            .mount(&s)
            .await;
        let rows = provider(&s, &[])
            .artifacts(&RepoPath("app".into()), &|_| {})
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn caps_advertised_have_impls() {
        let s = MockServer::start().await;
        mock_delete(&s, 202).await;
        let p = provider(&s, &[]);
        let c = p.caps();
        assert_eq!(c.inventory, Inventory::TaggedOnly);
        assert!(!c.pushed_at && !c.last_pulled);
        assert!(c.delete_manifest);
        let (r, _) = p.delete(&app(), &[manifest(D1)]).await.unwrap();
        assert_eq!(r.results[0].1, OpOutcome::Done);
    }

    fn app() -> RepoPath {
        RepoPath("app".into())
    }

    fn manifest(d: &str) -> DeleteOp {
        DeleteOp::DeleteManifest {
            digest: Digest::parse(d).unwrap(),
        }
    }

    /// Probe 200; every DELETE answered with `status`.
    async fn mock_delete(server: &MockServer, status: u16) {
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(server)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(status))
            .mount(server)
            .await;
    }

    async fn delete_paths(server: &MockServer) -> Vec<String> {
        let reqs = server.received_requests().await.unwrap();
        reqs.into_iter()
            .filter(|r| r.method.as_str() == "DELETE")
            .map(|r| r.url.path().to_string())
            .collect()
    }

    #[tokio::test]
    async fn oci_delete_manifest_by_digest() {
        let s = MockServer::start().await;
        mock_delete(&s, 202).await;
        let (r, stop) = provider(&s, &[])
            .delete(&app(), &[manifest(D1), manifest(D2)])
            .await
            .unwrap();
        assert_eq!(
            r.results,
            [
                (manifest(D1), OpOutcome::Done),
                (manifest(D2), OpOutcome::Done)
            ]
        );
        assert!(stop.is_none());
        assert_eq!(
            delete_paths(&s).await,
            [
                format!("/v2/app/manifests/{D1}"),
                format!("/v2/app/manifests/{D2}")
            ]
        );
    }

    #[tokio::test]
    async fn oci_delete_405_aborts() {
        let s = MockServer::start().await;
        mock_delete(&s, 405).await;
        let (r, _) = provider(&s, &[])
            .delete(&app(), &[manifest(D1), manifest(D2)])
            .await
            .unwrap();
        assert_eq!(
            r.results,
            [
                (manifest(D1), OpOutcome::Failed(DELETION_DISABLED.into())),
                (manifest(D2), OpOutcome::NotAttempted),
            ]
        );
        assert!(r.aborted_unsupported);
        assert_eq!(delete_paths(&s).await, [format!("/v2/app/manifests/{D1}")]);
    }

    #[tokio::test]
    async fn oci_remove_tag_fails_without_request() {
        let s = MockServer::start().await;
        let op = DeleteOp::RemoveTag {
            tag: "v1".into(),
            expected: Digest::parse(D1).unwrap(),
        };
        let (r, _) = provider(&s, &[])
            .delete(&app(), std::slice::from_ref(&op))
            .await
            .unwrap();
        assert_eq!(
            r.results,
            [(
                op,
                OpOutcome::Failed("tag removal is not supported on this registry".into())
            )]
        );
        assert!(s.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn oci_tag_policy_unknown() {
        let s = MockServer::start().await;
        assert_eq!(
            provider(&s, &[])
                .tag_policy(&RepoPath("app".into()), "1.0")
                .await,
            Ok(TagPolicy::Unknown)
        );
    }
}

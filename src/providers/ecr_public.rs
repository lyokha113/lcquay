//! Amazon ECR Public provider (DescribeRepositories / DescribeImages).

use std::sync::Arc;

use async_trait::async_trait;
use aws_sdk_ecrpublic::config::retry::RetryConfig;
use aws_sdk_ecrpublic::types::{ImageDetail, ImageIdentifier};

use super::ecr::{DELETE_BATCH, failure_text, image_id, match_outcomes, put, to_time, write_call};
use super::{DeleteResult, Provider, run_batches};
use crate::auth::CredentialSource;
use crate::auth::ecr::{EcrPublicSource, load_sdk_config, sdk_call};
use crate::domain::{
    ArtifactStatus, ArtifactSummary, Caps, DeleteOp, Details, Digest, Inventory, RegistryError,
    RepoPath, RepoSummary, TagPolicy,
};
use crate::registry::{CursorGuard, Gate};
use crate::reqlog::RegLog;

/// ECR Public is a global service whose API and tokens live in us-east-1 (spec §2).
const REGION: &str = "us-east-1";

pub struct EcrPublicProvider {
    client: aws_sdk_ecrpublic::Client,
    alias: String,
    gate: Arc<Gate>,
    log: RegLog,
}

impl EcrPublicProvider {
    pub fn new(client: aws_sdk_ecrpublic::Client, alias: String, gate: Arc<Gate>) -> Self {
        Self {
            client,
            alias,
            gate,
            log: RegLog::default(),
        }
    }

    pub fn with_log(mut self, log: RegLog) -> Self {
        self.log = log;
        self
    }

    /// The API's repository name: `repo` without the `<alias>/` prefix.
    fn name<'a>(&self, repo: &'a RepoPath) -> Result<&'a str, RegistryError> {
        repo.0
            .strip_prefix(&self.alias)
            .and_then(|r| r.strip_prefix('/'))
            .filter(|n| !n.is_empty())
            .ok_or(RegistryError::NotFound)
    }
}

/// Builds the credential source and provider for the caller's public registry; no network I/O.
pub async fn build(
    profile: Option<&str>,
    alias: &str,
    gate: Arc<Gate>,
    log: RegLog,
) -> Result<(Arc<dyn CredentialSource>, EcrPublicProvider), RegistryError> {
    let sdk = load_sdk_config(REGION, profile).await?;
    let client = aws_sdk_ecrpublic::Client::new(&sdk);
    Ok((
        Arc::new(EcrPublicSource::new(client.clone()).with_log(log.clone())),
        EcrPublicProvider::new(client, alias.to_string(), gate).with_log(log),
    ))
}

fn map_image(d: &ImageDetail) -> Option<ArtifactSummary> {
    let digest = Digest::parse(d.image_digest()?).ok()?;
    let mut tags = d.image_tags().to_vec();
    tags.sort();
    Some(ArtifactSummary {
        digest,
        tags,
        media_type: d.image_manifest_media_type().map(str::to_string),
        artifact_type: d.artifact_media_type().map(str::to_string),
        // ECR Public reports neither a subject, a status nor a last-pull time.
        subject: None,
        size: d.image_size_in_bytes().and_then(|n| u64::try_from(n).ok()),
        pushed_at: d.image_pushed_at().and_then(to_time),
        last_pulled: None,
        status: ArtifactStatus::Active,
    })
}

#[async_trait]
impl Provider for EcrPublicProvider {
    fn caps(&self) -> Caps {
        Caps {
            inventory: Inventory::Complete,
            pushed_at: true,
            last_pulled: false,
            // Verified 2026-10-29 (spec §13).
            delete_tag: true,
            delete_manifest: true,
        }
    }

    async fn repositories(
        &self,
        progress: &(dyn Fn(usize) + Sync),
    ) -> Result<Vec<RepoSummary>, RegistryError> {
        let (mut out, mut token, mut pages) = (Vec::new(), None::<String>, 0);
        let mut guard = CursorGuard::default();
        loop {
            // One permit per SDK call, not held across pages. No `registry_id`: the caller's own.
            let page = {
                let _permit = self.gate.acquire().await;
                sdk_call(
                    &self.log,
                    "ecr-public:DescribeRepositories",
                    self.client
                        .describe_repositories()
                        .set_next_token(token.take())
                        .send(),
                )
                .await?
            };
            let before = out.len();
            out.extend(page.repositories().iter().filter_map(|r| {
                Some(RepoSummary {
                    path: RepoPath(format!("{}/{}", self.alias, r.repository_name()?)),
                    created_at: r.created_at().and_then(to_time),
                })
            }));
            pages += 1;
            progress(pages);
            match page.next_token() {
                Some(t) => {
                    guard.check(t, out.len() - before, true)?;
                    token = Some(t.to_string());
                }
                None => return Ok(out),
            }
        }
    }

    async fn artifacts(
        &self,
        repo: &RepoPath,
        progress: &(dyn Fn(usize) + Sync),
    ) -> Result<Vec<ArtifactSummary>, RegistryError> {
        let name = self.name(repo)?;
        let (mut out, mut token, mut pages) = (Vec::new(), None::<String>, 0);
        let mut guard = CursorGuard::default();
        loop {
            let page = {
                let _permit = self.gate.acquire().await;
                sdk_call(
                    &self.log,
                    "ecr-public:DescribeImages",
                    self.client
                        .describe_images()
                        .repository_name(name)
                        .set_next_token(token.take())
                        .send(),
                )
                .await?
            };
            let before = out.len();
            out.extend(page.image_details().iter().filter_map(map_image));
            pages += 1;
            progress(pages);
            match page.next_token() {
                Some(t) => {
                    guard.check(t, out.len() - before, true)?;
                    token = Some(t.to_string());
                }
                None => return Ok(out),
            }
        }
    }

    /// ECR Public has no tag immutability.
    async fn tag_policy(&self, _repo: &RepoPath, _tag: &str) -> Result<TagPolicy, RegistryError> {
        Ok(TagPolicy::Mutable)
    }

    async fn registry_details(&self) -> Result<Details, RegistryError> {
        let out = {
            let _permit = self.gate.acquire().await;
            sdk_call(
                &self.log,
                "ecr-public:DescribeRegistries",
                self.client.describe_registries().send(),
            )
            .await?
        };
        let r = out
            .registries()
            .iter()
            .find(|r| r.aliases().iter().any(|a| a.name() == self.alias))
            .ok_or(RegistryError::NotFound)?;
        let mut fields = vec![("Alias".to_string(), self.alias.clone())];
        put(
            &mut fields,
            "Verified",
            Some(if r.verified() { "yes" } else { "no" }),
        );
        put(&mut fields, "Registry", Some(r.registry_id()));
        Ok(Details { fields })
    }

    async fn repo_details(&self, repo: &RepoPath) -> Result<Details, RegistryError> {
        let name = self.name(repo)?;
        let out = {
            let _permit = self.gate.acquire().await;
            sdk_call(
                &self.log,
                "ecr-public:DescribeRepositories",
                self.client
                    .describe_repositories()
                    .repository_names(name)
                    .send(),
            )
            .await?
        };
        let r = out.repositories().first().ok_or(RegistryError::NotFound)?;
        let mut fields = Vec::new();
        put(&mut fields, "ARN", r.repository_arn());
        put(&mut fields, "URI", r.repository_uri());
        Ok(Details { fields })
    }

    async fn delete(
        &self,
        repo: &RepoPath,
        ops: &[DeleteOp],
    ) -> Result<DeleteResult, RegistryError> {
        let name = self.name(repo)?;
        run_batches(ops, DELETE_BATCH, |chunk| async move {
            let ids = chunk
                .iter()
                .map(|op| {
                    let (digest, tag) = image_id(op);
                    ImageIdentifier::builder()
                        .image_digest(digest)
                        .set_image_tag(tag)
                        .build()
                })
                .collect();
            // No `registry_id`: the caller's own.
            let out = {
                let _permit = self.gate.acquire().await;
                write_call(
                    &self.log,
                    "ecr-public:BatchDeleteImage",
                    self.client
                        .batch_delete_image()
                        .repository_name(name)
                        .set_image_ids(Some(ids))
                        .customize()
                        .config_override(
                            aws_sdk_ecrpublic::config::Builder::default()
                                .retry_config(RetryConfig::disabled()),
                        )
                        .send(),
                )
                .await?
            };
            let entry = |i: Option<&ImageIdentifier>| {
                (
                    i.and_then(|i| i.image_digest()).map(str::to_string),
                    i.and_then(|i| i.image_tag()).map(str::to_string),
                )
            };
            let deleted: Vec<_> = out.image_ids().iter().map(|i| entry(Some(i))).collect();
            let failures: Vec<_> = out
                .failures()
                .iter()
                .map(|f| {
                    let (d, t) = entry(f.image_id());
                    let code = f.failure_code().map(|c| c.as_str());
                    (d, t, failure_text(code, f.failure_reason()))
                })
                .collect();
            Ok(match_outcomes(&chunk, &deleted, &failures))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use aws_sdk_ecrpublic::operation::batch_delete_image::BatchDeleteImageOutput;
    use aws_sdk_ecrpublic::operation::describe_images::DescribeImagesOutput;
    use aws_sdk_ecrpublic::operation::describe_repositories::DescribeRepositoriesOutput;
    use aws_sdk_ecrpublic::primitives::DateTime;
    use aws_sdk_ecrpublic::types::{ImageFailure, ImageFailureCode, ImageIdentifier, Repository};
    use aws_smithy_mocks::{Rule, RuleMode, mock, mock_client};

    use super::*;
    use crate::domain::OpOutcome;

    const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const D2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    fn provider(rules: &[&Rule]) -> EcrPublicProvider {
        let client = mock_client!(aws_sdk_ecrpublic, rules);
        EcrPublicProvider::new(client, "a1b2c3d4".into(), Arc::new(Gate::new(8)))
    }

    #[tokio::test]
    async fn repositories_prefix_alias_and_paginate() {
        let r1 = mock!(aws_sdk_ecrpublic::Client::describe_repositories)
            .match_requests(|i| i.registry_id().is_none() && i.next_token().is_none())
            .then_output(|| {
                DescribeRepositoriesOutput::builder()
                    .repositories(
                        Repository::builder()
                            .repository_name("api-svc")
                            .created_at(DateTime::from_secs(1_700_000_000))
                            .build(),
                    )
                    .next_token("t1")
                    .build()
            });
        let r2 = mock!(aws_sdk_ecrpublic::Client::describe_repositories)
            .match_requests(|i| i.next_token() == Some("t1"))
            .then_output(|| {
                DescribeRepositoriesOutput::builder()
                    .repositories(Repository::builder().repository_name("ttl").build())
                    .build()
            });
        let calls = Mutex::new(Vec::new());
        let got = provider(&[&r1, &r2])
            .repositories(&|n| calls.lock().unwrap().push(n))
            .await
            .unwrap();
        assert_eq!(got[0].path, RepoPath("a1b2c3d4/api-svc".into()));
        assert_eq!(got[0].created_at.unwrap().unix_timestamp(), 1_700_000_000);
        assert_eq!(got[1].path, RepoPath("a1b2c3d4/ttl".into()));
        assert_eq!(*calls.lock().unwrap(), [1, 2]);
    }

    #[tokio::test]
    async fn artifacts_strip_alias_and_paginate() {
        let r1 = mock!(aws_sdk_ecrpublic::Client::describe_images)
            .match_requests(|i| {
                i.repository_name() == Some("api-svc")
                    && i.registry_id().is_none()
                    && i.next_token().is_none()
            })
            .then_output(|| {
                DescribeImagesOutput::builder()
                    .image_details(
                        ImageDetail::builder()
                            .image_digest(D1)
                            .image_tags("b")
                            .image_tags("a")
                            .image_size_in_bytes(42)
                            .image_pushed_at(DateTime::from_secs(1_700_000_000))
                            .build(),
                    )
                    .next_token("t1")
                    .build()
            });
        let r2 = mock!(aws_sdk_ecrpublic::Client::describe_images)
            .match_requests(|i| {
                i.repository_name() == Some("api-svc") && i.next_token() == Some("t1")
            })
            .then_output(|| {
                DescribeImagesOutput::builder()
                    .image_details(ImageDetail::builder().image_digest(D2).build())
                    .build()
            });
        let calls = Mutex::new(Vec::new());
        let got = provider(&[&r1, &r2])
            .artifacts(&RepoPath("a1b2c3d4/api-svc".into()), &|n| {
                calls.lock().unwrap().push(n)
            })
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].tags, ["a", "b"]);
        assert_eq!(got[0].size, Some(42));
        assert_eq!(got[0].pushed_at.unwrap().unix_timestamp(), 1_700_000_000);
        assert!(got[1].tags.is_empty());
        assert!(got.iter().all(|a| a.status == ArtifactStatus::Active
            && a.last_pulled.is_none()
            && a.subject.is_none()));
        assert_eq!(*calls.lock().unwrap(), [1, 2]);
        assert_eq!((r1.num_calls(), r2.num_calls()), (1, 1));
    }

    #[tokio::test]
    async fn artifacts_rejects_path_without_alias() {
        let r = mock!(aws_sdk_ecrpublic::Client::describe_images)
            .then_output(|| DescribeImagesOutput::builder().build());
        let p = provider(&[&r]);
        for path in ["other/api-svc", "api-svc", "a1b2c3d4/", "a1b2c3d4x/y"] {
            let e = p
                .artifacts(&RepoPath(path.into()), &|_| {})
                .await
                .unwrap_err();
            assert_eq!(e, RegistryError::NotFound, "{path}");
        }
        assert_eq!(r.num_calls(), 0);
    }

    #[test]
    fn caps_complete_no_last_pull() {
        assert_eq!(
            provider(&[]).caps(),
            Caps {
                inventory: Inventory::Complete,
                pushed_at: true,
                last_pulled: false,
                delete_tag: true,
                delete_manifest: true,
            }
        );
    }

    #[tokio::test]
    async fn repeated_next_token_errors() {
        let page = || {
            DescribeImagesOutput::builder()
                .image_details(ImageDetail::builder().image_digest(D1).build())
                .next_token("same")
                .build()
        };
        let (r1, r2) = (
            mock!(aws_sdk_ecrpublic::Client::describe_images).then_output(page),
            mock!(aws_sdk_ecrpublic::Client::describe_images).then_output(page),
        );
        let e = provider(&[&r1, &r2])
            .artifacts(&RepoPath("a1b2c3d4/api-svc".into()), &|_| {})
            .await
            .unwrap_err();
        assert!(matches!(e, RegistryError::RepeatedCursor(_)), "{e:?}");
    }

    fn rm_tag(tag: &str, d: &str) -> DeleteOp {
        DeleteOp::RemoveTag {
            tag: tag.into(),
            expected: Digest::parse(d).unwrap(),
        }
    }

    fn id(tag: Option<&str>, d: &str) -> ImageIdentifier {
        ImageIdentifier::builder()
            .set_image_tag(tag.map(Into::into))
            .image_digest(d)
            .build()
    }

    #[tokio::test]
    async fn ecr_public_delete_strips_alias() {
        let r = mock!(aws_sdk_ecrpublic::Client::batch_delete_image)
            .match_requests(|i| {
                i.repository_name() == Some("app")
                    && i.registry_id().is_none()
                    && i.image_ids() == [id(None, D1)]
            })
            .then_output(|| {
                BatchDeleteImageOutput::builder()
                    .image_ids(id(Some("x"), D1))
                    .build()
            });
        let op = DeleteOp::DeleteManifest {
            digest: Digest::parse(D1).unwrap(),
        };
        let (rep, stop) = provider(&[&r])
            .delete(&RepoPath("a1b2c3d4/app".into()), std::slice::from_ref(&op))
            .await
            .unwrap();
        assert_eq!(rep.results, [(op, OpOutcome::Done)]);
        assert!(stop.is_none());
        assert_eq!(r.num_calls(), 1);
    }

    #[tokio::test]
    async fn ecr_public_remove_tag_sends_tag_and_digest() {
        let r = mock!(aws_sdk_ecrpublic::Client::batch_delete_image)
            .match_requests(|i| i.image_ids() == [id(Some("1"), D1)])
            .then_output(|| {
                BatchDeleteImageOutput::builder()
                    .failures(
                        ImageFailure::builder()
                            .image_id(id(Some("1"), D1))
                            .failure_code(ImageFailureCode::ImageTagDoesNotMatchDigest)
                            .failure_reason("tag points elsewhere")
                            .build(),
                    )
                    .build()
            });
        let (rep, _) = provider(&[&r])
            .delete(&RepoPath("a1b2c3d4/app".into()), &[rm_tag("1", D1)])
            .await
            .unwrap();
        assert!(
            matches!(&rep.results[..], [(_, OpOutcome::Failed(t))] if t.starts_with("ImageTagDoesNotMatchDigest")),
            "{rep:?}"
        );
        assert_eq!(r.num_calls(), 1);
    }

    #[tokio::test]
    async fn ecr_public_delete_not_retried() {
        use aws_sdk_ecrpublic::config::http::HttpResponse;
        let r = mock!(aws_sdk_ecrpublic::Client::batch_delete_image).then_http_response(|| {
            let body = r#"{"__type":"ServerException","message":"boom"}"#;
            HttpResponse::new(500u16.try_into().unwrap(), body.into())
        });
        let client = mock_client!(aws_sdk_ecrpublic, RuleMode::MatchAny, [&r]);
        let p = EcrPublicProvider::new(client, "a1b2c3d4".into(), Arc::new(Gate::new(8)));
        let (rep, _) = p
            .delete(&RepoPath("a1b2c3d4/app".into()), &[rm_tag("1", D1)])
            .await
            .unwrap();
        assert_eq!(r.num_calls(), 1);
        assert!(
            matches!(&rep.results[..], [(_, OpOutcome::Uncertain(t))] if t.contains("ServerException")),
            "{rep:?}"
        );
    }

    #[tokio::test]
    async fn ecr_public_delete_bad_path_sends_nothing() {
        let r = mock!(aws_sdk_ecrpublic::Client::batch_delete_image)
            .then_output(|| BatchDeleteImageOutput::builder().build());
        let e = provider(&[&r])
            .delete(&RepoPath("other/app".into()), &[rm_tag("1", D1)])
            .await
            .unwrap_err();
        assert_eq!(e, RegistryError::NotFound);
        assert_eq!(r.num_calls(), 0);
    }

    #[tokio::test]
    async fn ecr_public_tag_policy_mutable() {
        // No rules: any SDK call would fail.
        assert_eq!(
            provider(&[])
                .tag_policy(&RepoPath("a1b2c3d4/api-svc".into()), "1.0")
                .await,
            Ok(TagPolicy::Mutable)
        );
    }

    fn registry(id: &str, alias: &str, verified: bool) -> aws_sdk_ecrpublic::types::Registry {
        use aws_sdk_ecrpublic::types::{Registry, RegistryAlias, RegistryAliasStatus};
        Registry::builder()
            .registry_id(id)
            .registry_arn("arn")
            .registry_uri("uri")
            .verified(verified)
            .aliases(
                RegistryAlias::builder()
                    .name(alias)
                    .status(RegistryAliasStatus::Active)
                    .primary_registry_alias(true)
                    .default_registry_alias(true)
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()
    }

    fn registries(regs: Vec<aws_sdk_ecrpublic::types::Registry>) -> Rule {
        use aws_sdk_ecrpublic::operation::describe_registries::DescribeRegistriesOutput;
        mock!(aws_sdk_ecrpublic::Client::describe_registries).then_output(move || {
            DescribeRegistriesOutput::builder()
                .set_registries(Some(regs.clone()))
                .build()
                .unwrap()
        })
    }

    #[tokio::test]
    async fn registry_details_matches_alias() {
        let r = registries(vec![
            registry("111111111111", "other", false),
            registry("123456789012", "a1b2c3d4", true),
        ]);
        let d = provider(&[&r]).registry_details().await.unwrap();
        let want = [
            ("Alias", "a1b2c3d4"),
            ("Verified", "yes"),
            ("Registry", "123456789012"),
        ];
        assert_eq!(
            d.fields,
            want.map(|(k, v)| (k.to_string(), v.to_string())).to_vec()
        );
    }

    #[tokio::test]
    async fn registry_details_alias_not_found() {
        let r = registries(vec![registry("111111111111", "other", false)]);
        assert_eq!(
            provider(&[&r]).registry_details().await,
            Err(RegistryError::NotFound)
        );
    }

    #[tokio::test]
    async fn repo_details_strips_alias() {
        let r = mock!(aws_sdk_ecrpublic::Client::describe_repositories)
            .match_requests(|i| i.repository_names() == ["api-svc"] && i.registry_id().is_none())
            .then_output(|| {
                DescribeRepositoriesOutput::builder()
                    .repositories(
                        Repository::builder()
                            .repository_name("api-svc")
                            .repository_arn("arn:aws:ecr-public::123456789012:repository/api-svc")
                            .repository_uri("public.ecr.aws/a1b2c3d4/api-svc")
                            .build(),
                    )
                    .build()
            });
        let d = provider(&[&r])
            .repo_details(&RepoPath("a1b2c3d4/api-svc".into()))
            .await
            .unwrap();
        let want = [
            ("ARN", "arn:aws:ecr-public::123456789012:repository/api-svc"),
            ("URI", "public.ecr.aws/a1b2c3d4/api-svc"),
        ];
        assert_eq!(
            d.fields,
            want.map(|(k, v)| (k.to_string(), v.to_string())).to_vec()
        );
    }

    #[tokio::test]
    async fn repo_details_not_found() {
        let r = mock!(aws_sdk_ecrpublic::Client::describe_repositories)
            .then_output(|| DescribeRepositoriesOutput::builder().build());
        assert_eq!(
            provider(&[&r])
                .repo_details(&RepoPath("a1b2c3d4/api-svc".into()))
                .await,
            Err(RegistryError::NotFound)
        );
    }
}

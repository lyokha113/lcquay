//! Amazon ECR provider (DescribeRepositories / DescribeImages).

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use aws_sdk_ecr::config::retry::RetryConfig;
use aws_sdk_ecr::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_ecr::types::DescribeImagesFilter;
use aws_sdk_ecr::types::{
    ImageDetail, ImageIdentifier, ImageStatus, ImageStatusFilter, ImageTagMutability,
    ImageTagMutabilityExclusionFilter, ImageTagMutabilityExclusionFilterType, Repository,
    TagStatus,
};
use time::OffsetDateTime;

use super::{DeleteResult, Provider, run_batches};
use crate::auth::CredentialSource;
use crate::auth::ecr::{EcrSource, credentials_failed, load_sdk_config, map_sdk_error, sdk_call};
use crate::domain::{
    ArtifactStatus, ArtifactSummary, Caps, DeleteOp, Details, Digest, Inventory, OpOutcome,
    RegistryError, RepoPath, RepoSummary, TagPolicy, glob_match,
};
use crate::registry::oci::cut;
use crate::registry::{CursorGuard, DeleteError, Gate};
use crate::reqlog::RegLog;

/// Image ids per `BatchDeleteImage` call (the API maximum).
pub(crate) const DELETE_BATCH: usize = 100;

pub struct EcrProvider {
    client: aws_sdk_ecr::Client,
    sts: aws_sdk_sts::Client,
    registry_id: String,
    gate: Arc<Gate>,
    log: RegLog,
}

impl EcrProvider {
    pub fn new(
        client: aws_sdk_ecr::Client,
        sts: aws_sdk_sts::Client,
        registry_id: String,
        gate: Arc<Gate>,
    ) -> Self {
        Self {
            client,
            sts,
            registry_id,
            gate,
            log: RegLog::default(),
        }
    }

    pub fn with_log(mut self, log: RegLog) -> Self {
        self.log = log;
        self
    }

    /// The repository `repo` as `DescribeRepositories` answers for it; an empty answer is `NotFound`.
    async fn describe_repo(&self, repo: &RepoPath) -> Result<Repository, RegistryError> {
        let out = {
            let _permit = self.gate.acquire().await;
            sdk_call(
                &self.log,
                "ecr:DescribeRepositories",
                self.client
                    .describe_repositories()
                    .registry_id(&self.registry_id)
                    .repository_names(&repo.0)
                    .send(),
            )
            .await?
        };
        out.repositories()
            .first()
            .cloned()
            .ok_or(RegistryError::NotFound)
    }
}

/// An STS client for the profile and region (the connection test's caller identity); no
/// network I/O.
pub async fn sts_client(
    region: &str,
    profile: Option<&str>,
) -> Result<aws_sdk_sts::Client, RegistryError> {
    Ok(aws_sdk_sts::Client::new(
        &load_sdk_config(region, profile).await?,
    ))
}

/// Builds the credential source and provider for one ECR registry; no network I/O.
pub async fn build(
    region: &str,
    profile: Option<&str>,
    account_id: &str,
    gate: Arc<Gate>,
    log: RegLog,
) -> Result<(Arc<dyn CredentialSource>, EcrProvider), RegistryError> {
    let sdk = load_sdk_config(region, profile).await?;
    let client = aws_sdk_ecr::Client::new(&sdk);
    Ok((
        Arc::new(EcrSource::new(client.clone()).with_log(log.clone())),
        EcrProvider::new(
            client,
            aws_sdk_sts::Client::new(&sdk),
            account_id.to_string(),
            gate,
        )
        .with_log(log),
    ))
}

/// Appends `(label, v)` to a details list when the SDK returned a value.
pub(crate) fn put(fields: &mut Vec<(String, String)>, label: &str, v: Option<&str>) {
    fields.extend(v.map(|v| (label.to_string(), v.to_string())));
}

pub(crate) fn to_time(t: &aws_sdk_ecr::primitives::DateTime) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(t.as_nanos()).ok()
}

pub(crate) fn map_image(d: &ImageDetail) -> Option<ArtifactSummary> {
    let digest = Digest::parse(d.image_digest()?).ok()?;
    let mut tags = d.image_tags().to_vec();
    tags.sort();
    let status = match d.image_status() {
        None | Some(ImageStatus::Active) => ArtifactStatus::Active,
        Some(ImageStatus::Archived) => ArtifactStatus::Archived,
        Some(other) => ArtifactStatus::Other(other.as_str().to_string()),
    };
    Some(ArtifactSummary {
        digest,
        tags,
        media_type: d.image_manifest_media_type().map(str::to_string),
        artifact_type: d.artifact_media_type().map(str::to_string),
        subject: d
            .subject_manifest_digest()
            .and_then(|s| Digest::parse(s).ok()),
        size: d.image_size_in_bytes().and_then(|n| u64::try_from(n).ok()),
        pushed_at: d.image_pushed_at().and_then(to_time),
        last_pulled: d.last_recorded_pull_time().and_then(to_time),
        status,
    })
}

/// Policy of `tag` from the repository's mutability and exclusion filters. Any filter of a type
/// this build doesn't know makes an `*_WITH_EXCLUSION` result `Unknown`.
pub(crate) fn policy_for(
    mutability: &ImageTagMutability,
    exclusion_filters: &[ImageTagMutabilityExclusionFilter],
    tag: &str,
) -> TagPolicy {
    let (base, other) = match mutability {
        ImageTagMutability::Mutable => return TagPolicy::Mutable,
        ImageTagMutability::Immutable => return TagPolicy::Immutable,
        ImageTagMutability::ImmutableWithExclusion => (TagPolicy::Immutable, TagPolicy::Mutable),
        ImageTagMutability::MutableWithExclusion => (TagPolicy::Mutable, TagPolicy::Immutable),
        _ => return TagPolicy::Unknown,
    };
    let mut excluded = false;
    for f in exclusion_filters {
        if *f.filter_type() != ImageTagMutabilityExclusionFilterType::Wildcard {
            return TagPolicy::Unknown;
        }
        excluded |= glob_match(f.filter(), tag);
    }
    if excluded { other } else { base }
}

/// `(digest, tag)` as `BatchDeleteImage` takes it; a tag is guarded by its expected digest.
pub(crate) fn image_id(op: &DeleteOp) -> (String, Option<String>) {
    match op {
        DeleteOp::RemoveTag { tag, expected } => (expected.to_string(), Some(tag.clone())),
        DeleteOp::DeleteManifest { digest } => (digest.to_string(), None),
    }
}

/// `"<code>: <reason>"`, cut like every registry error text.
pub(crate) fn failure_text(code: Option<&str>, reason: Option<&str>) -> String {
    cut(format!(
        "{}: {}",
        code.unwrap_or("unknown"),
        reason.unwrap_or_default()
    ))
}

/// Per-op outcomes of one `BatchDeleteImage` answer, one per op in input order. Entries are
/// `(digest, tag)`. A `RemoveTag` matches its tag (and its digest, when the entry has one); a
/// `DeleteManifest` matches any entry with its digest. Failures win over deletions.
/// Precondition (the planner's): a batch never holds both a `RemoveTag` and a `DeleteManifest`
/// of the same digest.
pub(crate) fn match_outcomes(
    ops: &[DeleteOp],
    deleted: &[(Option<String>, Option<String>)],
    failures: &[(Option<String>, Option<String>, String)],
) -> Vec<(DeleteOp, OpOutcome)> {
    let hit = |op: &DeleteOp, d: &Option<String>, t: &Option<String>| match op {
        DeleteOp::RemoveTag { tag, expected } => {
            t.as_ref() == Some(tag) && d.as_ref().is_none_or(|d| *d == expected.to_string())
        }
        DeleteOp::DeleteManifest { digest } => d.as_ref() == Some(&digest.to_string()),
    };
    ops.iter()
        .map(|op| {
            let out = match failures.iter().find(|(d, t, _)| hit(op, d, t)) {
                Some((_, _, text)) => OpOutcome::Failed(text.clone()),
                None if deleted.iter().any(|(d, t)| hit(op, d, t)) => OpOutcome::Done,
                None => OpOutcome::Uncertain("no answer for this image".into()),
            };
            (op.clone(), out)
        })
        .collect()
}

/// Classifies a failed write call by whether it may have reached AWS: a service answer below
/// 500 was refused; a 5xx or any error after dispatch may have been applied. Credential
/// failures, before dispatch or rejected by the service, are `NotSent` and stop the run.
pub(crate) fn call_error<E>(op: &str, e: SdkError<E>) -> DeleteError
where
    E: std::error::Error + ProvideErrorMetadata + Send + Sync + 'static,
{
    if let SdkError::ServiceError(se) = &e {
        let text = failure_text(se.err().code(), se.err().message());
        return match se.raw().status().as_u16() {
            500.. => DeleteError::Uncertain(text),
            _ => match map_sdk_error(op, e) {
                c @ RegistryError::AwsCredentials(_) => DeleteError::NotSent(c),
                _ => DeleteError::Rejected(text),
            },
        };
    }
    match e {
        SdkError::ConstructionFailure(_) => DeleteError::NotSent(map_sdk_error(op, e)),
        SdkError::DispatchFailure(_) if credentials_failed(&e) => {
            DeleteError::NotSent(map_sdk_error(op, e))
        }
        _ => DeleteError::Uncertain(cut(map_sdk_error(op, e).to_string())),
    }
}

/// Awaits one write SDK call, records it in `log` and classifies its error by `call_error`.
/// `Rejected` and `Uncertain` are logged as `registry error`.
pub(crate) async fn write_call<T, E>(
    log: &RegLog,
    op: &str,
    call: impl Future<Output = Result<T, SdkError<E>>>,
) -> Result<T, DeleteError>
where
    E: std::error::Error + ProvideErrorMetadata + Send + Sync + 'static,
{
    let t = Instant::now();
    let r = call.await.map_err(|e| call_error(op, e));
    let logged = match &r {
        Ok(_) => Ok(()),
        Err(DeleteError::NotSent(e)) => Err(e.clone()),
        Err(_) => Err(RegistryError::Provider(String::new())),
    };
    log.call(op, "", t, logged.as_ref().map(|_| ()));
    r
}

#[async_trait]
impl Provider for EcrProvider {
    fn caps(&self) -> Caps {
        Caps {
            inventory: Inventory::Complete,
            pushed_at: true,
            last_pulled: true,
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
            // One permit per SDK call, not held across pages.
            let page = {
                let _permit = self.gate.acquire().await;
                sdk_call(
                    &self.log,
                    "ecr:DescribeRepositories",
                    self.client
                        .describe_repositories()
                        .registry_id(&self.registry_id)
                        .set_next_token(token.take())
                        .send(),
                )
                .await?
            };
            let before = out.len();
            out.extend(page.repositories().iter().filter_map(|r| {
                Some(RepoSummary {
                    path: RepoPath(r.repository_name()?.to_string()),
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
        let filter = DescribeImagesFilter::builder()
            .tag_status(TagStatus::Any)
            .image_status(ImageStatusFilter::Any)
            .build();
        let (mut out, mut token, mut pages) = (Vec::new(), None::<String>, 0);
        let mut guard = CursorGuard::default();
        loop {
            let page = {
                let _permit = self.gate.acquire().await;
                sdk_call(
                    &self.log,
                    "ecr:DescribeImages",
                    self.client
                        .describe_images()
                        .registry_id(&self.registry_id)
                        .repository_name(&repo.0)
                        .filter(filter.clone())
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

    async fn tag_policy(&self, repo: &RepoPath, tag: &str) -> Result<TagPolicy, RegistryError> {
        let r = self.describe_repo(repo).await?;
        Ok(match r.image_tag_mutability() {
            Some(m) => policy_for(m, r.image_tag_mutability_exclusion_filters(), tag),
            None => TagPolicy::Unknown,
        })
    }

    async fn registry_details(&self) -> Result<Details, RegistryError> {
        let id = {
            let _permit = self.gate.acquire().await;
            sdk_call(
                &self.log,
                "sts:GetCallerIdentity",
                self.sts.get_caller_identity().send(),
            )
            .await?
        };
        let mut fields = vec![("Registry".to_string(), self.registry_id.clone())];
        put(&mut fields, "Account", id.account());
        put(&mut fields, "Caller", id.arn());
        Ok(Details { fields })
    }

    async fn repo_details(&self, repo: &RepoPath) -> Result<Details, RegistryError> {
        let r = self.describe_repo(repo).await?;
        let exclusions = r
            .image_tag_mutability_exclusion_filters()
            .iter()
            .map(|f| f.filter())
            .collect::<Vec<_>>()
            .join(", ");
        let enc = r.encryption_configuration();
        let scan = r
            .image_scanning_configuration()
            .map(|c| if c.scan_on_push() { "yes" } else { "no" });
        let mut fields = Vec::new();
        put(&mut fields, "ARN", r.repository_arn());
        put(&mut fields, "URI", r.repository_uri());
        put(
            &mut fields,
            "Mutability",
            r.image_tag_mutability().map(|m| m.as_str()),
        );
        put(
            &mut fields,
            "Exclusions",
            Some(exclusions.as_str()).filter(|s| !s.is_empty()),
        );
        put(
            &mut fields,
            "Encryption",
            enc.map(|e| e.encryption_type().as_str()),
        );
        put(&mut fields, "KMS key", enc.and_then(|e| e.kms_key()));
        put(&mut fields, "Scan push", scan);
        Ok(Details { fields })
    }

    async fn delete(
        &self,
        repo: &RepoPath,
        ops: &[DeleteOp],
    ) -> Result<DeleteResult, RegistryError> {
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
            let out = {
                let _permit = self.gate.acquire().await;
                write_call(
                    &self.log,
                    "ecr:BatchDeleteImage",
                    self.client
                        .batch_delete_image()
                        .registry_id(&self.registry_id)
                        .repository_name(&repo.0)
                        .set_image_ids(Some(ids))
                        .customize()
                        .config_override(
                            aws_sdk_ecr::config::Builder::default()
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

    use aws_sdk_ecr::config::http::HttpResponse;
    use aws_sdk_ecr::error::ErrorMetadata;
    use aws_sdk_ecr::operation::batch_delete_image::{
        BatchDeleteImageInput, BatchDeleteImageOutput,
    };
    use aws_sdk_ecr::operation::describe_images::{DescribeImagesError, DescribeImagesOutput};
    use aws_sdk_ecr::types::{ImageFailure, ImageFailureCode, ImageIdentifier};
    use aws_smithy_mocks::{Rule, RuleMode, mock, mock_client};

    use super::*;

    const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const D2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    const D3: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";

    fn provider(rules: &[&Rule]) -> EcrProvider {
        let client = mock_client!(aws_sdk_ecr, rules);
        EcrProvider::new(
            client,
            mock_client!(aws_sdk_sts, &[]),
            "123456789012".into(),
            Arc::new(Gate::new(8)),
        )
    }

    fn provider_sts(rules: &[&Rule]) -> EcrProvider {
        EcrProvider::new(
            mock_client!(aws_sdk_ecr, &[]),
            mock_client!(aws_sdk_sts, rules),
            "123456789012".into(),
            Arc::new(Gate::new(8)),
        )
    }

    fn detail(digest: &str, status: ImageStatus) -> ImageDetail {
        ImageDetail::builder()
            .image_digest(digest)
            .image_tags("b")
            .image_tags("a")
            .image_status(status)
            .build()
    }

    fn repo() -> RepoPath {
        RepoPath("app".into())
    }

    #[tokio::test]
    async fn describe_images_sends_status_any_and_paginates() {
        let r1 = mock!(aws_sdk_ecr::Client::describe_images)
            .match_requests(|i| {
                let f = i.filter().expect("filter");
                f.image_status() == Some(&ImageStatusFilter::Any)
                    && f.tag_status() == Some(&TagStatus::Any)
                    && i.next_token().is_none()
                    && i.registry_id() == Some("123456789012")
                    && i.repository_name() == Some("app")
            })
            .then_output(|| {
                DescribeImagesOutput::builder()
                    .image_details(detail(D1, ImageStatus::Active))
                    .next_token("t1")
                    .build()
            });
        let r2 = mock!(aws_sdk_ecr::Client::describe_images)
            .match_requests(|i| i.next_token() == Some("t1"))
            .then_output(|| {
                DescribeImagesOutput::builder()
                    .image_details(detail(D2, ImageStatus::Active))
                    .build()
            });
        let calls = Mutex::new(Vec::new());
        let got = provider(&[&r1, &r2])
            .artifacts(&repo(), &|n| calls.lock().unwrap().push(n))
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].tags, ["a", "b"]);
        assert_eq!(*calls.lock().unwrap(), [1, 2]);
        assert_eq!((r1.num_calls(), r2.num_calls()), (1, 1));
    }

    #[tokio::test]
    async fn archived_maps_to_archived() {
        let r = mock!(aws_sdk_ecr::Client::describe_images).then_output(|| {
            DescribeImagesOutput::builder()
                .image_details(detail(D1, ImageStatus::Archived))
                .image_details(detail(D2, ImageStatus::Activating))
                .build()
        });
        let got = provider(&[&r]).artifacts(&repo(), &|_| {}).await.unwrap();
        assert_eq!(got[0].status, ArtifactStatus::Archived);
        assert_eq!(got[1].status, ArtifactStatus::Other("ACTIVATING".into()));
    }

    #[tokio::test]
    async fn repeated_next_token_errors() {
        let page = || {
            DescribeImagesOutput::builder()
                .image_details(detail(D1, ImageStatus::Active))
                .next_token("same")
                .build()
        };
        let (r1, r2) = (
            mock!(aws_sdk_ecr::Client::describe_images).then_output(page),
            mock!(aws_sdk_ecr::Client::describe_images).then_output(page),
        );
        let e = provider(&[&r1, &r2])
            .artifacts(&repo(), &|_| {})
            .await
            .unwrap_err();
        assert!(matches!(e, RegistryError::RepeatedCursor(_)), "{e:?}");
    }

    #[tokio::test]
    async fn access_denied_maps_to_forbidden() {
        let r = mock!(aws_sdk_ecr::Client::describe_images).then_error(|| {
            DescribeImagesError::generic(
                ErrorMetadata::builder()
                    .code("AccessDeniedException")
                    .message("nope")
                    .build(),
            )
        });
        let e = provider(&[&r])
            .artifacts(&repo(), &|_| {})
            .await
            .unwrap_err();
        assert_eq!(e, RegistryError::Forbidden("ecr:DescribeImages".into()));
    }

    #[tokio::test]
    async fn repository_not_found_maps_to_not_found() {
        let r = mock!(aws_sdk_ecr::Client::describe_images).then_error(|| {
            DescribeImagesError::generic(
                ErrorMetadata::builder()
                    .code("RepositoryNotFoundException")
                    .build(),
            )
        });
        let e = provider(&[&r])
            .artifacts(&repo(), &|_| {})
            .await
            .unwrap_err();
        assert_eq!(e, RegistryError::NotFound);
    }

    #[tokio::test]
    async fn repositories_paginate_and_map() {
        use aws_sdk_ecr::operation::describe_repositories::DescribeRepositoriesOutput;
        use aws_sdk_ecr::types::Repository;
        let r1 = mock!(aws_sdk_ecr::Client::describe_repositories)
            .match_requests(|i| i.registry_id() == Some("123456789012") && i.next_token().is_none())
            .then_output(|| {
                DescribeRepositoriesOutput::builder()
                    .repositories(
                        Repository::builder()
                            .repository_name("team/app")
                            .created_at(aws_sdk_ecr::primitives::DateTime::from_secs(1_700_000_000))
                            .build(),
                    )
                    .next_token("t1")
                    .build()
            });
        let r2 = mock!(aws_sdk_ecr::Client::describe_repositories)
            .match_requests(|i| i.next_token() == Some("t1"))
            .then_output(|| {
                DescribeRepositoriesOutput::builder()
                    .repositories(Repository::builder().repository_name("zed").build())
                    .build()
            });
        let calls = Mutex::new(Vec::new());
        let got = provider(&[&r1, &r2])
            .repositories(&|n| calls.lock().unwrap().push(n))
            .await
            .unwrap();
        assert_eq!(got[0].path, RepoPath("team/app".into()));
        assert_eq!(got[0].created_at.unwrap().unix_timestamp(), 1_700_000_000);
        assert_eq!(got[1].path, RepoPath("zed".into()));
        assert_eq!(*calls.lock().unwrap(), [1, 2]);
    }

    #[test]
    fn map_image_fields() {
        let d = ImageDetail::builder()
            .image_digest(D1)
            .image_manifest_media_type("m")
            .artifact_media_type("a")
            .subject_manifest_digest(D2)
            .image_size_in_bytes(42)
            .build();
        let s = map_image(&d).unwrap();
        assert_eq!(s.media_type.as_deref(), Some("m"));
        assert_eq!(s.artifact_type.as_deref(), Some("a"));
        assert_eq!(s.subject.unwrap().to_string(), D2);
        assert_eq!(s.size, Some(42));
        assert_eq!(s.status, ArtifactStatus::Active);
        assert!(map_image(&ImageDetail::builder().build()).is_none());
    }

    fn flt(t: ImageTagMutabilityExclusionFilterType, f: &str) -> ImageTagMutabilityExclusionFilter {
        ImageTagMutabilityExclusionFilter::builder()
            .filter_type(t)
            .filter(f)
            .build()
            .unwrap()
    }

    fn wild(fs: &[&str]) -> Vec<ImageTagMutabilityExclusionFilter> {
        fs.iter()
            .map(|f| flt(ImageTagMutabilityExclusionFilterType::Wildcard, f))
            .collect()
    }

    #[test]
    fn ecr_policy_basic() {
        use ImageTagMutability as M;
        assert_eq!(policy_for(&M::Mutable, &[], "x"), TagPolicy::Mutable);
        assert_eq!(policy_for(&M::Immutable, &[], "x"), TagPolicy::Immutable);
        assert_eq!(policy_for(&M::from("FUTURE"), &[], "x"), TagPolicy::Unknown);
    }

    #[test]
    fn ecr_policy_exclusions() {
        use ImageTagMutability as M;
        let f = wild(&["dev-*", "latest"]);
        let p = |t| policy_for(&M::ImmutableWithExclusion, &f, t);
        assert_eq!(p("dev-1"), TagPolicy::Mutable);
        assert_eq!(p("latest"), TagPolicy::Mutable);
        assert_eq!(p("1.0.0"), TagPolicy::Immutable);
        let f = wild(&["release-*"]);
        let p = |t| policy_for(&M::MutableWithExclusion, &f, t);
        assert_eq!(p("release-1"), TagPolicy::Immutable);
        assert_eq!(p("x"), TagPolicy::Mutable);
    }

    #[test]
    fn ecr_policy_unknown_filter_type() {
        let f = [flt(
            ImageTagMutabilityExclusionFilterType::from("Regex"),
            "x",
        )];
        for m in [
            ImageTagMutability::ImmutableWithExclusion,
            ImageTagMutability::MutableWithExclusion,
        ] {
            assert_eq!(policy_for(&m, &f, "x"), TagPolicy::Unknown);
        }
    }

    #[tokio::test]
    async fn ecr_tag_policy_calls_describe_repositories() {
        use aws_sdk_ecr::operation::describe_repositories::DescribeRepositoriesOutput;
        use aws_sdk_ecr::types::Repository;
        let r = mock!(aws_sdk_ecr::Client::describe_repositories)
            .match_requests(|i| {
                i.repository_names() == ["app"] && i.registry_id() == Some("123456789012")
            })
            .then_output(|| {
                DescribeRepositoriesOutput::builder()
                    .repositories(
                        Repository::builder()
                            .repository_name("app")
                            .image_tag_mutability(ImageTagMutability::Immutable)
                            .build(),
                    )
                    .build()
            });
        let gate = Arc::new(Gate::new(1));
        let p = EcrProvider::new(
            mock_client!(aws_sdk_ecr, [&r]),
            mock_client!(aws_sdk_sts, &[]),
            "123456789012".into(),
            gate.clone(),
        );
        assert_eq!(p.tag_policy(&repo(), "1.0").await, Ok(TagPolicy::Immutable));
        assert_eq!(r.num_calls(), 1);
        // The single permit was released after the call.
        drop(gate.acquire().await);
    }

    fn caller(account: &str, arn: &str) -> Rule {
        use aws_sdk_sts::operation::get_caller_identity::GetCallerIdentityOutput;
        let (account, arn) = (account.to_string(), arn.to_string());
        mock!(aws_sdk_sts::Client::get_caller_identity).then_output(move || {
            GetCallerIdentityOutput::builder()
                .account(&account)
                .arn(&arn)
                .build()
        })
    }

    fn describe_one(repo: aws_sdk_ecr::types::Repository) -> Rule {
        use aws_sdk_ecr::operation::describe_repositories::DescribeRepositoriesOutput;
        mock!(aws_sdk_ecr::Client::describe_repositories)
            .match_requests(|i| {
                i.registry_id() == Some("123456789012") && i.repository_names() == ["team/app"]
            })
            .then_output(move || {
                DescribeRepositoriesOutput::builder()
                    .repositories(repo.clone())
                    .build()
            })
    }

    fn pair(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    #[tokio::test]
    async fn registry_details_from_sts() {
        let arn = "arn:aws:sts::111122223333:assumed-role/dev/me";
        let r = caller("111122223333", arn);
        let d = provider_sts(&[&r]).registry_details().await.unwrap();
        assert_eq!(
            d.fields,
            [
                pair("Registry", "123456789012"),
                pair("Account", "111122223333"),
                pair("Caller", arn)
            ]
        );
    }

    #[tokio::test]
    async fn registry_details_sts_error() {
        use aws_sdk_sts::operation::get_caller_identity::GetCallerIdentityError;
        let r = mock!(aws_sdk_sts::Client::get_caller_identity).then_error(|| {
            GetCallerIdentityError::generic(ErrorMetadata::builder().code("AccessDenied").build())
        });
        let e = provider_sts(&[&r]).registry_details().await;
        assert_eq!(
            e,
            Err(RegistryError::Forbidden("sts:GetCallerIdentity".into()))
        );
    }

    #[tokio::test]
    async fn repo_details_full() {
        use aws_sdk_ecr::types::{
            EncryptionConfiguration, EncryptionType, ImageScanningConfiguration,
        };
        let key = "arn:aws:kms:us-west-2:123456789012:key/k1";
        let filter = |f: &str| {
            ImageTagMutabilityExclusionFilter::builder()
                .filter(f)
                .filter_type(ImageTagMutabilityExclusionFilterType::Wildcard)
                .build()
                .unwrap()
        };
        let r = describe_one(
            aws_sdk_ecr::types::Repository::builder()
                .repository_arn("arn:aws:ecr:us-west-2:123456789012:repository/team/app")
                .repository_uri("123456789012.dkr.ecr.us-west-2.amazonaws.com/team/app")
                .image_tag_mutability(ImageTagMutability::ImmutableWithExclusion)
                .image_tag_mutability_exclusion_filters(filter("latest"))
                .image_tag_mutability_exclusion_filters(filter("dev-*"))
                .encryption_configuration(
                    EncryptionConfiguration::builder()
                        .encryption_type(EncryptionType::Kms)
                        .kms_key(key)
                        .build()
                        .unwrap(),
                )
                .image_scanning_configuration(
                    ImageScanningConfiguration::builder()
                        .scan_on_push(true)
                        .build(),
                )
                .build(),
        );
        let d = provider(&[&r])
            .repo_details(&RepoPath("team/app".into()))
            .await
            .unwrap();
        assert_eq!(
            d.fields,
            [
                pair(
                    "ARN",
                    "arn:aws:ecr:us-west-2:123456789012:repository/team/app"
                ),
                pair(
                    "URI",
                    "123456789012.dkr.ecr.us-west-2.amazonaws.com/team/app"
                ),
                pair("Mutability", "IMMUTABLE_WITH_EXCLUSION"),
                pair("Exclusions", "latest, dev-*"),
                pair("Encryption", "KMS"),
                pair("KMS key", key),
                pair("Scan push", "yes"),
            ]
        );
    }

    #[tokio::test]
    async fn repo_details_minimal() {
        let r = describe_one(
            aws_sdk_ecr::types::Repository::builder()
                .repository_arn("arn:x")
                .build(),
        );
        let d = provider(&[&r])
            .repo_details(&RepoPath("team/app".into()))
            .await
            .unwrap();
        assert_eq!(d.fields, [pair("ARN", "arn:x")]);
    }

    #[tokio::test]
    async fn repo_details_unknown_enums() {
        use aws_sdk_ecr::types::{EncryptionConfiguration, EncryptionType};
        let r = describe_one(
            aws_sdk_ecr::types::Repository::builder()
                .image_tag_mutability(ImageTagMutability::from("SOMETHING_NEW"))
                .encryption_configuration(
                    EncryptionConfiguration::builder()
                        .encryption_type(EncryptionType::from("NEW_ENC"))
                        .build()
                        .unwrap(),
                )
                .build(),
        );
        let d = provider(&[&r])
            .repo_details(&RepoPath("team/app".into()))
            .await
            .unwrap();
        assert_eq!(
            d.fields,
            [
                pair("Mutability", "SOMETHING_NEW"),
                pair("Encryption", "NEW_ENC")
            ]
        );
    }

    #[tokio::test]
    async fn repo_details_not_found() {
        use aws_sdk_ecr::operation::describe_repositories::DescribeRepositoriesOutput;
        let r = mock!(aws_sdk_ecr::Client::describe_repositories)
            .then_output(|| DescribeRepositoriesOutput::builder().build());
        let e = provider(&[&r]).repo_details(&repo()).await;
        assert_eq!(e, Err(RegistryError::NotFound));
    }

    fn rm_tag(tag: &str, d: &str) -> DeleteOp {
        DeleteOp::RemoveTag {
            tag: tag.into(),
            expected: Digest::parse(d).unwrap(),
        }
    }

    fn rm_manifest(d: &str) -> DeleteOp {
        DeleteOp::DeleteManifest {
            digest: Digest::parse(d).unwrap(),
        }
    }

    fn manifests(n: u64) -> Vec<DeleteOp> {
        (1..=n)
            .map(|i| DeleteOp::DeleteManifest {
                digest: crate::app::planner::tests::d(i),
            })
            .collect()
    }

    fn id(tag: Option<&str>, d: &str) -> ImageIdentifier {
        ImageIdentifier::builder()
            .set_image_tag(tag.map(Into::into))
            .image_digest(d)
            .build()
    }

    /// Answers every requested image as deleted.
    fn echo(i: &BatchDeleteImageInput) -> BatchDeleteImageOutput {
        BatchDeleteImageOutput::builder()
            .set_image_ids(Some(i.image_ids().to_vec()))
            .build()
    }

    fn http(status: u16, body: &'static str) -> HttpResponse {
        HttpResponse::new(status.try_into().unwrap(), body.into())
    }

    /// Rules may answer more than once, so a retry would be visible in `num_calls`.
    fn provider_any(rules: &[&Rule]) -> EcrProvider {
        let client = mock_client!(aws_sdk_ecr, RuleMode::MatchAny, rules);
        EcrProvider::new(
            client,
            mock_client!(aws_sdk_sts, &[]),
            "123456789012".into(),
            Arc::new(Gate::new(8)),
        )
    }

    fn outcomes(r: &DeleteResult) -> Vec<OpOutcome> {
        r.0.results.iter().map(|(_, o)| o.clone()).collect()
    }

    #[test]
    fn match_outcomes_maps() {
        let ops = [rm_tag("1", D1), rm_manifest(D2), rm_manifest(D3)];
        let got = match_outcomes(
            &ops,
            &[(Some(D1.into()), Some("1".into()))],
            &[(
                Some(D2.into()),
                None,
                "ImageReferencedByManifestList: in use".into(),
            )],
        );
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], (ops[0].clone(), OpOutcome::Done));
        assert!(
            matches!(&got[1], (o, OpOutcome::Failed(t)) if *o == ops[1] && t.contains("ImageReferencedByManifestList")),
            "{got:?}"
        );
        assert_eq!(
            got[2],
            (
                ops[2].clone(),
                OpOutcome::Uncertain("no answer for this image".into())
            )
        );
        // A digest-only entry does not answer a tag removal.
        let got = match_outcomes(&ops[..1], &[(Some(D1.into()), None)], &[]);
        assert!(matches!(got[0].1, OpOutcome::Uncertain(_)), "{got:?}");
        // A tag entry for another digest is not this op's answer.
        let got = match_outcomes(&ops[..1], &[(Some(D2.into()), Some("1".into()))], &[]);
        assert!(matches!(got[0].1, OpOutcome::Uncertain(_)), "{got:?}");
    }

    #[tokio::test]
    async fn ecr_delete_sends_tag_and_digest() {
        let r = mock!(aws_sdk_ecr::Client::batch_delete_image)
            .match_requests(|i| {
                i.registry_id() == Some("123456789012")
                    && i.repository_name() == Some("app")
                    && i.image_ids() == [id(Some("1"), D1), id(None, D2)]
            })
            .then_compute_output(echo);
        let (rep, stop) = provider(&[&r])
            .delete(&repo(), &[rm_tag("1", D1), rm_manifest(D2)])
            .await
            .unwrap();
        assert_eq!(
            outcomes(&(rep, stop.clone())),
            [OpOutcome::Done, OpOutcome::Done]
        );
        assert!(stop.is_none());
        assert_eq!(r.num_calls(), 1);
    }

    #[tokio::test]
    async fn ecr_delete_batches_of_100() {
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let seen = sizes.clone();
        let r = mock!(aws_sdk_ecr::Client::batch_delete_image)
            .match_requests(move |i| {
                seen.lock().unwrap().push(i.image_ids().len());
                true
            })
            .then_compute_output(echo);
        let res = provider_any(&[&r])
            .delete(&repo(), &manifests(150))
            .await
            .unwrap();
        assert!(outcomes(&res).iter().all(|o| *o == OpOutcome::Done));
        assert_eq!(res.0.results.len(), 150);
        assert_eq!(r.num_calls(), 2);
        assert_eq!(*sizes.lock().unwrap(), [100, 50]);
    }

    #[tokio::test]
    async fn ecr_delete_tag_mismatch_failure() {
        let r = mock!(aws_sdk_ecr::Client::batch_delete_image).then_output(|| {
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
        let res = provider(&[&r])
            .delete(&repo(), &[rm_tag("1", D1)])
            .await
            .unwrap();
        assert!(
            matches!(&outcomes(&res)[..], [OpOutcome::Failed(t)] if t.starts_with("ImageTagDoesNotMatchDigest")),
            "{res:?}"
        );
    }

    #[tokio::test]
    async fn ecr_delete_not_retried() {
        let r = mock!(aws_sdk_ecr::Client::batch_delete_image).then_http_response(|| {
            http(
                400,
                r#"{"__type":"ThrottlingException","message":"slow down"}"#,
            )
        });
        let res = provider_any(&[&r])
            .delete(&repo(), &[rm_manifest(D1), rm_manifest(D2)])
            .await
            .unwrap();
        assert_eq!(r.num_calls(), 1);
        assert!(
            outcomes(&res)
                .iter()
                .all(|o| matches!(o, OpOutcome::Failed(t) if t.contains("ThrottlingException"))),
            "{res:?}"
        );
        assert!(res.1.is_none());
    }

    #[tokio::test]
    async fn ecr_delete_server_exception_uncertain_stops() {
        let r = mock!(aws_sdk_ecr::Client::batch_delete_image)
            .then_http_response(|| http(500, r#"{"__type":"ServerException","message":"boom"}"#));
        let res = provider_any(&[&r])
            .delete(&repo(), &manifests(150))
            .await
            .unwrap();
        let o = outcomes(&res);
        assert_eq!(o.len(), 150);
        assert!(
            o[..100]
                .iter()
                .all(|o| matches!(o, OpOutcome::Uncertain(t) if t.contains("ServerException"))),
            "{o:?}"
        );
        assert!(o[100..].iter().all(|o| *o == OpOutcome::NotAttempted));
        assert_eq!(r.num_calls(), 1);
    }

    #[tokio::test]
    async fn ecr_delete_expired_token_not_sent() {
        let r = mock!(aws_sdk_ecr::Client::batch_delete_image).then_http_response(|| {
            http(
                400,
                r#"{"__type":"ExpiredTokenException","message":"expired"}"#,
            )
        });
        let p = provider_any(&[&r]);
        // First batch: nothing dispatched, so the whole delete is an error.
        let e = p.delete(&repo(), &manifests(150)).await.unwrap_err();
        assert!(matches!(e, RegistryError::AwsCredentials(_)), "{e:?}");
        assert_eq!(r.num_calls(), 1);
    }

    #[tokio::test]
    async fn ecr_delete_credential_failure_not_attempted() {
        use aws_credential_types::provider::error::CredentialsError;
        use aws_credential_types::provider::future::ProvideCredentials as Fut;
        use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
        #[derive(Debug)]
        struct NoCreds;
        impl ProvideCredentials for NoCreds {
            fn provide_credentials<'a>(&'a self) -> Fut<'a>
            where
                Self: 'a,
            {
                Fut::ready(Err(CredentialsError::not_loaded("no profile")))
            }
        }
        // An HTTP answer is served at transmit, after identity resolution (a modeled output
        // would short-circuit before it).
        let r = mock!(aws_sdk_ecr::Client::batch_delete_image)
            .then_http_response(|| http(200, r#"{"imageIds":[],"failures":[]}"#));
        let client = mock_client!(aws_sdk_ecr, RuleMode::MatchAny, [&r], |c| c
            .credentials_provider(SharedCredentialsProvider::new(NoCreds)));
        let p = EcrProvider::new(
            client,
            mock_client!(aws_sdk_sts, &[]),
            "123456789012".into(),
            Arc::new(Gate::new(8)),
        );
        // Through the real orchestrator: identity resolution fails before transmit.
        let e = p.delete(&repo(), &manifests(2)).await.unwrap_err();
        assert!(matches!(e, RegistryError::AwsCredentials(_)), "{e:?}");
    }

    #[test]
    fn call_error_classification() {
        use aws_credential_types::provider::error::CredentialsError;
        use aws_sdk_ecr::error::{ConnectorError, SdkError};
        use aws_sdk_ecr::operation::batch_delete_image::BatchDeleteImageError;
        let e = SdkError::<BatchDeleteImageError, HttpResponse>::construction_failure("x");
        assert!(matches!(
            call_error("ecr:BatchDeleteImage", e),
            DeleteError::NotSent(_)
        ));
        let e = SdkError::<BatchDeleteImageError, HttpResponse>::timeout_error("x");
        assert!(matches!(
            call_error("ecr:BatchDeleteImage", e),
            DeleteError::Uncertain(_)
        ));
        let cause = Box::new(CredentialsError::not_loaded("x"));
        let e = SdkError::<BatchDeleteImageError, HttpResponse>::dispatch_failure(
            ConnectorError::other(cause, None),
        );
        assert!(
            matches!(
                call_error("ecr:BatchDeleteImage", e),
                DeleteError::NotSent(RegistryError::AwsCredentials(_))
            ),
            "credential failure is not sent"
        );
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let e = SdkError::<BatchDeleteImageError, HttpResponse>::dispatch_failure(
            ConnectorError::io(Box::new(io)),
        );
        assert!(matches!(
            call_error("ecr:BatchDeleteImage", e),
            DeleteError::Uncertain(_)
        ));
    }

    #[test]
    fn ecr_caps_delete_tag_true() {
        assert!(provider(&[]).caps().delete_tag);
    }

    #[tokio::test]
    async fn ecr_tag_policy_missing_repo_is_not_found() {
        use aws_sdk_ecr::operation::describe_repositories::DescribeRepositoriesOutput;
        let r = mock!(aws_sdk_ecr::Client::describe_repositories)
            .then_output(|| DescribeRepositoriesOutput::builder().build());
        assert_eq!(
            provider(&[&r]).tag_policy(&repo(), "1.0").await,
            Err(RegistryError::NotFound)
        );
    }

    #[tokio::test]
    async fn sdk_calls_logged() {
        use crate::reqlog::RequestLog;
        use aws_sdk_ecr::operation::describe_repositories::{
            DescribeRepositoriesError, DescribeRepositoriesOutput,
        };
        let ok = mock!(aws_sdk_ecr::Client::describe_repositories)
            .then_output(|| DescribeRepositoriesOutput::builder().build());
        let denied = mock!(aws_sdk_ecr::Client::describe_repositories).then_error(|| {
            DescribeRepositoriesError::generic(
                ErrorMetadata::builder()
                    .code("AccessDeniedException")
                    .build(),
            )
        });
        let log = RequestLog::default();
        let p = provider(&[&ok, &denied]).with_log(log.scoped("ecr"));
        p.repositories(&|_| {}).await.unwrap();
        p.repositories(&|_| {}).await.unwrap_err();
        let got: Vec<_> = log
            .snapshot()
            .into_iter()
            .map(|e| (e.registry, e.op, e.path, e.outcome))
            .collect();
        let op = "ecr:DescribeRepositories";
        assert_eq!(
            got,
            [
                ("ecr".into(), op.into(), String::new(), "ok".into()),
                (
                    "ecr".into(),
                    op.into(),
                    String::new(),
                    "permission denied".into()
                ),
            ]
        );
    }
}

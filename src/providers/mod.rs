pub mod ecr;
pub mod ecr_public;
pub mod oci;

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::auth::CredentialSource;
use crate::auth::basic::BasicSource;
use crate::auth::docker_config::{DockerConfigSource, default_config_dir};
use crate::config::{AuthCfg, Config, ProviderCfg, RegistryCfg};
use crate::domain::{
    ArtifactSummary, Caps, DeleteOp, DeleteReport, Details, OpOutcome, RegistryError, RegistryId,
    RepoPath, RepoSummary, TagPolicy,
};
use crate::registry::{DeleteError, Gate, OciClient};
use crate::reqlog::RequestLog;

pub use ecr::EcrProvider;
pub use ecr_public::EcrPublicProvider;
pub use oci::OciProvider;

#[async_trait]
pub trait Provider: Send + Sync {
    fn caps(&self) -> Caps;
    /// `progress(pages_so_far)` is called after each page.
    async fn repositories(
        &self,
        progress: &(dyn Fn(usize) + Sync),
    ) -> Result<Vec<RepoSummary>, RegistryError>;
    async fn artifacts(
        &self,
        repo: &RepoPath,
        progress: &(dyn Fn(usize) + Sync),
    ) -> Result<Vec<ArtifactSummary>, RegistryError>;
    /// Whether `tag` of `repo` can be moved; `Unknown` when the registry has no way to say.
    async fn tag_policy(&self, _repo: &RepoPath, _tag: &str) -> Result<TagPolicy, RegistryError> {
        Ok(TagPolicy::Unknown)
    }
    /// Provider-reported facts about the registry; empty when the provider has none.
    async fn registry_details(&self) -> Result<Details, RegistryError> {
        Ok(Details::default())
    }
    /// Provider-reported facts about `repo`; empty when the provider has none.
    async fn repo_details(&self, _repo: &RepoPath) -> Result<Details, RegistryError> {
        Ok(Details::default())
    }
    /// `Err` only if no op can have been dispatched. Once any call may have been dispatched,
    /// returns `Ok` with an outcome for every op. Write calls are made with SDK/HTTP retries disabled.
    async fn delete(
        &self,
        _repo: &RepoPath,
        _ops: &[DeleteOp],
    ) -> Result<DeleteResult, RegistryError> {
        Err(RegistryError::Unsupported("delete".into()))
    }
}

/// The report, and why the run stopped: the error of a call that was not sent
/// after earlier calls had been dispatched.
pub type DeleteResult = (DeleteReport, Option<RegistryError>);

pub const DELETION_DISABLED: &str = "deletion disabled on this registry";

/// Sends `ops` in chunks of `size`, one `call` per chunk, and turns each call's
/// `DeleteError` into per-op outcomes and stop rules. `call` answers each op of
/// its chunk once, in order; any other answer marks the unmatched ops `Uncertain`
/// and stops the run.
pub(crate) async fn run_batches<F, Fut>(
    ops: &[DeleteOp],
    size: usize,
    mut call: F,
) -> Result<DeleteResult, RegistryError>
where
    F: FnMut(Vec<DeleteOp>) -> Fut,
    Fut: Future<Output = Result<Vec<(DeleteOp, OpOutcome)>, DeleteError>>,
{
    fn each(c: &[DeleteOp], o: OpOutcome) -> impl Iterator<Item = (DeleteOp, OpOutcome)> + '_ {
        c.iter().map(move |op| (op.clone(), o.clone()))
    }
    debug_assert!(size > 0);
    let mut results = Vec::with_capacity(ops.len());
    let mut aborted_unsupported = false;
    let mut stopped = None;
    let mut chunks = ops.chunks(size);
    for (i, c) in chunks.by_ref().enumerate() {
        let stop = match call(c.to_vec()).await {
            // Matched by position; a missing or mismatched answer is `Uncertain`.
            Ok(v) => {
                let mut stop = v.len() != c.len();
                for (j, op) in c.iter().enumerate() {
                    let out = match v.get(j) {
                        Some((o, out)) if o == op => out.clone(),
                        _ => OpOutcome::Uncertain("no answer for this op".into()),
                    };
                    stop |= matches!(out, OpOutcome::Uncertain(_));
                    results.push((op.clone(), out));
                }
                stop
            }
            Err(DeleteError::NotSent(e)) if i == 0 => return Err(e),
            Err(DeleteError::NotSent(e)) => {
                results.extend(each(c, OpOutcome::NotAttempted));
                stopped = Some(e);
                true
            }
            Err(DeleteError::Rejected(m)) => {
                results.extend(each(c, OpOutcome::Failed(m)));
                false
            }
            Err(DeleteError::Uncertain(m)) => {
                results.extend(each(c, OpOutcome::Uncertain(m)));
                true
            }
            Err(DeleteError::Unsupported) => {
                results.push((c[0].clone(), OpOutcome::Failed(DELETION_DISABLED.into())));
                results.extend(each(&c[1..], OpOutcome::NotAttempted));
                aborted_unsupported = true;
                true
            }
        };
        if stop {
            break;
        }
    }
    for c in chunks {
        results.extend(each(c, OpOutcome::NotAttempted));
    }
    Ok((
        DeleteReport {
            results,
            aborted_unsupported,
        },
        stopped,
    ))
}

pub struct Registry {
    pub id: RegistryId,
    pub cfg: RegistryCfg,
    pub provider: Arc<dyn Provider>,
    pub oci: Arc<OciClient>,
    pub gate: Arc<Gate>,
    /// The source given to `oci`; also issues the `docker login` credential.
    pub creds: Arc<dyn CredentialSource>,
}

/// Builds every configured registry; makes no network calls.
pub async fn build_registries(
    cfg: &Config,
    log: &RequestLog,
) -> Result<Vec<Registry>, RegistryError> {
    let mut out = Vec::new();
    for rc in &cfg.registries {
        out.push(build_registry(rc, log, &default_config_dir()).await?);
    }
    Ok(out)
}

/// Builds one registry, its calls logged under its id; makes no network calls.
pub async fn build_registry(
    rc: &RegistryCfg,
    log: &RequestLog,
    docker_config_dir: &Path,
) -> Result<Registry, RegistryError> {
    let gate = Arc::new(Gate::new(8));
    let reg_log = log.scoped(&rc.id.0);
    let named = |e| RegistryError::Provider(format!("registry \"{}\": {e}", rc.id.0));
    let (creds, aws): (Arc<dyn CredentialSource>, Option<Arc<dyn Provider>>) = match &rc.provider {
        ProviderCfg::Ecr {
            account_id,
            region,
            profile,
        } => {
            let (creds, p) = ecr::build(
                region,
                profile.as_deref(),
                account_id,
                gate.clone(),
                reg_log.clone(),
            )
            .await
            .map_err(named)?;
            (creds, Some(Arc::new(p)))
        }
        ProviderCfg::EcrPublic { alias, profile } => {
            let (creds, p) =
                ecr_public::build(profile.as_deref(), alias, gate.clone(), reg_log.clone())
                    .await
                    .map_err(named)?;
            (creds, Some(Arc::new(p)))
        }
        ProviderCfg::Oci => {
            let creds: Arc<dyn CredentialSource> = match &rc.auth {
                AuthCfg::Anonymous => Arc::new(crate::auth::AnonymousSource),
                AuthCfg::DockerConfig => Arc::new(DockerConfigSource::new(
                    docker_config_dir.to_path_buf(),
                    rc.ref_host.clone(),
                )),
                AuthCfg::Basic {
                    username,
                    password_env,
                } => Arc::new(BasicSource::new(username.clone(), password_env.clone())),
                // Rejected by config validation for oci registries.
                AuthCfg::Ecr => {
                    return Err(RegistryError::Internal(format!(
                        "registry {}: auth type ecr is not valid for provider oci",
                        rc.id
                    )));
                }
            };
            (creds, None)
        }
    };
    let oci = Arc::new(
        OciClient::new(
            rc.endpoint.clone(),
            rc.trusted_realms.clone(),
            creds.clone(),
            gate.clone(),
        )
        .with_log(reg_log),
    );
    let provider: Arc<dyn Provider> = match aws {
        Some(p) => p,
        None => Arc::new(OciProvider::new(oci.clone(), rc.repositories.clone())),
    };
    Ok(Registry {
        id: rc.id.clone(),
        cfg: rc.clone(),
        provider,
        oci,
        gate,
        creds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::planner::tests::d;
    use OpOutcome::*;

    type Answer = Result<Vec<(DeleteOp, OpOutcome)>, DeleteError>;

    fn done(chunk: &[DeleteOp]) -> Answer {
        Ok(chunk.iter().map(|o| (o.clone(), Done)).collect())
    }

    /// Runs `n` manifest ops in chunks of `size`; chunk `i` is answered by
    /// `script(i, chunk)`. Returns the result and the chunk sizes sent.
    /// Asserts that a report lists every input op once, in order.
    async fn run(
        n: u64,
        size: usize,
        script: impl Fn(usize, &[DeleteOp]) -> Answer,
    ) -> (Result<DeleteResult, RegistryError>, Vec<usize>) {
        let ops: Vec<_> = (1..=n)
            .map(|i| DeleteOp::DeleteManifest { digest: d(i) })
            .collect();
        let mut sent = Vec::new();
        let r = run_batches(&ops, size, |chunk| {
            let a = script(sent.len(), &chunk);
            sent.push(chunk.len());
            async move { a }
        })
        .await;
        if let Ok((rep, _)) = &r {
            let got: Vec<_> = rep.results.iter().map(|(o, _)| o.clone()).collect();
            assert_eq!(got, ops);
        }
        (r, sent)
    }

    fn outcomes(r: &DeleteResult) -> Vec<OpOutcome> {
        r.0.results.iter().map(|(_, o)| o.clone()).collect()
    }

    #[tokio::test]
    async fn batches_all_ok() {
        let (r, sent) = run(3, 2, |_, c| done(c)).await;
        let r = r.unwrap();
        assert_eq!(sent, [2, 1]);
        assert_eq!(outcomes(&r), [Done, Done, Done]);
        assert!(!r.0.aborted_unsupported && r.1.is_none());
    }

    #[tokio::test]
    async fn first_chunk_not_sent_is_err() {
        let e = RegistryError::AwsCredentials("expired".into());
        let (r, sent) = run(3, 2, |_, _| Err(DeleteError::NotSent(e.clone()))).await;
        assert_eq!(r, Err(e));
        assert_eq!(sent, [2]);
    }

    #[tokio::test]
    async fn later_not_sent_stops() {
        let (r, sent) = run(5, 2, |i, c| match i {
            0 => done(c),
            _ => Err(DeleteError::NotSent(RegistryError::AwsCredentials(
                "expired".into(),
            ))),
        })
        .await;
        let r = r.unwrap();
        assert_eq!(sent.len(), 2);
        let na = NotAttempted;
        assert_eq!(outcomes(&r), [Done, Done, na.clone(), na.clone(), na]);
        assert!(matches!(r.1, Some(RegistryError::AwsCredentials(_))));
    }

    #[tokio::test]
    async fn uncertain_stops() {
        let (r, sent) = run(5, 2, |i, c| match i {
            0 => done(c),
            _ => Err(DeleteError::Uncertain("m".into())),
        })
        .await;
        let r = r.unwrap();
        assert_eq!(sent.len(), 2);
        let u = Uncertain("m".into());
        assert_eq!(outcomes(&r), [Done, Done, u.clone(), u, NotAttempted]);
        assert!(r.1.is_none());
    }

    #[tokio::test]
    async fn ok_batch_with_unanswered_op_stops() {
        let (r, sent) = run(4, 2, |_, c| {
            Ok(vec![
                (c[0].clone(), Done),
                (c[1].clone(), Uncertain("no answer for this image".into())),
            ])
        })
        .await;
        let r = r.unwrap();
        assert_eq!(sent.len(), 1);
        let u = Uncertain("no answer for this image".into());
        assert_eq!(outcomes(&r), [Done, u, NotAttempted, NotAttempted]);
        assert!(r.1.is_none());
    }

    #[tokio::test]
    async fn rejected_continues() {
        let (r, sent) = run(4, 2, |i, c| match i {
            0 => Err(DeleteError::Rejected("x".into())),
            _ => done(c),
        })
        .await;
        let r = r.unwrap();
        assert_eq!(sent, [2, 2]);
        let f = Failed("x".into());
        assert_eq!(outcomes(&r), [f.clone(), f, Done, Done]);
        assert!(!r.0.aborted_unsupported && r.1.is_none());
    }

    #[tokio::test]
    async fn unsupported_aborts() {
        let (r, sent) = run(3, 1, |_, _| Err(DeleteError::Unsupported)).await;
        let r = r.unwrap();
        assert_eq!(sent.len(), 1);
        let f = Failed(DELETION_DISABLED.into());
        assert_eq!(outcomes(&r), [f, NotAttempted, NotAttempted]);
        assert!(r.0.aborted_unsupported && r.1.is_none());
    }

    #[tokio::test]
    async fn every_op_once_in_order() {
        // Each answer kind at each chunk position; `run` checks the order.
        let answer = |kind: usize, c: &[DeleteOp]| match kind {
            0 => Err(DeleteError::NotSent(RegistryError::Network("n".into()))),
            1 => Err(DeleteError::Rejected("x".into())),
            2 => Err(DeleteError::Uncertain("m".into())),
            3 => Err(DeleteError::Unsupported),
            _ => Ok(vec![
                (c[0].clone(), Uncertain("m".into())),
                (c[1].clone(), Done),
            ]),
        };
        for at in 0..3 {
            for kind in 0..5 {
                let (r, _) = run(6, 2, |i, c| match i == at {
                    true => answer(kind, c),
                    false => done(c),
                })
                .await;
                match (at, kind) {
                    (0, 0) => assert_eq!(r, Err(RegistryError::Network("n".into()))),
                    _ => assert_eq!(r.unwrap().0.results.len(), 6),
                }
            }
        }
    }

    #[tokio::test]
    async fn ok_batch_short_answer_stops() {
        let (r, sent) = run(4, 2, |_, c| Ok(vec![(c[0].clone(), Done)])).await;
        let r = r.unwrap();
        assert_eq!(sent.len(), 1);
        let u = Uncertain("no answer for this op".into());
        assert_eq!(outcomes(&r), [Done, u, NotAttempted, NotAttempted]);
        assert!(r.1.is_none());
    }

    #[tokio::test]
    async fn ok_batch_reordered_answer_stops() {
        let (r, sent) = run(4, 2, |_, c| {
            Ok(vec![(c[1].clone(), Done), (c[0].clone(), Done)])
        })
        .await;
        let r = r.unwrap();
        assert_eq!(sent.len(), 1);
        let u = Uncertain("no answer for this op".into());
        assert_eq!(outcomes(&r), [u.clone(), u, NotAttempted, NotAttempted]);
        assert!(r.1.is_none());
    }
}

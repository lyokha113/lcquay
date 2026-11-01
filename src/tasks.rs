//! Runtime: runs `Effect`s as supervised tokio tasks and posts `Event`s (spec §8).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Instant;

use futures::{StreamExt, stream};
use time::OffsetDateTime;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::{AbortHandle, JoinError, JoinHandle};

use crate::app::messages::describe;
use crate::app::planner::{self, fallback_targets, may_be_index};
use crate::app::state::{CacheKey, CacheValue, Effect, Event, RegistryView, RequestId};
use crate::app::transfer::TransferOutcome;
use crate::app::write::{OpId, PushOutcome};
use crate::auth::docker_config::default_config_dir;
use crate::auth::{Credential, Destination, with_credential_timeout};
use crate::conntest::{self, Check, CheckStatus};
use crate::docker::login::docker_login;
use crate::docker::{Engine, PushStep, UNAVAILABLE, engine_auth};
use crate::domain::{
    ArtifactStatus, DeleteOp, DeletePlan, DeleteReport, DepSnapshot, Digest, ItemStep,
    ManifestDocument, ManifestKind, OpOutcome, Reference, RegistryError, RegistryId, RepoPath,
    RepoSummary, SnapshotError, TransferStep, image_ref,
};
use crate::export::{ExportError, ExportJob, expand_home, export, part_path};
use crate::providers::{Provider, Registry};
use crate::registry::FOCUSED;
use crate::registry::copy::{CHUNK, CopyError, CopyJob, copy_image};
use crate::reqlog::RequestLog;

/// At most one copy / export `Bytes` progress step per this interval.
const PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_millis(100);

/// The engines report about one `Bytes` step per network read; `send` gets one per
/// `PROGRESS_EVERY` (a blob's last one always goes) and every other step as is.
fn thinned(send: impl Fn(TransferStep) + Sync) -> impl Fn(TransferStep) + Sync {
    let last = Mutex::new(None::<Instant>);
    move |step: TransferStep| {
        if let ItemStep::Bytes { current, total } = step.step
            && current < total
        {
            let mut last = last.lock().unwrap_or_else(PoisonError::into_inner);
            if last.is_some_and(|t| t.elapsed() < PROGRESS_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        send(step)
    }
}

/// `Effect::Export` with its path expanded.
struct ExportArgs {
    repo: RepoPath,
    top: Digest,
    platform: Option<Digest>,
    tag: Option<String>,
    full_ref: String,
    path: std::path::PathBuf,
    overwrite: bool,
}

/// The slice of a registry the pure `app` layer sees.
pub fn view(r: &Registry) -> RegistryView {
    RegistryView {
        id: r.id.clone(),
        ref_host: r.cfg.ref_host.clone(),
        endpoint: r.cfg.endpoint.clone(),
        provider: r.cfg.provider.kind(),
        auth: r.cfg.auth.clone(),
        caps: r.provider.caps(),
        auth_failed: false,
        can_login: r.creds.can_login(),
        login: None,
        pinned: r
            .cfg
            .pinned
            .iter()
            .map(|p| RepoSummary {
                path: p.clone(),
                created_at: None,
            })
            .collect(),
    }
}

/// A connection test's step reporter, shared with its supervisor.
type Reporter = Arc<dyn Fn(usize, Check) + Send + Sync>;

pub trait Clipboard: Send {
    fn set(&mut self, text: String) -> Result<(), String>;
}

/// System clipboard, opened on first use.
#[derive(Default)]
pub struct ArboardClipboard(Option<arboard::Clipboard>);

impl Clipboard for ArboardClipboard {
    fn set(&mut self, text: String) -> Result<(), String> {
        let clip = match &mut self.0 {
            Some(c) => c,
            None => self
                .0
                .insert(arboard::Clipboard::new().map_err(|e| e.to_string())?),
        };
        clip.set_text(text).map_err(|e| e.to_string())
    }
}

struct Inner {
    registries: Vec<Registry>,
    local: Option<Arc<dyn Engine>>,
    clipboard: Mutex<Box<dyn Clipboard>>,
    tx: UnboundedSender<Event>,
    /// Written only by `SetFocused`; read by gate probes (short read, never across an await).
    focused: Arc<RwLock<HashSet<CacheKey>>>,
    /// Manifests fetched this session, so `Config` resolves without a second request.
    // ponytail: unbounded, like the cache it backs (spec §8: no size bound).
    manifests: Mutex<HashMap<Digest, ManifestDocument>>,
    /// The docker CLI run for `docker login`.
    docker_program: String,
    /// Running cancellable ops (push, pull, copy, export), so `Cancel` can abort them.
    ops: Mutex<HashMap<OpId, AbortHandle>>,
    /// Where Docker calls are recorded.
    log: RequestLog,
}

pub struct Runtime {
    inner: Arc<Inner>,
}

impl Runtime {
    pub fn new(
        registries: Vec<Registry>,
        local: Option<Arc<dyn Engine>>,
        clipboard: Box<dyn Clipboard>,
        tx: UnboundedSender<Event>,
    ) -> Self {
        Runtime {
            inner: Arc::new(Inner {
                registries,
                local,
                clipboard: Mutex::new(clipboard),
                tx,
                focused: Arc::default(),
                manifests: Mutex::default(),
                docker_program: "docker".into(),
                ops: Mutex::default(),
                log: RequestLog::default(),
            }),
        }
    }

    pub fn with_log(mut self, log: RequestLog) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("runtime not shared yet")
            .log = log;
        self
    }

    #[cfg(test)]
    fn with_docker_program(mut self, program: String) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("runtime not shared yet")
            .docker_program = program;
        self
    }

    /// Non-blocking; `Quit` is handled by `main`.
    pub fn run(&self, effect: Effect) {
        match effect {
            Effect::Fetch { key, req } => {
                let fut = self.inner.clone().fetch(key.clone());
                self.spawn_fetch(key, req, fut);
            }
            Effect::SetFocused(keys) => {
                *self
                    .inner
                    .focused
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = keys.into_iter().collect();
            }
            Effect::Copy(text) => {
                let inner = self.inner.clone();
                tokio::task::spawn_blocking(move || {
                    let r = inner
                        .clipboard
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .set(text);
                    let _ = inner.tx.send(Event::CopyResult(r));
                });
            }
            Effect::Login(id) => {
                let inner = self.inner.clone();
                let lid = id.clone();
                // No gate permit: `docker login` is not a registry HTTP request.
                let worker = tokio::spawn(async move { inner.login(&lid).await });
                let tx = self.inner.tx.clone();
                tokio::spawn(async move {
                    let result = worker.await.unwrap_or_else(|_| {
                        Err(RegistryError::Internal("login worker failed".into()))
                    });
                    if let Err(e) = &result {
                        tracing::warn!(registry = %id, error = %e, "login failed");
                    }
                    let _ = tx.send(Event::LoginDone {
                        registry: id,
                        result,
                    });
                });
            }
            Effect::Push {
                op,
                registry,
                repo,
                image_id,
                tag,
            } => self.spawn_push(op, registry, repo, image_id, tag),
            Effect::Cancel(op) => {
                let ops = self
                    .inner
                    .ops
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some(worker) = ops.get(&op) {
                    // Dropping the worker drops its stream; the supervisor reports it.
                    worker.abort();
                }
            }
            Effect::Pull {
                op,
                registry,
                image,
                platform,
            } => self.spawn_pull(op, registry, image, platform),
            Effect::CopyImage {
                op,
                src,
                src_repo,
                digest,
                dst,
                dst_repo,
                tag,
            } => self.spawn_copy(op, src, src_repo, digest, dst, dst_repo, tag),
            Effect::Export {
                op,
                registry,
                repo,
                top,
                platform,
                tag,
                full_ref,
                path,
                overwrite,
            } => {
                let job = ExportArgs {
                    repo,
                    top,
                    platform,
                    tag,
                    full_ref,
                    path: expand_home(&path),
                    overwrite,
                };
                self.spawn_export(op, registry, job);
            }
            Effect::DeleteSnapshot {
                op,
                registry,
                repo,
                marked,
            } => {
                let inner = self.inner.clone();
                let (r, p) = (registry.clone(), repo.clone());
                // No permit across the run: `OciClient` takes one per request.
                let worker = tokio::spawn(async move {
                    let reg = inner.registry(Some(&r)).map_err(SnapshotError::Inventory)?;
                    inner.snapshot(reg, &p, &marked).await
                });
                let tx = self.inner.tx.clone();
                tokio::spawn(async move {
                    let result = worker.await.unwrap_or_else(|_| {
                        Err(SnapshotError::Inventory(RegistryError::Internal(
                            "snapshot worker failed".into(),
                        )))
                    });
                    let outcome = match &result {
                        Ok(_) => "ok",
                        Err(SnapshotError::Inventory(_)) => "inventory failed",
                        Err(SnapshotError::UnreadableIndex) => "unreadable index",
                    };
                    tracing::info!(registry = %registry, repo = %repo, op = "snapshot", outcome, "delete snapshot");
                    let _ = tx.send(Event::DeleteSnapshot { op, result });
                });
            }
            Effect::Delete {
                op,
                registry,
                repo,
                plan,
                parents,
            } => self.spawn_delete(op, registry, repo, plan, parents),
            Effect::TestConnection { op, cfg } => {
                let log = self.inner.log.clone();
                // The throwaway registry is dropped when the test ends; running ones are untouched.
                self.spawn_conn_test(op, move |report| async move {
                    conntest::test(&cfg, &log, &default_config_dir(), &*report).await;
                });
            }
            // Handled by `main`.
            Effect::Quit
            | Effect::EditConfig
            | Effect::SaveTheme(_)
            | Effect::OpenRegistryForm(_)
            | Effect::SaveRegistry { .. }
            | Effect::RemoveRegistry(_) => {}
        }
    }

    /// Runs `fut` as a worker under the focus probe for `key`; a supervisor posts exactly one
    /// `Loaded` (a panic or abort becomes `Err`). Returns the worker's abort handle and the
    /// supervisor's join handle.
    pub(crate) fn spawn_fetch(
        &self,
        key: CacheKey,
        req: RequestId,
        fut: impl Future<Output = Result<CacheValue, RegistryError>> + Send + 'static,
    ) -> (AbortHandle, JoinHandle<()>) {
        let (focused, k) = (self.inner.focused.clone(), key.clone());
        let probe = Arc::new(move || {
            focused
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&k)
        });
        let worker = tokio::spawn(FOCUSED.scope(probe, fut));
        let abort = worker.abort_handle();
        let tx = self.inner.tx.clone();
        let supervisor = tokio::spawn(async move {
            let result = worker
                .await
                .unwrap_or_else(|e| Err(RegistryError::Internal(format!("worker failed: {e}"))));
            if let Err(e) = &result {
                tracing::warn!(
                    registry = key.registry().map(|r| r.0.as_str()).unwrap_or("local"),
                    what = %describe(&key),
                    error = %e,
                    "fetch failed"
                );
            }
            // Receiver gone = the UI loop ended; nothing to report to.
            let _ = tx.send(Event::Loaded { key, req, result });
        });
        (abort, supervisor)
    }

    /// Runs `body` (given the step reporter) as the worker of connection test `op`. If the worker
    /// panics or is aborted, its supervisor reports the first unreported step as
    /// `Failed("internal error")` and the rest `Skipped`, so the form never stays running.
    fn spawn_conn_test<Fut>(&self, op: OpId, body: impl FnOnce(Reporter) -> Fut)
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        let tx = self.inner.tx.clone();
        // The step after the last one reported.
        let next = Arc::new(AtomicUsize::new(1));
        let report: Reporter = {
            let (tx, next) = (tx.clone(), next.clone());
            Arc::new(move |step, check| {
                next.store(step + 1, Ordering::SeqCst);
                let _ = tx.send(Event::ConnCheck { op, step, check });
            })
        };
        let joined = self.spawn_op(op, body(report));
        tokio::spawn(async move {
            if let Err(e) = joined.await {
                tracing::warn!(op = op.0, error = %e, "connection test worker failed");
                let first = next.load(Ordering::SeqCst);
                for step in first..conntest::STEPS.len() {
                    let check = if step == first {
                        Check {
                            status: CheckStatus::Failed,
                            detail: "internal error".into(),
                            ms: 0,
                        }
                    } else {
                        Check::skipped()
                    };
                    let _ = tx.send(Event::ConnCheck { op, step, check });
                }
            }
        });
    }

    /// Spawns `fut` as the worker of `op`, registered for `Effect::Cancel` until it ends. The
    /// returned future awaits the worker and unregisters `op`; the caller's supervisor runs it.
    fn spawn_op<T: Send + 'static>(
        &self,
        op: OpId,
        fut: impl Future<Output = T> + Send + 'static,
    ) -> impl Future<Output = Result<T, JoinError>> + Send + 'static {
        let worker = tokio::spawn(fut);
        // Registered before the supervisor exists, so its removal always comes after.
        self.inner
            .ops
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(op, worker.abort_handle());
        let inner = self.inner.clone();
        async move {
            let joined = worker.await;
            inner
                .ops
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&op);
            joined
        }
    }

    /// Pull worker plus a supervisor that posts exactly one `PullDone` (abort → `Cancelled`,
    /// panic → `Failed`). No gate permit and no write lock: the Engine is not a registry request.
    fn spawn_pull(&self, op: OpId, registry: RegistryId, image: String, platform: Option<String>) {
        tracing::info!(registry = %registry, image = %image, "pull started");
        let inner = self.inner.clone();
        let (r, i) = (registry.clone(), image.clone());
        let worker = self.spawn_op(op, async move {
            inner.pull(op, &r, &i, platform.as_deref()).await
        });
        let tx = self.inner.tx.clone();
        tokio::spawn(async move {
            let outcome = match worker.await {
                Ok(Ok(())) => TransferOutcome::Done,
                Ok(Err(e)) => TransferOutcome::Failed(e),
                Err(e) if e.is_cancelled() => TransferOutcome::Cancelled,
                Err(_) => {
                    TransferOutcome::Failed(RegistryError::Internal("pull worker failed".into()))
                }
            };
            match &outcome {
                TransferOutcome::Failed(e) => {
                    tracing::warn!(registry = %registry, image = %image, error = %e, "pull failed")
                }
                o => {
                    tracing::info!(registry = %registry, image = %image, outcome = ?o, "pull finished")
                }
            }
            let _ = tx.send(Event::PullDone { op, outcome });
        });
    }

    /// Copy worker plus a supervisor that posts exactly one `CopyDone`. An abort is `Cancelled`
    /// until the engine dispatched its first manifest `PUT`, then `Uncertain` (the tag may have
    /// moved). Each side uses its own registry's client; requests are never retried.
    #[allow(clippy::too_many_arguments)]
    fn spawn_copy(
        &self,
        op: OpId,
        src: RegistryId,
        src_repo: RepoPath,
        digest: Digest,
        dst: RegistryId,
        dst_repo: RepoPath,
        tag: String,
    ) {
        tracing::info!(src = %src, src_repo = %src_repo, dst = %dst, dst_repo = %dst_repo, digest = %digest, "copy started");
        let target = match self.inner.registry(Some(&dst)) {
            Ok(r) => image_ref(&r.cfg.ref_host, &dst_repo, &Reference::Tag(tag.clone())),
            Err(_) => format!("{dst}/{dst_repo}:{tag}"),
        };
        let sent = Arc::new(AtomicBool::new(false));
        let inner = self.inner.clone();
        let (s, sr, d, dr, dg, f) = (
            src.clone(),
            src_repo.clone(),
            dst.clone(),
            dst_repo.clone(),
            digest.clone(),
            sent.clone(),
        );
        let worker = self.spawn_op(op, async move {
            inner.copy(op, &s, &sr, &d, &dr, &dg, &tag, &f).await
        });
        let tx = self.inner.tx.clone();
        tokio::spawn(async move {
            let after_put = format!("the manifest upload started — check {target}");
            let outcome = match worker.await {
                Ok(Ok(())) => TransferOutcome::Done,
                Ok(Err(CopyError::Failed(m))) => {
                    TransferOutcome::Failed(RegistryError::Provider(m))
                }
                Ok(Err(CopyError::Uncertain(m))) => TransferOutcome::Uncertain(m),
                Ok(Err(CopyError::DigestDiffers { got, want })) => {
                    TransferOutcome::DigestDiffers { got, want }
                }
                Err(e) if sent.load(Ordering::SeqCst) => {
                    let what = if e.is_cancelled() {
                        "cancelled"
                    } else {
                        "copy worker failed"
                    };
                    TransferOutcome::Uncertain(format!("{what} after {after_put}"))
                }
                Err(e) if e.is_cancelled() => TransferOutcome::Cancelled,
                Err(_) => {
                    TransferOutcome::Failed(RegistryError::Internal("copy worker failed".into()))
                }
            };
            let kind = match &outcome {
                TransferOutcome::Done => "done",
                TransferOutcome::Cancelled => "cancelled",
                TransferOutcome::Failed(_) => "failed",
                TransferOutcome::Uncertain(_) => "uncertain",
                TransferOutcome::DigestDiffers { .. } => "digest differs",
                TransferOutcome::Exists => "exists",
            };
            // Kinds only: error texts may carry request details.
            tracing::info!(src = %src, src_repo = %src_repo, dst = %dst, dst_repo = %dst_repo, digest = %digest, outcome = kind, "copy finished");
            let _ = tx.send(Event::CopyDone { op, outcome });
        });
    }

    /// Export worker plus a supervisor that posts exactly one `ExportDone` and removes
    /// `<path>.part` on every other outcome (abort and panic included). No write lock and no
    /// gate permit across the run: each request takes its own.
    fn spawn_export(&self, op: OpId, registry: RegistryId, job: ExportArgs) {
        tracing::info!(registry = %registry, repo = %job.repo, digest = %job.top, "export started");
        let part = part_path(&job.path);
        let inner = self.inner.clone();
        let r = registry.clone();
        let worker = self.spawn_op(op, async move { inner.export(op, &r, &job).await });
        let tx = self.inner.tx.clone();
        tokio::spawn(async move {
            let (outcome, bytes) = match worker.await {
                Ok(Ok(n)) => (TransferOutcome::Done, n),
                Ok(Err(ExportError::Exists)) => (TransferOutcome::Exists, 0),
                Ok(Err(ExportError::Failed(m))) => {
                    (TransferOutcome::Failed(RegistryError::Provider(m)), 0)
                }
                Err(e) if e.is_cancelled() => (TransferOutcome::Cancelled, 0),
                Err(_) => (
                    TransferOutcome::Failed(RegistryError::Internal("export worker failed".into())),
                    0,
                ),
            };
            if outcome != TransferOutcome::Done
                && let Err(e) = std::fs::remove_file(&part)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(error = %e, "could not remove the partial export");
            }
            let kind = match &outcome {
                TransferOutcome::Done => "done",
                TransferOutcome::Cancelled => "cancelled",
                TransferOutcome::Failed(_) => "failed",
                TransferOutcome::Exists => "exists",
                TransferOutcome::Uncertain(_) | TransferOutcome::DigestDiffers { .. } => "other",
            };
            // Kinds only: error texts may carry request details.
            tracing::info!(registry = %registry, outcome = kind, bytes, "export finished");
            let _ = tx.send(Event::ExportDone { op, outcome });
        });
    }

    /// Push worker plus a supervisor that posts exactly one `PushDone` (abort → `Cancelled`,
    /// panic → `Failed`). No retries and no gate permit: the Engine is not a registry request.
    fn spawn_push(
        &self,
        op: OpId,
        registry: RegistryId,
        repo: RepoPath,
        image_id: String,
        tag: String,
    ) {
        tracing::info!(registry = %registry, repo = %repo, tag = %tag, "push started");
        let inner = self.inner.clone();
        let (r, p, t) = (registry.clone(), repo.clone(), tag.clone());
        let worker = self.spawn_op(op, async move {
            match inner.push(op, &r, &p, &image_id, &t).await {
                Ok(digest) => PushOutcome::Pushed(digest),
                Err(e) => PushOutcome::Failed(e),
            }
        });
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let outcome = match worker.await {
                Ok(outcome) => outcome,
                Err(e) if e.is_cancelled() => PushOutcome::Cancelled,
                Err(_) => PushOutcome::Failed(RegistryError::Internal("push worker failed".into())),
            };
            let kind = match &outcome {
                PushOutcome::Pushed(_) => "pushed",
                PushOutcome::Cancelled => "cancelled",
                PushOutcome::Failed(e) => {
                    tracing::warn!(registry = %registry, repo = %repo, tag = %tag, error = %e, "push failed");
                    "failed"
                }
            };
            tracing::info!(registry = %registry, repo = %repo, tag = %tag, outcome = kind, "push finished");
            let _ = inner.tx.send(Event::PushDone { op, outcome });
        });
    }

    /// Delete worker plus a supervisor that posts exactly one `DeleteDone`. Answers reach the
    /// supervisor through `answered` as they arrive, so after a panic or abort every op
    /// without an answer is `Uncertain` (spec §7.4 execution 5).
    fn spawn_delete(
        &self,
        op: OpId,
        registry: RegistryId,
        repo: RepoPath,
        plan: DeletePlan,
        parents: BTreeMap<Digest, BTreeSet<Digest>>,
    ) {
        tracing::info!(registry = %registry, repo = %repo, ops = plan.ops().len(), "delete started");
        let started = std::time::Instant::now();
        let answered = Arc::new(Mutex::new(vec![]));
        let inner = self.inner.clone();
        let (r, p, pl, a) = (
            registry.clone(),
            repo.clone(),
            plan.clone(),
            answered.clone(),
        );
        // No gate permit across the run: providers take one per request.
        let worker = tokio::spawn(async move {
            match inner.registry(Some(&r)) {
                Ok(reg) => execute(reg.provider.as_ref(), &p, &pl, &parents, &a).await,
                Err(e) => (false, Some(e)),
            }
        });
        let tx = self.inner.tx.clone();
        tokio::spawn(async move {
            let joined = worker.await;
            let answered =
                std::mem::take(&mut *answered.lock().unwrap_or_else(PoisonError::into_inner));
            let (aborted_unsupported, stopped, results) = match joined {
                Ok((aborted, stopped)) => (
                    aborted,
                    stopped,
                    finish(&plan, answered, OpOutcome::NotAttempted),
                ),
                Err(_) => {
                    tracing::warn!(registry = %registry, repo = %repo, "delete worker failed");
                    let fill = OpOutcome::Uncertain("delete worker stopped".into());
                    let stopped = RegistryError::Internal("delete worker failed".into());
                    (false, Some(stopped), finish(&plan, answered, fill))
                }
            };
            let count = |f: fn(&OpOutcome) -> bool| results.iter().filter(|(_, o)| f(o)).count();
            for (op, out) in &results {
                if let OpOutcome::Failed(reason) = out {
                    let (kind, digest) = match op {
                        DeleteOp::RemoveTag { expected, .. } => ("remove tag", expected),
                        DeleteOp::DeleteManifest { digest } => ("delete manifest", digest),
                    };
                    tracing::warn!(registry = %registry, repo = %repo, op = kind, digest = %digest, reason = %reason, "delete op failed");
                }
            }
            tracing::info!(
                registry = %registry,
                repo = %repo,
                done = count(|o| matches!(o, OpOutcome::Done)),
                failed = count(|o| matches!(o, OpOutcome::Failed(_))),
                skipped = count(|o| matches!(o, OpOutcome::Skipped(_))),
                uncertain = count(|o| matches!(o, OpOutcome::Uncertain(_))),
                not_attempted = count(|o| matches!(o, OpOutcome::NotAttempted)),
                aborted_unsupported,
                // The kind only: error texts may carry request details.
                stopped = stopped.as_ref().map_or("none", RegistryError::kind),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "delete finished"
            );
            let report = DeleteReport {
                results,
                aborted_unsupported,
            };
            let _ = tx.send(Event::DeleteDone {
                op,
                report,
                stopped,
            });
        });
    }
}

/// Spec §7.4 execution 2–5: `RemoveTag` ops, then manifest layers in order, one provider
/// call each, never retried. Before each layer, a manifest with a parent in `parents` that
/// is not `Done` (failed, skipped, or unanswered) is skipped. Stops on a provider `Err`, a
/// stop reason, `aborted_unsupported`, or any `Uncertain`/`NotAttempted` answer. Every answer
/// is pushed into `answered` as it arrives. Returns `(aborted_unsupported, stopped)`.
pub(crate) async fn execute(
    provider: &dyn Provider,
    repo: &RepoPath,
    plan: &DeletePlan,
    parents: &BTreeMap<Digest, BTreeSet<Digest>>,
    answered: &Mutex<Vec<(DeleteOp, OpOutcome)>>,
) -> (bool, Option<RegistryError>) {
    let record = |op: DeleteOp, out: OpOutcome| {
        answered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((op, out));
    };
    let manifests = plan.layers.iter().map(|l| {
        l.iter()
            .map(|d| DeleteOp::DeleteManifest { digest: d.clone() })
            .collect()
    });
    let mut done = BTreeSet::new();
    for ops in std::iter::once(plan.remove_tags.clone()).chain(manifests) {
        let mut send: Vec<DeleteOp> = vec![];
        for op in ops {
            let blocked = match &op {
                DeleteOp::DeleteManifest { digest } => parents
                    .get(digest)
                    .is_some_and(|ps| ps.iter().any(|p| !done.contains(p))),
                DeleteOp::RemoveTag { .. } => false,
            };
            if blocked {
                record(op, OpOutcome::Skipped("parent not deleted".into()));
            } else {
                send.push(op);
            }
        }
        if send.is_empty() {
            continue;
        }
        let (report, stopped) = match provider.delete(repo, &send).await {
            Ok(r) => r,
            Err(e) => return (false, Some(e)),
        };
        // Matched by position like `run_batches`; a missing or mismatched answer is `Uncertain`.
        let mut stop =
            report.aborted_unsupported || stopped.is_some() || report.results.len() != send.len();
        for (j, op) in send.into_iter().enumerate() {
            let out = match report.results.get(j) {
                Some((o, out)) if *o == op => out.clone(),
                _ => OpOutcome::Uncertain("no answer for this op".into()),
            };
            stop |= matches!(out, OpOutcome::Uncertain(_) | OpOutcome::NotAttempted);
            if let (OpOutcome::Done, DeleteOp::DeleteManifest { digest }) = (&out, &op) {
                done.insert(digest.clone());
            }
            record(op, out);
        }
        if stop {
            return (report.aborted_unsupported, stopped);
        }
    }
    (false, None)
}

/// Every op of `plan.ops()`, in that order, with its answer; an unanswered op gets `fill`.
pub(crate) fn finish(
    plan: &DeletePlan,
    mut answered: Vec<(DeleteOp, OpOutcome)>,
    fill: OpOutcome,
) -> Vec<(DeleteOp, OpOutcome)> {
    // ponytail: linear search per op, O(n²); fine for one repo's delete set.
    plan.ops()
        .into_iter()
        .map(|op| {
            let out = match answered.iter().position(|(o, _)| *o == op) {
                Some(i) => answered.swap_remove(i).1,
                None => fill.clone(),
            };
            (op, out)
        })
        .collect()
}

impl Inner {
    fn registry(&self, id: Option<&RegistryId>) -> Result<&Registry, RegistryError> {
        self.registries
            .iter()
            .find(|r| Some(&r.id) == id)
            .ok_or_else(|| RegistryError::Internal("unknown registry".into()))
    }

    /// Awaits one Docker call `op` (e.g. `docker pull`) and records it under `registry`.
    async fn docker_call<T>(
        &self,
        registry: &str,
        op: &str,
        path: &str,
        call: impl Future<Output = Result<T, RegistryError>>,
    ) -> Result<T, RegistryError> {
        let t = Instant::now();
        let r = call.await;
        self.log
            .scoped(registry)
            .call(op, path, t, r.as_ref().map(|_| ()));
        r
    }

    async fn fetch(self: Arc<Self>, key: CacheKey) -> Result<CacheValue, RegistryError> {
        if key == CacheKey::LocalImages {
            return match &self.local {
                Some(l) => self
                    .docker_call("local", "docker images", "", l.list())
                    .await
                    .map(CacheValue::LocalImages),
                None => Err(RegistryError::Provider(UNAVAILABLE.into())),
            };
        }
        let reg = self.registry(key.registry())?;
        let tx = self.tx.clone();
        let pkey = key.clone();
        let progress = move |pages| {
            let _ = tx.send(Event::Progress {
                key: pkey.clone(),
                pages,
            });
        };
        match &key {
            CacheKey::Repos(_) => reg
                .provider
                .repositories(&progress)
                .await
                .map(CacheValue::Repos),
            CacheKey::Inventory(_, repo) => reg
                .provider
                .artifacts(repo, &progress)
                .await
                .map(CacheValue::Inventory),
            CacheKey::Manifest(_, repo, d) => {
                self.manifest(reg, repo, d).await.map(CacheValue::Manifest)
            }
            CacheKey::Config(_, repo, d) => {
                let cached = self
                    .manifests
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(d)
                    .cloned();
                let m = match cached {
                    Some(m) => m,
                    None => self.manifest(reg, repo, d).await?,
                };
                match &m.kind {
                    ManifestKind::Image { config, .. } => reg
                        .oci
                        .image_config(repo, config)
                        .await
                        .map(CacheValue::Config),
                    ManifestKind::Index { .. } => Ok(CacheValue::Config(None)),
                }
            }
            CacheKey::Referrers(_, repo, d) => {
                reg.oci.referrers(repo, d).await.map(CacheValue::Referrers)
            }
            CacheKey::TagPolicy(_, repo, tag) => reg
                .provider
                .tag_policy(repo, tag)
                .await
                .map(CacheValue::TagPolicy),
            CacheKey::RegistryDetails(_) => reg
                .provider
                .registry_details()
                .await
                .map(CacheValue::Details),
            CacheKey::RepoDetails(_, repo) => reg
                .provider
                .repo_details(repo)
                .await
                .map(CacheValue::Details),
            CacheKey::LocalImages => unreachable!("handled above"),
        }
    }

    /// `docker login` with the registry's `Login` credential; `Ok` carries its expiry.
    async fn login(&self, id: &RegistryId) -> Result<Option<OffsetDateTime>, RegistryError> {
        let reg = self.registry(Some(id))?;
        let issued = with_credential_timeout(&*reg.creds, Destination::Login).await?;
        let Credential::Basic { username, password } = issued.credential else {
            return Err(RegistryError::Internal(
                "login credential is not Basic".into(),
            ));
        };
        let host = &reg.cfg.ref_host;
        let login = docker_login(&self.docker_program, host, &username, &password);
        self.docker_call(&id.0, "docker login", host, login).await?;
        Ok(issued.expires_at)
    }

    /// Engine credential → `docker tag` → push stream; stops (dropping the stream) at the
    /// first error entry. `Ok` carries the last digest the stream reported.
    async fn push(
        &self,
        op: OpId,
        id: &RegistryId,
        repo: &RepoPath,
        image_id: &str,
        tag: &str,
    ) -> Result<Option<Digest>, RegistryError> {
        let engine = (self.local)
            .as_ref()
            .ok_or_else(|| RegistryError::Provider(UNAVAILABLE.into()))?;
        let reg = self.registry(Some(id))?;
        let issued = with_credential_timeout(&*reg.creds, Destination::Engine).await?;
        let auth = engine_auth(&issued.credential, &reg.cfg.ref_host)?;
        let target = format!("{}/{repo}", reg.cfg.ref_host);
        let image = format!("{target}:{tag}");
        let tagged = engine.tag(image_id, &target, tag);
        self.docker_call(&id.0, "docker tag", &image, tagged)
            .await?;
        let pushed = async {
            let mut steps = engine.push(&target, tag, auth);
            let mut digest = None;
            while let Some(step) = steps.next().await {
                let step = step?;
                if let PushStep::Digest(d) = &step {
                    digest = Some(d.clone());
                }
                let _ = self.tx.send(Event::PushProgress { op, step });
            }
            Ok(digest)
        };
        self.docker_call(&id.0, "docker push", &image, pushed).await
    }

    /// Engine credential → pull stream; stops at the first error entry.
    async fn pull(
        &self,
        op: OpId,
        id: &RegistryId,
        image: &str,
        platform: Option<&str>,
    ) -> Result<(), RegistryError> {
        let engine = (self.local)
            .as_ref()
            .ok_or_else(|| RegistryError::Provider(UNAVAILABLE.into()))?;
        let reg = self.registry(Some(id))?;
        let issued = with_credential_timeout(&*reg.creds, Destination::Engine).await?;
        let auth = engine_auth(&issued.credential, &reg.cfg.ref_host)?;
        let pulled = async {
            let mut steps = engine.pull(image, platform, auth);
            while let Some(step) = steps.next().await {
                let _ = self.tx.send(Event::PullProgress { op, step: step? });
            }
            Ok(())
        };
        self.docker_call(&id.0, "docker pull", image, pulled).await
    }

    /// Runs the copy engine, posting its steps as `CopyProgress`. The engine reports about one
    /// `Bytes` step per network read; those are thinned to one per `PROGRESS_EVERY` (a blob's
    /// last one always goes), every other step is posted as is.
    #[allow(clippy::too_many_arguments)]
    async fn copy(
        &self,
        op: OpId,
        src: &RegistryId,
        src_repo: &RepoPath,
        dst: &RegistryId,
        dst_repo: &RepoPath,
        digest: &Digest,
        tag: &str,
        manifest_sent: &AtomicBool,
    ) -> Result<(), CopyError> {
        let (s, d) = (self.registry(Some(src))?, self.registry(Some(dst))?);
        let job = CopyJob {
            src: &s.oci,
            src_repo,
            dst: &d.oci,
            dst_repo,
            digest,
            tag,
            same_registry: src == dst,
            chunk: CHUNK,
            manifest_sent,
        };
        let progress = thinned(|step| {
            let _ = self.tx.send(Event::CopyProgress { op, step });
        });
        copy_image(&job, &progress).await
    }

    /// Runs the export engine, posting its (thinned) steps as `ExportProgress`.
    async fn export(&self, op: OpId, id: &RegistryId, a: &ExportArgs) -> Result<u64, ExportError> {
        let reg = self.registry(Some(id))?;
        let job = ExportJob {
            oci: &reg.oci,
            repo: &a.repo,
            top: &a.top,
            platform: a.platform.as_ref(),
            tag: a.tag.as_deref(),
            full_ref: &a.full_ref,
            path: &a.path,
            overwrite: a.overwrite,
        };
        let progress = thinned(|step| {
            let _ = self.tx.send(Event::ExportProgress { op, step });
        });
        export(&job, &progress).await
    }

    /// Spec §7.4 dependency snapshot, always fresh: neither the cache nor `manifests` is read
    /// or written. One GET per distinct manifest; archived rows are never fetched.
    async fn snapshot(
        &self,
        reg: &Registry,
        repo: &RepoPath,
        marked: &BTreeSet<Digest>,
    ) -> Result<DepSnapshot, SnapshotError> {
        let inventory = reg
            .provider
            .artifacts(repo, &|_| {})
            .await
            .map_err(SnapshotError::Inventory)?;
        let status = |d: &Digest| inventory.iter().find(|a| &a.digest == d).map(|a| &a.status);
        let indexes: Vec<_> = inventory.iter().filter(|a| may_be_index(a)).collect();
        // An archived index counts as unreadable: stop before any request.
        if indexes.iter().any(|a| a.status != ArtifactStatus::Active) {
            return Err(SnapshotError::UnreadableIndex);
        }
        let (mut tried, mut manifests) = (BTreeSet::new(), BTreeMap::new());
        let indexes = indexes.into_iter().map(|a| a.digest.clone());
        if !read_manifests(reg, repo, indexes, &mut tried, &mut manifests).await {
            return Err(SnapshotError::UnreadableIndex);
        }
        // Children absent from the inventory (`TaggedOnly`) are fetched; archived ones are not.
        let fallback = fallback_targets(&inventory, &manifests)
            .into_iter()
            .filter(|d| status(d).is_none_or(|s| *s == ArtifactStatus::Active));
        read_manifests(reg, repo, fallback, &mut tried, &mut manifests).await;
        let active: Vec<Digest> = marked
            .iter()
            .filter(|d| status(d) == Some(&ArtifactStatus::Active))
            .cloned()
            .collect();
        read_manifests(reg, repo, active.clone(), &mut tried, &mut manifests).await;
        let referrers = stream::iter(active)
            .map(|d| async move {
                let found = reg.oci.referrers(repo, &d).await.ok().flatten();
                (d, found.map(|r| r.into_iter().map(|x| x.digest).collect()))
            })
            .buffer_unordered(8)
            .collect()
            .await;
        planner::assemble(inventory, &manifests, referrers, marked)
    }

    async fn manifest(
        &self,
        reg: &Registry,
        repo: &RepoPath,
        d: &Digest,
    ) -> Result<ManifestDocument, RegistryError> {
        let m = reg
            .oci
            .manifest(repo, &Reference::Digest(d.clone()))
            .await?;
        self.manifests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(d.clone(), m.clone());
        Ok(m)
    }
}

/// GETs every digest of `want` not tried yet, 8 at a time, and keeps the readable ones;
/// `false` if any failed.
async fn read_manifests(
    reg: &Registry,
    repo: &RepoPath,
    want: impl IntoIterator<Item = Digest>,
    tried: &mut BTreeSet<Digest>,
    manifests: &mut BTreeMap<Digest, ManifestDocument>,
) -> bool {
    let todo: Vec<Digest> = want
        .into_iter()
        .filter(|d| tried.insert(d.clone()))
        .collect();
    let got: Vec<_> = stream::iter(todo)
        .map(|d| async move {
            let m = reg.oci.manifest(repo, &Reference::Digest(d.clone())).await;
            (d, m)
        })
        .buffer_unordered(8)
        .collect()
        .await;
    let mut ok = true;
    for (d, m) in got {
        match m {
            Ok(m) => {
                manifests.insert(d, m);
            }
            Err(_) => ok = false,
        }
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use futures::stream::{self, BoxStream, StreamExt};
    use serde_json::json;
    use time::macros::datetime;
    use tokio::sync::mpsc;
    use tokio::time::sleep;
    use url::Url;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::app::state::{CacheKey, CacheValue, Effect, Event, RequestId};
    use crate::app::transfer::TransferOutcome;
    use crate::app::write::{OpId, PushOutcome};
    use crate::auth::{AnonymousSource, CredentialSource, Issued, Secret};
    use crate::config::{AuthCfg, ProviderCfg, RegistryCfg};
    use crate::docker::{Engine, EngineAuth, LocalImages, PushStep, UNAVAILABLE};
    use crate::domain::{
        ArtifactStatus, ArtifactSummary, Caps, DeleteOp, DeletePlan, DeleteReport, DepSnapshot,
        Digest, Inventory, ItemStep, LocalImage, OpOutcome, RegistryError, RegistryId, RepoPath,
        RepoSummary, SnapshotError, TagPolicy, TransferStep,
    };
    use crate::providers::{DELETION_DISABLED, DeleteResult, Provider, Registry};
    use crate::registry::{Gate, OciClient};

    /// Every page takes one gate permit for `delay`, then reports progress.
    struct Fake {
        gate: Arc<Gate>,
        pages: usize,
        delay: Duration,
    }

    impl Fake {
        async fn paged(&self, pages: usize, progress: &(dyn Fn(usize) + Sync)) {
            for i in 1..=pages {
                let permit = self.gate.acquire().await;
                sleep(self.delay).await;
                drop(permit);
                progress(i);
            }
        }
    }

    #[async_trait]
    impl Provider for Fake {
        fn caps(&self) -> Caps {
            Caps {
                inventory: Inventory::Complete,
                pushed_at: false,
                last_pulled: false,
                delete_tag: false,
                delete_manifest: false,
            }
        }
        async fn repositories(
            &self,
            progress: &(dyn Fn(usize) + Sync),
        ) -> Result<Vec<RepoSummary>, RegistryError> {
            self.paged(1, progress).await;
            Ok(vec![])
        }
        async fn artifacts(
            &self,
            _repo: &RepoPath,
            progress: &(dyn Fn(usize) + Sync),
        ) -> Result<Vec<ArtifactSummary>, RegistryError> {
            self.paged(self.pages, progress).await;
            Ok(vec![])
        }
        async fn tag_policy(&self, _: &RepoPath, _: &str) -> Result<TagPolicy, RegistryError> {
            Ok(TagPolicy::Immutable)
        }
    }

    /// `tag`, `push` and `pull` are scripted per test; the default tags fine and pushes and
    /// pulls nothing.
    #[derive(Default)]
    struct FakeLocal {
        tag: Option<RegistryError>,
        tag_panics: bool,
        push: Vec<Result<PushStep, RegistryError>>,
        pull: Vec<Result<TransferStep, RegistryError>>,
        /// After the scripted steps the stream pends forever instead of ending.
        hang: bool,
        /// `tag <image> <target> <tag>` / `push <target> <tag> <user>@<server>` /
        /// `pull <image> [<platform>] <user>@<server>` per call.
        calls: Mutex<Vec<String>>,
        /// Set when the push or pull stream is dropped.
        dropped: Arc<AtomicBool>,
    }

    impl FakeLocal {
        /// `script`, then pending forever when `hang`; sets `dropped` when dropped.
        fn stream<T: Clone + Send + 'static>(&self, script: &[T]) -> BoxStream<'static, T> {
            let tail = if self.hang {
                stream::pending().boxed()
            } else {
                stream::empty().boxed()
            };
            let guard = OnDrop(self.dropped.clone());
            stream::iter(script.to_vec())
                .chain(tail)
                .map(move |s| {
                    let _ = &guard;
                    s
                })
                .boxed()
        }
    }

    struct OnDrop(Arc<AtomicBool>);

    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl LocalImages for FakeLocal {
        async fn list(&self) -> Result<Vec<LocalImage>, RegistryError> {
            Ok(vec![])
        }
    }

    #[async_trait]
    impl Engine for FakeLocal {
        async fn tag(&self, image: &str, target: &str, tag: &str) -> Result<(), RegistryError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("tag {image} {target} {tag}"));
            assert!(!self.tag_panics, "tag boom");
            self.tag.clone().map_or(Ok(()), Err)
        }
        fn push(
            &self,
            target: &str,
            tag: &str,
            auth: Option<EngineAuth>,
        ) -> BoxStream<'static, Result<PushStep, RegistryError>> {
            let who = auth.map_or("none".into(), |a| format!("{}@{}", a.username, a.server));
            self.calls
                .lock()
                .unwrap()
                .push(format!("push {target} {tag} {who}"));
            self.stream(&self.push)
        }
        fn pull(
            &self,
            image: &str,
            platform: Option<&str>,
            auth: Option<EngineAuth>,
        ) -> BoxStream<'static, Result<TransferStep, RegistryError>> {
            let who = auth.map_or("none".into(), |a| format!("{}@{}", a.username, a.server));
            let platform = platform.map(|p| format!(" {p}")).unwrap_or_default();
            self.calls
                .lock()
                .unwrap()
                .push(format!("pull {image}{platform} {who}"));
            self.stream(&self.pull)
        }
    }

    struct FakeClip(Result<(), String>);

    impl Clipboard for FakeClip {
        fn set(&mut self, _text: String) -> Result<(), String> {
            self.0.clone()
        }
    }

    fn rid() -> RegistryId {
        RegistryId("r".into())
    }

    fn repo(s: &str) -> RepoPath {
        RepoPath(s.into())
    }

    fn inv(s: &str) -> CacheKey {
        CacheKey::Inventory(rid(), repo(s))
    }

    fn registry(endpoint: &str, gate: Arc<Gate>, pages: usize, delay: Duration) -> Registry {
        let endpoint = Url::parse(endpoint).unwrap();
        Registry {
            id: rid(),
            cfg: RegistryCfg {
                id: rid(),
                provider: ProviderCfg::Oci,
                endpoint: endpoint.clone(),
                ref_host: "h".into(),
                repositories: vec![],
                pinned: vec![],
                trusted_realms: vec![],
                auth: AuthCfg::Anonymous,
            },
            provider: Arc::new(Fake {
                gate: gate.clone(),
                pages,
                delay,
            }),
            oci: Arc::new(OciClient::new(
                endpoint,
                vec![],
                Arc::new(AnonymousSource),
                gate.clone(),
            )),
            gate,
            creds: Arc::new(AnonymousSource),
        }
    }

    /// Runtime over one fake registry (unused OCI endpoint) and no Docker.
    fn runtime(
        gate: Arc<Gate>,
        pages: usize,
        delay: Duration,
    ) -> (Runtime, mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let reg = registry("http://127.0.0.1:9", gate, pages, delay);
        let rt = Runtime::new(vec![reg], None, Box::new(FakeClip(Ok(()))), tx);
        (rt, rx)
    }

    fn fetch(key: CacheKey, req: u64) -> Effect {
        Effect::Fetch {
            key,
            req: RequestId(req),
        }
    }

    /// Next `Loaded`, skipping progress.
    async fn next_loaded(
        rx: &mut mpsc::UnboundedReceiver<Event>,
    ) -> (CacheKey, RequestId, Result<CacheValue, RegistryError>) {
        loop {
            if let Event::Loaded { key, req, result } = rx.recv().await.expect("channel closed") {
                return (key, req, result);
            }
        }
    }

    fn worker_failed(r: &Result<CacheValue, RegistryError>) -> bool {
        matches!(r, Err(RegistryError::Internal(m)) if m.starts_with("worker failed"))
    }

    #[tokio::test]
    async fn tag_policy_fetch_through_runtime() {
        let (rt, mut rx) = runtime(Arc::new(Gate::new(8)), 1, Duration::ZERO);
        let key = CacheKey::TagPolicy(rid(), repo("app"), "1.0".into());
        rt.run(fetch(key.clone(), 1));
        assert_eq!(
            next_loaded(&mut rx).await,
            (
                key,
                RequestId(1),
                Ok(CacheValue::TagPolicy(TagPolicy::Immutable))
            )
        );
    }

    #[tokio::test]
    async fn details_fetch_through_runtime() {
        let (rt, mut rx) = runtime(Arc::new(Gate::new(8)), 1, Duration::ZERO);
        for (req, key) in [
            (1, CacheKey::RegistryDetails(rid())),
            (2, CacheKey::RepoDetails(rid(), repo("app"))),
        ] {
            rt.run(fetch(key.clone(), req));
            assert_eq!(
                next_loaded(&mut rx).await,
                (
                    key,
                    RequestId(req),
                    Ok(CacheValue::Details(Default::default()))
                )
            );
        }
    }

    #[tokio::test]
    async fn exactly_one_loaded_per_fetch() {
        let (rt, mut rx) = runtime(Arc::new(Gate::new(8)), 2, Duration::ZERO);
        rt.run(fetch(CacheKey::Repos(rid()), 1));
        rt.run(fetch(inv("a"), 2));
        rt.run(fetch(CacheKey::Repos(RegistryId("nope".into())), 3));
        rt.run(fetch(CacheKey::LocalImages, 4));
        rt.run(Effect::Quit);
        drop(rt);
        let mut loaded: HashMap<u64, Result<CacheValue, RegistryError>> = HashMap::new();
        while let Some(ev) = rx.recv().await {
            if let Event::Loaded { req, result, .. } = ev {
                assert!(loaded.insert(req.0, result).is_none(), "second Loaded");
            }
        }
        assert_eq!(loaded.len(), 4);
        assert_eq!(loaded[&1], Ok(CacheValue::Repos(vec![])));
        assert_eq!(loaded[&2], Ok(CacheValue::Inventory(vec![])));
        assert_eq!(
            loaded[&3],
            Err(RegistryError::Internal("unknown registry".into()))
        );
        assert!(loaded[&4].is_err());
    }

    #[tokio::test]
    async fn panicking_worker_reports_err() {
        let (rt, mut rx) = runtime(Arc::new(Gate::new(8)), 1, Duration::ZERO);
        rt.spawn_fetch(inv("a"), RequestId(7), async { panic!("boom") });
        let (key, req, result) = next_loaded(&mut rx).await;
        assert_eq!((key, req), (inv("a"), RequestId(7)));
        assert!(worker_failed(&result), "{result:?}");
    }

    #[tokio::test]
    async fn panicking_conn_test_fails_the_unreported_steps() {
        let (rt, mut rx) = runtime(Arc::new(Gate::new(8)), 1, Duration::ZERO);
        rt.spawn_conn_test(OpId(5), |report| async move {
            report(1, Check::skipped());
            panic!("boom");
        });
        let mut got = vec![];
        while got.len() < 4 {
            match rx.recv().await.unwrap() {
                Event::ConnCheck { op, step, check } => {
                    assert_eq!(op, OpId(5));
                    got.push((step, check.status, check.detail));
                }
                e => panic!("{e:?}"),
            }
        }
        let s = |step| (step, CheckStatus::Skipped, String::new());
        assert_eq!(
            got,
            [
                s(1),
                (2, CheckStatus::Failed, "internal error".into()),
                s(3),
                s(4)
            ]
        );
        assert!(rt.inner.ops.lock().unwrap().is_empty(), "op unregistered");
    }

    #[tokio::test]
    async fn aborted_worker_reports_err() {
        let (rt, mut rx) = runtime(Arc::new(Gate::new(8)), 1, Duration::ZERO);
        let (worker, _) = rt.spawn_fetch(inv("a"), RequestId(7), std::future::pending());
        worker.abort();
        let (key, req, result) = next_loaded(&mut rx).await;
        assert_eq!((key, req), (inv("a"), RequestId(7)));
        assert!(worker_failed(&result), "{result:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn focused_fetch_overtakes_background() {
        let gate = Arc::new(Gate::new(1));
        let (rt, mut rx) = runtime(gate.clone(), 1, Duration::ZERO);
        let held = gate.acquire().await;
        for (i, r) in ["b1", "b2", "b3"].into_iter().enumerate() {
            rt.run(fetch(inv(r), i as u64));
        }
        sleep(Duration::from_millis(1)).await;
        rt.run(Effect::SetFocused(vec![inv("f")]));
        rt.run(fetch(inv("f"), 9));
        sleep(Duration::from_millis(1)).await;
        drop(held);
        let mut got = vec![];
        for _ in 0..4 {
            got.push(next_loaded(&mut rx).await.0);
        }
        assert_eq!(got, vec![inv("f"), inv("b1"), inv("b2"), inv("b3")]);
    }

    #[tokio::test]
    async fn permits_released_between_pages() {
        let gate = Arc::new(Gate::new(1));
        let (rt, mut rx) = runtime(gate, 3, Duration::from_millis(20));
        rt.run(fetch(inv("a"), 1));
        // after page 1 of the inventory, queue a one-page fetch on the same gate
        loop {
            if let Some(Event::Progress { pages: 1, .. }) = rx.recv().await {
                break;
            }
        }
        rt.run(fetch(CacheKey::Repos(rid()), 2));
        let (_, first, _) = next_loaded(&mut rx).await;
        let (_, second, result) = next_loaded(&mut rx).await;
        assert_eq!((first, second), (RequestId(2), RequestId(1)));
        assert_eq!(result, Ok(CacheValue::Inventory(vec![])));
    }

    #[tokio::test(start_paused = true)]
    async fn long_healthy_inventory_completes() {
        let (rt, mut rx) = runtime(Arc::new(Gate::new(8)), 10, Duration::from_secs(20));
        rt.run(fetch(inv("a"), 1));
        let mut progress = vec![];
        let result = loop {
            match rx.recv().await.unwrap() {
                Event::Progress { key, pages } => {
                    assert_eq!(key, inv("a"));
                    progress.push(pages);
                }
                Event::Loaded { result, .. } => break result,
                Event::CopyResult(_)
                | Event::LoginDone { .. }
                | Event::PushProgress { .. }
                | Event::PushDone { .. }
                | Event::PullProgress { .. }
                | Event::PullDone { .. }
                | Event::CopyProgress { .. }
                | Event::CopyDone { .. }
                | Event::ExportProgress { .. }
                | Event::ExportDone { .. }
                | Event::DeleteSnapshot { .. }
                | Event::DeleteDone { .. }
                | Event::ConnCheck { .. } => unreachable!(),
            }
        };
        assert_eq!(result, Ok(CacheValue::Inventory(vec![])));
        assert_eq!(progress, (1..=10).collect::<Vec<_>>());
    }

    /// Anonymous `oci`, so the default docker config dir is never read.
    #[tokio::test]
    async fn test_connection_reports_every_step() {
        use crate::conntest::CheckStatus;
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/_catalog"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"repositories":["a"]}"#))
            .mount(&s)
            .await;
        let (rt, mut rx) = runtime(Arc::new(Gate::new(8)), 1, Duration::ZERO);
        let mut cfg = registry(&s.uri(), Arc::new(Gate::new(8)), 1, Duration::ZERO).cfg;
        cfg.id = RegistryId("new".into());
        rt.run(Effect::TestConnection { op: OpId(3), cfg });
        let mut steps = vec![];
        while steps.len() < 4 {
            match rx.recv().await.unwrap() {
                Event::ConnCheck { op, step, check } => {
                    assert_eq!(op, OpId(3));
                    assert_eq!(check.status, CheckStatus::Ok, "{check:?}");
                    steps.push(step);
                }
                e => panic!("{e:?}"),
            }
        }
        assert_eq!(steps, [1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn docker_unavailable_only_affects_local() {
        let (rt, mut rx) = runtime(Arc::new(Gate::new(8)), 1, Duration::ZERO);
        rt.run(fetch(CacheKey::LocalImages, 1));
        rt.run(fetch(CacheKey::Repos(rid()), 2));
        let mut got = HashMap::new();
        for _ in 0..2 {
            let (_, req, result) = next_loaded(&mut rx).await;
            got.insert(req.0, result);
        }
        assert_eq!(got[&1], Err(RegistryError::Provider(UNAVAILABLE.into())));
        assert_eq!(got[&2], Ok(CacheValue::Repos(vec![])));

        let (tx, mut rx) = mpsc::unbounded_channel();
        let rt = Runtime::new(
            vec![],
            Some(Arc::new(FakeLocal::default())),
            Box::new(FakeClip(Ok(()))),
            tx,
        );
        rt.run(fetch(CacheKey::LocalImages, 3));
        assert_eq!(
            next_loaded(&mut rx).await.2,
            Ok(CacheValue::LocalImages(vec![]))
        );
    }

    #[tokio::test]
    async fn receiver_dropped_is_silent() {
        let (rt, rx) = runtime(Arc::new(Gate::new(8)), 3, Duration::ZERO);
        drop(rx);
        rt.run(fetch(inv("a"), 1));
        rt.run(Effect::Copy("x".into()));
        let (_, supervisor) = rt.spawn_fetch(inv("b"), RequestId(2), async {
            Ok(CacheValue::Repos(vec![]))
        });
        supervisor.await.expect("supervisor must not panic");
        let (_, supervisor) = rt.spawn_fetch(inv("c"), RequestId(3), async { panic!("boom") });
        supervisor.await.expect("supervisor must not panic");
    }

    #[tokio::test]
    async fn clipboard_error_reported() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let rt = Runtime::new(
            vec![],
            None,
            Box::new(FakeClip(Err("no display".into()))),
            tx,
        );
        rt.run(Effect::Copy("x".into()));
        assert!(matches!(
            rx.recv().await,
            Some(Event::CopyResult(Err(e))) if e == "no display"
        ));
    }

    fn dg(c: char) -> String {
        format!("sha256:{}", c.to_string().repeat(64))
    }

    fn json_resp(v: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_string(v.to_string())
    }

    #[tokio::test]
    async fn config_resolved_through_manifest() {
        let s = MockServer::start().await;
        let (img, idx) = (dg('a'), dg('b'));
        Mock::given(path(format!("/v2/x/manifests/{img}")))
            .respond_with(json_resp(json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": dg('c'), "size": 2},
                "layers": []
            })))
            .expect(1) // fetched once, reused for the config
            .mount(&s)
            .await;
        Mock::given(path(format!("/v2/x/manifests/{idx}")))
            .respond_with(json_resp(json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.index.v1+json",
                "manifests": []
            })))
            .mount(&s)
            .await;
        Mock::given(path(format!("/v2/x/blobs/{}", dg('c'))))
            .respond_with(json_resp(json!({"config": {"User": "app"}})))
            .mount(&s)
            .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let gate = Arc::new(Gate::new(8));
        let reg = registry(&s.uri(), gate, 1, Duration::ZERO);
        let rt = Runtime::new(vec![reg], None, Box::new(FakeClip(Ok(()))), tx);
        let d = |s: &str| Digest::parse(s).unwrap();

        rt.run(fetch(CacheKey::Manifest(rid(), repo("x"), d(&img)), 1));
        let (_, _, m) = next_loaded(&mut rx).await;
        assert!(matches!(m, Ok(CacheValue::Manifest(_))), "{m:?}");

        rt.run(fetch(CacheKey::Config(rid(), repo("x"), d(&img)), 2));
        let (_, _, c) = next_loaded(&mut rx).await;
        let Ok(CacheValue::Config(Some(cfg))) = c else {
            panic!("{c:?}")
        };
        assert_eq!(cfg.user.as_deref(), Some("app"));

        rt.run(fetch(CacheKey::Config(rid(), repo("x"), d(&idx)), 3));
        assert_eq!(next_loaded(&mut rx).await.2, Ok(CacheValue::Config(None)));
    }

    // ---- login ----

    enum Creds {
        Basic(Option<OffsetDateTime>),
        Anonymous,
        Bearer,
        AwsError,
        Panic,
        Hang,
    }

    /// `Creds` that must be asked for this destination.
    struct For(Destination, Creds);

    #[async_trait]
    impl CredentialSource for For {
        async fn credential(&self, dest: Destination) -> Result<Issued, RegistryError> {
            assert_eq!(dest, self.0);
            self.1.issue().await
        }
        fn can_login(&self) -> bool {
            true
        }
    }

    impl Creds {
        async fn issue(&self) -> Result<Issued, RegistryError> {
            let credential = match self {
                Creds::Basic(expires_at) => {
                    return Ok(Issued {
                        credential: Credential::Basic {
                            username: "AWS".into(),
                            password: Secret("pw".into()),
                        },
                        expires_at: *expires_at,
                    });
                }
                Creds::Anonymous => Credential::Anonymous,
                Creds::Bearer => Credential::Bearer(Secret("tok".into())),
                Creds::AwsError => return Err(RegistryError::AwsCredentials("expired".into())),
                Creds::Panic => panic!("boom"),
                Creds::Hang => std::future::pending().await,
            };
            Ok(Issued {
                credential,
                expires_at: None,
            })
        }
    }

    /// Runs `Effect::Login` on a registry with `creds` and a fake docker that creates
    /// `<dir>/ran` and exits 0; returns every event posted and whether docker ran.
    async fn login_events(creds: Creds) -> (Vec<Event>, bool) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (marker, docker) = (dir.path().join("ran"), dir.path().join("docker"));
        let body = format!("#!/bin/sh\ncat >/dev/null\ntouch '{}'\n", marker.display());
        std::fs::write(&docker, body).unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut reg = registry(
            "http://127.0.0.1:9",
            Arc::new(Gate::new(8)),
            1,
            Duration::ZERO,
        );
        reg.creds = Arc::new(For(Destination::Login, creds));
        let rt = Runtime::new(vec![reg], None, Box::new(FakeClip(Ok(()))), tx)
            .with_docker_program(docker.to_str().unwrap().into());
        rt.run(Effect::Login(rid()));
        drop(rt);
        let mut events = vec![];
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        (events, marker.exists())
    }

    /// The single `LoginDone` result for `rid()`.
    fn only_login_done(events: Vec<Event>) -> Result<Option<OffsetDateTime>, RegistryError> {
        match <[Event; 1]>::try_from(events) {
            Ok([Event::LoginDone { registry, result }]) if registry == rid() => result,
            other => panic!("want exactly one LoginDone: {other:?}"),
        }
    }

    #[tokio::test]
    async fn login_effect_posts_login_done_with_expiry() {
        let t = datetime!(2026-10-29 23:12 UTC);
        let (events, ran) = login_events(Creds::Basic(Some(t))).await;
        assert_eq!(only_login_done(events), Ok(Some(t)));
        assert!(ran);
    }

    #[tokio::test]
    async fn login_bearer_or_anonymous_credential_is_internal_error() {
        let (events, ran) = login_events(Creds::Anonymous).await;
        let r = only_login_done(events);
        assert!(matches!(r, Err(RegistryError::Internal(_))), "{r:?}");
        assert!(!ran, "docker must not run");
    }

    #[tokio::test]
    async fn login_credential_error_propagates() {
        let (events, ran) = login_events(Creds::AwsError).await;
        assert_eq!(
            only_login_done(events),
            Err(RegistryError::AwsCredentials("expired".into()))
        );
        assert!(!ran);
    }

    #[tokio::test]
    async fn login_worker_panic_reports_err() {
        let (events, ran) = login_events(Creds::Panic).await;
        let r = only_login_done(events);
        assert!(matches!(r, Err(RegistryError::Internal(_))), "{r:?}");
        assert!(!ran);
    }

    #[tokio::test(start_paused = true)]
    async fn login_credential_timeout() {
        let start = tokio::time::Instant::now();
        let (events, ran) = login_events(Creds::Hang).await;
        assert!(matches!(
            only_login_done(events),
            Err(RegistryError::Timeout(_))
        ));
        assert!(start.elapsed() >= Duration::from_secs(30));
        assert!(!ran);
    }

    // ---- push ----

    /// Runtime over one registry (`ref_host` "h") with Engine `creds` and `local` as Docker.
    fn push_rt(
        local: Option<Arc<FakeLocal>>,
        creds: Creds,
    ) -> (Runtime, mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut reg = registry(
            "http://127.0.0.1:9",
            Arc::new(Gate::new(8)),
            1,
            Duration::ZERO,
        );
        reg.creds = Arc::new(For(Destination::Engine, creds));
        let local = local.map(|l| l as Arc<dyn Engine>);
        let rt = Runtime::new(vec![reg], local, Box::new(FakeClip(Ok(()))), tx);
        (rt, rx)
    }

    fn push(op: u64) -> Effect {
        Effect::Push {
            op: OpId(op),
            registry: rid(),
            repo: repo("app"),
            image_id: "img".into(),
            tag: "1.0.1".into(),
        }
    }

    /// Drops `rt` and returns every event still to come (the channel closes once all
    /// workers and supervisors are done).
    async fn drain(rt: Runtime, rx: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
        drop(rt);
        let mut events = vec![];
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        events
    }

    /// Asserts exactly one `PushDone`, for `op`, as the last event; returns its outcome.
    fn only_done(events: &[Event], op: u64) -> PushOutcome {
        let dones: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::PushDone { .. }))
            .collect();
        match (dones.as_slice(), events.last()) {
            ([Event::PushDone { op: o, outcome }], Some(Event::PushDone { .. }))
                if *o == OpId(op) =>
            {
                outcome.clone()
            }
            _ => panic!("want exactly one trailing PushDone for op {op}: {events:?}"),
        }
    }

    fn calls(l: &FakeLocal) -> Vec<String> {
        l.calls.lock().unwrap().clone()
    }

    fn d(c: char) -> Digest {
        Digest::parse(&dg(c)).unwrap()
    }

    #[tokio::test]
    async fn push_runs_tag_then_push_with_engine_auth() {
        let steps = vec![
            PushStep::Status("Preparing".into()),
            PushStep::Bytes {
                current: 1,
                total: 2,
            },
            PushStep::LayerDone,
            PushStep::Digest(d('a')),
        ];
        let l = Arc::new(FakeLocal {
            push: steps.iter().cloned().map(Ok).collect(),
            ..Default::default()
        });
        let (rt, mut rx) = push_rt(Some(l.clone()), Creds::Basic(None));
        rt.run(push(1));
        let events = drain(rt, &mut rx).await;
        assert_eq!(
            calls(&l),
            vec!["tag img h/app 1.0.1", "push h/app 1.0.1 AWS@h"]
        );
        let progress: Vec<_> = steps
            .into_iter()
            .map(|step| Event::PushProgress { op: OpId(1), step })
            .collect();
        assert_eq!(events[..progress.len()], progress[..]);
        assert_eq!(events.len(), progress.len() + 1);
        assert_eq!(only_done(&events, 1), PushOutcome::Pushed(Some(d('a'))));
    }

    // ---- pull ----

    fn pull(op: u64) -> Effect {
        Effect::Pull {
            op: OpId(op),
            registry: rid(),
            image: "h/app:1.0.1".into(),
            platform: None,
        }
    }

    fn layer(id: &str, step: ItemStep) -> TransferStep {
        TransferStep {
            id: id.into(),
            step,
        }
    }

    #[tokio::test]
    async fn pull_posts_progress_then_done() {
        let steps = vec![
            layer("aaa", ItemStep::Waiting { total: None }),
            layer(
                "aaa",
                ItemStep::Bytes {
                    current: 5,
                    total: 10,
                },
            ),
            layer("aaa", ItemStep::Done),
        ];
        let l = Arc::new(FakeLocal {
            pull: steps.iter().cloned().map(Ok).collect(),
            ..Default::default()
        });
        let (rt, mut rx) = push_rt(Some(l.clone()), Creds::Basic(None));
        rt.run(pull(3));
        let events = drain(rt, &mut rx).await;
        assert_eq!(calls(&l), vec!["pull h/app:1.0.1 AWS@h"]);
        let mut want: Vec<_> = steps
            .into_iter()
            .map(|step| Event::PullProgress { op: OpId(3), step })
            .collect();
        want.push(Event::PullDone {
            op: OpId(3),
            outcome: TransferOutcome::Done,
        });
        assert_eq!(events, want);
    }

    #[tokio::test]
    async fn pull_passes_platform() {
        let l = Arc::new(FakeLocal::default());
        let (rt, mut rx) = push_rt(Some(l.clone()), Creds::Basic(None));
        rt.run(Effect::Pull {
            op: OpId(1),
            registry: rid(),
            image: "h/app:1.0".into(),
            platform: Some("linux/arm64".into()),
        });
        drain(rt, &mut rx).await;
        assert_eq!(calls(&l), vec!["pull h/app:1.0 linux/arm64 AWS@h"]);
    }

    #[tokio::test]
    async fn pull_cancel_posts_one_done() {
        let l = Arc::new(FakeLocal {
            pull: vec![Ok(layer("aaa", ItemStep::Waiting { total: None }))],
            hang: true,
            ..Default::default()
        });
        let (rt, mut rx) = push_rt(Some(l.clone()), Creds::Basic(None));
        rt.run(pull(1));
        assert!(matches!(
            rx.recv().await,
            Some(Event::PullProgress { op: OpId(1), .. })
        ));
        rt.run(Effect::Cancel(OpId(1)));
        let events = drain(rt, &mut rx).await;
        assert_eq!(
            events,
            vec![Event::PullDone {
                op: OpId(1),
                outcome: TransferOutcome::Cancelled
            }]
        );
        assert!(l.dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancel_aborts_only_its_op() {
        let l = Arc::new(FakeLocal {
            hang: true,
            ..Default::default()
        });
        let (rt, mut rx) = push_rt(Some(l), Creds::Basic(None));
        rt.run(push(1));
        rt.run(pull(2));
        rt.run(Effect::Cancel(OpId(2)));
        assert_eq!(
            rx.recv().await,
            Some(Event::PullDone {
                op: OpId(2),
                outcome: TransferOutcome::Cancelled
            })
        );
        rt.run(Effect::Cancel(OpId(1)));
        let events = drain(rt, &mut rx).await;
        assert_eq!(only_done(&events, 1), PushOutcome::Cancelled);
    }

    #[tokio::test]
    async fn pull_error_entry_fails() {
        let e = RegistryError::Provider("docker pull failed: manifest unknown".into());
        let l = Arc::new(FakeLocal {
            pull: vec![Err(e.clone()), Ok(layer("aaa", ItemStep::Done))],
            ..Default::default()
        });
        let (rt, mut rx) = push_rt(Some(l), Creds::Basic(None));
        rt.run(pull(1));
        let events = drain(rt, &mut rx).await;
        assert_eq!(
            events,
            vec![Event::PullDone {
                op: OpId(1),
                outcome: TransferOutcome::Failed(e)
            }]
        );
    }

    #[tokio::test]
    async fn pull_refuses_bearer_and_missing_engine() {
        let l = Arc::new(FakeLocal::default());
        let (rt, mut rx) = push_rt(Some(l.clone()), Creds::Bearer);
        rt.run(pull(1));
        let events = drain(rt, &mut rx).await;
        assert!(
            matches!(
                events.as_slice(),
                [Event::PullDone {
                    outcome: TransferOutcome::Failed(RegistryError::Internal(_)),
                    ..
                }]
            ),
            "{events:?}"
        );
        assert!(calls(&l).is_empty(), "bearer never reaches the engine");
        let (rt, mut rx) = push_rt(None, Creds::Basic(None));
        rt.run(pull(1));
        let events = drain(rt, &mut rx).await;
        assert_eq!(
            events,
            vec![Event::PullDone {
                op: OpId(1),
                outcome: TransferOutcome::Failed(RegistryError::Provider(UNAVAILABLE.into()))
            }]
        );
    }

    #[tokio::test]
    async fn stream_error_entry_fails_push() {
        let denied =
            RegistryError::Provider("denied: requested access to the resource is denied".into());
        let l = Arc::new(FakeLocal {
            push: vec![
                Ok(PushStep::Status("Preparing".into())),
                Err(denied.clone()),
                Ok(PushStep::Status("after the error".into())),
                Ok(PushStep::Digest(d('a'))),
            ],
            ..Default::default()
        });
        let (rt, mut rx) = push_rt(Some(l.clone()), Creds::Basic(None));
        rt.run(push(1));
        let events = drain(rt, &mut rx).await;
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(only_done(&events, 1), PushOutcome::Failed(denied));
        assert!(l.dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancel_posts_one_done() {
        let l = Arc::new(FakeLocal {
            push: vec![Ok(PushStep::Status("Preparing".into()))],
            hang: true,
            ..Default::default()
        });
        let (rt, mut rx) = push_rt(Some(l.clone()), Creds::Basic(None));
        rt.run(push(1));
        assert!(matches!(
            rx.recv().await,
            Some(Event::PushProgress { op: OpId(1), .. })
        ));
        assert!(!l.dropped.load(Ordering::SeqCst));
        rt.run(Effect::Cancel(OpId(1)));
        let events = drain(rt, &mut rx).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(only_done(&events, 1), PushOutcome::Cancelled);
        assert!(l.dropped.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_completion_race_one_done() {
        for wait_for_last_step in [false, true] {
            let l = Arc::new(FakeLocal {
                push: vec![Ok(PushStep::Digest(d('a')))],
                ..Default::default()
            });
            let (rt, mut rx) = push_rt(Some(l), Creds::Basic(None));
            rt.run(push(1));
            let mut events = vec![];
            if wait_for_last_step {
                events.push(rx.recv().await.unwrap());
            }
            rt.run(Effect::Cancel(OpId(1)));
            events.extend(drain(rt, &mut rx).await);
            let outcome = only_done(&events, 1);
            assert!(
                matches!(
                    outcome,
                    PushOutcome::Pushed(Some(_)) | PushOutcome::Cancelled
                ),
                "{outcome:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn credential_timeout_fails() {
        let start = tokio::time::Instant::now();
        let l = Arc::new(FakeLocal::default());
        let (rt, mut rx) = push_rt(Some(l.clone()), Creds::Hang);
        rt.run(push(1));
        let events = drain(rt, &mut rx).await;
        assert!(matches!(
            only_done(&events, 1),
            PushOutcome::Failed(RegistryError::Timeout(_))
        ));
        assert_eq!(events.len(), 1);
        assert!(start.elapsed() >= Duration::from_secs(30));
        assert!(calls(&l).is_empty(), "no tag");
    }

    #[tokio::test]
    async fn no_engine_fails_with_unavailable() {
        let (rt, mut rx) = push_rt(None, Creds::Basic(None));
        rt.run(push(1));
        let events = drain(rt, &mut rx).await;
        assert_eq!(
            only_done(&events, 1),
            PushOutcome::Failed(RegistryError::Provider(UNAVAILABLE.into()))
        );
    }

    #[tokio::test]
    async fn bearer_credential_fails_without_tagging() {
        let l = Arc::new(FakeLocal::default());
        let (rt, mut rx) = push_rt(Some(l.clone()), Creds::Bearer);
        rt.run(push(1));
        let events = drain(rt, &mut rx).await;
        assert!(matches!(
            only_done(&events, 1),
            PushOutcome::Failed(RegistryError::Internal(_))
        ));
        assert!(calls(&l).is_empty(), "no tag");
    }

    #[tokio::test]
    async fn push_worker_panic_reports_failed() {
        let l = Arc::new(FakeLocal {
            tag_panics: true,
            ..Default::default()
        });
        let (rt, mut rx) = push_rt(Some(l), Creds::Basic(None));
        rt.run(push(1));
        let events = drain(rt, &mut rx).await;
        assert_eq!(events.len(), 1);
        assert!(matches!(
            only_done(&events, 1),
            PushOutcome::Failed(RegistryError::Internal(_))
        ));
    }

    #[tokio::test]
    async fn cancel_unknown_op_is_noop() {
        let l = Arc::new(FakeLocal {
            push: vec![Ok(PushStep::Digest(d('a')))],
            ..Default::default()
        });
        let (rt, mut rx) = push_rt(Some(l), Creds::Basic(None));
        rt.run(Effect::Cancel(OpId(9)));
        rt.run(push(1));
        rt.run(Effect::Cancel(OpId(2)));
        let events = drain(rt, &mut rx).await;
        assert_eq!(only_done(&events, 1), PushOutcome::Pushed(Some(d('a'))));
    }

    // ---- copy ----

    const COPY_TAG: &str = "v2";

    fn sha(b: &[u8]) -> Digest {
        let mut c = crate::registry::DigestCheck::new();
        c.update(b);
        c.finish()
    }

    /// An image manifest (config `{}` and one layer) served from repo `x`; returns its digest
    /// and its layer's digest.
    async fn serve_copy_source(s: &MockServer) -> (Digest, Digest) {
        let (config, layer) = (b"{}".as_slice(), b"layer-1".as_slice());
        let bytes = json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": sha(config).to_string(), "size": config.len()},
            "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                        "digest": sha(layer).to_string(), "size": layer.len()}],
        })
        .to_string();
        let digest = sha(bytes.as_bytes());
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(s)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/v2/x/manifests/{digest}")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                bytes.into_bytes(),
                "application/vnd.oci.image.manifest.v1+json",
            ))
            .mount(s)
            .await;
        Mock::given(method("HEAD"))
            .and(path(format!("/v2/{}/manifests/{COPY_TAG}", "x")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("docker-content-digest", digest.to_string()),
            )
            .mount(s)
            .await;
        (digest, sha(layer))
    }

    /// Runtime over the registry `r` (ref host `h`) served by `s`.
    fn copy_rt(s: &MockServer) -> (Runtime, mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let reg = registry(&s.uri(), Arc::new(Gate::new(8)), 1, Duration::ZERO);
        (
            Runtime::new(vec![reg], None, Box::new(FakeClip(Ok(()))), tx),
            rx,
        )
    }

    /// Op 1: `r/x@digest` to `r/<dst_repo>:v2`.
    fn copy(digest: &Digest, dst_repo: &str) -> Effect {
        Effect::CopyImage {
            op: OpId(1),
            src: rid(),
            src_repo: repo("x"),
            digest: digest.clone(),
            dst: rid(),
            dst_repo: repo(dst_repo),
            tag: COPY_TAG.into(),
        }
    }

    /// Waits (up to 5 s) until `s` has received a `m` request on `p`.
    async fn received(s: &MockServer, m: &str, p: &str) {
        for _ in 0..500 {
            let got = s.received_requests().await.unwrap_or_default();
            if got
                .iter()
                .any(|r| r.method.as_str() == m && r.url.path() == p)
            {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("no {m} {p}");
    }

    /// Asserts only `CopyProgress` events and then exactly one trailing `CopyDone` for op 1.
    fn only_copy_done(events: &[Event]) -> TransferOutcome {
        match events.split_last() {
            Some((
                Event::CopyDone {
                    op: OpId(1),
                    outcome,
                },
                rest,
            )) if rest
                .iter()
                .all(|e| matches!(e, Event::CopyProgress { op: OpId(1), .. })) =>
            {
                outcome.clone()
            }
            _ => panic!("want progress, then exactly one CopyDone for op 1: {events:?}"),
        }
    }

    #[tokio::test]
    async fn copy_effect_runs_engine_and_posts_one_done() {
        let s = MockServer::start().await;
        let (digest, _) = serve_copy_source(&s).await;
        Mock::given(method("PUT"))
            .and(path(format!("/v2/x/manifests/{COPY_TAG}")))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&s)
            .await;
        let (rt, mut rx) = copy_rt(&s);
        rt.run(copy(&digest, "x"));
        let mut events = vec![];
        while !matches!(events.last(), Some(Event::CopyDone { .. })) {
            events.push(rx.recv().await.expect("channel closed"));
        }
        assert!(rt.inner.ops.lock().unwrap().is_empty(), "op unregistered");
        events.extend(drain(rt, &mut rx).await);
        assert_eq!(only_copy_done(&events), TransferOutcome::Done);
    }

    #[tokio::test]
    async fn copy_cancel_posts_cancelled() {
        let s = MockServer::start().await;
        let (digest, layer) = serve_copy_source(&s).await;
        Mock::given(method("HEAD"))
            .and(path_regex("^/v2/y/blobs/"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/y/blobs/uploads/"))
            .respond_with(
                ResponseTemplate::new(202)
                    .insert_header("location", "/v2/y/blobs/uploads/u1?_state=x"),
            )
            .mount(&s)
            .await;
        // Every source blob read hangs: the copy is stuck mid-upload.
        Mock::given(method("GET"))
            .and(path_regex("^/v2/x/blobs/"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&s)
            .await;
        let (rt, mut rx) = copy_rt(&s);
        rt.run(copy(&digest, "y"));
        let blobs = [sha(b"{}"), layer].map(|d| format!("/v2/x/blobs/{d}"));
        received(&s, "GET", &blobs[0]).await;
        rt.run(Effect::Cancel(OpId(1)));
        let events = drain(rt, &mut rx).await;
        assert_eq!(only_copy_done(&events), TransferOutcome::Cancelled);
        let puts = s.received_requests().await.unwrap();
        assert!(!puts.iter().any(|r| r.method.as_str() == "PUT"), "{puts:?}");
    }

    #[tokio::test]
    async fn copy_cancel_after_manifest_put_is_uncertain() {
        let s = MockServer::start().await;
        let (digest, _) = serve_copy_source(&s).await;
        Mock::given(method("PUT"))
            .and(path(format!("/v2/x/manifests/{COPY_TAG}")))
            .respond_with(ResponseTemplate::new(201).set_delay(Duration::from_secs(30)))
            .mount(&s)
            .await;
        let (rt, mut rx) = copy_rt(&s);
        rt.run(copy(&digest, "x"));
        received(&s, "PUT", &format!("/v2/x/manifests/{COPY_TAG}")).await;
        rt.run(Effect::Cancel(OpId(1)));
        let events = drain(rt, &mut rx).await;
        assert_eq!(
            events,
            [Event::CopyDone {
                op: OpId(1),
                outcome: TransferOutcome::Uncertain(
                    "cancelled after the manifest upload started — check h/x:v2".into()
                ),
            }]
        );
    }

    // ---- export ----

    /// Op 1: `r/x@digest` to `path`.
    fn export(digest: &Digest, path: &std::path::Path, overwrite: bool) -> Effect {
        Effect::Export {
            op: OpId(1),
            registry: rid(),
            repo: repo("x"),
            top: digest.clone(),
            platform: None,
            tag: Some("1.0".into()),
            full_ref: "h/x:1.0".into(),
            path: path.to_str().unwrap().into(),
            overwrite,
        }
    }

    /// Asserts only `ExportProgress` events and then exactly one trailing `ExportDone` for op 1.
    fn only_export_done(events: &[Event]) -> TransferOutcome {
        match events.split_last() {
            Some((
                Event::ExportDone {
                    op: OpId(1),
                    outcome,
                },
                rest,
            )) if rest
                .iter()
                .all(|e| matches!(e, Event::ExportProgress { op: OpId(1), .. })) =>
            {
                outcome.clone()
            }
            _ => panic!("want progress, then exactly one ExportDone for op 1: {events:?}"),
        }
    }

    #[tokio::test]
    async fn export_effect_posts_one_done() {
        let s = MockServer::start().await;
        let (digest, layer) = serve_copy_source(&s).await;
        for (d, body) in [(sha(b"{}"), b"{}".as_slice()), (layer, b"layer-1")] {
            Mock::given(method("GET"))
                .and(path(format!("/v2/x/blobs/{d}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
                .mount(&s)
                .await;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("x.tar");
        std::fs::write(&out, b"old").unwrap();

        let (rt, mut rx) = copy_rt(&s);
        rt.run(export(&digest, &out, false));
        let events = drain(rt, &mut rx).await;
        assert_eq!(
            events,
            [Event::ExportDone {
                op: OpId(1),
                outcome: TransferOutcome::Exists
            }]
        );
        assert_eq!(std::fs::read(&out).unwrap(), b"old");

        let (rt, mut rx) = copy_rt(&s);
        rt.run(export(&digest, &out, true));
        let mut events = vec![];
        while !matches!(events.last(), Some(Event::ExportDone { .. })) {
            events.push(rx.recv().await.expect("channel closed"));
        }
        assert!(rt.inner.ops.lock().unwrap().is_empty(), "op unregistered");
        events.extend(drain(rt, &mut rx).await);
        assert_eq!(only_export_done(&events), TransferOutcome::Done);
        assert!(std::fs::read(&out).unwrap().starts_with(b"oci-layout"));
        assert!(!crate::export::part_path(&out).exists());
    }

    #[tokio::test]
    async fn export_cancel_removes_part() {
        let s = MockServer::start().await;
        let (digest, layer) = serve_copy_source(&s).await;
        Mock::given(method("GET"))
            .and(path(format!("/v2/x/blobs/{}", sha(b"{}"))))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"{}".as_slice()))
            .mount(&s)
            .await;
        // The layer read hangs: the export is stuck mid-blob.
        let layer_path = format!("/v2/x/blobs/{layer}");
        Mock::given(method("GET"))
            .and(path(layer_path.as_str()))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&s)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("x.tar");
        let part = crate::export::part_path(&out);
        std::fs::write(&out, b"old").unwrap();

        let (rt, mut rx) = copy_rt(&s);
        rt.run(export(&digest, &out, true));
        received(&s, "GET", &layer_path).await;
        assert!(part.exists(), "the archive is being written");
        rt.run(Effect::Cancel(OpId(1)));
        let events = drain(rt, &mut rx).await;
        assert_eq!(only_export_done(&events), TransferOutcome::Cancelled);
        assert!(!part.exists());
        assert_eq!(std::fs::read(&out).unwrap(), b"old");
    }

    // ---- delete snapshot ----

    const INDEX: &str = "application/vnd.oci.image.index.v1+json";
    const IMAGE: &str = "application/vnd.oci.image.manifest.v1+json";

    /// One scripted `delete` answer: outcomes zipped with the ops given, `aborted_unsupported`,
    /// stop reason. `None` panics.
    type Answer = Option<Result<(Vec<OpOutcome>, bool, Option<RegistryError>), RegistryError>>;

    /// `artifacts` answers the scripted inventory (`None` panics); `delete` pops the next
    /// scripted answer and records the ops it was given. `wrong_first` replaces the op of
    /// every answer's first result.
    #[derive(Default)]
    struct FakeDelete {
        inventory: Option<Result<Vec<ArtifactSummary>, RegistryError>>,
        deletes: Mutex<VecDeque<Answer>>,
        calls: Mutex<Vec<Vec<DeleteOp>>>,
        wrong_first: Option<DeleteOp>,
    }

    #[async_trait]
    impl Provider for FakeDelete {
        fn caps(&self) -> Caps {
            Caps {
                inventory: Inventory::Complete,
                pushed_at: false,
                last_pulled: false,
                delete_tag: false,
                delete_manifest: true,
            }
        }
        async fn repositories(
            &self,
            _: &(dyn Fn(usize) + Sync),
        ) -> Result<Vec<RepoSummary>, RegistryError> {
            Ok(vec![])
        }
        async fn artifacts(
            &self,
            _: &RepoPath,
            _: &(dyn Fn(usize) + Sync),
        ) -> Result<Vec<ArtifactSummary>, RegistryError> {
            self.inventory.clone().expect("artifacts boom")
        }
        async fn delete(
            &self,
            _: &RepoPath,
            ops: &[DeleteOp],
        ) -> Result<DeleteResult, RegistryError> {
            self.calls.lock().unwrap().push(ops.to_vec());
            let answer = self.deletes.lock().unwrap().pop_front();
            let (outcomes, aborted_unsupported, stopped) =
                answer.expect("unscripted call").expect("delete boom")?;
            let mut results: Vec<_> = ops.iter().cloned().zip(outcomes).collect();
            if let (Some(op), Some(first)) = (&self.wrong_first, results.first_mut()) {
                first.0 = op.clone();
            }
            Ok((
                DeleteReport {
                    results,
                    aborted_unsupported,
                },
                stopped,
            ))
        }
    }

    fn row(c: char, tags: &[&str], media: &str) -> ArtifactSummary {
        ArtifactSummary {
            digest: d(c),
            media_type: Some(media.into()),
            ..crate::app::update::tests::art('0', tags)
        }
    }

    fn index_doc(children: &[char]) -> ResponseTemplate {
        let m: Vec<_> = children
            .iter()
            .map(|c| json!({"mediaType": IMAGE, "digest": dg(*c), "size": 1}))
            .collect();
        json_resp(json!({"schemaVersion": 2, "mediaType": INDEX, "manifests": m}))
    }

    fn image_doc(subject: Option<char>) -> ResponseTemplate {
        let mut v = json!({
            "schemaVersion": 2,
            "mediaType": IMAGE,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": dg('e'), "size": 2},
            "layers": []
        });
        if let Some(s) = subject {
            v["subject"] = json!({"mediaType": IMAGE, "digest": dg(s), "size": 1});
        }
        json_resp(v)
    }

    async fn serve(s: &MockServer, what: &str, c: char, resp: ResponseTemplate, times: u64) {
        Mock::given(path(format!("/v2/x/{what}/{}", dg(c))))
            .respond_with(resp)
            .expect(times)
            .mount(s)
            .await;
    }

    /// Runs `runs` `DeleteSnapshot` effects (op 1..) for repo `x` and returns every event,
    /// each checked to be a `DeleteSnapshot`, one per op.
    async fn snapshots(
        s: &MockServer,
        inventory: Option<Result<Vec<ArtifactSummary>, RegistryError>>,
        marked: &[char],
        runs: u64,
    ) -> Vec<Result<DepSnapshot, SnapshotError>> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut reg = registry(&s.uri(), Arc::new(Gate::new(8)), 1, Duration::ZERO);
        reg.provider = Arc::new(FakeDelete {
            inventory,
            ..Default::default()
        });
        let rt = Runtime::new(vec![reg], None, Box::new(FakeClip(Ok(()))), tx);
        for op in 1..=runs {
            rt.run(Effect::DeleteSnapshot {
                op: OpId(op),
                registry: rid(),
                repo: repo("x"),
                marked: marked.iter().map(|c| d(*c)).collect(),
            });
        }
        let mut got = BTreeMap::new();
        for e in drain(rt, &mut rx).await {
            let Event::DeleteSnapshot { op, result } = e else {
                panic!("unexpected {e:?}")
            };
            assert!(
                got.insert(op.0, result).is_none(),
                "second event for {op:?}"
            );
        }
        assert_eq!(
            got.keys().copied().collect::<Vec<_>>(),
            (1..=runs).collect::<Vec<_>>()
        );
        got.into_values().collect()
    }

    async fn requested(s: &MockServer) -> Vec<String> {
        let reqs = s.received_requests().await.unwrap_or_default();
        reqs.iter().map(|r| r.url.path().to_owned()).collect()
    }

    #[tokio::test]
    async fn snapshot_inventory_failure_stops() {
        let s = MockServer::start().await;
        let got = snapshots(&s, Some(Err(RegistryError::Network("x".into()))), &['a'], 1).await;
        assert!(
            matches!(
                &got[..],
                [Err(SnapshotError::Inventory(RegistryError::Network(_)))]
            ),
            "{got:?}"
        );
        assert!(requested(&s).await.is_empty());
    }

    #[tokio::test]
    async fn snapshot_unreadable_index_stops() {
        let s = MockServer::start().await;
        serve(&s, "manifests", '1', ResponseTemplate::new(500), 1).await;
        let inv = vec![row('1', &["v1"], INDEX), row('a', &["1.0"], IMAGE)];
        let got = snapshots(&s, Some(Ok(inv)), &['a'], 1).await;
        assert_eq!(got, vec![Err(SnapshotError::UnreadableIndex)]);
        let paths = requested(&s).await;
        assert!(
            !paths.iter().any(|p| p.contains("/referrers/")),
            "{paths:?}"
        );
    }

    #[tokio::test]
    async fn snapshot_archived_index_not_fetched() {
        let s = MockServer::start().await;
        let archived = ArtifactSummary {
            status: ArtifactStatus::Archived,
            ..row('1', &["v1"], INDEX)
        };
        let inv = vec![archived, row('a', &["1.0"], IMAGE)];
        let got = snapshots(&s, Some(Ok(inv)), &['a'], 1).await;
        assert_eq!(got, vec![Err(SnapshotError::UnreadableIndex)]);
        let paths = requested(&s).await;
        assert!(!paths.iter().any(|p| p.contains(&dg('1'))), "{paths:?}");
    }

    #[tokio::test]
    async fn snapshot_one_get_per_distinct_manifest() {
        // fallback index f ('1', tag sha256-<s>) lists r ('2', subject s = '5');
        // ordinary index i ('3') lists r and c ('4'); r is marked.
        let s = MockServer::start().await;
        serve(&s, "manifests", '1', index_doc(&['2']), 1).await;
        serve(&s, "manifests", '3', index_doc(&['2', '4']), 1).await;
        serve(&s, "manifests", '2', image_doc(Some('5')), 1).await;
        serve(&s, "manifests", '4', image_doc(None), 0).await;
        serve(&s, "referrers", '2', json_resp(json!({"manifests": []})), 1).await;
        let fallback = format!("sha256-{}", "5".repeat(64));
        let inv = vec![
            row('1', &[&fallback], INDEX),
            row('3', &["v1"], INDEX),
            row('5', &["1.0"], IMAGE),
            row('2', &[], IMAGE),
            row('4', &[], IMAGE),
        ];
        let got = snapshots(&s, Some(Ok(inv)), &['2'], 1).await;
        let [Ok(snap)] = &got[..] else {
            panic!("{got:?}")
        };
        assert_eq!(snap.subjects.get(&d('2')), Some(&d('5')));
        assert_eq!(snap.fallback_indexes.get(&fallback), Some(&d('5')));
        assert_eq!(snap.parents[&d('2')], BTreeSet::from([d('3')]));
        assert_eq!(snap.referrers[&d('2')], Some(vec![]));
        assert!(snap.unreadable.is_empty());
    }

    #[tokio::test]
    async fn snapshot_refetches_referrers_ignoring_cache() {
        let s = MockServer::start().await;
        serve(&s, "manifests", 'a', image_doc(None), 2).await;
        serve(&s, "referrers", 'a', json_resp(json!({"manifests": []})), 2).await;
        let inv = vec![row('a', &["1.0"], IMAGE)];
        let got = snapshots(&s, Some(Ok(inv)), &['a'], 2).await;
        for r in got {
            assert_eq!(
                r.map(|s| s.referrers),
                Ok(BTreeMap::from([(d('a'), Some(vec![]))]))
            );
        }
    }

    #[tokio::test]
    async fn snapshot_referrers_error_is_unknown() {
        let s = MockServer::start().await;
        serve(&s, "manifests", 'a', image_doc(None), 1).await;
        serve(&s, "referrers", 'a', ResponseTemplate::new(500), 1).await;
        let inv = vec![row('a', &["1.0"], IMAGE)];
        let got = snapshots(&s, Some(Ok(inv)), &['a'], 1).await;
        let [Ok(snap)] = &got[..] else {
            panic!("{got:?}")
        };
        assert_eq!(snap.referrers, BTreeMap::from([(d('a'), None)]));
    }

    #[tokio::test]
    async fn snapshot_marked_index_fetched_once() {
        let s = MockServer::start().await;
        serve(&s, "manifests", '1', index_doc(&[]), 1).await;
        serve(&s, "referrers", '1', json_resp(json!({"manifests": []})), 1).await;
        let inv = vec![row('1', &["v1"], INDEX)];
        let got = snapshots(&s, Some(Ok(inv)), &['1'], 1).await;
        let [Ok(snap)] = &got[..] else {
            panic!("{got:?}")
        };
        assert!(snap.unreadable.is_empty());
        assert_eq!(snap.referrers, BTreeMap::from([(d('1'), Some(vec![]))]));
    }

    #[tokio::test]
    async fn snapshot_marked_archived_not_fetched() {
        let s = MockServer::start().await;
        let archived = ArtifactSummary {
            status: ArtifactStatus::Archived,
            ..row('a', &["1.0"], IMAGE)
        };
        let got = snapshots(&s, Some(Ok(vec![archived])), &['a'], 1).await;
        let [Ok(snap)] = &got[..] else {
            panic!("{got:?}")
        };
        assert!(requested(&s).await.is_empty());
        assert!(!snap.referrers.contains_key(&d('a')));
        assert_eq!(snap.unreadable, BTreeSet::from([d('a')]));
    }

    #[tokio::test]
    async fn snapshot_worker_panic_reports_err() {
        let s = MockServer::start().await;
        let got = snapshots(&s, None, &['a'], 1).await;
        assert!(
            matches!(
                &got[..],
                [Err(SnapshotError::Inventory(RegistryError::Internal(_)))]
            ),
            "{got:?}"
        );
    }

    // ---- delete execution ----

    use OpOutcome::{Done, Failed, NotAttempted, Skipped, Uncertain};

    fn m(c: char) -> DeleteOp {
        DeleteOp::DeleteManifest { digest: d(c) }
    }

    fn tag_op() -> DeleteOp {
        DeleteOp::RemoveTag {
            tag: "v1".into(),
            expected: d('a'),
        }
    }

    fn layers(ls: &[&[char]]) -> DeletePlan {
        DeletePlan {
            layers: ls
                .iter()
                .map(|l| l.iter().map(|c| d(*c)).collect())
                .collect(),
            ..Default::default()
        }
    }

    fn ok(outcomes: Vec<OpOutcome>) -> Answer {
        Some(Ok((outcomes, false, None)))
    }

    /// Runs one `Effect::Delete` (op 7) for repo `x` against `script`; asserts exactly one
    /// `DeleteDone` for op 7 whose report lists `plan.ops()` once each, in order. Returns the
    /// report, the stop reason and the ops of each provider call.
    async fn run_delete(
        plan: DeletePlan,
        parents: &[(char, &[char])],
        script: Vec<Answer>,
    ) -> (DeleteReport, Option<RegistryError>, Vec<Vec<DeleteOp>>) {
        let fake = FakeDelete {
            deletes: Mutex::new(script.into()),
            ..Default::default()
        };
        run_delete_with(fake, rid(), plan, parents).await
    }

    /// `run_delete` with a prepared fake, sent to registry `id`.
    async fn run_delete_with(
        fake: FakeDelete,
        id: RegistryId,
        plan: DeletePlan,
        parents: &[(char, &[char])],
    ) -> (DeleteReport, Option<RegistryError>, Vec<Vec<DeleteOp>>) {
        let fake = Arc::new(fake);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut reg = registry(
            "http://127.0.0.1:9",
            Arc::new(Gate::new(8)),
            1,
            Duration::ZERO,
        );
        reg.provider = fake.clone();
        let rt = Runtime::new(vec![reg], None, Box::new(FakeClip(Ok(()))), tx);
        let parents = parents
            .iter()
            .map(|(c, ps)| (d(*c), ps.iter().map(|p| d(*p)).collect()))
            .collect();
        rt.run(Effect::Delete {
            op: OpId(7),
            registry: id,
            repo: repo("x"),
            plan: plan.clone(),
            parents,
        });
        let events = drain(rt, &mut rx).await;
        let [
            Event::DeleteDone {
                op,
                report,
                stopped,
            },
        ] = &events[..]
        else {
            panic!("want exactly one DeleteDone: {events:?}")
        };
        assert_eq!(*op, OpId(7));
        let ops: Vec<_> = report.results.iter().map(|(o, _)| o.clone()).collect();
        assert_eq!(ops, plan.ops(), "every op once, in plan order");
        let calls = fake.calls.lock().unwrap().clone();
        (report.clone(), stopped.clone(), calls)
    }

    fn outcomes(r: &DeleteReport) -> Vec<OpOutcome> {
        r.results.iter().map(|(_, o)| o.clone()).collect()
    }

    fn parent_skip() -> OpOutcome {
        Skipped("parent not deleted".into())
    }

    #[tokio::test]
    async fn executor_runs_tags_then_layers() {
        let plan = DeletePlan {
            remove_tags: vec![tag_op()],
            ..layers(&[&['1'], &['2']])
        };
        let script = vec![ok(vec![Done]), ok(vec![Done]), ok(vec![Done])];
        let (report, stopped, calls) = run_delete(plan, &[('2', &['1'])], script).await;
        assert_eq!(calls, vec![vec![tag_op()], vec![m('1')], vec![m('2')]]);
        assert_eq!(outcomes(&report), vec![Done, Done, Done]);
        assert!(!report.aborted_unsupported);
        assert_eq!(stopped, None);
    }

    #[tokio::test]
    async fn failed_parent_skips_transitively() {
        let plan = layers(&[&['1'], &['2'], &['3']]);
        let script = vec![ok(vec![Failed("ImageReferencedByManifestList".into())])];
        let parents: &[(char, &[char])] = &[('2', &['1']), ('3', &['2'])];
        let (report, stopped, calls) = run_delete(plan, parents, script).await;
        assert_eq!(calls, vec![vec![m('1')]]);
        assert_eq!(
            outcomes(&report),
            vec![
                Failed("ImageReferencedByManifestList".into()),
                parent_skip(),
                parent_skip()
            ]
        );
        assert_eq!(stopped, None);
    }

    #[tokio::test]
    async fn skip_sends_only_unblocked_ops() {
        // '3' hangs off failed '1', '4' off done '2'; '5' has no parent.
        let plan = layers(&[&['1', '2'], &['3', '4', '5']]);
        let script = vec![ok(vec![Failed("x".into()), Done]), ok(vec![Done, Done])];
        let parents: &[(char, &[char])] = &[('3', &['1']), ('4', &['2'])];
        let (report, _, calls) = run_delete(plan, parents, script).await;
        assert_eq!(calls, vec![vec![m('1'), m('2')], vec![m('4'), m('5')]]);
        assert_eq!(
            outcomes(&report),
            vec![Failed("x".into()), Done, parent_skip(), Done, Done]
        );
    }

    #[tokio::test]
    async fn unsupported_marks_rest_not_attempted() {
        let plan = layers(&[&['1'], &['2']]);
        let script = vec![Some(Ok((
            vec![Failed(DELETION_DISABLED.into())],
            true,
            None,
        )))];
        let (report, stopped, calls) = run_delete(plan, &[], script).await;
        assert_eq!(calls.len(), 1);
        assert!(report.aborted_unsupported);
        assert_eq!(
            outcomes(&report),
            vec![Failed(DELETION_DISABLED.into()), NotAttempted]
        );
        assert_eq!(stopped, None);
    }

    #[tokio::test]
    async fn network_error_mid_run() {
        let plan = layers(&[&['1'], &['2'], &['3']]);
        let script = vec![ok(vec![Done]), ok(vec![Uncertain("network".into())])];
        let (report, stopped, calls) = run_delete(plan, &[], script).await;
        assert_eq!(calls.len(), 2);
        assert_eq!(
            outcomes(&report),
            vec![Done, Uncertain("network".into()), NotAttempted]
        );
        assert!(!report.aborted_unsupported);
        assert_eq!(stopped, None);
    }

    #[tokio::test]
    async fn credential_failure_before_dispatch() {
        let plan = DeletePlan {
            remove_tags: vec![tag_op()],
            ..layers(&[&['1'], &['2']])
        };
        let script = vec![Some(Err(RegistryError::AwsCredentials("expired".into())))];
        let (report, stopped, calls) = run_delete(plan, &[], script).await;
        assert_eq!(calls.len(), 1);
        assert_eq!(outcomes(&report), vec![NotAttempted; 3]);
        assert!(matches!(stopped, Some(RegistryError::AwsCredentials(_))));
    }

    #[tokio::test]
    async fn credential_failure_before_later_batch() {
        // A third layer makes "no further call" observable.
        let plan = layers(&[&['1'], &['2', '3'], &['4']]);
        let expired = RegistryError::AwsCredentials("expired".into());
        let script = vec![
            ok(vec![Done]),
            Some(Ok((vec![Done, NotAttempted], false, Some(expired)))),
        ];
        let (report, stopped, calls) = run_delete(plan, &[], script).await;
        assert_eq!(calls.len(), 2);
        assert_eq!(
            outcomes(&report),
            vec![Done, Done, NotAttempted, NotAttempted]
        );
        assert!(matches!(stopped, Some(RegistryError::AwsCredentials(_))));
    }

    #[tokio::test]
    async fn writes_not_retried() {
        let plan = layers(&[&['1'], &['2'], &['3']]);
        let script = (0..3).map(|_| ok(vec![Failed("x".into())])).collect();
        let (report, stopped, calls) = run_delete(plan, &[], script).await;
        assert_eq!(calls, vec![vec![m('1')], vec![m('2')], vec![m('3')]]);
        assert_eq!(outcomes(&report), vec![Failed("x".into()); 3]);
        assert_eq!(stopped, None);
    }

    #[tokio::test]
    async fn short_provider_report_is_uncertain_and_stops() {
        let plan = layers(&[&['1', '2'], &['3']]);
        let (report, stopped, calls) = run_delete(plan, &[], vec![ok(vec![Done])]).await;
        assert_eq!(calls.len(), 1);
        assert_eq!(
            outcomes(&report),
            vec![
                Done,
                Uncertain("no answer for this op".into()),
                NotAttempted
            ]
        );
        assert_eq!(stopped, None);
    }

    #[tokio::test]
    async fn stop_reason_alone_stops() {
        let plan = layers(&[&['1'], &['2']]);
        let net = RegistryError::Network("reset".into());
        let script = vec![Some(Ok((vec![Done], false, Some(net.clone()))))];
        let (report, stopped, calls) = run_delete(plan, &[], script).await;
        assert_eq!(calls.len(), 1);
        assert_eq!(outcomes(&report), vec![Done, NotAttempted]);
        assert_eq!(stopped, Some(net));
    }

    #[tokio::test]
    async fn wrong_op_in_answer_is_uncertain_and_stops() {
        let plan = layers(&[&['1', '2'], &['3']]);
        let fake = FakeDelete {
            deletes: Mutex::new([ok(vec![Done, Done])].into()),
            wrong_first: Some(m('9')),
            ..Default::default()
        };
        let (report, stopped, calls) = run_delete_with(fake, rid(), plan, &[]).await;
        assert_eq!(calls.len(), 1);
        assert_eq!(
            outcomes(&report),
            vec![
                Uncertain("no answer for this op".into()),
                Done,
                NotAttempted
            ]
        );
        assert_eq!(stopped, None);
    }

    #[tokio::test]
    async fn unknown_registry_attempts_nothing() {
        let plan = layers(&[&['1'], &['2']]);
        let id = RegistryId("nope".into());
        let (report, stopped, calls) = run_delete_with(FakeDelete::default(), id, plan, &[]).await;
        assert!(calls.is_empty());
        assert_eq!(outcomes(&report), vec![NotAttempted; 2]);
        assert!(matches!(stopped, Some(RegistryError::Internal(_))));
    }

    /// Log lines written while the guard lives (this thread only: tests run current-thread).
    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Logs {
        fn capture() -> (Logs, tracing::subscriber::DefaultGuard) {
            // While at most one dispatcher is registered, tracing-core caches a new callsite's
            // interest from the registering thread's default alone: a test thread without a
            // subscriber that logs `delete finished` first caches `never` for every thread.
            // A second, permanent dispatcher makes it ask all live dispatchers (under a lock).
            static KEEP: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
            KEEP.get_or_init(|| {
                tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default())
            });
            let logs = Logs::default();
            let w = logs.clone();
            let sub = tracing_subscriber::fmt()
                .with_writer(move || w.clone())
                .with_ansi(false)
                .finish();
            (logs, tracing::subscriber::set_default(sub))
        }

        fn line(&self, msg: &str) -> String {
            let all = String::from_utf8(self.0.lock().unwrap().clone()).unwrap();
            let line = all.lines().find(|l| l.contains(msg));
            line.unwrap_or_else(|| panic!("no {msg:?} in {all}"))
                .to_owned()
        }
    }

    /// A callsite first hit on a thread without a subscriber still reaches a capture.
    #[test]
    fn capture_sees_callsite_registered_elsewhere() {
        fn probe() {
            tracing::info!("capture probe");
        }
        let (logs, _guard) = Logs::capture();
        std::thread::spawn(probe).join().unwrap();
        probe();
        logs.line("capture probe");
    }

    #[tokio::test]
    async fn delete_finished_logs_duration_and_stop_kind() {
        let (logs, _guard) = Logs::capture();
        let plan = layers(&[&['1'], &['2']]);
        let expired = RegistryError::AwsCredentials("expired-secret-text".into());
        let script = vec![Some(Ok((vec![Done], false, Some(expired))))];
        run_delete(plan, &[], script).await;
        let line = logs.line("delete finished");
        for want in [
            "elapsed_ms=",
            "aborted_unsupported=false",
            "stopped=\"credentials\"",
        ] {
            assert!(line.contains(want), "{want}: {line}");
        }
        assert!(!line.contains("expired-secret-text"), "{line}");
    }

    #[tokio::test]
    async fn executor_panic_reports_uncertain() {
        let (logs, _guard) = Logs::capture();
        let plan = layers(&[&['1'], &['2'], &['3']]);
        let (report, stopped, calls) = run_delete(plan, &[], vec![ok(vec![Done]), None]).await;
        let line = logs.line("delete worker failed");
        assert!(line.contains("WARN") && line.contains("repo=x"), "{line}");
        assert_eq!(calls.len(), 2);
        let stopped_worker = Uncertain("delete worker stopped".into());
        assert_eq!(
            outcomes(&report),
            vec![Done, stopped_worker.clone(), stopped_worker]
        );
        assert!(matches!(stopped, Some(RegistryError::Internal(_))));
    }

    #[tokio::test]
    async fn docker_calls_logged() {
        let l = Arc::new(FakeLocal {
            pull: vec![Ok(layer("aaa", ItemStep::Done))],
            ..Default::default()
        });
        let (rt, mut rx) = push_rt(Some(l), Creds::Basic(None));
        let log = crate::reqlog::RequestLog::default();
        let rt = rt.with_log(log.clone());
        rt.run(pull(1));
        drain(rt, &mut rx).await;
        let got: Vec<_> = log
            .snapshot()
            .into_iter()
            .map(|e| (e.registry, e.op, e.path, e.outcome))
            .collect();
        assert_eq!(
            got,
            [(
                "r".to_string(),
                "docker pull".to_string(),
                "h/app:1.0.1".to_string(),
                "ok".to_string()
            )]
        );
    }
}

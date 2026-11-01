//! The single write operation (spec §7.3, §8): one at a time, events keyed by `OpId`.

use time::OffsetDateTime;

use super::cache::Entry;
use super::delete::DelStage;
use super::messages::user_message;
use super::state::{Action, CacheKey, CacheValue, Effect, Pane, RegistryView, State, StatusKind};
use super::transfer::{Transfer, TransferKind, TransferOutcome, fmt_speed};
use crate::docker::PushStep;
use crate::domain::ArtifactStatus;
use crate::domain::{
    ArtifactSummary, Descriptor, Digest, ImageConfig, LocalImage, ManifestDocument, ManifestKind,
    Platform, Reference, RegistryError, RegistryId, RepoPath, TransferStep, image_ref,
};

/// Said when a push was cancelled (status line and wizard).
pub const CANCELLED: &str = "push cancelled — uploaded layers may remain; the tag is unchanged unless the manifest upload had completed";

/// A failed push for the status line and the wizard. `user_message` tells to press `r` to
/// retry, which does not retry a push, so the retryable errors name `p` instead.
pub fn push_failure_message(err: &RegistryError, reg: Option<&RegistryView>) -> String {
    let Some(r) = reg else {
        return format!("push failed: {err}");
    };
    let id = &r.id;
    match err {
        RegistryError::Network(m) => format!("network error: {m} — press p to push again"),
        RegistryError::Timeout(_) => format!("push to {id} timed out — press p to push again"),
        RegistryError::RateLimited => format!("rate limited by {id} — press p to push again"),
        _ => user_message(err, r, "push"),
    }
}

/// The write lock is taken: a push, delete or copy runs (one write at a time).
pub const BUSY: &str = "another push/delete/copy is running";

/// Said when `e` (reload) is refused during a transfer.
pub const RELOAD_BUSY: &str = "finish or cancel the running pull/copy/export first";

/// A failed pull: Docker's own message, without the registry prefix or a `press r` hint
/// (`r` refreshes browsing; it does not retry a pull).
pub fn pull_failure_message(err: &RegistryError, reg: Option<&RegistryView>) -> String {
    let m = match (err, reg) {
        (RegistryError::Provider(m), _) => m
            .strip_prefix("docker pull failed: ")
            .unwrap_or(m)
            .to_owned(),
        (_, Some(r)) => {
            let m = user_message(err, r, "image");
            m.split_once(" — press r")
                .map_or_else(|| m.clone(), |(m, _)| m.to_owned())
        }
        (_, None) => err.to_string(),
    };
    format!("pull failed: {m}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpId(pub u64);

#[derive(Clone, Debug, PartialEq)]
pub enum PushOutcome {
    Pushed(Option<Digest>),
    Cancelled,
    Failed(RegistryError),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub layers_done: usize,
    /// Latest byte snapshot of whichever transfer reported last (not an aggregate).
    pub bytes: Option<(u64, u64)>,
    pub last_status: String,
    pub digest: Option<Digest>,
}

impl Progress {
    fn apply(&mut self, step: PushStep) {
        match step {
            PushStep::Status(s) => self.last_status = s,
            PushStep::Bytes { current, total } => self.bytes = Some((current, total)),
            PushStep::LayerDone => {
                self.layers_done += 1;
                self.bytes = None;
            }
            PushStep::Digest(d) => self.digest = Some(d),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PullStage {
    /// What will be pulled; nothing runs yet.
    Details,
    Running,
    Result(TransferOutcome),
}

/// The pull modal (spec §7.7). Local-only, so outside the write lock; one pull at a time.
#[derive(Clone, Debug, PartialEq)]
pub struct PullModal {
    pub registry: RegistryId,
    pub repo: RepoPath,
    /// The selected (top-level) manifest.
    pub digest: Digest,
    /// `<ref_host>/<repo>:<tag>` or `@<digest>`.
    pub image: String,
    /// The selected child of an index; `None` for a single image.
    pub child: Option<Digest>,
    /// The selected platform of an index; `None` for a single image.
    pub platform: Option<Platform>,
    pub stage: PullStage,
    pub op: Option<OpId>,
    pub transfer: Option<Transfer>,
    /// Running on with the modal closed (Enter); `P` shows it again.
    pub hidden: bool,
}

impl PullModal {
    fn manifest<'s>(&self, s: &'s State, d: &Digest) -> Option<&'s ManifestDocument> {
        let key = CacheKey::Manifest(self.registry.clone(), self.repo.clone(), d.clone());
        match s.value(&key)? {
            CacheValue::Manifest(m) => Some(m),
            _ => None,
        }
    }

    /// The image manifest pulled (an index's child for `platform`) and its digest, once loaded.
    pub fn image_manifest<'s>(&self, s: &'s State) -> Option<(Digest, &'s ManifestDocument)> {
        let top = self.manifest(s, &self.digest)?;
        let ManifestKind::Index { .. } = &top.kind else {
            return Some((self.digest.clone(), top));
        };
        let child = self.child.as_ref()?;
        Some((child.clone(), self.manifest(s, child)?))
    }

    pub fn layers<'s>(&self, s: &'s State) -> Option<&'s [Descriptor]> {
        match &self.image_manifest(s)?.1.kind {
            ManifestKind::Image { layers, .. } => Some(layers),
            ManifestKind::Index { .. } => None,
        }
    }

    pub fn config<'s>(&self, s: &'s State) -> Option<&'s ImageConfig> {
        let (d, _) = self.image_manifest(s)?;
        match s.value(&CacheKey::Config(
            self.registry.clone(),
            self.repo.clone(),
            d,
        ))? {
            CacheValue::Config(Some(c)) => Some(c),
            _ => None,
        }
    }

    /// The registry's reported size of a single image, else config + layers of the image
    /// manifest (`display_size`, without depending on the selection).
    pub fn size(&self, s: &State) -> Option<u64> {
        let (d, m) = self.image_manifest(s)?;
        let ManifestKind::Image { config, layers } = &m.kind else {
            return None;
        };
        let key = CacheKey::Inventory(self.registry.clone(), self.repo.clone());
        let reported = match s.value(&key) {
            Some(CacheValue::Inventory(all)) if d == self.digest => {
                all.iter().find(|a| a.digest == d).and_then(|a| a.size)
            }
            _ => None,
        };
        reported.or(Some(
            config.size + layers.iter().map(|l| l.size).sum::<u64>(),
        ))
    }

    /// The local image of this reference, once the local images show it.
    pub fn local<'s>(&self, s: &'s State) -> Option<&'s LocalImage> {
        let reg = s.registries().iter().find(|r| r.id == self.registry)?;
        match s.value(&CacheKey::LocalImages)? {
            CacheValue::LocalImages(all) => {
                crate::docker::local_match(all, &reg.ref_host, &self.repo, &self.digest)
            }
            _ => None,
        }
    }

    /// `app:1.0`: the reference without its registry host.
    fn short_image(&self) -> &str {
        self.image.split_once('/').map_or(&self.image, |(_, r)| r)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WriteOp {
    pub op: OpId,
    pub registry: RegistryId,
    pub repo: RepoPath,
    pub tag: String,
    /// `<ref_host>/<repo>:<tag>`.
    pub target: String,
    pub progress: Progress,
    pub outcome: Option<PushOutcome>,
}

/// The pushed image to select once a post-push inventory shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingFocus {
    pub registry: RegistryId,
    pub repo: RepoPath,
    /// `None` = the push reported no digest; match on `tag`.
    pub digest: Option<Digest>,
    pub tag: String,
}

impl State {
    pub fn write(&self) -> Option<&WriteOp> {
        self.write.as_ref()
    }

    /// The pull modal, also while hidden (the UI draws it only when shown).
    pub fn pull_modal(&self) -> Option<&PullModal> {
        self.pull.as_ref()
    }

    /// A pull, copy or export is running.
    pub fn transfer_running(&self) -> bool {
        self.pull
            .as_ref()
            .is_some_and(|m| m.stage == PullStage::Running)
            || self.copy_running()
            || self.export_running()
    }

    /// Why a config reload (`e`) is refused now, if it is.
    pub fn reload_refusal(&self) -> Option<String> {
        let m = if self.write_running() {
            BUSY
        } else if self.delete.is_some() {
            // A reload would drop the hidden report and `delete_disabled`.
            "close the delete report first (press d)"
        } else if self.transfer_running() {
            RELOAD_BUSY
        } else {
            return None;
        };
        Some(m.to_owned())
    }

    /// The status line of a hidden running pull.
    pub fn pull_status(&self, now: OffsetDateTime) -> Option<String> {
        let m = self
            .pull
            .as_ref()
            .filter(|m| m.hidden && m.stage == PullStage::Running)?;
        let mut s = format!("pulling {}", m.short_image());
        if let Some(t) = &m.transfer {
            if let Some(p) = t.percent() {
                s += &format!(" {p}%");
            }
            s += &format!(" {}", fmt_speed(t.speed(now)));
        }
        Some(s + " — press P to show")
    }

    /// The finished pull's result line while its modal shows it.
    pub fn pull_result(&self) -> Option<(StatusKind, String)> {
        let m = self.pull.as_ref()?;
        match &m.stage {
            PullStage::Result(o) => Some(self.pull_result_text(m, o)),
            _ => None,
        }
    }

    fn pull_result_text(&self, m: &PullModal, o: &TransferOutcome) -> (StatusKind, String) {
        match o {
            TransferOutcome::Done => {
                let mut s = "pulled".to_owned();
                s += &m
                    .transfer
                    .as_ref()
                    .map(Transfer::summary)
                    .unwrap_or_default();
                if let Some(l) = m.local(self) {
                    let id = l.id.strip_prefix("sha256:").unwrap_or(&l.id);
                    s += &format!(" · image {}", id.chars().take(12).collect::<String>());
                }
                (StatusKind::Ok, s)
            }
            TransferOutcome::Cancelled => (StatusKind::Info, "pull cancelled".into()),
            TransferOutcome::Failed(e) => {
                let reg = self.registries.iter().find(|r| r.id == m.registry);
                (StatusKind::Error, pull_failure_message(e, reg))
            }
            // Copy/export outcomes; a pull never ends with these.
            TransferOutcome::Uncertain(_)
            | TransferOutcome::DigestDiffers { .. }
            | TransferOutcome::Exists => (StatusKind::Error, "pull ended unexpectedly".into()),
        }
    }

    /// A push or copy without an outcome, or a delete revalidating or running (the shared write
    /// lock).
    pub fn write_running(&self) -> bool {
        self.write.as_ref().is_some_and(|w| w.outcome.is_none())
            || self.copy_running()
            || self
                .delete
                .as_ref()
                .is_some_and(|m| matches!(m.stage, DelStage::Revalidate | DelStage::Running))
    }

    /// Takes the write lock for a push of `image_id` as `<ref_host>/<repo>:<tag>`.
    pub(super) fn start_push(
        &mut self,
        registry: RegistryId,
        repo: RepoPath,
        image_id: String,
        tag: String,
    ) -> Result<Effect, String> {
        if self.write_running() {
            return Err(BUSY.into());
        }
        let reg = self
            .registries
            .iter()
            .find(|r| r.id == registry)
            .ok_or_else(|| format!("unknown registry {registry}"))?;
        let target = image_ref(&reg.ref_host, &repo, &Reference::Tag(tag.clone()));
        self.next_op += 1;
        let op = OpId(self.next_op);
        self.write = Some(WriteOp {
            op,
            registry: registry.clone(),
            repo: repo.clone(),
            tag: tag.clone(),
            target,
            progress: Progress::default(),
            outcome: None,
        });
        Ok(Effect::Push {
            op,
            registry,
            repo,
            image_id,
            tag,
        })
    }

    /// The running op, if it is `op`; events for any other op are stale.
    fn running(&mut self, op: OpId) -> Option<&mut WriteOp> {
        self.write
            .as_mut()
            .filter(|w| w.op == op && w.outcome.is_none())
    }

    pub(super) fn push_progress(&mut self, op: OpId, step: PushStep) {
        if let Some(w) = self.running(op) {
            w.progress.apply(step);
        }
    }

    /// `P`: show a hidden pull, else open the pull modal for the selected Active image on the
    /// Images pane (spec §7.7). Nothing runs until Enter.
    pub(super) fn pull_start(&mut self) -> Vec<Effect> {
        if let Some(m) = self.pull.as_mut().filter(|m| m.hidden) {
            m.hidden = false;
            return vec![];
        }
        if self.focus != Pane::Images {
            return vec![];
        }
        let Some(img) = self.selected_image() else {
            return vec![];
        };
        if img.status != ArtifactStatus::Active {
            self.set_status(StatusKind::Error, "archived images cannot be pulled".into());
            return vec![];
        }
        if let Some(fx) = self.need_local_images() {
            return fx;
        }
        let (Some(reg), Some(repo), Some(img), Some(image)) = (
            self.selected_registry(),
            self.selected_repo(),
            self.selected_image(),
            self.image_reference(),
        ) else {
            return vec![];
        };
        let (child, platform) = match self.manifest(&img.digest).map(|m| &m.kind) {
            Some(ManifestKind::Index { manifests }) => {
                manifests.get(self.platform).map_or((None, None), |d| {
                    (Some(d.digest.clone()), d.platform.clone())
                })
            }
            _ => (None, None),
        };
        self.pull = Some(PullModal {
            registry: reg.id.clone(),
            repo: repo.path.clone(),
            digest: img.digest.clone(),
            image,
            child,
            platform,
            stage: PullStage::Details,
            op: None,
            transfer: None,
            hidden: false,
        });
        vec![]
    }

    /// A key while the pull modal is shown: Enter pulls / hides / closes, Esc closes / cancels.
    pub(super) fn pull_action(&mut self, a: Action, now: OffsetDateTime) -> Vec<Effect> {
        let Some(mut m) = self.pull.take() else {
            return vec![];
        };
        let mut fx = vec![];
        match (&m.stage, a) {
            (PullStage::Details, Action::Enter) => {
                // Docker's per-layer id is the layer digest's first 12 hex chars.
                let planned = m
                    .layers(self)
                    .unwrap_or_default()
                    .iter()
                    .map(|l| (l.digest.short(), l.size))
                    .collect();
                self.next_op += 1;
                let op = OpId(self.next_op);
                fx.push(Effect::Pull {
                    op,
                    registry: m.registry.clone(),
                    image: m.image.clone(),
                    platform: m.platform.as_ref().map(ToString::to_string),
                });
                m.op = Some(op);
                m.transfer = Some(Transfer::new(TransferKind::Pull, planned, now));
                m.stage = PullStage::Running;
            }
            (PullStage::Running, Action::Enter) => m.hidden = true,
            (PullStage::Running, Action::Back) => fx.extend(m.op.map(Effect::Cancel)),
            // Esc on the details, Enter / Esc on the result: close.
            (_, Action::Enter | Action::Back) => return vec![],
            _ => {}
        }
        self.pull = Some(m);
        fx
    }

    /// The running pull's modal, if `op` is it; events for any other op are stale.
    fn running_pull(&mut self, op: OpId) -> Option<&mut PullModal> {
        self.pull
            .as_mut()
            .filter(|m| m.op == Some(op) && m.stage == PullStage::Running)
    }

    pub(super) fn pull_progress(&mut self, op: OpId, step: TransferStep, now: OffsetDateTime) {
        if let Some(t) = self.running_pull(op).and_then(|m| m.transfer.as_mut()) {
            t.apply(step, now);
        }
    }

    /// Shows the outcome (in the modal, or on the status line when hidden, dropping the modal)
    /// and refreshes the local images (the row gets its local marker).
    pub(super) fn pull_done(
        &mut self,
        op: OpId,
        outcome: TransferOutcome,
        now: OffsetDateTime,
    ) -> Vec<Effect> {
        let Some(m) = self.running_pull(op) else {
            return vec![];
        };
        if let Some(t) = &mut m.transfer {
            t.finished = Some(now);
            if outcome == TransferOutcome::Done {
                t.settle();
            }
        }
        m.stage = PullStage::Result(outcome.clone());
        if m.hidden
            && let Some(m) = self.pull.take()
        {
            let (kind, msg) = self.pull_result_text(&m, &outcome);
            self.status = Some((None, kind, msg));
        }
        self.cache
            .invalidate(CacheKey::LocalImages)
            .into_iter()
            .collect()
    }

    /// Unlocks, reports the outcome and refreshes the target repo's images and the local
    /// images on every outcome (a `docker tag` may have succeeded before a failure).
    pub(super) fn push_done(&mut self, op: OpId, outcome: PushOutcome) -> Vec<Effect> {
        let Some(w) = self.running(op) else {
            return vec![];
        };
        w.outcome = Some(outcome.clone());
        let w = w.clone();
        let (kind, msg) = match outcome {
            PushOutcome::Pushed(digest) => {
                self.pending_focus = Some(PendingFocus {
                    registry: w.registry.clone(),
                    repo: w.repo.clone(),
                    digest,
                    tag: w.tag.clone(),
                });
                (StatusKind::Ok, format!("pushed {}", w.target))
            }
            PushOutcome::Cancelled => (StatusKind::Info, CANCELLED.to_owned()),
            PushOutcome::Failed(e) => {
                let reg = self.registries.iter().find(|r| r.id == w.registry);
                (StatusKind::Error, push_failure_message(&e, reg))
            }
        };
        self.status = Some((None, kind, msg));
        [
            CacheKey::Inventory(w.registry, w.repo),
            CacheKey::LocalImages,
        ]
        .into_iter()
        .filter_map(|k| self.cache.invalidate(k))
        .collect()
    }

    /// After an applied `Loaded` for `key`: once the push target's inventory shows the pushed
    /// image, select and focus it (only if that repo is still the selected one), clearing an
    /// image filter that hides it. A result without it (e.g. fetched before the push ended), or
    /// one applied while a refetch is still in flight (stale), keeps waiting.
    pub(super) fn focus_pushed(&mut self, key: &CacheKey) {
        let Some(pf) = &self.pending_focus else {
            return;
        };
        if *key != CacheKey::Inventory(pf.registry.clone(), pf.repo.clone())
            || matches!(self.cache.get(key), Some(Entry::Loading { .. }))
        {
            return;
        }
        let Some(CacheValue::Inventory(all)) = self.cache.value(key) else {
            return;
        };
        let is_it = |a: &ArtifactSummary| match &pf.digest {
            Some(d) => a.digest == *d,
            None => a.tags.contains(&pf.tag),
        };
        let Some(digest) = all.iter().find(|a| is_it(a)).map(|a| a.digest.clone()) else {
            return;
        };
        let here = self.selected_registry().map(|r| &r.id) == Some(&pf.registry)
            && self.selected_repo().map(|r| &r.path) == Some(&pf.repo);
        self.pending_focus = None;
        if !here {
            return;
        }
        if !self.visible_images().iter().any(|a| a.digest == digest) {
            self.image_filter.clear();
        }
        self.focus = Pane::Images;
        self.filter_editing = false;
        let row = self
            .visible_images()
            .iter()
            .position(|a| a.digest == digest)
            .unwrap_or(0);
        self.select(Pane::Images, row);
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;
    use crate::app::state::{Action, Event, KeyMode};
    use crate::app::update::tests::{
        T, art, at_images, desc, dg, fetched, inv, man, manifest, rid, rp,
    };
    use crate::domain::{ItemStep, LocalImage, ManifestKind, Platform};

    /// Images pane of ecr/app (`h.example`) with `images`, local images loaded, and a push of
    /// `tag` to ecr/`repo` started as op 1.
    fn pushing(images: Vec<crate::domain::ArtifactSummary>, repo: &str, tag: &str) -> T {
        let (mut t, _) = at_images(images);
        t.ok(CacheKey::LocalImages, CacheValue::LocalImages(vec![]));
        t.s.start_push(rid("ecr"), rp(repo), "img".into(), tag.into())
            .unwrap();
        t
    }

    fn progress(op: u64, step: PushStep) -> Event {
        Event::PushProgress { op: OpId(op), step }
    }

    fn done(op: u64, outcome: PushOutcome) -> Event {
        Event::PushDone {
            op: OpId(op),
            outcome,
        }
    }

    // ---- pull ----

    const NOW: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

    fn at(s: f64) -> OffsetDateTime {
        NOW + time::Duration::seconds_f64(s)
    }

    /// Image manifest with one layer per `(digest char, size)`.
    fn image_of(layers: &[(char, u64)]) -> CacheValue {
        manifest(ManifestKind::Image {
            config: desc('c', 10),
            layers: layers.iter().map(|(c, s)| desc(*c, *s)).collect(),
        })
    }

    fn plat(s: &str) -> Platform {
        let mut p = s.split('/').map(str::to_owned);
        Platform {
            os: p.next().unwrap(),
            architecture: p.next().unwrap(),
            variant: p.next(),
        }
    }

    /// Images pane of ecr/app with `app:1.0` (digest `a`), local images and its manifest `m`
    /// loaded.
    fn at_pull(m: CacheValue) -> T {
        let (mut t, _) = at_images(vec![art('a', &["1.0"])]);
        t.ok(CacheKey::LocalImages, CacheValue::LocalImages(vec![]));
        t.ok(man("ecr", 'a'), m);
        t
    }

    fn pull_fx(platform: Option<&str>) -> Vec<Effect> {
        vec![Effect::Pull {
            op: OpId(1),
            registry: rid("ecr"),
            image: "h.example/app:1.0".into(),
            platform: platform.map(Into::into),
        }]
    }

    fn pstep(c: char, step: ItemStep) -> Event {
        Event::PullProgress {
            op: OpId(1),
            step: TransferStep {
                id: c.to_string().repeat(12),
                step,
            },
        }
    }

    fn pdone(outcome: TransferOutcome) -> Event {
        Event::PullDone {
            op: OpId(1),
            outcome,
        }
    }

    fn stage(t: &T) -> Option<PullStage> {
        t.s.pull_modal().map(|m| m.stage.clone())
    }

    fn status(t: &T) -> Option<(StatusKind, &str)> {
        t.s.status()
    }

    /// 10.0MB in eight layers `1`..`8`.
    const EIGHT: [(char, u64); 8] = [
        ('1', 3_000_000),
        ('2', 2_000_000),
        ('3', 1_500_000),
        ('4', 1_000_000),
        ('5', 1_000_000),
        ('6', 800_000),
        ('7', 500_000),
        ('8', 200_000),
    ];

    #[test]
    fn pull_opens_details_without_network() {
        let mut t = at_pull(image_of(&[('1', 100)]));
        assert!(t.act(Action::PullStart).is_empty());
        let m = t.s.pull_modal().unwrap();
        assert_eq!(m.stage, PullStage::Details);
        assert_eq!((m.image.as_str(), m.hidden), ("h.example/app:1.0", false));
        assert_eq!(t.s.key_mode(), KeyMode::List);
        assert!(!t.s.transfer_running());
        assert!(t.act(Action::Back).is_empty());
        assert!(t.s.pull_modal().is_none());
        assert_eq!(t.s.key_mode(), KeyMode::Normal);

        // Untagged: by digest.
        let (mut t, _) = at_images(vec![art('b', &[])]);
        t.ok(CacheKey::LocalImages, CacheValue::LocalImages(vec![]));
        t.act(Action::PullStart);
        let m = t.s.pull_modal().unwrap();
        assert_eq!(m.image, format!("h.example/app@{}", dg('b')));
    }

    #[test]
    fn pull_refuses_archived_and_other_panes() {
        let mut old = art('a', &["1.0"]);
        old.status = crate::domain::ArtifactStatus::Archived;
        let (mut t, _) = at_images(vec![old]);
        t.ok(CacheKey::LocalImages, CacheValue::LocalImages(vec![]));
        assert!(t.act(Action::PullStart).is_empty());
        assert_eq!(
            status(&t),
            Some((StatusKind::Error, "archived images cannot be pulled"))
        );
        assert!(t.s.pull_modal().is_none());

        let mut t = at_pull(image_of(&[('1', 100)]));
        t.act(Action::PrevPane);
        assert!(t.act(Action::PullStart).is_empty());
        assert!(t.s.pull_modal().is_none() && t.s.status().is_none());
    }

    #[test]
    fn pull_without_docker_says_unavailable() {
        let (mut t, _) = at_images(vec![art('a', &["1.0"])]);
        t.load(
            CacheKey::LocalImages,
            Err(RegistryError::Provider("x".into())),
        );
        let fx = t.act(Action::PullStart);
        assert_eq!(fetched(&fx), vec![CacheKey::LocalImages]);
        assert_eq!(
            status(&t),
            Some((StatusKind::Error, crate::docker::UNAVAILABLE))
        );
        assert!(t.s.pull_modal().is_none());
    }

    #[test]
    fn pull_index_uses_selected_platform() {
        let child = |c, p: &str| {
            let mut d = desc(c, 500);
            d.platform = Some(plat(p));
            d
        };
        let idx = manifest(ManifestKind::Index {
            manifests: vec![
                child('d', "linux/amd64"),
                child('e', "linux/arm64"),
                child('f', "linux/arm/v7"),
            ],
        });
        for (n, want) in [(1, "linux/arm64"), (2, "linux/arm/v7")] {
            let mut t = at_pull(idx.clone());
            for _ in 0..n {
                t.act(Action::PlatformNext);
            }
            t.act(Action::PullStart);
            assert_eq!(t.s.pull_modal().unwrap().platform, Some(plat(want)));
            if n == 1 {
                // The child's layers are the planned items.
                t.ok(man("ecr", 'e'), image_of(&[('7', 70)]));
            }
            assert_eq!(t.act(Action::Enter), pull_fx(Some(want)), "{want}");
            let items = &t.s.pull_modal().unwrap().transfer.as_ref().unwrap().items;
            if n == 1 {
                assert_eq!(items.len(), 1);
                assert_eq!(
                    (items[0].id.as_str(), items[0].size),
                    ("777777777777", Some(70))
                );
            } else {
                assert!(items.is_empty(), "child manifest not loaded");
            }
        }
        let mut t = at_pull(image_of(&[('1', 100), ('2', 50)]));
        t.act(Action::PullStart);
        assert_eq!(t.act(Action::Enter), pull_fx(None));
        let m = t.s.pull_modal().unwrap();
        assert_eq!((m.stage.clone(), m.op), (PullStage::Running, Some(OpId(1))));
        let ids: Vec<_> = m
            .transfer
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|i| (i.id.as_str(), i.size))
            .collect();
        assert_eq!(
            ids,
            [("111111111111", Some(100)), ("222222222222", Some(50))]
        );
    }

    #[test]
    fn pull_index_child_by_digest() {
        // Two children with one platform: the selected row's digest decides.
        let child = |c| {
            let mut d = desc(c, 500);
            d.platform = Some(plat("linux/amd64"));
            d
        };
        let mut t = at_pull(manifest(ManifestKind::Index {
            manifests: vec![child('d'), child('e')],
        }));
        t.act(Action::PlatformNext);
        t.act(Action::PullStart);
        t.ok(man("ecr", 'e'), image_of(&[('7', 70)]));
        let m = t.s.pull_modal().unwrap();
        assert_eq!(m.image_manifest(&t.s).map(|(d, _)| d), Some(dg('e')));
    }

    #[test]
    fn pull_enter_while_loading() {
        let (mut t, _) = at_images(vec![art('a', &["1.0"])]);
        t.ok(CacheKey::LocalImages, CacheValue::LocalImages(vec![]));
        t.act(Action::PullStart);
        assert_eq!(t.act(Action::Enter), pull_fx(None));
        let m = t.s.pull_modal().unwrap();
        assert!(m.transfer.as_ref().unwrap().items.is_empty());
        assert!(t.s.transfer_running());
    }

    #[test]
    fn pull_running_hide_reopen_cancel() {
        let mut t = at_pull(image_of(&EIGHT));
        t.act(Action::PullStart);
        t.act(Action::Enter);
        // Stale op: ignored.
        t.ev(Event::PullProgress {
            op: OpId(9),
            step: TransferStep {
                id: "111111111111".into(),
                step: ItemStep::Done,
            },
        });
        t.ev_at(
            pstep(
                '1',
                ItemStep::Bytes {
                    current: 100_000,
                    total: 3_000_000,
                },
            ),
            at(2.0),
        );
        for (c, s) in [
            ('1', ItemStep::Done),
            (
                '2',
                ItemStep::Bytes {
                    current: 1_500_000,
                    total: 2_000_000,
                },
            ),
            (
                '3',
                ItemStep::Bytes {
                    current: 500_000,
                    total: 1_500_000,
                },
            ),
            ('6', ItemStep::Extracting),
            ('7', ItemStep::Done),
            ('8', ItemStep::Exists),
        ] {
            t.ev_at(pstep(c, s), at(4.0));
        }
        assert!(t.act(Action::Enter).is_empty());
        assert!(t.s.pull_modal().unwrap().hidden);
        assert_eq!(t.s.key_mode(), KeyMode::Normal);
        assert_eq!(
            t.s.pull_status(at(4.0)).as_deref(),
            Some("pulling app:1.0 65% 3.1 MB/s — press P to show")
        );
        // Not a registry write: the write lock stays free.
        assert!(!t.s.write_running() && t.s.transfer_running());

        // `P` shows it again (from any pane).
        t.act(Action::PrevPane);
        assert!(t.act(Action::PullStart).is_empty());
        assert!(!t.s.pull_modal().unwrap().hidden);
        assert_eq!(t.s.pull_status(at(4.0)), None);

        assert_eq!(t.act(Action::Back), vec![Effect::Cancel(OpId(1))]);
        assert_eq!(stage(&t), Some(PullStage::Running));
        t.ev(pdone(TransferOutcome::Cancelled));
        assert_eq!(
            stage(&t),
            Some(PullStage::Result(TransferOutcome::Cancelled))
        );
        assert!(!t.s.transfer_running());
        assert_eq!(
            t.s.pull_result(),
            Some((StatusKind::Info, "pull cancelled".into()))
        );
        assert!(t.act(Action::Back).is_empty());
        assert!(t.s.pull_modal().is_none());
    }

    #[test]
    fn pull_done_result_and_refresh() {
        let mut t = at_pull(image_of(&[('1', 12_600_000), ('2', 300_000)]));
        t.act(Action::PullStart);
        t.act(Action::Enter);
        t.ev_at(pstep('2', ItemStep::Exists), at(1.0));
        t.ev_at(pstep('1', ItemStep::Done), at(4.0));
        // Stale op: ignored.
        assert!(
            t.ev(Event::PullDone {
                op: OpId(9),
                outcome: TransferOutcome::Done
            })
            .is_empty()
        );
        assert_eq!(stage(&t), Some(PullStage::Running));
        let fx = t.ev_at(pdone(TransferOutcome::Done), at(4.2));
        assert_eq!(fetched(&fx), vec![CacheKey::LocalImages]);
        assert_eq!(stage(&t), Some(PullStage::Result(TransferOutcome::Done)));
        // Exists layers count toward the size, not the speed.
        let ok = |s: &str| Some((StatusKind::Ok, s.to_owned()));
        assert_eq!(
            t.s.pull_result(),
            ok("pulled · 12.9MB in 4.2s (avg 3.0 MB/s)")
        );
        t.ok(
            CacheKey::LocalImages,
            CacheValue::LocalImages(vec![LocalImage {
                id: format!("sha256:88d90c6be1e0{}", "e".repeat(52)),
                repo_tags: vec![],
                repo_digests: vec![format!("h.example/app@{}", dg('a'))],
                manifests: vec![],
                created: None,
                size: 1,
            }]),
        );
        assert_eq!(
            t.s.pull_result(),
            ok("pulled · 12.9MB in 4.2s (avg 3.0 MB/s) · image 88d90c6be1e0")
        );
        assert!(t.act(Action::Enter).is_empty());
        assert!(t.s.pull_modal().is_none());
        // A new pull can start.
        t.act(Action::PullStart);
        assert_eq!(stage(&t), Some(PullStage::Details));
    }

    #[test]
    fn pull_done_settles_waiting_layers() {
        let mut t = at_pull(image_of(&[('1', 1_900_000)]));
        t.act(Action::PullStart);
        t.act(Action::Enter);
        t.ev_at(pdone(TransferOutcome::Done), at(1.6));
        let m = t.s.pull_modal().unwrap();
        let tr = m.transfer.as_ref().unwrap();
        assert_eq!(tr.items[0].state, ItemStep::Exists);
        let (_, text) = t.s.pull_result().unwrap();
        assert!(!text.contains("avg"), "{text}");
    }

    #[test]
    fn pull_done_while_hidden_goes_to_status() {
        let mut t = at_pull(image_of(&[('1', 100)]));
        t.act(Action::PullStart);
        t.act(Action::Enter);
        t.act(Action::Enter);
        let fx = t.ev(pdone(TransferOutcome::Failed(RegistryError::Provider(
            "docker pull failed: manifest unknown".into(),
        ))));
        assert_eq!(fetched(&fx), vec![CacheKey::LocalImages]);
        assert!(t.s.pull_modal().is_none());
        assert_eq!(
            status(&t),
            Some((StatusKind::Error, "pull failed: manifest unknown"))
        );
    }

    #[test]
    fn pull_failure_message_drops_prefixes() {
        use RegistryError as E;
        let v = t_view();
        let m = |e: E| pull_failure_message(&e, Some(&v));
        assert_eq!(
            m(E::Provider("docker pull failed: x".into())),
            "pull failed: x"
        );
        assert_eq!(m(E::Provider("x".into())), "pull failed: x");
        assert_eq!(
            m(E::Network("down".into())),
            "pull failed: network error: down"
        );
        assert_eq!(m(E::RateLimited), "pull failed: rate limited by ecr");
        assert_eq!(
            pull_failure_message(&E::Network("down".into()), None),
            "pull failed: network error: down"
        );
    }

    fn sel(t: &T) -> Option<Digest> {
        t.s.selected_image().map(|a| a.digest.clone())
    }

    #[test]
    fn start_push_allocates_op_and_locks() {
        let (mut t, _) = at_images(vec![art('a', &["1.0.0"])]);
        let fx =
            t.s.start_push(rid("ecr"), rp("app"), "img".into(), "1.0.1".into());
        assert_eq!(
            fx,
            Ok(Effect::Push {
                op: OpId(1),
                registry: rid("ecr"),
                repo: rp("app"),
                image_id: "img".into(),
                tag: "1.0.1".into(),
            })
        );
        assert!(t.s.write_running());
        let w = t.s.write().unwrap();
        assert_eq!((w.op, w.target.as_str()), (OpId(1), "h.example/app:1.0.1"));

        let again =
            t.s.start_push(rid("ecr"), rp("app"), "img".into(), "1.0.2".into());
        assert_eq!(again, Err(BUSY.into()));
        assert_eq!(t.s.write().unwrap().tag, "1.0.1", "no new op");

        t.ev(done(1, PushOutcome::Cancelled));
        let next =
            t.s.start_push(rid("ecr"), rp("app"), "img".into(), "1.0.2".into());
        assert!(
            matches!(next, Ok(Effect::Push { op: OpId(2), .. })),
            "{next:?}"
        );
    }

    #[test]
    fn progress_applies_only_to_current_op() {
        let mut t = pushing(vec![], "app", "1.0.1");
        t.ev(progress(9, PushStep::Status("other".into())));
        assert_eq!(t.s.write().unwrap().progress, Progress::default());

        t.ev(progress(1, PushStep::Status("Pushing".into())));
        t.ev(progress(
            1,
            PushStep::Bytes {
                current: 5,
                total: 10,
            },
        ));
        assert_eq!(t.s.write().unwrap().progress.bytes, Some((5, 10)));
        t.ev(progress(1, PushStep::LayerDone));
        t.ev(progress(1, PushStep::Digest(dg('c'))));
        assert_eq!(
            t.s.write().unwrap().progress,
            Progress {
                layers_done: 1,
                bytes: None,
                last_status: "Pushing".into(),
                digest: Some(dg('c')),
            }
        );
    }

    #[test]
    fn done_ok_unlocks_refreshes_and_sets_pending_focus() {
        let mut t = pushing(vec![art('a', &["1.0.0"])], "app", "1.0.1");
        let fx = t.ev(done(1, PushOutcome::Pushed(Some(dg('c')))));
        let w = t.s.write().unwrap();
        assert_eq!(w.outcome, Some(PushOutcome::Pushed(Some(dg('c')))));
        assert!(!t.s.write_running());
        assert_eq!(
            t.s.status(),
            Some((StatusKind::Ok, "pushed h.example/app:1.0.1"))
        );
        let f = fetched(&fx);
        assert!(f.contains(&inv("ecr", "app")), "{f:?}");
        assert!(f.contains(&CacheKey::LocalImages), "{f:?}");
        assert_eq!(
            t.s.pending_focus,
            Some(PendingFocus {
                registry: rid("ecr"),
                repo: rp("app"),
                digest: Some(dg('c')),
                tag: "1.0.1".into(),
            })
        );
    }

    #[test]
    fn done_failed_and_cancelled_also_refresh_local_and_inventory() {
        let e =
            RegistryError::Provider("denied: requested access to the resource is denied".into());
        let want = user_message(&e, &t_view(), "push");
        for (outcome, kind, msg) in [
            (PushOutcome::Failed(e), StatusKind::Error, want.as_str()),
            (
                PushOutcome::Cancelled,
                StatusKind::Info,
                "push cancelled — uploaded layers may remain; the tag is unchanged unless the manifest upload had completed",
            ),
        ] {
            let mut t = pushing(vec![art('a', &["1.0.0"])], "app", "1.0.1");
            let fx = t.ev(done(1, outcome.clone()));
            assert_eq!(t.s.write().unwrap().outcome, Some(outcome));
            assert!(!t.s.write_running());
            assert_eq!(t.s.status(), Some((kind, msg)));
            let f = fetched(&fx);
            assert!(f.contains(&inv("ecr", "app")), "{f:?}");
            assert!(f.contains(&CacheKey::LocalImages), "{f:?}");
            assert_eq!(t.s.pending_focus, None);
        }
    }

    #[test]
    fn push_failure_message_names_p_not_r() {
        use RegistryError as E;
        let v = t_view();
        for (e, want) in [
            (
                E::Network("reset".into()),
                "network error: reset — press p to push again",
            ),
            (
                E::Timeout("x".into()),
                "push to ecr timed out — press p to push again",
            ),
            (
                E::RateLimited,
                "rate limited by ecr — press p to push again",
            ),
            (E::NotFound, "push not found on ecr"),
        ] {
            assert_eq!(push_failure_message(&e, Some(&v)), want);
            assert!(!push_failure_message(&e, Some(&v)).contains("press r"));
        }
        assert_eq!(
            push_failure_message(&E::Unauthorized, None),
            "push failed: unauthorized"
        );
    }

    fn t_view() -> crate::app::state::RegistryView {
        crate::app::update::tests::ecr()
    }

    #[test]
    fn stale_done_ignored() {
        let mut t = pushing(vec![art('a', &["1.0.0"])], "app", "1.0.1");
        let before = (t.s.write().cloned(), t.s.status.clone());
        let fx = t.ev(done(7, PushOutcome::Pushed(None)));
        assert!(fetched(&fx).is_empty());
        assert_eq!((t.s.write().cloned(), t.s.status.clone()), before);
        assert!(t.s.write_running());
        // A second `PushDone` for a finished op changes nothing either.
        t.ev(done(1, PushOutcome::Cancelled));
        let fx = t.ev(done(1, PushOutcome::Pushed(None)));
        assert!(fetched(&fx).is_empty());
        assert_eq!(t.s.write().unwrap().outcome, Some(PushOutcome::Cancelled));
    }

    #[test]
    fn pending_focus_waits_for_matching_result() {
        let old = vec![art('a', &["1.0.0"]), art('b', &["0.9.0"])];
        let mut t = pushing(old.clone(), "app", "1.0.1");
        t.act(Action::PrevPane);
        // An inventory fetch is in flight when the push ends: the refresh only sets `refetch`.
        t.act(Action::Refresh);
        let fx = t.ev(done(1, PushOutcome::Pushed(Some(dg('c')))));
        assert!(!fetched(&fx).contains(&inv("ecr", "app")));

        let fx = t.ok(inv("ecr", "app"), CacheValue::Inventory(old.clone()));
        assert!(fetched(&fx).contains(&inv("ecr", "app")), "refetch issued");
        assert!(t.s.pending_focus.is_some());
        assert_eq!((t.s.focus(), sel(&t)), (Pane::Repos, Some(dg('a'))));

        let mut new = old;
        new.push(art('c', &["1.0.1"]));
        t.ok(inv("ecr", "app"), CacheValue::Inventory(new.clone()));
        assert_eq!((t.s.focus(), sel(&t)), (Pane::Images, Some(dg('c'))));
        assert_eq!(t.s.pending_focus, None);

        // No digest reported: match on the tag.
        let mut t = pushing(vec![art('a', &["1.0.0"])], "app", "1.0.1");
        t.ev(done(1, PushOutcome::Pushed(None)));
        t.ok(inv("ecr", "app"), CacheValue::Inventory(new));
        assert_eq!((t.s.focus(), sel(&t)), (Pane::Images, Some(dg('c'))));
        assert_eq!(t.s.pending_focus, None);
    }

    #[test]
    fn pending_focus_ignores_stale_while_refetching() {
        let mut t = pushing(vec![art('a', &["1.0.0"])], "app", "1.0.1");
        t.act(Action::PrevPane);
        t.act(Action::Refresh);
        // No digest reported: match on the tag. The in-flight (pre-push) result still has the
        // tag on an old image; it is stale while the refetch runs.
        t.ev(done(1, PushOutcome::Pushed(None)));
        t.ok(
            inv("ecr", "app"),
            CacheValue::Inventory(vec![art('a', &["1.0.0"]), art('b', &["1.0.1"])]),
        );
        assert!(t.s.pending_focus.is_some());
        assert_eq!(t.s.focus(), Pane::Repos);

        t.ok(
            inv("ecr", "app"),
            CacheValue::Inventory(vec![art('a', &["1.0.0"]), art('c', &["1.0.1"])]),
        );
        assert_eq!((t.s.focus(), sel(&t)), (Pane::Images, Some(dg('c'))));
        assert_eq!(t.s.pending_focus, None);
    }

    #[test]
    fn pending_focus_elsewhere_clears_without_moving() {
        let mut t = pushing(vec![art('a', &["1.0.0"])], "web", "1.0.1");
        t.ev(done(1, PushOutcome::Pushed(Some(dg('c')))));
        t.ok(
            inv("ecr", "web"),
            CacheValue::Inventory(vec![art('c', &["1.0.1"])]),
        );
        assert_eq!(t.s.pending_focus, None);
        assert_eq!(sel(&t), Some(dg('a')));
    }

    #[test]
    fn pending_focus_clears_filter_that_hides_it() {
        let (mut t, _) = at_images(vec![art('a', &["1.0.0"])]);
        t.act(Action::FilterStart);
        for c in "1.0.0".chars() {
            t.act(Action::FilterChar(c));
        }
        t.act(Action::FilterApply);
        t.s.start_push(rid("ecr"), rp("app"), "img".into(), "2.0.0".into())
            .unwrap();
        t.ev(done(1, PushOutcome::Pushed(Some(dg('c')))));
        t.ok(
            inv("ecr", "app"),
            CacheValue::Inventory(vec![art('a', &["1.0.0"]), art('c', &["2.0.0"])]),
        );
        assert_eq!(t.s.filter(Pane::Images), "");
        assert_eq!(sel(&t), Some(dg('c')));
    }

    #[test]
    fn pending_focus_dropped_on_user_navigation() {
        for a in [Action::Up, Action::Down, Action::NextPane, Action::PrevPane] {
            let mut t = pushing(vec![art('a', &["1.0.0"])], "app", "1.0.1");
            t.ev(done(1, PushOutcome::Pushed(Some(dg('c')))));
            t.act(a);
            assert_eq!(t.s.pending_focus, None, "{a:?}");
        }
        // Registry change (Up/Down in the registries pane).
        let mut t = pushing(vec![art('a', &["1.0.0"])], "app", "1.0.1");
        t.act(Action::PrevPane);
        t.act(Action::PrevPane);
        t.ev(done(1, PushOutcome::Pushed(Some(dg('c')))));
        t.act(Action::Down);
        assert_eq!(t.s.selected_registry().unwrap().id, rid("oci"));
        assert_eq!(t.s.pending_focus, None);
    }

    #[test]
    fn write_events_apply_with_wizard_hidden() {
        let mut t = pushing(vec![], "app", "1.0.1");
        t.ev(progress(1, PushStep::Status("Preparing".into())));
        assert_eq!(t.s.write().unwrap().progress.last_status, "Preparing");
        t.ev(done(1, PushOutcome::Pushed(None)));
        assert_eq!(
            t.s.write().unwrap().outcome,
            Some(PushOutcome::Pushed(None))
        );
    }
}

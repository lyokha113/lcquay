//! The copy / retag wizard (`C`): pick the target registry, repository (a typed name that is
//! not listed is a new repository) and tag, confirm with warnings, then follow the copy. The
//! copy takes the write lock; it runs on while the wizard is hidden.

use time::OffsetDateTime;

use super::cache::Entry;
use super::messages::user_message;
use super::push::{step, valid_tag};
use super::state::{Action, CacheKey, CacheValue, Effect, Pane, RegistryView, State, StatusKind};
use super::transfer::{Transfer, TransferKind, TransferOutcome};
use super::write::{BUSY, OpId};
use crate::domain::{
    ArtifactStatus, Digest, Reference, RegistryError, RegistryId, RepoPath, TagPolicy,
    TransferStep, image_ref,
};

/// Shown for a target repository the registry did not list.
pub const NEW_REPO: &str =
    "not in the repository list — ECR rejects unknown repositories; registry:2 / Harbor create it";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyStage {
    PickRegistry,
    PickRepo,
    Tag,
    Confirm,
    /// The running copy, then its result.
    Progress,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CopyWizard {
    pub stage: CopyStage,
    pub src_registry: RegistryId,
    pub src_repo: RepoPath,
    /// The selected (top-level) manifest.
    pub digest: Digest,
    /// Target registry (highlighted in `PickRegistry`).
    pub registry: RegistryId,
    /// Repo filter / new repo name (`PickRepo`).
    pub filter: String,
    /// Highlighted repo in `PickRepo`; the chosen one after it.
    pub repo: Option<RepoPath>,
    pub tag: String,
    pub op: Option<OpId>,
    pub transfer: Option<Transfer>,
    pub outcome: Option<TransferOutcome>,
    /// Running on with the wizard closed (Enter); `C` shows it again.
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CopyWarning {
    /// The tag is on this other image now.
    TagWillMove(Digest),
    TagWillBeRejected,
    /// The target inventory failed: whether the tag exists is unknown.
    TagExistenceUnknown,
    TagExistsPolicyUnknown,
    NewRepo,
    /// The target inventory or the tag policy is loading.
    CheckingTag,
}

impl CopyWizard {
    fn view<'s>(&self, s: &'s State) -> Option<&'s RegistryView> {
        s.registries.iter().find(|r| r.id == self.registry)
    }

    fn listed<'s>(&self, s: &'s State) -> Option<impl Iterator<Item = &'s RepoPath>> {
        match s.value(&CacheKey::Repos(self.registry.clone()))? {
            CacheValue::Repos(all) => Some(all.iter().map(|r| &r.path)),
            _ => None,
        }
    }

    /// Repos of the target registry containing the filter, by name.
    pub fn repos<'a>(&self, s: &'a State) -> Vec<&'a RepoPath> {
        let mut v: Vec<_> = self
            .listed(s)
            .into_iter()
            .flatten()
            .filter(|r| r.0.contains(&self.filter))
            .collect();
        v.sort();
        v
    }

    /// In `PickRepo` the highlighted match (the first when the highlight is filtered out), else
    /// the typed name; later the chosen repo.
    pub fn target_repo(&self, s: &State) -> Option<RepoPath> {
        if self.stage != CopyStage::PickRepo {
            return self.repo.clone();
        }
        let v = self.repos(s);
        match v
            .iter()
            .find(|r| Some(**r) == self.repo.as_ref())
            .or(v.first())
        {
            Some(r) => Some((*r).clone()),
            None if !self.filter.is_empty() => Some(RepoPath(self.filter.clone())),
            None => None,
        }
    }

    /// The target repo is missing from the registry's (loaded) repository list.
    pub fn new_repo(&self, s: &State) -> bool {
        match (self.listed(s), self.target_repo(s)) {
            (Some(mut all), Some(t)) => !all.any(|r| *r == t),
            _ => false,
        }
    }

    /// `Copy → <ref_host>/<repo|?>:<tag|?>`.
    pub fn title(&self, s: &State) -> String {
        format!(
            "Copy → {}/{}:{}",
            self.view(s).map_or("?", |r| r.ref_host.as_str()),
            self.target_repo(s).map_or_else(|| "?".into(), |r| r.0),
            if self.tag.is_empty() { "?" } else { &self.tag },
        )
    }

    /// `<ref_host>/<repo>:<tag>` of the target.
    pub fn target(&self, s: &State) -> Option<String> {
        let tag = Reference::Tag(self.tag.clone());
        Some(image_ref(
            &self.view(s)?.ref_host,
            &self.target_repo(s)?,
            &tag,
        ))
    }

    /// `<ref_host>/<repo>@<algorithm>:<short digest>…` of the source.
    pub fn source(&self, s: &State) -> String {
        let host = s.registries.iter().find(|r| r.id == self.src_registry);
        format!(
            "{}/{}@{}:{}…",
            host.map_or("?", |r| r.ref_host.as_str()),
            self.src_repo,
            self.digest.algorithm(),
            self.digest.short()
        )
    }

    /// Whether `key` is one the wizard loads (repos, target inventory, tag policy).
    pub(super) fn loads(&self, key: &CacheKey) -> bool {
        match key {
            CacheKey::Repos(r) => *r == self.registry,
            CacheKey::Inventory(r, p) | CacheKey::TagPolicy(r, p, _) => {
                *r == self.registry && Some(p) == self.repo.as_ref()
            }
            _ => false,
        }
    }

    fn inventory_key(&self) -> Option<CacheKey> {
        Some(CacheKey::Inventory(
            self.registry.clone(),
            self.repo.clone()?,
        ))
    }

    fn policy_key(&self) -> Option<CacheKey> {
        Some(CacheKey::TagPolicy(
            self.registry.clone(),
            self.repo.clone()?,
            self.tag.clone(),
        ))
    }

    /// The digest the target tag is on: `None` until the inventory is `Ready`, `Some(None)`
    /// when the tag is absent.
    fn tagged<'s>(&self, s: &'s State) -> Option<Option<&'s Digest>> {
        match s.cache.get(&self.inventory_key()?)? {
            Entry::Ready {
                value: CacheValue::Inventory(v),
                ..
            } => Some(
                v.iter()
                    .find(|a| a.tags.contains(&self.tag))
                    .map(|a| &a.digest),
            ),
            _ => None,
        }
    }

    /// The tag is on another image (so its policy matters).
    fn moves_tag(&self, s: &State) -> bool {
        matches!(self.tagged(s), Some(Some(d)) if *d != self.digest)
    }

    /// Confirm warnings from the live cache state (a refetch in flight counts as checking). A
    /// new repo has no inventory to check; a failed inventory leaves the tag unknown.
    pub fn warnings(&self, s: &State) -> Vec<CopyWarning> {
        if self.new_repo(s) {
            return vec![CopyWarning::NewRepo];
        }
        let (Some(inv), Some(policy)) = (self.inventory_key(), self.policy_key()) else {
            return vec![];
        };
        let w = match (s.cache.get(&inv), self.tagged(s)) {
            (None | Some(Entry::Loading { .. }), _) => CopyWarning::CheckingTag,
            (Some(Entry::Failed { .. }), _) => CopyWarning::TagExistenceUnknown,
            (_, Some(Some(d))) if *d != self.digest => match s.cache.get(&policy) {
                None | Some(Entry::Loading { .. }) => CopyWarning::CheckingTag,
                Some(Entry::Ready {
                    value: CacheValue::TagPolicy(TagPolicy::Mutable),
                    ..
                }) => CopyWarning::TagWillMove(d.clone()),
                Some(Entry::Ready {
                    value: CacheValue::TagPolicy(TagPolicy::Immutable),
                    ..
                }) => CopyWarning::TagWillBeRejected,
                Some(_) => CopyWarning::TagExistsPolicyUnknown,
            },
            _ => return vec![],
        };
        vec![w]
    }
}

/// The OCI distribution name grammar: `/`-separated components, each
/// `[a-z0-9]+(?:(?:[._]|__|[-]*)[a-z0-9]+)*`. No `.`/`..` segments, which `Url::join` resolves.
fn valid_repo(r: &str) -> bool {
    let alnum = |ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit();
    r.split('/').all(|c| {
        c.starts_with(alnum)
            && c.ends_with(alnum)
            && c.split(alnum)
                .all(|sep| matches!(sep, "." | "_" | "__") || sep.chars().all(|ch| ch == '-'))
    })
}

/// A failed copy: the registry's message without a `<id>:` prefix or a `press r` hint (`r`
/// refreshes browsing; it does not retry a copy).
pub fn copy_failure_message(err: &RegistryError, reg: Option<&RegistryView>) -> String {
    let m = match (err, reg) {
        (RegistryError::Provider(m), _) => m.clone(),
        (_, Some(r)) => {
            let m = user_message(err, r, "copy");
            m.split_once(" — press r")
                .map_or_else(|| m.clone(), |(m, _)| m.to_owned())
        }
        (_, None) => err.to_string(),
    };
    format!("copy failed: {m}")
}

impl State {
    pub fn copy_wizard(&self) -> Option<&CopyWizard> {
        self.copy.as_ref()
    }

    /// A copy holds the write lock from Confirm until its outcome.
    pub(super) fn copy_running(&self) -> bool {
        self.copy
            .as_ref()
            .is_some_and(|w| w.op.is_some() && w.outcome.is_none())
    }

    /// `C`: show a hidden copy, else open the wizard for the selected Active image on the
    /// Images pane, targeting its own registry, repo and first tag.
    pub(super) fn copy_start(&mut self) -> Vec<Effect> {
        if let Some(w) = self.copy.as_mut().filter(|w| w.hidden) {
            w.hidden = false;
            return vec![];
        }
        if self.focus != Pane::Images {
            return vec![];
        }
        let (Some(reg), Some(repo), Some(img)) = (
            self.selected_registry(),
            self.selected_repo(),
            self.selected_image(),
        ) else {
            return vec![];
        };
        if img.status != ArtifactStatus::Active {
            self.set_status(StatusKind::Error, "archived images cannot be copied".into());
            return vec![];
        }
        self.copy = Some(CopyWizard {
            stage: CopyStage::PickRegistry,
            src_registry: reg.id.clone(),
            src_repo: repo.path.clone(),
            digest: img.digest.clone(),
            registry: reg.id.clone(),
            filter: String::new(),
            repo: Some(repo.path.clone()),
            tag: img.tags.first().cloned().unwrap_or_default(),
            op: None,
            transfer: None,
            outcome: None,
            hidden: false,
        });
        vec![]
    }

    /// An action while the wizard is shown (all but `Quit` / `ForceQuit` / `ClampScroll`).
    pub(super) fn copy_action(&mut self, a: Action, now: OffsetDateTime) -> Vec<Effect> {
        let Some(mut w) = self.copy.take() else {
            return vec![];
        };
        let mut fx = vec![];
        let back = matches!(a, Action::Back | Action::FilterCancel);
        let enter = matches!(a, Action::Enter | Action::FilterApply);
        let (up, down) = (a == Action::Up, a == Action::Down);
        let open = match w.stage {
            CopyStage::PickRegistry => {
                if up || down {
                    let ids: Vec<RegistryId> =
                        self.registries.iter().map(|r| r.id.clone()).collect();
                    if let Some(id) = step(&ids, Some(&w.registry), down) {
                        w.registry = id;
                    }
                } else if enter {
                    w.stage = CopyStage::PickRepo;
                    fx.extend(self.load(CacheKey::Repos(w.registry.clone())));
                }
                !back
            }
            CopyStage::PickRepo => {
                match a {
                    Action::FilterChar(c) => w.filter.push(c),
                    Action::FilterBackspace => {
                        w.filter.pop();
                    }
                    _ if up || down => {
                        let repos: Vec<RepoPath> = w.repos(self).into_iter().cloned().collect();
                        let cur = w.target_repo(self);
                        if let Some(r) = step(&repos, cur.as_ref(), down) {
                            w.repo = Some(r);
                        }
                    }
                    _ if enter => match w.target_repo(self) {
                        Some(r) if !valid_repo(&r.0) => {
                            self.set_status(StatusKind::Error, "invalid repository name".into());
                        }
                        Some(r) => {
                            w.repo = Some(r);
                            w.stage = CopyStage::Tag;
                            // Never trust a cached inventory: a stale one may hide the tag.
                            if !w.new_repo(self) {
                                fx.extend(w.inventory_key().and_then(|k| self.cache.refresh(k)));
                            }
                        }
                        // Nothing to pick: retry a failed list.
                        None => fx.extend(self.load(CacheKey::Repos(w.registry.clone()))),
                    },
                    _ if back => w.stage = CopyStage::PickRegistry,
                    _ => {}
                }
                true
            }
            CopyStage::Tag => {
                match a {
                    Action::FilterChar(c) => w.tag.push(c),
                    Action::FilterBackspace => {
                        w.tag.pop();
                    }
                    _ if enter && w.tag.is_empty() => {}
                    _ if enter && !valid_tag(&w.tag) => {
                        self.set_status(StatusKind::Error, "invalid tag".into());
                    }
                    _ if enter => {
                        w.stage = CopyStage::Confirm;
                        fx.extend(self.copy_policy(&w));
                    }
                    _ if back => w.stage = CopyStage::PickRepo,
                    _ => {}
                }
                true
            }
            CopyStage::Confirm => {
                if back {
                    w.stage = CopyStage::Tag;
                } else if enter {
                    fx.extend(self.copy_confirm(&mut w, now));
                }
                true
            }
            CopyStage::Progress => match (&w.outcome, w.op) {
                (None, Some(op)) if back => {
                    fx.push(Effect::Cancel(op));
                    true
                }
                (None, _) if enter => {
                    let target = w.target(self).unwrap_or_default();
                    let msg = format!("copying {target}… — press C to show");
                    self.set_status(StatusKind::Info, msg);
                    w.hidden = true;
                    true
                }
                _ => !(back || enter),
            },
        };
        if open {
            self.copy = Some(w);
        }
        fx
    }

    /// Enter on Confirm: starts the copy unless a check is pending, the tag is already on this
    /// image, or another write holds the lock.
    fn copy_confirm(&mut self, w: &mut CopyWizard, now: OffsetDateTime) -> Option<Effect> {
        let target = w.target(self).unwrap_or_default();
        let refusal = if w.warnings(self).contains(&CopyWarning::CheckingTag) {
            Some((
                StatusKind::Info,
                "still checking the target tag…".to_owned(),
            ))
        } else if w.tagged(self) == Some(Some(&w.digest)) {
            let m = format!("nothing to copy: {target} already points to this image");
            Some((StatusKind::Error, m))
        } else if self.write_running() {
            Some((StatusKind::Error, BUSY.to_owned()))
        } else {
            None
        };
        if let Some((kind, m)) = refusal {
            self.set_status(kind, m);
            return None;
        }
        let dst_repo = w.repo.clone()?;
        self.next_op += 1;
        let op = OpId(self.next_op);
        w.op = Some(op);
        w.transfer = Some(Transfer::new(TransferKind::Copy, vec![], now));
        w.stage = CopyStage::Progress;
        Some(Effect::CopyImage {
            op,
            src: w.src_registry.clone(),
            src_repo: w.src_repo.clone(),
            digest: w.digest.clone(),
            dst: w.registry.clone(),
            dst_repo,
            tag: w.tag.clone(),
        })
    }

    /// Never trust a cached policy: the registry settings may have changed. Only a tag on
    /// another image needs it.
    fn copy_policy(&mut self, w: &CopyWizard) -> Option<Effect> {
        if !w.moves_tag(self) {
            return None;
        }
        self.cache.refresh(w.policy_key()?)
    }

    /// After an applied `Loaded`: the target inventory arriving on Confirm fetches the policy
    /// of a tag that would move.
    pub(super) fn copy_loaded(&mut self, key: &CacheKey) -> Option<Effect> {
        let w = self.copy.clone()?;
        (w.stage == CopyStage::Confirm && w.inventory_key().as_ref() == Some(key))
            .then(|| self.copy_policy(&w))
            .flatten()
    }

    fn running_copy(&mut self, op: OpId) -> Option<&mut CopyWizard> {
        self.copy
            .as_mut()
            .filter(|w| w.op == Some(op) && w.outcome.is_none())
    }

    pub(super) fn copy_progress(&mut self, op: OpId, step: TransferStep, now: OffsetDateTime) {
        if let Some(t) = self.running_copy(op).and_then(|w| w.transfer.as_mut()) {
            t.apply(step, now);
        }
    }

    /// Unlocks and shows the outcome (in the wizard, or on the status line when hidden,
    /// dropping the wizard). Every outcome refreshes the target inventory (a copy may have
    /// changed it before failing), and the repo list when the repo was new.
    pub(super) fn copy_done(
        &mut self,
        op: OpId,
        outcome: TransferOutcome,
        now: OffsetDateTime,
    ) -> Vec<Effect> {
        let Some(w) = self.running_copy(op) else {
            return vec![];
        };
        if let Some(t) = &mut w.transfer {
            t.finished = Some(now);
        }
        w.outcome = Some(outcome);
        let w = w.clone();
        if w.hidden {
            self.status = self.copy_result().map(|(k, m)| (None, k, m));
            self.copy = None;
        }
        let mut keys: Vec<CacheKey> = w.inventory_key().into_iter().collect();
        if w.new_repo(self) {
            keys.push(CacheKey::Repos(w.registry.clone()));
        }
        keys.into_iter()
            .filter_map(|k| self.cache.invalidate(k))
            .collect()
    }

    /// The finished copy's result line.
    pub fn copy_result(&self) -> Option<(StatusKind, String)> {
        let w = self.copy.as_ref()?;
        let target = w.target(self).unwrap_or_default();
        let check = format!(" — check {target}");
        Some(match w.outcome.as_ref()? {
            TransferOutcome::Done => {
                let mut s = format!("copied to {target}");
                s += &w
                    .transfer
                    .as_ref()
                    .map(Transfer::summary)
                    .unwrap_or_default();
                (StatusKind::Ok, s)
            }
            TransferOutcome::Cancelled => (
                StatusKind::Info,
                "copy cancelled — uploaded blobs may remain; the tag is unchanged".into(),
            ),
            TransferOutcome::Failed(e) => {
                (StatusKind::Error, copy_failure_message(e, w.view(self)))
            }
            // The runtime's cancel message already names the reference.
            TransferOutcome::Uncertain(m) => (
                StatusKind::Error,
                format!(
                    "copy outcome uncertain: {}{check}",
                    m.strip_suffix(&check).unwrap_or(m)
                ),
            ),
            TransferOutcome::DigestDiffers { got, want } => (
                StatusKind::Error,
                format!(
                    "copied but digest differs ({} vs {})",
                    got.short(),
                    want.short()
                ),
            ),
            TransferOutcome::Exists => (
                StatusKind::Info,
                format!("nothing to copy: {target} already points to this image"),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;
    use crate::app::delete::DelStage;
    use crate::app::state::{Event, KeyMode};
    use crate::app::update::QUIT_HINT;
    use crate::app::update::tests::{T, art, at_images, dg, ecr, fetched, inv, rid, rp};
    use crate::app::write::{BUSY, OpId};
    use crate::domain::{
        ArtifactStatus, ArtifactSummary, DepSnapshot, ItemStep, RegistryError, TagPolicy,
        TransferStep,
    };

    const NOW: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

    /// Images pane of ecr/app (`h.example`): `a` = `1.0`,`latest` (selected), `b` = `2.0`.
    fn on_a() -> T {
        at_images(vec![art('a', &["1.0", "latest"]), art('b', &["2.0"])]).0
    }

    fn wiz(t: &T) -> &CopyWizard {
        t.s.copy_wizard().expect("copy wizard")
    }

    fn stage(t: &T) -> Option<CopyStage> {
        t.s.copy_wizard().map(|w| w.stage)
    }

    fn typ(t: &mut T, s: &str) {
        for c in s.chars() {
            t.act(Action::FilterChar(c));
        }
    }

    fn warnings(t: &T) -> Vec<CopyWarning> {
        wiz(t).warnings(&t.s)
    }

    fn copies(fx: &[Effect]) -> Vec<&Effect> {
        fx.iter()
            .filter(|e| matches!(e, Effect::CopyImage { .. }))
            .collect()
    }

    fn policy(repo: &str, tag: &str) -> CacheKey {
        CacheKey::TagPolicy(rid("ecr"), rp(repo), tag.into())
    }

    /// `C` on `a`, registry ecr, repo `repo` (picked by filter), tag `tag`; the target inventory
    /// refreshed on entering `Tag` answered `images`. Returns the effects of entering `Confirm`.
    fn at_confirm(images: Vec<ArtifactSummary>, repo: &str, tag: &str) -> (T, Vec<Effect>) {
        let mut t = on_a();
        t.act(Action::CopyStart);
        t.act(Action::Enter);
        typ(&mut t, repo);
        let fx = t.act(Action::FilterApply);
        assert_eq!(stage(&t), Some(CopyStage::Tag));
        assert!(fetched(&fx).contains(&inv("ecr", repo)), "{fx:?}");
        for _ in 0..wiz(&t).tag.len() {
            t.act(Action::FilterBackspace);
        }
        typ(&mut t, tag);
        t.ok(inv("ecr", repo), CacheValue::Inventory(images));
        let fx = t.act(Action::FilterApply);
        assert_eq!(stage(&t), Some(CopyStage::Confirm));
        (t, fx)
    }

    /// `at_confirm` to ecr/web:1.0 (absent), started as op 1.
    fn copying() -> T {
        let (mut t, _) = at_confirm(vec![], "web", "1.0");
        assert_eq!(copies(&t.act(Action::Enter)).len(), 1);
        t
    }

    fn done(op: u64, outcome: TransferOutcome) -> Event {
        Event::CopyDone {
            op: OpId(op),
            outcome,
        }
    }

    fn blob(c: char, step: ItemStep) -> Event {
        Event::CopyProgress {
            op: OpId(1),
            step: TransferStep {
                id: c.to_string().repeat(12),
                step,
            },
        }
    }

    #[test]
    fn copy_opens_with_defaults() {
        let mut t = on_a();
        assert!(copies(&t.act(Action::CopyStart)).is_empty());
        let w = wiz(&t);
        assert_eq!(
            (
                w.stage,
                &w.registry,
                &w.src_registry,
                &w.src_repo,
                &w.digest,
                w.tag.as_str()
            ),
            (
                CopyStage::PickRegistry,
                &rid("ecr"),
                &rid("ecr"),
                &rp("app"),
                &dg('a'),
                "1.0"
            )
        );
        assert_eq!(t.s.key_mode(), KeyMode::List);
        assert_eq!(wiz(&t).title(&t.s), "Copy → h.example/app:1.0");
        let fx = t.act(Action::Enter);
        assert!(fetched(&fx).is_empty(), "repos already loaded: {fx:?}");
        assert_eq!(stage(&t), Some(CopyStage::PickRepo));
        assert_eq!(t.s.key_mode(), KeyMode::Text);
        assert_eq!(wiz(&t).repo, Some(rp("app")));
        assert_eq!(wiz(&t).repos(&t.s), [&rp("app"), &rp("web")]);
        t.act(Action::Down);
        assert_eq!(wiz(&t).title(&t.s), "Copy → h.example/web:1.0");

        // ↑↓ pick the registry; Enter loads its repos; Esc walks back and closes.
        let mut t = on_a();
        t.act(Action::CopyStart);
        t.act(Action::Down);
        assert_eq!(wiz(&t).registry, rid("oci"));
        let fx = t.act(Action::Enter);
        assert_eq!(fetched(&fx), [CacheKey::Repos(rid("oci"))]);
        t.act(Action::FilterCancel);
        assert_eq!(stage(&t), Some(CopyStage::PickRegistry));
        t.act(Action::Back);
        assert_eq!(t.s.copy_wizard(), None);
        assert_eq!(t.s.key_mode(), KeyMode::Normal);

        // Only an Active image on the Images pane.
        let mut old = art('c', &["old"]);
        old.status = ArtifactStatus::Archived;
        let (mut t, _) = at_images(vec![old]);
        t.act(Action::CopyStart);
        assert_eq!(t.s.copy_wizard(), None);
        assert_eq!(
            t.s.status(),
            Some((StatusKind::Error, "archived images cannot be copied"))
        );
        let mut t = on_a();
        t.act(Action::PrevPane);
        t.act(Action::CopyStart);
        assert_eq!(t.s.copy_wizard(), None);
    }

    #[test]
    fn copy_repo_filter_and_new_name() {
        let mut t = on_a();
        t.act(Action::CopyStart);
        t.act(Action::Enter);
        typ(&mut t, "we");
        assert_eq!(wiz(&t).repos(&t.s), [&rp("web")]);
        assert!(!wiz(&t).new_repo(&t.s));
        assert_eq!(warnings(&t), []);
        t.act(Action::FilterBackspace);
        t.act(Action::FilterBackspace);

        typ(&mut t, "new/x");
        assert!(wiz(&t).repos(&t.s).is_empty());
        assert!(wiz(&t).new_repo(&t.s));
        assert_eq!(warnings(&t), [CopyWarning::NewRepo]);
        assert_eq!(wiz(&t).title(&t.s), "Copy → h.example/new/x:1.0");
        assert_eq!(
            NEW_REPO,
            "not in the repository list — ECR rejects unknown repositories; registry:2 / Harbor create it"
        );
        let fx = t.act(Action::FilterApply);
        assert_eq!(stage(&t), Some(CopyStage::Tag));
        assert_eq!(wiz(&t).repo, Some(rp("new/x")));
        assert!(
            fetched(&fx).is_empty(),
            "a new repo has no inventory: {fx:?}"
        );
        t.act(Action::FilterApply);
        assert_eq!(stage(&t), Some(CopyStage::Confirm));
        assert_eq!(warnings(&t), [CopyWarning::NewRepo]);
        let fx = t.act(Action::Enter);
        assert_eq!(
            copies(&fx),
            [&Effect::CopyImage {
                op: OpId(1),
                src: rid("ecr"),
                src_repo: rp("app"),
                digest: dg('a'),
                dst: rid("ecr"),
                dst_repo: rp("new/x"),
                tag: "1.0".into(),
            }]
        );

        // A name the registry API cannot carry stays in the picker.
        let mut t = on_a();
        t.act(Action::CopyStart);
        t.act(Action::Enter);
        typ(&mut t, "Bad Name");
        t.act(Action::FilterApply);
        assert_eq!(stage(&t), Some(CopyStage::PickRepo));
        assert_eq!(
            t.s.status(),
            Some((StatusKind::Error, "invalid repository name"))
        );
    }

    #[test]
    fn repo_name_follows_oci_grammar() {
        for bad in [
            "a/../b", ".", "..", "a..b", "-a", "a-", "", "a//b", "/a", "a/", "A", "a b", "_a",
            "a.", "a___b", "a._b", "a-.b",
        ] {
            assert!(!valid_repo(bad), "{bad:?} accepted");
        }
        for good in [
            "eng/api/fo",
            "a_b",
            "a__b",
            "a--b",
            "a---b",
            "a.b",
            "x9.y-z",
        ] {
            assert!(valid_repo(good), "{good:?} rejected");
        }
    }

    #[test]
    fn copy_preflight() {
        // The tag is on another digest: the policy decides.
        for (p, want) in [
            (TagPolicy::Mutable, CopyWarning::TagWillMove(dg('b'))),
            (TagPolicy::Immutable, CopyWarning::TagWillBeRejected),
            (TagPolicy::Unknown, CopyWarning::TagExistsPolicyUnknown),
        ] {
            let (mut t, fx) = at_confirm(vec![art('b', &["2.0"])], "web", "2.0");
            assert!(fetched(&fx).contains(&policy("web", "2.0")), "{fx:?}");
            assert_eq!(warnings(&t), [CopyWarning::CheckingTag]);
            t.ok(policy("web", "2.0"), CacheValue::TagPolicy(p));
            assert_eq!(warnings(&t), [want]);
            // Advisory: the registry's answer wins.
            assert_eq!(copies(&t.act(Action::Enter)).len(), 1);
        }

        // Absent tag: no policy fetch, no warning.
        let (t, fx) = at_confirm(vec![art('b', &["2.0"])], "web", "3.0");
        assert!(!fetched(&fx).contains(&policy("web", "3.0")), "{fx:?}");
        assert_eq!(warnings(&t), []);

        // The inventory arrives on Confirm.
        let mut t = on_a();
        t.act(Action::CopyStart);
        t.act(Action::Enter);
        t.act(Action::Down);
        t.act(Action::FilterApply);
        t.act(Action::FilterApply);
        assert_eq!(warnings(&t), [CopyWarning::CheckingTag]);
        assert!(copies(&t.act(Action::Enter)).is_empty());
        assert_eq!(
            t.s.status(),
            Some((StatusKind::Info, "still checking the target tag…"))
        );
        let fx = t.ok(
            inv("ecr", "web"),
            CacheValue::Inventory(vec![art('b', &["1.0"])]),
        );
        assert!(fetched(&fx).contains(&policy("web", "1.0")), "{fx:?}");

        // Same registry and repo, the tag already on this image: nothing to copy.
        let (mut t, _) = at_confirm(vec![art('a', &["1.0", "latest"])], "app", "1.0");
        assert_eq!(warnings(&t), []);
        assert!(copies(&t.act(Action::Enter)).is_empty());
        assert_eq!(stage(&t), Some(CopyStage::Confirm));
        assert_eq!(
            t.s.status(),
            Some((
                StatusKind::Error,
                "nothing to copy: h.example/app:1.0 already points to this image"
            ))
        );

        // A new tag in the same repo is a retag.
        let (mut t, _) = at_confirm(vec![art('a', &["1.0", "latest"])], "app", "1.1");
        let fx = t.act(Action::Enter);
        assert!(
            matches!(copies(&fx)[..], [Effect::CopyImage { src, dst, src_repo, dst_repo, .. }]
                if src == dst && src_repo == dst_repo),
            "{fx:?}"
        );
        assert_eq!(stage(&t), Some(CopyStage::Progress));
    }

    #[test]
    fn copy_takes_write_lock() {
        // A push runs: Confirm refuses.
        let (mut t, _) = at_confirm(vec![], "web", "1.0");
        t.s.start_push(rid("ecr"), rp("web"), "img".into(), "x".into())
            .unwrap();
        assert!(copies(&t.act(Action::Enter)).is_empty());
        assert_eq!(stage(&t), Some(CopyStage::Confirm));
        assert_eq!(t.s.status(), Some((StatusKind::Error, BUSY)));

        // A copy runs: push and delete refuse, `q` asks again, `e` refuses.
        let mut t = copying();
        assert!(t.s.write_running() && t.s.transfer_running());
        assert_eq!(
            t.s.start_push(rid("ecr"), rp("web"), "img".into(), "x".into()),
            Err(BUSY.into())
        );
        t.act(Action::Enter);
        assert_eq!(t.s.key_mode(), KeyMode::Normal, "hidden");
        t.act(Action::Mark);
        t.act(Action::DeleteStart);
        assert_eq!(t.s.delete_modal(), None);
        assert_eq!(t.s.status(), Some((StatusKind::Error, BUSY)));
        assert_eq!(t.act(Action::EditConfig), vec![]);
        assert_eq!(t.s.status(), Some((StatusKind::Error, BUSY)));
        assert_eq!(t.act(Action::Quit), vec![]);
        assert_eq!(t.s.status(), Some((StatusKind::Info, QUIT_HINT)));
        assert_eq!(t.act(Action::Quit), vec![Effect::Quit]);

        // The typed delete confirm refuses once a copy runs.
        let mut t = on_a();
        t.act(Action::Mark);
        t.act(Action::DeleteStart);
        let op = t.s.delete_modal().unwrap().op;
        t.ev(Event::DeleteSnapshot {
            op,
            result: Ok(DepSnapshot {
                inventory: vec![art('a', &["1.0", "latest"]), art('b', &["2.0"])],
                ..DepSnapshot::default()
            }),
        });
        t.act(Action::Enter);
        typ(&mut t, "app");
        let mut w = copying().s.copy.unwrap();
        w.hidden = true;
        t.s.copy = Some(w);
        let fx = t.act(Action::FilterApply);
        assert!(
            !fx.iter()
                .any(|e| matches!(e, Effect::DeleteSnapshot { .. }))
        );
        assert_eq!(t.s.delete_modal().unwrap().stage, DelStage::Confirm);
        assert_eq!(t.s.status(), Some((StatusKind::Error, BUSY)));
    }

    #[test]
    fn copy_progress_hide_cancel() {
        let mut t = copying();
        assert_eq!(stage(&t), Some(CopyStage::Progress));
        assert_eq!(t.s.key_mode(), KeyMode::List);
        assert_eq!(t.act(Action::Back), [Effect::Cancel(OpId(1))]);
        assert_eq!(
            stage(&t),
            Some(CopyStage::Progress),
            "open until the op ends"
        );

        t.act(Action::Enter);
        assert!(wiz(&t).hidden);
        assert_eq!(t.s.key_mode(), KeyMode::Normal);
        assert_eq!(
            t.s.status(),
            Some((
                StatusKind::Info,
                "copying h.example/web:1.0… — press C to show"
            ))
        );
        t.act(Action::CopyStart);
        assert!(!wiz(&t).hidden);
        assert_eq!(stage(&t), Some(CopyStage::Progress));

        // Events of another op are stale.
        t.ev(Event::CopyProgress {
            op: OpId(9),
            step: TransferStep {
                id: "x".into(),
                step: ItemStep::Done,
            },
        });
        t.ev(done(9, TransferOutcome::Done));
        let w = wiz(&t);
        assert_eq!(
            (w.transfer.as_ref().unwrap().items.len(), &w.outcome),
            (0, &None)
        );

        t.ev(blob('1', ItemStep::Waiting { total: Some(10) }));
        assert_eq!(wiz(&t).transfer.as_ref().unwrap().items.len(), 1);
        t.ev(done(1, TransferOutcome::Cancelled));
        assert_eq!(wiz(&t).outcome, Some(TransferOutcome::Cancelled));
        assert!(!t.s.write_running() && !t.s.transfer_running());
        assert!(!t.act(Action::Back).contains(&Effect::Cancel(OpId(1))));
        assert_eq!(t.s.copy_wizard(), None);

        // Done while hidden: the wizard goes, the status line has the result.
        let mut t = copying();
        t.act(Action::Enter);
        t.ev(done(1, TransferOutcome::Uncertain("x".into())));
        assert_eq!(t.s.copy_wizard(), None);
        assert_eq!(
            t.s.status(),
            Some((
                StatusKind::Error,
                "copy outcome uncertain: x — check h.example/web:1.0"
            ))
        );
    }

    #[test]
    fn copy_done_invalidates_destination() {
        for outcome in [
            TransferOutcome::Done,
            TransferOutcome::Uncertain("cancelled after the manifest upload started".into()),
            TransferOutcome::DigestDiffers {
                got: dg('c'),
                want: dg('a'),
            },
        ] {
            let mut t = copying();
            let fx = t.ev(done(1, outcome.clone()));
            assert!(fetched(&fx).contains(&inv("ecr", "web")), "{outcome:?}");
            assert!(!t.s.write_running());
        }

        let at = |s: f64| NOW + time::Duration::seconds_f64(s);
        let mut t = copying();
        t.ev(blob(
            '1',
            ItemStep::Waiting {
                total: Some(12_600_000),
            },
        ));
        t.ev(blob(
            '2',
            ItemStep::Waiting {
                total: Some(300_000),
            },
        ));
        t.ev_at(blob('2', ItemStep::Exists), at(0.1));
        t.ev_at(
            blob(
                '1',
                ItemStep::Bytes {
                    current: 6_000_000,
                    total: 12_600_000,
                },
            ),
            at(2.0),
        );
        t.ev_at(blob('1', ItemStep::Done), at(4.0));
        t.ev_at(done(1, TransferOutcome::Done), at(4.2));
        assert_eq!(
            t.s.copy_result(),
            Some((
                StatusKind::Ok,
                "copied to h.example/web:1.0 · 12.9MB in 4.2s (avg 3.0 MB/s)".into()
            ))
        );

        let texts = [
            (
                TransferOutcome::Uncertain(
                    "cancelled after the manifest upload started — check h.example/web:1.0".into(),
                ),
                "copy outcome uncertain: cancelled after the manifest upload started — check h.example/web:1.0",
            ),
            (
                TransferOutcome::DigestDiffers {
                    got: dg('c'),
                    want: dg('a'),
                },
                "copied but digest differs (cccccccccccc vs aaaaaaaaaaaa)",
            ),
        ];
        for (o, want) in texts {
            let mut t = copying();
            t.ev(done(1, o));
            assert_eq!(t.s.copy_result(), Some((StatusKind::Error, want.into())));
        }
        // A retag moves no blobs.
        let mut t = copying();
        t.ev_at(done(1, TransferOutcome::Done), at(0.5));
        assert_eq!(
            t.s.copy_result(),
            Some((StatusKind::Ok, "copied to h.example/web:1.0 in 0.5s".into()))
        );
    }

    #[test]
    fn copy_new_repo_refreshes_repo_list() {
        let mut t = on_a();
        t.act(Action::CopyStart);
        t.act(Action::Enter);
        typ(&mut t, "new/x");
        t.act(Action::FilterApply);
        t.act(Action::FilterApply);
        t.act(Action::Enter);
        let fx = t.ev(done(1, TransferOutcome::Done));
        let keys = fetched(&fx);
        assert!(keys.contains(&CacheKey::Repos(rid("ecr"))), "{keys:?}");
        assert!(keys.contains(&inv("ecr", "new/x")), "{keys:?}");
    }

    #[test]
    fn copy_failure_text() {
        let reg = ecr();
        let m = |e: RegistryError| copy_failure_message(&e, Some(&reg));
        assert_eq!(
            m(RegistryError::Provider("manifest blob unknown".into())),
            "copy failed: manifest blob unknown"
        );
        let net = m(RegistryError::Network("down".into()));
        assert!(
            net.starts_with("copy failed: ") && !net.contains("press r"),
            "{net}"
        );

        let mut t = copying();
        t.ev(done(
            1,
            TransferOutcome::Failed(RegistryError::Provider("manifest blob unknown".into())),
        ));
        assert_eq!(
            t.s.copy_result(),
            Some((
                StatusKind::Error,
                "copy failed: manifest blob unknown".into()
            ))
        );
    }

    #[test]
    fn copy_load_failure_does_not_name_r() {
        let mut t = on_a();
        t.act(Action::CopyStart);
        t.act(Action::Enter);
        t.act(Action::Down);
        t.act(Action::FilterApply);
        t.load(
            inv("ecr", "web"),
            Err(RegistryError::Network("down".into())),
        );
        let (_, m) = t.s.status().unwrap();
        assert!(!m.contains("press r"), "{m}");
        // Unknown tag existence: warned (as push does), but advisory: Enter copies.
        t.act(Action::FilterApply);
        assert_eq!(warnings(&t), [CopyWarning::TagExistenceUnknown]);
        assert_eq!(copies(&t.act(Action::Enter)).len(), 1);
    }
}

//! Delete marks, the glob mark prompt, the delete-key gating and the delete modal: preview,
//! typed confirm, revalidation, run and report (spec §7.4).

use std::collections::{BTreeMap, BTreeSet};

use super::cache::Entry;
use super::messages::{describe, user_message};
use super::planner::{self, UNREADABLE_INDEX};
use super::state::{Action, CacheKey, CacheValue, Effect, Pane, State, StatusKind};
use super::write::{BUSY, OpId};
use crate::domain::{
    ArtifactSummary, DeleteOp, DeletePlan, DeleteReport, DepSnapshot, Digest, Inventory, MarkKind,
    Marks, OpOutcome, RegistryError, RegistryId, RepoPath, SnapshotError, glob_match,
};

pub const CHANGED: &str = "the repository changed since the preview; review and confirm again";

#[derive(Clone, Debug, PartialEq)]
pub struct DeleteModal {
    pub registry: RegistryId,
    pub repo: RepoPath,
    /// The pending snapshot or the running delete; events for other ids are ignored.
    pub op: OpId,
    pub stage: DelStage,
    pub whole: bool,
    pub typed: String,
    pub snapshot: Option<DepSnapshot>,
    pub plan: Option<DeletePlan>,
    /// Revalidation found a different plan; shown as a banner.
    pub changed: bool,
    pub scroll: u16,
    /// Hidden while running or holding an unread report; browsing continues, and `d` reopens.
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DelStage {
    Snapshot,
    /// Nothing can be confirmed; the reason.
    Blocked(String),
    Preview,
    /// Typing the repository name.
    Confirm,
    Revalidate,
    Running,
    Report {
        report: DeleteReport,
        stopped: Option<RegistryError>,
        /// The repo inventory was fetched after the run, so `observed` applies.
        refreshed: bool,
    },
}

/// What a refreshed inventory shows of an `Uncertain` op; `None` = cannot be observed (an
/// unlisted digest on a `TaggedOnly` registry).
pub fn observed(op: &DeleteOp, inventory: &[ArtifactSummary], inv: Inventory) -> Option<String> {
    let text = match op {
        DeleteOp::RemoveTag { tag, expected } => {
            match inventory.iter().find(|a| a.tags.contains(tag)) {
                Some(a) if &a.digest == expected => "tag still present",
                // Re-pushed: the tag now names another manifest.
                Some(a) => return Some(format!("tag moved to {}", a.digest.short())),
                None => "tag gone",
            }
        }
        DeleteOp::DeleteManifest { digest } if inventory.iter().any(|a| &a.digest == digest) => {
            "still present"
        }
        DeleteOp::DeleteManifest { .. } => (inv == Inventory::Complete).then_some("gone")?,
    };
    Some(text.to_owned())
}

/// The executor's skip rule needs parents within the delete set only: a kept parent never
/// becomes `Done` and would skip its (planned) child.
fn parents_in(plan: &DeletePlan, snap: &DepSnapshot) -> BTreeMap<Digest, BTreeSet<Digest>> {
    let set: BTreeSet<&Digest> = plan.layers.iter().flatten().collect();
    set.iter()
        .filter_map(|d| {
            let ps: BTreeSet<Digest> = snap
                .parents
                .get(*d)?
                .iter()
                .filter(|p| set.contains(p))
                .cloned()
                .collect();
            (!ps.is_empty()).then(|| ((*d).clone(), ps))
        })
        .collect()
}

/// `deleted: N done`, or every outcome count when any op did not succeed.
fn summary(r: &DeleteReport) -> (StatusKind, String) {
    let n = |f: fn(&OpOutcome) -> bool| r.results.iter().filter(|(_, o)| f(o)).count();
    let done = n(|o| *o == OpOutcome::Done);
    if done == r.results.len() {
        return (StatusKind::Ok, format!("deleted: {done} done"));
    }
    let msg = format!(
        "delete finished: {done} done, {} failed, {} skipped, {} uncertain, {} not attempted",
        n(|o| matches!(o, OpOutcome::Failed(_))),
        n(|o| matches!(o, OpOutcome::Skipped(_))),
        n(|o| matches!(o, OpOutcome::Uncertain(_))),
        n(|o| *o == OpOutcome::NotAttempted),
    );
    (StatusKind::Error, msg)
}

/// `summary`, or `deletion disabled on <id>` when the run was aborted.
fn report_status(r: &DeleteReport, registry: &RegistryId) -> (StatusKind, String) {
    if r.aborted_unsupported {
        (
            StatusKind::Error,
            format!("deletion disabled on {registry}"),
        )
    } else {
        summary(r)
    }
}

/// Marked tags of `a` in `m`, or 1 for a marked untagged manifest. A tag mark counts only while
/// the tag is still on that digest (ruling "marks are matched against the snapshot").
fn marked_in(m: &Marks, a: &ArtifactSummary) -> usize {
    if a.tags.is_empty() {
        return usize::from(m.untagged.contains_key(&a.digest));
    }
    m.tags
        .get(&a.digest)
        .map_or(0, |t| a.tags.iter().filter(|x| t.contains(*x)).count())
}

impl State {
    fn repo_key(&self) -> Option<(RegistryId, RepoPath)> {
        Some((
            self.selected_registry()?.id.clone(),
            self.selected_repo()?.path.clone(),
        ))
    }

    /// Unfiltered inventory of the selected repo (empty until loaded).
    fn inventory(&self) -> &[ArtifactSummary] {
        let Some((reg, repo)) = self.repo_key() else {
            return &[];
        };
        match self.value(&CacheKey::Inventory(reg, repo)) {
            Some(CacheValue::Inventory(v)) => v,
            _ => &[],
        }
    }

    /// Marks of the selected repo.
    pub fn marks(&self) -> Option<&Marks> {
        self.marks.get(&self.repo_key()?)
    }

    /// `Some(true)` fully marked, `Some(false)` partially, `None` unmarked.
    pub fn mark_state(&self, a: &ArtifactSummary) -> Option<bool> {
        let n = marked_in(self.marks()?, a);
        (n > 0).then(|| n == a.tags.len().max(1))
    }

    /// Selected tags plus marked untagged manifests.
    pub fn marked_count(&self) -> usize {
        let Some(m) = self.marks() else { return 0 };
        self.inventory().iter().map(|a| marked_in(m, a)).sum()
    }

    /// The delete keys apply: Images pane of a repo whose registry can delete manifests.
    pub fn can_delete(&self) -> bool {
        self.focus == Pane::Images
            && self.selected_repo().is_some()
            && self
                .selected_registry()
                .is_some_and(|r| r.caps.delete_manifest && !self.delete_disabled.contains(&r.id))
    }

    pub fn range_anchor(&self) -> Option<&Digest> {
        self.range_anchor.as_ref()
    }

    pub fn glob(&self) -> Option<&str> {
        self.glob.as_deref()
    }

    /// Tags of the repo inventory matching the open prompt's pattern.
    pub fn glob_matches(&self) -> usize {
        let Some(p) = self.glob() else { return 0 };
        self.inventory()
            .iter()
            .flat_map(|a| &a.tags)
            .filter(|t| glob_match(p, t))
            .count()
    }

    /// `space`, `u`, `a`, Esc; inert unless `can_delete`.
    pub(super) fn mark_action(&mut self, a: Action) {
        let Some(key) = self.repo_key().filter(|_| self.can_delete()) else {
            return;
        };
        match a {
            Action::Mark => {
                let Some(img) = self.selected_image().cloned() else {
                    return;
                };
                let full = self.mark_state(&img) == Some(true);
                let m = self.marks.entry(key.clone()).or_default();
                if img.tags.is_empty() {
                    // Explicit → unmarked; unmarked or Automatic → Explicit.
                    if m.untagged.get(&img.digest) == Some(&MarkKind::Explicit) {
                        m.untagged.remove(&img.digest);
                    } else {
                        m.untagged.insert(img.digest, MarkKind::Explicit);
                    }
                } else if full {
                    m.tags.remove(&img.digest);
                } else {
                    m.tags.insert(img.digest, img.tags.into_iter().collect());
                }
                if m.tags.is_empty() && m.untagged.is_empty() {
                    self.marks.remove(&key);
                }
            }
            Action::MarkUntagged => {
                if self.selected_registry().map(|r| r.caps.inventory) != Some(Inventory::Complete) {
                    return;
                }
                let untagged: Vec<_> = self
                    .inventory()
                    .iter()
                    .filter(|a| a.tags.is_empty())
                    .map(|a| a.digest.clone())
                    .collect();
                if untagged.is_empty() {
                    return;
                }
                let m = self.marks.entry(key).or_default();
                for d in untagged {
                    m.untagged.entry(d).or_insert(MarkKind::Automatic);
                }
            }
            Action::MarkGlob => self.glob = Some(format!("{}*", self.image_filter)),
            Action::RangeMark => self.range_mark(&key),
            _ => {}
        }
    }

    /// `v`: with no visible anchor, anchors the cursor row; else marks every tag of every row
    /// between the anchor and the cursor (an untagged row becomes `Explicit`). Sets, never
    /// toggles.
    fn range_mark(&mut self, key: &(RegistryId, RepoPath)) {
        let Some(cursor) = self.selected_image().map(|a| a.digest.clone()) else {
            return;
        };
        let rows = self.visible_images();
        let pos = |d: &Digest| rows.iter().position(|a| &a.digest == d);
        let (Some(i), Some(j)) = (self.range_anchor.as_ref().and_then(pos), pos(&cursor)) else {
            self.range_anchor = Some(cursor);
            return;
        };
        let picked: Vec<_> = rows[i.min(j)..=i.max(j)]
            .iter()
            .map(|a| (a.digest.clone(), a.tags.clone()))
            .collect();
        let m = self.marks.entry(key.clone()).or_default();
        for (d, tags) in picked {
            if tags.is_empty() {
                m.untagged.insert(d, MarkKind::Explicit);
            } else {
                m.tags.insert(d, tags.into_iter().collect());
            }
        }
        self.range_anchor = None;
    }

    /// Esc on the Images pane: clears the first of the range anchor, the compare base, the marks.
    pub(super) fn esc_images(&mut self) {
        if self.range_anchor.take().is_some() || self.compare_base.take().is_some() {
            return;
        }
        if let Some(key) = self.repo_key().filter(|_| self.can_delete()) {
            self.marks.remove(&key);
        }
    }

    /// Every action but quit goes here while the prompt is open (`/` filter editing is the model).
    pub(super) fn glob_action(&mut self, a: Action) {
        let Some(g) = self.glob.as_mut() else { return };
        match a {
            Action::FilterChar(c) => g.push(c),
            Action::FilterBackspace => {
                g.pop();
            }
            Action::FilterCancel => self.glob = None,
            Action::FilterApply => self.apply_glob(),
            _ => {}
        }
    }

    /// Adds exactly the matching tags to the repo's marks and closes the prompt.
    fn apply_glob(&mut self) {
        let (Some(p), Some(key)) = (
            self.glob.take(),
            self.repo_key().filter(|_| self.can_delete()),
        ) else {
            return;
        };
        let hits: Vec<_> = self
            .inventory()
            .iter()
            .map(|a| {
                let t: Vec<String> = a
                    .tags
                    .iter()
                    .filter(|t| glob_match(&p, t))
                    .cloned()
                    .collect();
                (a.digest.clone(), t)
            })
            .filter(|(_, t)| !t.is_empty())
            .collect();
        let n: usize = hits.iter().map(|(_, t)| t.len()).sum();
        if n > 0 {
            let m = self.marks.entry(key).or_default();
            for (d, t) in hits {
                m.tags.entry(d).or_default().extend(t);
            }
        }
        let msg = match n {
            0 => "no tags match".to_owned(),
            1 => "marked 1 tag".to_owned(),
            n => format!("marked {n} tags"),
        };
        self.set_status(StatusKind::Info, msg);
    }

    /// The delete modal, also while hidden (the UI draws it only when shown).
    pub fn delete_modal(&self) -> Option<&DeleteModal> {
        self.delete.as_ref()
    }

    /// The status line of a hidden delete: running, or holding an unread report.
    pub fn delete_status(&self) -> Option<String> {
        let m = self.delete.as_ref().filter(|m| m.hidden)?;
        match &m.stage {
            DelStage::Running => Some(format!("deleting in {}… — press d to show", m.repo)),
            DelStage::Report { report, .. } => Some(format!(
                "{} — press d for the report",
                report_status(report, &m.registry).1
            )),
            _ => None,
        }
    }

    /// Installs `m` as the delete modal (UI render tests).
    #[cfg(test)]
    pub(crate) fn set_delete_modal(&mut self, m: DeleteModal) {
        self.delete = Some(m);
    }

    /// `d`: reopen a hidden modal, else snapshot the marked repo for a preview.
    pub(super) fn delete_start(&mut self) -> Vec<Effect> {
        if let Some(m) = self.delete.as_mut().filter(|m| m.hidden) {
            m.hidden = false;
            return vec![];
        }
        let Some((registry, repo)) = self.repo_key().filter(|_| self.can_delete()) else {
            return vec![];
        };
        if self.marked_count() == 0 {
            self.set_status(StatusKind::Info, "nothing marked".into());
            return vec![];
        }
        if self.write_running() {
            self.set_status(StatusKind::Error, BUSY.into());
            return vec![];
        }
        let mut m = DeleteModal {
            registry,
            repo,
            op: OpId(0),
            stage: DelStage::Snapshot,
            whole: false,
            typed: String::new(),
            snapshot: None,
            plan: None,
            changed: false,
            scroll: 0,
            hidden: false,
        };
        let fx = self.snapshot_effect(&mut m);
        self.delete = Some(m);
        fx
    }

    /// A fresh snapshot of `m`'s repo for its marked digests, under a new op id.
    fn snapshot_effect(&mut self, m: &mut DeleteModal) -> Vec<Effect> {
        self.next_op += 1;
        m.op = OpId(self.next_op);
        let marked = self
            .marks
            .get(&(m.registry.clone(), m.repo.clone()))
            .map(|k| k.tags.keys().chain(k.untagged.keys()).cloned().collect())
            .unwrap_or_default();
        vec![Effect::DeleteSnapshot {
            op: m.op,
            registry: m.registry.clone(),
            repo: m.repo.clone(),
            marked,
        }]
    }

    /// The plan of `m`'s marks over `snap` (`None` only for an unknown registry).
    fn plan_for(&self, m: &DeleteModal, snap: &DepSnapshot) -> Option<DeletePlan> {
        let caps = self.registries.iter().find(|r| r.id == m.registry)?.caps;
        let none = Marks::default();
        let marks = self
            .marks
            .get(&(m.registry.clone(), m.repo.clone()))
            .unwrap_or(&none);
        Some(planner::plan(marks, snap, caps, m.whole))
    }

    /// Every action but quit goes here while the modal is shown.
    pub(super) fn delete_action(&mut self, a: Action) -> Vec<Effect> {
        let Some(mut m) = self.delete.take() else {
            return vec![];
        };
        let enter = matches!(a, Action::Enter | Action::FilterApply);
        let back = matches!(a, Action::Back | Action::FilterCancel);
        match a {
            Action::Up => m.scroll = m.scroll.saturating_sub(1),
            Action::Down => m.scroll = m.scroll.saturating_add(1),
            _ => {}
        }
        let mut fx = vec![];
        let open = match m.stage {
            DelStage::Snapshot | DelStage::Blocked(_) | DelStage::Preview if back => false,
            DelStage::Report { .. } => !(enter || back),
            DelStage::Preview if a == Action::ToggleWhole => {
                m.whole = !m.whole;
                m.plan = m.snapshot.as_ref().and_then(|s| self.plan_for(&m, s));
                true
            }
            DelStage::Preview if enter => {
                if m.plan.as_ref().is_some_and(|p| !p.ops().is_empty()) {
                    m.stage = DelStage::Confirm;
                } else {
                    self.set_status(StatusKind::Info, "nothing to delete".into());
                }
                true
            }
            DelStage::Confirm => {
                match a {
                    Action::FilterChar(c) => m.typed.push(c),
                    Action::FilterBackspace => {
                        m.typed.pop();
                    }
                    _ if back => {
                        m.typed.clear();
                        m.stage = DelStage::Preview;
                    }
                    _ if enter && m.typed == m.repo.0 => {
                        // Takes the write lock (`write_running`) until the run ends.
                        if self.write_running() {
                            self.set_status(StatusKind::Error, BUSY.into());
                        } else {
                            m.stage = DelStage::Revalidate;
                            fx = self.snapshot_effect(&mut m);
                        }
                    }
                    _ => {}
                }
                true
            }
            // Releases the lock; the late snapshot is ignored (stage is no longer `Revalidate`).
            DelStage::Revalidate if back => {
                m.typed.clear();
                m.stage = DelStage::Preview;
                true
            }
            DelStage::Running if enter || back => {
                m.hidden = true;
                true
            }
            _ => true,
        };
        if open {
            self.delete = Some(m);
        }
        fx
    }

    /// Builds the preview, or at revalidation runs the confirmed plan only when the fresh
    /// snapshot yields the same plan (spec §7.4 execution 1); anything else needs a new confirm.
    pub(super) fn delete_snapshot(
        &mut self,
        op: OpId,
        result: Result<DepSnapshot, SnapshotError>,
    ) -> Vec<Effect> {
        let Some(mut m) = self.delete.take_if(|m| {
            m.op == op && matches!(m.stage, DelStage::Snapshot | DelStage::Revalidate)
        }) else {
            return vec![];
        };
        let mut fx = vec![];
        let fresh = result.map(|s| (self.plan_for(&m, &s), s));
        match fresh {
            Ok((Some(plan), snap)) => {
                let confirmed = m.stage == DelStage::Revalidate;
                let same = matches!((&m.plan, &m.snapshot), (Some(p), Some(s))
                    if planner::same_plan(p, s, &plan, &snap));
                if confirmed && same {
                    self.next_op += 1;
                    m.op = OpId(self.next_op);
                    m.stage = DelStage::Running;
                    fx.push(Effect::Delete {
                        op: m.op,
                        registry: m.registry.clone(),
                        repo: m.repo.clone(),
                        plan: plan.clone(),
                        parents: parents_in(&plan, &snap),
                    });
                } else {
                    m.changed |= confirmed;
                    m.typed.clear();
                    m.stage = DelStage::Preview;
                }
                m.plan = Some(plan);
                m.snapshot = Some(snap);
            }
            Ok((None, _)) => {
                m.stage = DelStage::Blocked(format!("unknown registry {}", m.registry))
            }
            Err(SnapshotError::UnreadableIndex) => {
                m.stage = DelStage::Blocked(UNREADABLE_INDEX.into());
            }
            Err(SnapshotError::Inventory(e)) => {
                let key = CacheKey::Inventory(m.registry.clone(), m.repo.clone());
                let msg = match self.registries.iter().find(|r| r.id == m.registry) {
                    Some(r) => user_message(&e, r, &describe(&key)),
                    None => e.to_string(),
                };
                m.stage = DelStage::Blocked(msg);
            }
        }
        self.delete = Some(m);
        fx
    }

    /// Shows the report (spec §7.4 execution 6–7): clears the repo's marks and refreshes its
    /// cached data. A registry that reported deletion unsupported is disabled for the session.
    pub(super) fn delete_done(
        &mut self,
        op: OpId,
        report: DeleteReport,
        stopped: Option<RegistryError>,
    ) -> Vec<Effect> {
        let Some(m) = self
            .delete
            .as_mut()
            .filter(|m| m.op == op && m.stage == DelStage::Running)
        else {
            return vec![];
        };
        let (registry, repo, hidden) = (m.registry.clone(), m.repo.clone(), m.hidden);
        let (kind, mut msg) = report_status(&report, &registry);
        if report.aborted_unsupported {
            self.delete_disabled.insert(registry.clone());
        }
        if hidden {
            msg.push_str(" — press d for the report");
        }
        self.set_status(kind, msg);
        // Deleted manifests are not refetched: they would only answer "not found" (and
        // replace the summary above on the status line).
        let gone: BTreeSet<&Digest> = report
            .results
            .iter()
            .filter_map(|(op, o)| match op {
                DeleteOp::DeleteManifest { digest } if *o == OpOutcome::Done => Some(digest),
                _ => None,
            })
            .collect();
        let inv = CacheKey::Inventory(registry.clone(), repo.clone());
        let mut keys = vec![inv.clone()];
        keys.extend(
            self.cache
                .keys()
                .filter(|k| {
                    **k != inv && k.registry() == Some(&registry) && k.repo() == Some(&repo)
                })
                .filter(|k| k.digest().is_none_or(|d| !gone.contains(d)))
                .cloned(),
        );
        self.marks.remove(&(registry, repo));
        if let Some(m) = self.delete.as_mut() {
            m.stage = DelStage::Report {
                report,
                stopped,
                refreshed: false,
            };
        }
        keys.into_iter()
            .filter_map(|k| self.cache.invalidate(k))
            .collect()
    }

    /// After an applied `Loaded`: a report's `observed` states apply once the repo inventory
    /// is `Ready` again (not while a refetch is still in flight).
    pub(super) fn delete_loaded(&mut self, key: &CacheKey) {
        let ready = matches!(self.cache.get(key), Some(Entry::Ready { .. }));
        if let Some(m) = self.delete.as_mut()
            && let DelStage::Report { refreshed, .. } = &mut m.stage
            && ready
            && *key == CacheKey::Inventory(m.registry.clone(), m.repo.clone())
        {
            *refreshed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::messages::user_message;
    use crate::app::planner::{self, UNREADABLE_INDEX};
    use crate::app::state::{Event, KeyMode, Pane, StatusKind};
    use crate::app::update::QUIT_HINT;
    use crate::app::update::tests::{T, art, at_images, dg, ecr, fetched, inv, repos, rid, rp};
    use crate::app::write::PushOutcome;
    use crate::domain::{ArtifactSummary, Digest, Inventory, MarkKind, OpOutcome};
    use crate::keys::key_to_action;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::BTreeSet;

    fn set(tags: &[&str]) -> BTreeSet<String> {
        tags.iter().map(|t| t.to_string()).collect()
    }

    /// Digest `d(n)`: `art_d` keeps the fixture's other fields.
    fn d(n: u8) -> Digest {
        Digest::parse(&format!("sha256:{n:064x}")).unwrap()
    }
    fn art_d(n: u8, tags: &[&str]) -> ArtifactSummary {
        ArtifactSummary {
            digest: d(n),
            ..art('0', tags)
        }
    }

    fn key(t: &T, code: KeyCode) -> Option<Action> {
        key_to_action(KeyEvent::new(code, KeyModifiers::NONE), t.s.key_mode())
    }

    fn press(t: &mut T, code: KeyCode) -> Vec<Effect> {
        key(t, code).map(|a| t.act(a)).unwrap_or_default()
    }

    fn marks(t: &T) -> Marks {
        t.s.marks().cloned().unwrap_or_default()
    }

    #[test]
    fn space_toggles_marks() {
        let (mut t, _) = at_images(vec![art('a', &["1", "2"]), art_d(1, &[]), art_d(2, &[])]);
        let a = t.s.visible_images()[0].digest.clone();
        assert_eq!(a, dg('a'));
        t.act(Action::Mark);
        assert_eq!(marks(&t).tags[&dg('a')], set(&["1", "2"]));
        assert_eq!(t.s.mark_state(t.s.selected_image().unwrap()), Some(true));
        t.act(Action::Mark);
        assert_eq!(t.s.marks(), None, "unmarked");

        // A partial glob mark of `1` is completed by space.
        t.s.marks.insert(
            (rid("ecr"), rp("app")),
            Marks {
                tags: [(dg('a'), set(&["1"]))].into(),
                ..Marks::default()
            },
        );
        assert_eq!(t.s.mark_state(t.s.selected_image().unwrap()), Some(false));
        t.act(Action::Mark);
        assert_eq!(marks(&t).tags[&dg('a')], set(&["1", "2"]));
        t.act(Action::Mark);

        // Untagged: Explicit, then removed; an Automatic mark is upgraded.
        t.act(Action::Down);
        let u = t.s.selected_image().unwrap().digest.clone();
        t.act(Action::Mark);
        assert_eq!(marks(&t).untagged[&u], MarkKind::Explicit);
        t.act(Action::Mark);
        assert!(marks(&t).untagged.is_empty());
        t.s.marks
            .entry((rid("ecr"), rp("app")))
            .or_default()
            .untagged
            .insert(u.clone(), MarkKind::Automatic);
        t.act(Action::Mark);
        assert_eq!(marks(&t).untagged[&u], MarkKind::Explicit);
    }

    /// a(1.0), b(untagged), c(2.0, latest), d(3.0), in inventory order.
    fn four() -> T {
        at_images(vec![
            art('a', &["1.0"]),
            art('b', &[]),
            art('c', &["2.0", "latest"]),
            art('d', &["3.0"]),
        ])
        .0
    }

    fn range_marks() -> Marks {
        Marks {
            tags: [(dg('a'), set(&["1.0"])), (dg('c'), set(&["2.0", "latest"]))].into(),
            untagged: [(dg('b'), MarkKind::Explicit)].into(),
        }
    }

    #[test]
    fn range_marks_between_anchor_and_cursor() {
        let mut t = four();
        assert_eq!(key(&t, KeyCode::Char('v')), Some(Action::RangeMark));
        t.act(Action::RangeMark);
        assert_eq!(t.s.range_anchor(), Some(&dg('a')));
        assert_eq!(t.s.marks(), None, "the first v only anchors");
        t.act(Action::Down);
        t.act(Action::Down);
        t.act(Action::RangeMark);
        assert_eq!(marks(&t), range_marks(), "d is untouched");
        assert_eq!(t.s.range_anchor(), None);

        // Upward: anchor c, cursor a.
        let mut t = four();
        t.act(Action::Down);
        t.act(Action::Down);
        t.act(Action::RangeMark);
        t.act(Action::Up);
        t.act(Action::Up);
        t.act(Action::RangeMark);
        assert_eq!(marks(&t), range_marks());
        assert_eq!(t.s.range_anchor(), None);
    }

    #[test]
    fn range_never_unmarks() {
        let mut t = four();
        t.act(Action::Mark); // a fully marked
        t.act(Action::RangeMark);
        t.act(Action::Down);
        t.act(Action::RangeMark);
        let m = marks(&t);
        assert_eq!(m.tags[&dg('a')], set(&["1.0"]), "a stays marked");
        assert_eq!(m.untagged[&dg('b')], MarkKind::Explicit);

        // A partial mark is completed, an Automatic one upgraded.
        let mut t = four();
        t.s.marks.insert(
            (rid("ecr"), rp("app")),
            Marks {
                tags: [(dg('c'), set(&["latest"]))].into(),
                untagged: [(dg('b'), MarkKind::Automatic)].into(),
            },
        );
        t.act(Action::Down);
        t.act(Action::RangeMark);
        t.act(Action::Down);
        t.act(Action::RangeMark);
        let m = marks(&t);
        assert_eq!(m.tags[&dg('c')], set(&["2.0", "latest"]));
        assert_eq!(m.untagged[&dg('b')], MarkKind::Explicit);
    }

    #[test]
    fn esc_cancels_anchor_before_marks() {
        let mut t = four();
        t.act(Action::Mark);
        let before = marks(&t);
        t.act(Action::Down);
        t.act(Action::RangeMark);
        assert!(t.s.range_anchor().is_some());
        assert_eq!(key(&t, KeyCode::Esc), Some(Action::ClearMarks));
        press(&mut t, KeyCode::Esc);
        assert_eq!(t.s.range_anchor(), None);
        assert_eq!(marks(&t), before, "marks unchanged");
        press(&mut t, KeyCode::Esc);
        assert_eq!(t.s.marks(), None, "the second Esc clears the marks");
    }

    #[test]
    fn range_anchor_resets() {
        // An anchor whose row left the view: v anchors the cursor row instead.
        let mut t = four();
        t.act(Action::RangeMark);
        t.s.range_anchor = Some(dg('f'));
        t.act(Action::Down);
        t.act(Action::RangeMark);
        assert_eq!(t.s.range_anchor(), Some(&dg('b')));
        assert_eq!(t.s.marks(), None);
        // Moving to another repo drops the anchor.
        t.act(Action::PrevPane);
        t.act(Action::Down);
        assert_eq!(t.s.range_anchor(), None);
    }

    #[test]
    fn range_hidden_without_delete() {
        let mut t = four();
        t.s.delete_disabled.insert(rid("ecr"));
        t.act(Action::RangeMark);
        assert_eq!(t.s.range_anchor(), None);
        t.act(Action::Down);
        t.act(Action::RangeMark);
        assert_eq!(t.s.range_anchor(), None);
        assert_eq!(t.s.marks(), None);
    }

    #[test]
    fn mark_untagged_keeps_explicit() {
        let (mut t, _) = at_images(vec![art_d(1, &[]), art_d(2, &[]), art('a', &["1"])]);
        t.s.marks
            .entry((rid("ecr"), rp("app")))
            .or_default()
            .untagged
            .insert(d(1), MarkKind::Explicit);
        t.act(Action::MarkUntagged);
        let m = marks(&t);
        assert_eq!(m.untagged[&d(1)], MarkKind::Explicit);
        assert_eq!(m.untagged[&d(2)], MarkKind::Automatic);
        assert_eq!(m.untagged.len(), 2);
        assert!(m.tags.is_empty());
    }

    #[test]
    fn mark_untagged_needs_complete() {
        let (mut t, _) = at_images(vec![art_d(1, &[])]);
        t.s.registries[0].caps.inventory = Inventory::TaggedOnly;
        t.act(Action::MarkUntagged);
        assert_eq!(t.s.marks(), None);
    }

    #[test]
    fn glob_prompt_prefill_count_and_add() {
        let (mut t, _) = at_images(vec![
            art('a', &["0.1.5-dev"]),
            art('b', &["10.1.0"]),
            art('c', &["0.10.0"]),
            art('d', &["other"]),
        ]);
        t.s.marks
            .entry((rid("ecr"), rp("app")))
            .or_default()
            .tags
            .insert(dg('d'), set(&["other"]));
        t.act(Action::FilterStart);
        for c in "0.1.".chars() {
            t.act(Action::FilterChar(c));
        }
        t.act(Action::FilterApply);
        assert_eq!(t.s.glob(), None);
        t.act(Action::MarkGlob);
        assert_eq!(t.s.glob(), Some("0.1.*"));
        assert_eq!(t.s.key_mode(), KeyMode::Text);
        assert_eq!(t.s.glob_matches(), 1);
        t.act(Action::FilterBackspace);
        t.act(Action::FilterBackspace);
        t.act(Action::FilterChar('*'));
        assert_eq!(t.s.glob(), Some("0.1*"));
        assert_eq!(t.s.glob_matches(), 2);
        // Typed keys and navigation never leak past the prompt.
        t.act(Action::Mark);
        t.act(Action::Down);
        assert_eq!(marks(&t).tags.len(), 1);

        t.act(Action::FilterApply);
        assert_eq!(t.s.glob(), None);
        assert_eq!(t.s.key_mode(), KeyMode::Normal);
        let m = marks(&t);
        assert_eq!(m.tags[&dg('a')], set(&["0.1.5-dev"]));
        assert_eq!(m.tags[&dg('c')], set(&["0.10.0"]));
        assert_eq!(m.tags[&dg('d')], set(&["other"]), "earlier marks kept");
        assert_eq!(m.tags.len(), 3);
        assert_eq!(t.s.status(), Some((StatusKind::Info, "marked 2 tags")));

        t.act(Action::MarkGlob);
        t.act(Action::FilterChar('x'));
        t.act(Action::FilterCancel);
        assert_eq!(t.s.glob(), None);
        assert_eq!(marks(&t), m);

        t.act(Action::MarkGlob);
        t.act(Action::FilterChar('x'));
        t.act(Action::FilterApply);
        assert_eq!(t.s.status(), Some((StatusKind::Info, "no tags match")));
        assert_eq!(marks(&t), m);
    }

    #[test]
    fn clear_marks_on_images_pane() {
        let (mut t, _) = at_images(vec![art('a', &["1"])]);
        t.act(Action::Mark);
        t.s.marks
            .entry((rid("ecr"), rp("web")))
            .or_default()
            .tags
            .insert(dg('b'), set(&["x"]));
        t.act(Action::PrevPane);
        assert_eq!(t.s.focus(), Pane::Repos);
        t.act(Action::ClearMarks);
        assert_eq!(marks(&t).tags.len(), 1, "no-op off the images pane");
        t.act(Action::NextPane);
        t.act(Action::ClearMarks);
        assert_eq!(t.s.marks(), None);
        assert!(t.s.marks.contains_key(&(rid("ecr"), rp("web"))));
    }

    #[test]
    fn esc_cancels_input_before_clearing_marks() {
        let (mut t, _) = at_images(vec![art('a', &["1"])]);
        t.act(Action::Mark);

        press(&mut t, KeyCode::Char('/'));
        assert!(t.s.filter_editing());
        press(&mut t, KeyCode::Char('x'));
        press(&mut t, KeyCode::Esc);
        assert!(!t.s.filter_editing());
        assert_eq!(t.s.marked_count(), 1, "filter Esc keeps the marks");

        press(&mut t, KeyCode::Char('a'));
        assert_eq!(t.s.glob(), Some("*"));
        press(&mut t, KeyCode::Esc);
        assert_eq!(t.s.glob(), None);
        assert_eq!(t.s.marked_count(), 1, "glob Esc keeps the marks");

        press(&mut t, KeyCode::Esc);
        assert_eq!(t.s.marks(), None, "second Esc clears");
    }

    #[test]
    fn delete_keys_inert_when_disabled() {
        let all = [
            Action::Mark,
            Action::MarkUntagged,
            Action::MarkGlob,
            Action::ClearMarks,
            Action::DeleteStart,
        ];
        for how in 0..2 {
            let (mut t, _) = at_images(vec![art('a', &["1"]), art_d(1, &[])]);
            t.act(Action::Mark);
            let before = marks(&t);
            if how == 0 {
                t.s.delete_disabled.insert(rid("ecr"));
            } else {
                t.s.registries[0].caps.delete_manifest = false;
            }
            assert!(!t.s.can_delete());
            for a in all {
                let fx = t.act(a);
                assert_eq!(marks(&t), before, "{a:?}");
                assert_eq!(t.s.glob(), None, "{a:?}");
                assert_eq!(t.s.delete_modal(), None, "{a:?}");
                assert!(fx.is_empty(), "{a:?}: {fx:?}");
            }
        }
        // No marks: no modal, no effect.
        let (mut t, _) = at_images(vec![art('a', &["1"])]);
        let fx = t.act(Action::DeleteStart);
        assert_eq!((t.s.delete_modal(), fx), (None, vec![]));

        // Deletion disabled while the glob prompt is open: Enter adds nothing.
        let (mut t, _) = at_images(vec![art('a', &["1"])]);
        t.act(Action::MarkGlob);
        t.s.delete_disabled.insert(rid("ecr"));
        t.act(Action::FilterApply);
        assert_eq!((t.s.glob(), t.s.marks()), (None, None));
    }

    #[test]
    fn marks_per_repo() {
        let (mut t, _) = at_images(vec![art('a', &["1"])]);
        assert!(t.s.can_delete());
        t.act(Action::Mark);
        t.act(Action::PrevPane);
        t.act(Action::Down);
        assert_eq!(t.s.selected_repo().unwrap().path, rp("web"));
        assert_eq!(t.s.marks(), None);
        assert_eq!(t.s.marked_count(), 0);
        t.act(Action::Up);
        assert_eq!(t.s.marked_count(), 1);
    }

    // ---- the delete modal ----

    fn imgs() -> Vec<ArtifactSummary> {
        vec![art('a', &["1"]), art('b', &["2"]), art('c', &["3"])]
    }

    fn snap(inventory: Vec<ArtifactSummary>) -> DepSnapshot {
        DepSnapshot {
            inventory,
            ..DepSnapshot::default()
        }
    }

    /// Images pane of ecr/app with `imgs()`, `a` marked.
    fn marked() -> T {
        let (mut t, _) = at_images(imgs());
        t.act(Action::Mark);
        t
    }

    fn modal(t: &T) -> &DeleteModal {
        t.s.delete_modal().expect("modal")
    }

    fn stage(t: &T) -> DelStage {
        modal(t).stage.clone()
    }

    fn op(t: &T) -> OpId {
        modal(t).op
    }

    fn snapshot_fx(fx: &[Effect]) -> Option<(OpId, BTreeSet<Digest>)> {
        fx.iter().find_map(|e| match e {
            Effect::DeleteSnapshot { op, marked, .. } => Some((*op, marked.clone())),
            _ => None,
        })
    }

    fn has_delete(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::Delete { .. }))
    }

    fn snapshot_ev(t: &mut T, result: Result<DepSnapshot, SnapshotError>) -> Vec<Effect> {
        let op = op(t);
        t.ev(Event::DeleteSnapshot { op, result })
    }

    fn type_str(t: &mut T, s: &str) {
        for c in s.chars() {
            t.act(Action::FilterChar(c));
        }
    }

    /// `d` and a snapshot of `inv`: stage `Preview`.
    fn preview(t: &mut T, inv: Vec<ArtifactSummary>) {
        t.act(Action::DeleteStart);
        snapshot_ev(t, Ok(snap(inv)));
        assert_eq!(stage(t), DelStage::Preview);
    }

    /// Preview of `imgs()`, Enter, `app`, Enter: stage `Revalidate`.
    fn confirmed(t: &mut T) {
        preview(t, imgs());
        t.act(Action::Enter);
        type_str(t, "app");
        t.act(Action::FilterApply);
        assert_eq!(stage(t), DelStage::Revalidate);
    }

    /// `a` marked, confirmed and revalidated unchanged: stage `Running`.
    fn running() -> T {
        let mut t = marked();
        confirmed(&mut t);
        snapshot_ev(&mut t, Ok(snap(imgs())));
        assert_eq!(stage(&t), DelStage::Running);
        t
    }

    fn report(results: Vec<(DeleteOp, OpOutcome)>) -> DeleteReport {
        DeleteReport {
            results,
            aborted_unsupported: false,
        }
    }

    fn del(c: char) -> DeleteOp {
        DeleteOp::DeleteManifest { digest: dg(c) }
    }

    fn done(t: &mut T, report: DeleteReport) -> Vec<Effect> {
        let op = op(t);
        t.ev(Event::DeleteDone {
            op,
            report,
            stopped: None,
        })
    }

    fn ctrl_c_quits(t: &mut T) {
        let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let a = key_to_action(k, t.s.key_mode()).unwrap();
        assert!(t.act(a).contains(&Effect::Quit), "{:?}", t.s.key_mode());
    }

    #[test]
    fn delete_start_requires_marks_and_free_lock() {
        let (mut t, _) = at_images(imgs());
        let fx = t.act(Action::DeleteStart);
        assert_eq!(t.s.status(), Some((StatusKind::Info, "nothing marked")));
        assert_eq!((t.s.delete_modal(), snapshot_fx(&fx)), (None, None));

        t.act(Action::Mark);
        t.s.start_push(rid("ecr"), rp("web"), "img".into(), "x".into())
            .unwrap();
        let fx = t.act(Action::DeleteStart);
        assert_eq!(t.s.status(), Some((StatusKind::Error, BUSY)));
        assert_eq!((t.s.delete_modal(), snapshot_fx(&fx)), (None, None));

        t.ev(Event::PushDone {
            op: OpId(1),
            outcome: PushOutcome::Cancelled,
        });
        let fx = t.act(Action::DeleteStart);
        assert_eq!(stage(&t), DelStage::Snapshot);
        assert_eq!(op(&t), OpId(2));
        assert!(!t.s.write_running(), "the lock is taken at typed confirm");
        assert!(fx.contains(&Effect::DeleteSnapshot {
            op: OpId(2),
            registry: rid("ecr"),
            repo: rp("app"),
            marked: [dg('a')].into(),
        }));
    }

    #[test]
    fn snapshot_result_builds_preview() {
        let mut t = marked();
        t.act(Action::DeleteStart);
        let s = snap(imgs());
        let other = OpId(op(&t).0 + 9);
        t.ev(Event::DeleteSnapshot {
            op: other,
            result: Ok(s.clone()),
        });
        assert_eq!(stage(&t), DelStage::Snapshot);
        assert_eq!(t.s.key_mode(), KeyMode::Preview);

        snapshot_ev(&mut t, Ok(s.clone()));
        let want = planner::plan(t.s.marks().unwrap(), &s, t.s.registries[0].caps, false);
        assert_eq!(want.layers, vec![vec![dg('a')]]);
        assert_eq!(stage(&t), DelStage::Preview);
        assert_eq!(modal(&t).plan, Some(want));
        assert_eq!(modal(&t).snapshot, Some(s));
        assert_eq!(t.s.key_mode(), KeyMode::Preview);
    }

    #[test]
    fn snapshot_errors_block() {
        let e = RegistryError::NotFound;
        let inv_msg = user_message(&e, &ecr(), "images of app");
        for (err, want) in [
            (SnapshotError::UnreadableIndex, UNREADABLE_INDEX.to_owned()),
            (SnapshotError::Inventory(e), inv_msg),
        ] {
            let mut t = marked();
            t.act(Action::DeleteStart);
            snapshot_ev(&mut t, Err(err));
            assert_eq!(stage(&t), DelStage::Blocked(want.clone()));
            assert_eq!(t.s.key_mode(), KeyMode::Preview);
            let fx = press(&mut t, KeyCode::Enter);
            assert_eq!(stage(&t), DelStage::Blocked(want));
            assert!(snapshot_fx(&fx).is_none() && !has_delete(&fx));
            press(&mut t, KeyCode::Esc);
            assert_eq!(t.s.delete_modal(), None);
            assert_eq!(t.s.key_mode(), KeyMode::Normal);
        }
    }

    #[test]
    fn toggle_whole_replans_without_fetch() {
        let inv = vec![art('a', &["1", "2"]), art('b', &["3"])];
        let (mut t, _) = at_images(inv.clone());
        t.s.marks.insert(
            (rid("ecr"), rp("app")),
            Marks {
                tags: [(dg('a'), set(&["1"]))].into(),
                ..Marks::default()
            },
        );
        preview(&mut t, inv.clone());
        let (m, s, caps) = (marks(&t), snap(inv), t.s.registries[0].caps);
        let part = planner::plan(&m, &s, caps, false);
        let whole = planner::plan(&m, &s, caps, true);
        assert_ne!(part, whole);
        assert_eq!(modal(&t).plan.as_ref(), Some(&part));

        let fx = t.act(Action::ToggleWhole);
        assert!(modal(&t).whole);
        assert_eq!(modal(&t).plan.as_ref(), Some(&whole));
        assert!(fetched(&fx).is_empty() && snapshot_fx(&fx).is_none());
        t.act(Action::ToggleWhole);
        assert!(!modal(&t).whole);
        assert_eq!(modal(&t).plan.as_ref(), Some(&part));

        t.act(Action::ToggleWhole);
        t.act(Action::Enter);
        assert_eq!(stage(&t), DelStage::Confirm);
        press(&mut t, KeyCode::Char('w'));
        assert_eq!(modal(&t).typed, "w");
        assert!(modal(&t).whole);
        assert_eq!(modal(&t).plan.as_ref(), Some(&whole));
    }

    #[test]
    fn confirm_gate() {
        // Partial selection without `delete_tag` or `w`: nothing to delete.
        let (mut t, _) = at_images(vec![art('a', &["1", "2"])]);
        t.s.marks.insert(
            (rid("ecr"), rp("app")),
            Marks {
                tags: [(dg('a'), set(&["1"]))].into(),
                ..Marks::default()
            },
        );
        preview(&mut t, vec![art('a', &["1", "2"])]);
        t.act(Action::Enter);
        assert_eq!(stage(&t), DelStage::Preview);
        assert_eq!(t.s.status(), Some((StatusKind::Info, "nothing to delete")));

        let mut t = marked();
        preview(&mut t, imgs());
        t.act(Action::Enter);
        assert_eq!(stage(&t), DelStage::Confirm);
        assert_eq!(t.s.key_mode(), KeyMode::Text);
        let first = op(&t);
        type_str(&mut t, "ap");
        let fx = t.act(Action::FilterApply);
        assert_eq!(stage(&t), DelStage::Confirm);
        assert!(snapshot_fx(&fx).is_none() && !t.s.write_running());

        type_str(&mut t, "p");
        let fx = t.act(Action::FilterApply);
        assert_eq!(stage(&t), DelStage::Revalidate);
        assert_ne!(op(&t), first);
        assert_eq!(snapshot_fx(&fx), Some((op(&t), [dg('a')].into())));
        assert!(t.s.write_running());

        let mut t = marked();
        preview(&mut t, imgs());
        t.act(Action::Enter);
        type_str(&mut t, "ap");
        t.act(Action::FilterCancel);
        assert_eq!(stage(&t), DelStage::Preview);
        assert_eq!(modal(&t).typed, "");
    }

    #[test]
    fn revalidation_same_runs() {
        // Index d(1) with child `a`, both marked; `c` (unmarked) has parent d(2).
        let inv = vec![
            art('a', &["1"]),
            art('c', &["3"]),
            art_d(1, &["idx"]),
            art_d(2, &["idx2"]),
        ];
        let s = DepSnapshot {
            parents: [(dg('a'), [d(1)].into()), (dg('c'), [d(2)].into())].into(),
            ..snap(inv.clone())
        };
        let (mut t, _) = at_images(inv);
        t.s.marks.insert(
            (rid("ecr"), rp("app")),
            Marks {
                tags: [(dg('a'), set(&["1"])), (d(1), set(&["idx"]))].into(),
                ..Marks::default()
            },
        );
        t.act(Action::DeleteStart);
        snapshot_ev(&mut t, Ok(s.clone()));
        t.act(Action::Enter);
        type_str(&mut t, "app");
        t.act(Action::FilterApply);
        let reval = op(&t);
        let fx = snapshot_ev(&mut t, Ok(s.clone()));

        let plan = planner::plan(&marks(&t), &s, t.s.registries[0].caps, false);
        assert_eq!(plan.layers, vec![vec![d(1)], vec![dg('a')]]);
        assert_eq!(stage(&t), DelStage::Running);
        assert_ne!(op(&t), reval);
        assert_eq!(
            fx.iter().find(|e| matches!(e, Effect::Delete { .. })),
            Some(&Effect::Delete {
                op: op(&t),
                registry: rid("ecr"),
                repo: rp("app"),
                plan,
                parents: [(dg('a'), [d(1)].into())].into(),
            })
        );
        assert!(t.s.write_running());
        assert_eq!(t.s.key_mode(), KeyMode::List);
    }

    #[test]
    fn revalidation_change_releases_lock() {
        let mut t = marked();
        confirmed(&mut t);
        let moved = vec![art('a', &["1", "9"]), art('b', &["2"]), art('c', &["3"])];
        let fx = snapshot_ev(&mut t, Ok(snap(moved)));
        let m = modal(&t);
        assert_eq!(
            (m.stage.clone(), m.changed, m.typed.as_str()),
            (DelStage::Preview, true, "")
        );
        assert!(!t.s.write_running());
        assert!(!has_delete(&fx));

        let mut t = marked();
        confirmed(&mut t);
        let fx = snapshot_ev(&mut t, Err(SnapshotError::UnreadableIndex));
        assert_eq!(stage(&t), DelStage::Blocked(UNREADABLE_INDEX.into()));
        assert!(!t.s.write_running() && !has_delete(&fx));

        let mut t = marked();
        confirmed(&mut t);
        let late = op(&t);
        press(&mut t, KeyCode::Esc);
        assert_eq!(stage(&t), DelStage::Preview);
        assert_eq!(modal(&t).typed, "");
        assert!(!t.s.write_running());
        let fx = t.ev(Event::DeleteSnapshot {
            op: late,
            result: Ok(snap(imgs())),
        });
        assert_eq!(stage(&t), DelStage::Preview);
        assert!(!has_delete(&fx) && !t.s.write_running());
    }

    #[test]
    fn write_lock_shared_with_push() {
        let mut t = marked();
        confirmed(&mut t);
        let push = |t: &mut T| {
            t.s.start_push(rid("ecr"), rp("web"), "img".into(), "x".into())
        };
        assert_eq!(push(&mut t), Err(BUSY.into()));
        snapshot_ev(&mut t, Ok(snap(imgs())));
        assert_eq!(stage(&t), DelStage::Running);
        assert_eq!(push(&mut t), Err(BUSY.into()));

        let mut t = marked();
        preview(&mut t, imgs());
        t.act(Action::Enter);
        type_str(&mut t, "app");
        push(&mut t).unwrap();
        let fx = t.act(Action::FilterApply);
        assert_eq!(t.s.status(), Some((StatusKind::Error, BUSY)));
        assert_eq!(stage(&t), DelStage::Confirm);
        assert!(snapshot_fx(&fx).is_none());
    }

    #[test]
    fn running_hide_and_reopen() {
        for hide in [Action::Enter, Action::Back] {
            let mut t = running();
            let before = modal(&t).clone();
            t.act(Action::ToggleWhole);
            assert_eq!(modal(&t), &before);
            t.act(hide);
            assert!(modal(&t).hidden, "{hide:?}");
        }

        let mut t = running();
        ctrl_c_quits(&mut t);
        press(&mut t, KeyCode::Esc);
        assert_eq!(t.s.key_mode(), KeyMode::Normal);
        assert!(t.s.write_running());
        assert_eq!(
            t.s.start_push(rid("ecr"), rp("web"), "img".into(), "x".into()),
            Err(BUSY.into())
        );
        assert_eq!(
            t.s.delete_status().as_deref(),
            Some("deleting in app… — press d to show")
        );
        ctrl_c_quits(&mut t);
        t.act(Action::Down);
        assert_eq!(t.s.selected_image().unwrap().digest, dg('b'));
        t.act(Action::PrevPane);
        t.act(Action::Down);
        assert_eq!(t.s.selected_repo().unwrap().path, rp("web"));
        t.act(Action::NextPane);
        assert_eq!(t.s.focus(), Pane::Images);
        t.act(Action::PrevPane);

        let fx = press(&mut t, KeyCode::Char('d'));
        assert!(!modal(&t).hidden);
        assert_eq!(stage(&t), DelStage::Running);
        assert_eq!(t.s.key_mode(), KeyMode::List);
        assert!(snapshot_fx(&fx).is_none());
        assert_eq!(t.s.delete_status(), None, "shown");
    }

    #[test]
    fn done_while_hidden_waits_for_d() {
        let mut t = running();
        t.act(Action::Back);
        let r = report(vec![(del('a'), OpOutcome::Done)]);
        done(&mut t, r.clone());
        let want = DelStage::Report {
            report: r,
            stopped: None,
            refreshed: false,
        };
        assert_eq!(stage(&t), want);
        assert!(modal(&t).hidden);
        assert_eq!(t.s.key_mode(), KeyMode::Normal);
        assert!(!t.s.write_running());
        assert_eq!(
            t.s.delete_status().as_deref(),
            Some("deleted: 1 done — press d for the report")
        );
        assert_eq!(
            t.s.status(),
            Some((StatusKind::Ok, "deleted: 1 done — press d for the report"))
        );
        // The hint outlives other status messages.
        t.s.set_status(StatusKind::Info, "other".into());
        assert_eq!(
            t.s.delete_status().as_deref(),
            Some("deleted: 1 done — press d for the report")
        );

        let fx = t.act(Action::DeleteStart);
        assert!(!modal(&t).hidden && snapshot_fx(&fx).is_none());
        assert_eq!(stage(&t), want);
        assert_eq!(t.s.key_mode(), KeyMode::List);
        t.act(Action::Enter);
        assert_eq!(t.s.delete_modal(), None);

        t.ok(inv("ecr", "app"), CacheValue::Inventory(imgs()));
        t.act(Action::Mark);
        let fx = t.act(Action::DeleteStart);
        assert_eq!(stage(&t), DelStage::Snapshot);
        assert!(snapshot_fx(&fx).is_some());
    }

    #[test]
    fn done_reports_clears_and_refreshes() {
        let (mut t, _) = at_images(imgs());
        // Settle the focus fetches of `a`, then `b`'s (selected next).
        let nf = || Err(RegistryError::NotFound);
        let key =
            |k: fn(RegistryId, RepoPath, Digest) -> CacheKey, c| k(rid("ecr"), rp("app"), dg(c));
        for c in ['a', 'b'] {
            if c == 'b' {
                t.act(Action::Down);
            }
            t.load(key(CacheKey::Manifest, c), nf());
            t.load(key(CacheKey::Referrers, c), nf());
        }
        t.act(Action::Up);
        t.act(Action::Mark);
        t.s.marks
            .entry((rid("ecr"), rp("web")))
            .or_default()
            .tags
            .insert(dg('b'), set(&["x"]));
        confirmed(&mut t);
        snapshot_ev(&mut t, Ok(snap(imgs())));

        let stale = OpId(op(&t).0 + 9);
        let r = report(vec![
            (del('a'), OpOutcome::Done),
            (
                DeleteOp::RemoveTag {
                    tag: "2".into(),
                    expected: dg('b'),
                },
                OpOutcome::Done,
            ),
            (del('c'), OpOutcome::Done),
        ]);
        let fx = t.ev(Event::DeleteDone {
            op: stale,
            report: r.clone(),
            stopped: None,
        });
        assert_eq!(stage(&t), DelStage::Running);
        assert!(fetched(&fx).is_empty());

        let fx = done(&mut t, r);
        assert!(matches!(stage(&t), DelStage::Report { .. }));
        assert_eq!(t.s.marks(), None);
        assert!(
            t.s.marks.contains_key(&(rid("ecr"), rp("web"))),
            "other repo kept"
        );
        let f: std::collections::HashSet<CacheKey> = fetched(&fx).into_iter().collect();
        let want = [
            inv("ecr", "app"),
            key(CacheKey::Manifest, 'b'),
            key(CacheKey::Referrers, 'b'),
        ];
        // Deleted manifests are not refetched: they would only answer "not found".
        assert_eq!(f, want.into_iter().collect());
        assert_eq!(t.s.status(), Some((StatusKind::Ok, "deleted: 3 done")));

        let mut t = running();
        done(
            &mut t,
            report(vec![
                (del('a'), OpOutcome::Done),
                (del('b'), OpOutcome::Done),
                (
                    del('c'),
                    OpOutcome::Failed("ImageReferencedByManifestList".into()),
                ),
            ]),
        );
        assert_eq!(
            t.s.status(),
            Some((
                StatusKind::Error,
                "delete finished: 2 done, 1 failed, 0 skipped, 0 uncertain, 0 not attempted"
            ))
        );
    }

    #[test]
    fn unsupported_disables_registry() {
        let mut t = running();
        done(
            &mut t,
            DeleteReport {
                results: vec![(
                    del('a'),
                    OpOutcome::Failed("deletion disabled on this registry".into()),
                )],
                aborted_unsupported: true,
            },
        );
        assert!(t.s.delete_disabled.contains(&rid("ecr")));
        assert_eq!(
            t.s.status(),
            Some((StatusKind::Error, "deletion disabled on ecr"))
        );
        t.act(Action::Enter);
        assert_eq!(t.s.delete_modal(), None);
        assert!(!t.s.can_delete());
        t.act(Action::Mark);
        let fx = t.act(Action::DeleteStart);
        assert_eq!((t.s.delete_modal(), fx), (None, vec![]));
    }

    #[test]
    fn observed_rules() {
        use Inventory::{Complete, TaggedOnly};
        let inv = [art('a', &["1"]), art('b', &[])];
        let tag = |t: &str, c| DeleteOp::RemoveTag {
            tag: t.into(),
            expected: dg(c),
        };
        let moved = format!("tag moved to {}", dg('a').short());
        for i in [Complete, TaggedOnly] {
            let o = |op: &DeleteOp| observed(op, &inv, i);
            assert_eq!(o(&tag("1", 'a')).as_deref(), Some("tag still present"));
            assert_eq!(o(&tag("1", 'b')), Some(moved.clone()), "re-pushed");
            assert_eq!(o(&tag("2", 'a')).as_deref(), Some("tag gone"));
            assert_eq!(o(&del('a')).as_deref(), Some("still present"));
            assert_eq!(o(&del('b')).as_deref(), Some("still present"));
        }
        assert_eq!(observed(&del('c'), &inv, Complete).as_deref(), Some("gone"));
        assert_eq!(observed(&del('c'), &inv, TaggedOnly), None);
    }

    fn refreshed(t: &T) -> bool {
        match stage(t) {
            DelStage::Report { refreshed, .. } => refreshed,
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn observed_only_after_fresh_inventory() {
        // The run's inventory refresh issues a fetch; its result is fresh.
        let mut t = running();
        done(
            &mut t,
            report(vec![(del('a'), OpOutcome::Uncertain("timeout".into()))]),
        );
        assert!(!refreshed(&t));
        t.ok(inv("ecr", "app"), CacheValue::Inventory(imgs()));
        assert!(refreshed(&t));

        // An inventory fetch in flight when the run ends is stale: wait for the refetch.
        let mut t = running();
        press(&mut t, KeyCode::Esc);
        t.act(Action::PrevPane);
        t.act(Action::Refresh);
        let fx = done(
            &mut t,
            report(vec![(del('a'), OpOutcome::Uncertain("timeout".into()))]),
        );
        assert!(
            !fetched(&fx).contains(&inv("ecr", "app")),
            "refetch flag only"
        );
        let fx = t.ok(inv("ecr", "app"), CacheValue::Inventory(imgs()));
        assert!(fetched(&fx).contains(&inv("ecr", "app")), "refetch issued");
        assert!(!refreshed(&t));
        t.ok(inv("ecr", "app"), CacheValue::Inventory(imgs()));
        assert!(refreshed(&t));
    }

    #[test]
    fn report_closes() {
        for close in [KeyCode::Enter, KeyCode::Esc] {
            let mut t = running();
            done(&mut t, report(vec![(del('a'), OpOutcome::Done)]));
            assert_eq!(t.s.key_mode(), KeyMode::List);
            press(&mut t, close);
            assert_eq!(t.s.delete_modal(), None, "{close:?}");
            assert_eq!(t.s.key_mode(), KeyMode::Normal);
        }
    }

    #[test]
    fn confirm_keys_end_to_end() {
        let (mut t, _) = T::new(vec![ecr()]);
        t.ok(CacheKey::Repos(rid("ecr")), repos(&["www-dq"]));
        t.act(Action::NextPane);
        t.ok(inv("ecr", "www-dq"), CacheValue::Inventory(imgs()));
        t.act(Action::NextPane);
        press(&mut t, KeyCode::Char(' '));

        press(&mut t, KeyCode::Char('d'));
        assert_eq!(stage(&t), DelStage::Snapshot);
        ctrl_c_quits(&mut t);
        snapshot_ev(&mut t, Ok(snap(imgs())));
        ctrl_c_quits(&mut t);
        press(&mut t, KeyCode::Char('w'));
        assert!(modal(&t).whole);
        press(&mut t, KeyCode::Char('w'));
        assert!(!modal(&t).whole);

        press(&mut t, KeyCode::Enter);
        assert_eq!(stage(&t), DelStage::Confirm);
        ctrl_c_quits(&mut t);
        let snap_op = op(&t);
        for c in "www-dq".chars() {
            let fx = press(&mut t, KeyCode::Char(c));
            assert!(!fx.contains(&Effect::Quit), "{c}");
        }
        let m = modal(&t);
        assert_eq!(
            (m.typed.as_str(), m.whole, m.op),
            ("www-dq", false, snap_op)
        );
        assert_eq!(stage(&t), DelStage::Confirm);

        let fx = press(&mut t, KeyCode::Enter);
        assert_eq!(stage(&t), DelStage::Revalidate);
        assert!(snapshot_fx(&fx).is_some());
        ctrl_c_quits(&mut t);
        snapshot_ev(&mut t, Ok(snap(imgs())));
        assert_eq!(stage(&t), DelStage::Running);
        ctrl_c_quits(&mut t);
        done(&mut t, report(vec![(del('a'), OpOutcome::Done)]));
        ctrl_c_quits(&mut t);

        press(&mut t, KeyCode::Enter);
        t.ok(inv("ecr", "www-dq"), CacheValue::Inventory(imgs()));
        press(&mut t, KeyCode::Char(' '));
        press(&mut t, KeyCode::Char('d'));
        snapshot_ev(&mut t, Err(SnapshotError::UnreadableIndex));
        assert!(matches!(stage(&t), DelStage::Blocked(_)));
        ctrl_c_quits(&mut t);
    }

    #[test]
    fn edit_config_refused_while_delete_modal_exists() {
        let mut t = running();
        press(&mut t, KeyCode::Esc);
        assert_eq!(press(&mut t, KeyCode::Char('e')), vec![]);
        assert_eq!(t.s.status(), Some((StatusKind::Error, BUSY)));

        let mut t = running();
        press(&mut t, KeyCode::Esc);
        t.s.delete_disabled.insert(rid("ecr"));
        done(&mut t, report(vec![(del('a'), OpOutcome::Done)]));
        assert_eq!(press(&mut t, KeyCode::Char('e')), vec![]);
        assert_eq!(
            t.s.status(),
            Some((StatusKind::Error, "close the delete report first (press d)"))
        );
        assert!(matches!(stage(&t), DelStage::Report { .. }));
        assert!(t.s.delete_disabled.contains(&rid("ecr")));

        press(&mut t, KeyCode::Char('d'));
        press(&mut t, KeyCode::Enter);
        assert_eq!(press(&mut t, KeyCode::Char('e')), vec![Effect::EditConfig]);
    }

    #[test]
    fn quit_while_running_needs_second_q() {
        let quits = |fx: Vec<Effect>| fx.contains(&Effect::Quit);
        let mut t = running();
        press(&mut t, KeyCode::Esc);
        assert!(!quits(press(&mut t, KeyCode::Char('q'))));
        assert_eq!(t.s.status(), Some((StatusKind::Info, QUIT_HINT)));
        t.act(Action::ClampScroll(0));
        assert!(quits(press(&mut t, KeyCode::Char('q'))));

        // Another action disarms; Ctrl-C quits at once.
        let mut t = running();
        press(&mut t, KeyCode::Esc);
        press(&mut t, KeyCode::Char('q'));
        t.act(Action::Down);
        assert!(!quits(press(&mut t, KeyCode::Char('q'))));
        ctrl_c_quits(&mut t);

        // No write running: q quits.
        let mut t = marked();
        assert!(quits(press(&mut t, KeyCode::Char('q'))));
    }
}

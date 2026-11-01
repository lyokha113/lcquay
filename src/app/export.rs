//! The export modal (`X`): details (`a` toggles all platforms of an index), the target path,
//! an overwrite prompt when the file exists, then the running export and its result. Local
//! file output only, so outside the write lock; one export at a time.

use time::OffsetDateTime;

use super::state::{Action, CacheKey, CacheValue, Effect, Pane, State, StatusKind};
use super::transfer::{Transfer, TransferKind, TransferOutcome};
use super::write::OpId;
use crate::domain::{
    ArtifactStatus, Digest, ManifestKind, Platform, RegistryError, RegistryId, RepoPath,
    TransferStep,
};
use crate::export::default_path;

/// Shown while all platforms are selected.
pub const ALL_NOTE: &str =
    "all platforms: for skopeo/crane/regctl, and docker load on the containerd image store only";

#[derive(Clone, Debug, PartialEq)]
pub enum ExportStage {
    Details,
    /// Typing the target path.
    Path,
    /// The target exists: `y` replaces it, `n` goes back to the path.
    Overwrite,
    Running,
    Result(TransferOutcome),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExportModal {
    pub registry: RegistryId,
    pub repo: RepoPath,
    /// The selected (top-level) manifest.
    pub digest: Digest,
    /// The selected child of an index; `None` for a single image.
    pub child: Option<Digest>,
    pub platform: Option<Platform>,
    /// Every platform of the index instead of `child`.
    pub all: bool,
    /// As typed (`~/` is expanded by the runtime).
    pub path: String,
    /// `<ref_host>/<repo>:<tag>` or `@<digest>`.
    pub image: String,
    pub tag: Option<String>,
    pub stage: ExportStage,
    pub op: Option<OpId>,
    pub transfer: Option<Transfer>,
    /// Running on with the modal closed (Enter); `X` shows it again.
    pub hidden: bool,
}

impl ExportModal {
    /// Config + layers of the exported image once its manifest is loaded; `None` for all
    /// platforms.
    pub fn size(&self, s: &State) -> Option<u64> {
        if self.all {
            return None;
        }
        let d = self.child.as_ref().unwrap_or(&self.digest);
        let key = CacheKey::Manifest(self.registry.clone(), self.repo.clone(), d.clone());
        match s.value(&key)? {
            CacheValue::Manifest(m) => match &m.kind {
                ManifestKind::Image { config, layers } => {
                    Some(config.size + layers.iter().map(|l| l.size).sum::<u64>())
                }
                ManifestKind::Index { .. } => None,
            },
            _ => None,
        }
    }
}

impl State {
    /// The export modal, also while hidden (the UI draws it only when shown).
    pub fn export_modal(&self) -> Option<&ExportModal> {
        self.export.as_ref()
    }

    pub(super) fn export_running(&self) -> bool {
        self.export
            .as_ref()
            .is_some_and(|m| m.stage == ExportStage::Running)
    }

    /// `X`: show a hidden export, else open the modal for the selected Active image on the
    /// Images pane once its manifest is loaded (it tells an index from an image).
    pub(super) fn export_start(&mut self) -> Vec<Effect> {
        if let Some(m) = self.export.as_mut().filter(|m| m.hidden) {
            m.hidden = false;
            return vec![];
        }
        if self.focus != Pane::Images {
            return vec![];
        }
        let (Some(reg), Some(repo), Some(img), Some(image)) = (
            self.selected_registry(),
            self.selected_repo(),
            self.selected_image(),
            self.image_reference(),
        ) else {
            return vec![];
        };
        if img.status != ArtifactStatus::Active {
            self.set_status(
                StatusKind::Error,
                "archived images cannot be exported".into(),
            );
            return vec![];
        }
        let (child, platform) = match self.manifest(&img.digest).map(|m| &m.kind) {
            None => {
                let m = "the image manifest is not loaded yet".into();
                self.set_status(StatusKind::Info, m);
                return vec![];
            }
            Some(ManifestKind::Index { manifests }) => {
                manifests.get(self.platform).map_or((None, None), |d| {
                    (Some(d.digest.clone()), d.platform.clone())
                })
            }
            Some(ManifestKind::Image { .. }) => (None, None),
        };
        let tag = img.tags.first().cloned();
        let path = default_path(&repo.path, tag.as_deref(), &img.digest);
        self.export = Some(ExportModal {
            registry: reg.id.clone(),
            repo: repo.path.clone(),
            digest: img.digest.clone(),
            child,
            platform,
            all: false,
            path,
            image,
            tag,
            stage: ExportStage::Details,
            op: None,
            transfer: None,
            hidden: false,
        });
        vec![]
    }

    /// A key while the export modal is shown.
    pub(super) fn export_action(&mut self, a: Action, now: OffsetDateTime) -> Vec<Effect> {
        let Some(mut m) = self.export.take() else {
            return vec![];
        };
        let mut fx = vec![];
        match (&m.stage, a) {
            (ExportStage::Details, Action::ToggleAll) if m.child.is_some() => m.all = !m.all,
            (ExportStage::Details, Action::Enter) => m.stage = ExportStage::Path,
            (ExportStage::Path, Action::FilterChar(c)) => m.path.push(c),
            (ExportStage::Path, Action::FilterBackspace) => {
                m.path.pop();
            }
            (ExportStage::Path, Action::FilterCancel) => m.stage = ExportStage::Details,
            (ExportStage::Path, Action::FilterApply) if !m.path.trim().is_empty() => {
                fx.push(self.export_effect(&mut m, false, now));
            }
            (ExportStage::Overwrite, Action::Confirm(true)) => {
                fx.push(self.export_effect(&mut m, true, now));
            }
            (ExportStage::Overwrite, Action::Confirm(false)) => m.stage = ExportStage::Path,
            (ExportStage::Running, Action::Enter) => {
                let msg = format!("exporting {}… — press X to show", m.image);
                self.set_status(StatusKind::Info, msg);
                m.hidden = true;
            }
            (ExportStage::Running, Action::Back) => fx.extend(m.op.map(Effect::Cancel)),
            // Esc on the details, Enter / Esc on the result: close.
            (ExportStage::Details, Action::Back)
            | (ExportStage::Result(_), Action::Enter | Action::Back) => return vec![],
            _ => {}
        }
        self.export = Some(m);
        fx
    }

    fn export_effect(
        &mut self,
        m: &mut ExportModal,
        overwrite: bool,
        now: OffsetDateTime,
    ) -> Effect {
        self.next_op += 1;
        let op = OpId(self.next_op);
        m.op = Some(op);
        m.stage = ExportStage::Running;
        m.transfer = Some(Transfer::new(TransferKind::Export, vec![], now));
        Effect::Export {
            op,
            registry: m.registry.clone(),
            repo: m.repo.clone(),
            top: m.digest.clone(),
            platform: if m.all { None } else { m.child.clone() },
            tag: m.tag.clone(),
            full_ref: m.image.clone(),
            path: m.path.clone(),
            overwrite,
        }
    }

    /// The running export's modal, if `op` is it; events for any other op are stale.
    fn running_export(&mut self, op: OpId) -> Option<&mut ExportModal> {
        self.export
            .as_mut()
            .filter(|m| m.op == Some(op) && m.stage == ExportStage::Running)
    }

    pub(super) fn export_progress(&mut self, op: OpId, step: TransferStep, now: OffsetDateTime) {
        if let Some(t) = self.running_export(op).and_then(|m| m.transfer.as_mut()) {
            t.apply(step, now);
        }
    }

    /// `Exists` asks to overwrite (showing a hidden modal); any other outcome is shown in the
    /// modal, or on the status line when hidden, dropping the modal.
    pub(super) fn export_done(&mut self, op: OpId, outcome: TransferOutcome, now: OffsetDateTime) {
        let Some(m) = self.running_export(op) else {
            return;
        };
        if outcome == TransferOutcome::Exists {
            m.stage = ExportStage::Overwrite;
            (m.op, m.transfer, m.hidden) = (None, None, false);
            return;
        }
        if let Some(t) = &mut m.transfer {
            t.finished = Some(now);
        }
        m.stage = ExportStage::Result(outcome);
        if m.hidden {
            self.status = self.export_result().map(|(k, msg)| (None, k, msg));
            self.export = None;
        }
    }

    /// The finished export's result line.
    pub fn export_result(&self) -> Option<(StatusKind, String)> {
        let m = self.export.as_ref()?;
        let ExportStage::Result(o) = &m.stage else {
            return None;
        };
        Some(match o {
            TransferOutcome::Done => {
                let mut s = format!("exported to {}", m.path);
                s += &m
                    .transfer
                    .as_ref()
                    .map(Transfer::summary)
                    .unwrap_or_default();
                (StatusKind::Ok, s)
            }
            TransferOutcome::Cancelled => (StatusKind::Info, "export cancelled".into()),
            TransferOutcome::Failed(e) => {
                let m = match e {
                    RegistryError::Provider(m) => m.clone(),
                    e => e.to_string(),
                };
                (StatusKind::Error, format!("export failed: {m}"))
            }
            // Copy outcomes; an export never ends with these (`Exists` asks to overwrite).
            TransferOutcome::Uncertain(_)
            | TransferOutcome::DigestDiffers { .. }
            | TransferOutcome::Exists => (StatusKind::Error, "export ended unexpectedly".into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::{Event, KeyMode};
    use crate::app::update::tests::{T, art, at_images, desc, dg, man, manifest, rid, rp};
    use crate::app::write::RELOAD_BUSY;
    use crate::domain::ItemStep;

    fn plat(arch: &str) -> Platform {
        Platform {
            os: "linux".into(),
            architecture: arch.into(),
            variant: None,
        }
    }

    /// Images pane of ecr/app with `app:1.0` (digest `a`), an index of `d` (amd64) and `e`
    /// (arm64); `e` is the selected platform.
    fn on_index() -> T {
        let (mut t, _) = at_images(vec![art('a', &["1.0"])]);
        let child = |c, arch| {
            let mut d = desc(c, 500);
            d.platform = Some(plat(arch));
            d
        };
        let idx = ManifestKind::Index {
            manifests: vec![child('d', "amd64"), child('e', "arm64")],
        };
        t.ok(man("ecr", 'a'), manifest(idx));
        t.act(Action::PlatformNext);
        t
    }

    fn modal(t: &T) -> &ExportModal {
        t.s.export_modal().expect("export modal")
    }

    fn stage(t: &T) -> Option<ExportStage> {
        t.s.export_modal().map(|m| m.stage.clone())
    }

    fn typ(t: &mut T, s: &str) {
        for c in s.chars() {
            t.act(Action::FilterChar(c));
        }
    }

    fn effect(op: u64, platform: Option<char>, path: &str, overwrite: bool) -> Vec<Effect> {
        vec![Effect::Export {
            op: OpId(op),
            registry: rid("ecr"),
            repo: rp("app"),
            top: dg('a'),
            platform: platform.map(dg),
            tag: Some("1.0".into()),
            full_ref: "h.example/app:1.0".into(),
            path: path.into(),
            overwrite,
        }]
    }

    fn done(op: u64, outcome: TransferOutcome) -> Event {
        Event::ExportDone {
            op: OpId(op),
            outcome,
        }
    }

    /// `on_index`, `X`, Enter, Enter: export op 1 of platform `e` to the default path.
    fn running() -> T {
        let mut t = on_index();
        t.act(Action::ExportStart);
        t.act(Action::Enter);
        let fx = t.act(Action::FilterApply);
        assert_eq!(fx, effect(1, Some('e'), "~/Downloads/app_1.0.tar", false));
        t
    }

    /// A hidden export asking to overwrite while the glob prompt is open: the prompt that is
    /// drawn on top takes the keys (render, `key_mode` and `update` share one order).
    #[test]
    fn overwrite_prompt_takes_keys_over_glob() {
        let mut t = running();
        t.act(Action::Enter);
        assert!(modal(&t).hidden);
        t.act(Action::MarkGlob);
        assert!(t.s.glob().is_some());
        t.ev(done(1, TransferOutcome::Exists));
        assert_eq!(t.s.key_mode(), KeyMode::Confirm);
        let fx = t.act(Action::Confirm(true));
        assert_eq!(fx, effect(2, Some('e'), "~/Downloads/app_1.0.tar", true));
        assert!(t.s.glob().is_some(), "the glob prompt is still open below");
    }

    #[test]
    fn export_flow() {
        let mut t = on_index();
        assert!(t.act(Action::ExportStart).is_empty());
        let m = modal(&t);
        assert_eq!(m.stage, ExportStage::Details);
        assert_eq!(
            (m.child.clone(), m.platform.clone()),
            (Some(dg('e')), Some(plat("arm64")))
        );
        assert_eq!((m.image.as_str(), m.all), ("h.example/app:1.0", false));
        assert_eq!(t.s.key_mode(), KeyMode::ExportDetails);

        t.act(Action::ToggleAll);
        assert!(modal(&t).all);
        t.act(Action::ToggleAll);
        assert!(!modal(&t).all);
        t.act(Action::ToggleAll);

        // Path: prefilled, edited by typing; Esc goes back to the details.
        assert!(t.act(Action::Enter).is_empty());
        assert_eq!(stage(&t), Some(ExportStage::Path));
        assert_eq!(t.s.key_mode(), KeyMode::Text);
        assert_eq!(modal(&t).path, "~/Downloads/app_1.0.tar");
        for _ in 0..4 {
            t.act(Action::FilterBackspace);
        }
        typ(&mut t, "-all.tar");
        t.act(Action::FilterCancel);
        assert_eq!(stage(&t), Some(ExportStage::Details));
        assert!(modal(&t).all, "kept");
        t.act(Action::Enter);
        assert_eq!(modal(&t).path, "~/Downloads/app_1.0-all.tar", "kept");

        // All platforms: no child.
        let fx = t.act(Action::FilterApply);
        assert_eq!(fx, effect(1, None, "~/Downloads/app_1.0-all.tar", false));
        assert_eq!(stage(&t), Some(ExportStage::Running));
        assert_eq!(t.s.key_mode(), KeyMode::List);
        assert!(t.s.transfer_running());

        // The target exists: y/n prompt; n goes back to the path, y overwrites.
        t.ev(done(1, TransferOutcome::Exists));
        assert_eq!(stage(&t), Some(ExportStage::Overwrite));
        assert_eq!(t.s.key_mode(), KeyMode::Confirm);
        assert!(!t.s.transfer_running());
        assert!(t.act(Action::Confirm(false)).is_empty());
        assert_eq!(stage(&t), Some(ExportStage::Path));
        t.act(Action::FilterApply);
        t.ev(done(2, TransferOutcome::Exists));
        let fx = t.act(Action::Confirm(true));
        assert_eq!(fx, effect(3, None, "~/Downloads/app_1.0-all.tar", true));
        assert_eq!(stage(&t), Some(ExportStage::Running));

        // Progress of a stale op is ignored.
        let step = |op| Event::ExportProgress {
            op: OpId(op),
            step: TransferStep {
                id: "dddddddddddd".into(),
                step: ItemStep::Waiting { total: Some(10) },
            },
        };
        t.ev(step(2));
        assert!(modal(&t).transfer.as_ref().unwrap().items.is_empty());
        t.ev(step(3));
        assert_eq!(modal(&t).transfer.as_ref().unwrap().items.len(), 1);

        t.ev(done(3, TransferOutcome::Done));
        assert_eq!(stage(&t), Some(ExportStage::Result(TransferOutcome::Done)));
        let (kind, msg) = t.s.export_result().unwrap();
        assert_eq!(kind, StatusKind::Ok);
        assert!(
            msg.starts_with("exported to ~/Downloads/app_1.0-all.tar · 10B"),
            "{msg}"
        );
        t.act(Action::Enter);
        assert!(t.s.export_modal().is_none());
    }

    #[test]
    fn export_single_image_has_no_platform() {
        let (mut t, _) = at_images(vec![art('a', &[])]);
        assert!(t.act(Action::ExportStart).is_empty());
        assert!(t.s.export_modal().is_none(), "manifest not loaded");
        assert_eq!(
            t.s.status(),
            Some((StatusKind::Info, "the image manifest is not loaded yet"))
        );
        let img = ManifestKind::Image {
            config: desc('c', 10),
            layers: vec![desc('1', 90)],
        };
        t.ok(man("ecr", 'a'), manifest(img));
        t.act(Action::ExportStart);
        let m = modal(&t);
        assert_eq!((m.child.clone(), m.tag.clone()), (None, None));
        assert_eq!(m.size(&t.s), Some(100));
        assert_eq!(m.path, format!("~/Downloads/app_{}.tar", dg('a').short()));
        t.act(Action::ToggleAll);
        assert!(!modal(&t).all, "nothing to toggle");
        t.act(Action::Enter);
        let fx = t.act(Action::FilterApply);
        assert!(
            matches!(
                &fx[..],
                [Effect::Export {
                    platform: None,
                    tag: None,
                    ..
                }]
            ),
            "{fx:?}"
        );
    }

    #[test]
    fn export_hide_cancel_and_result_while_hidden() {
        let mut t = running();
        t.act(Action::Enter);
        assert!(modal(&t).hidden);
        assert_eq!(t.s.key_mode(), KeyMode::Normal);
        assert_eq!(
            t.s.status(),
            Some((
                StatusKind::Info,
                "exporting h.example/app:1.0… — press X to show"
            ))
        );
        t.act(Action::ExportStart);
        assert!(!modal(&t).hidden);
        assert_eq!(t.act(Action::Back), vec![Effect::Cancel(OpId(1))]);
        t.act(Action::Enter);
        t.ev(done(1, TransferOutcome::Cancelled));
        assert!(t.s.export_modal().is_none());
        assert_eq!(t.s.status(), Some((StatusKind::Info, "export cancelled")));

        let mut t = running();
        let fail = RegistryError::Provider("digest mismatch for 111111111111".into());
        t.ev(done(1, TransferOutcome::Failed(fail)));
        assert_eq!(
            t.s.export_result(),
            Some((
                StatusKind::Error,
                "export failed: digest mismatch for 111111111111".into()
            ))
        );
        t.act(Action::Back);
        assert!(t.s.export_modal().is_none());
    }

    #[test]
    fn export_one_at_a_time() {
        let mut t = running();
        t.act(Action::Enter);
        assert!(
            t.act(Action::ExportStart).is_empty(),
            "reopens, starts nothing"
        );
        let m = modal(&t);
        assert_eq!(
            (m.hidden, m.op, &m.stage),
            (false, Some(OpId(1)), &ExportStage::Running)
        );
    }

    #[test]
    fn reload_refused_while_export_runs() {
        let mut t = running();
        t.act(Action::Enter);
        assert!(t.act(Action::EditConfig).is_empty());
        assert_eq!(t.s.status(), Some((StatusKind::Error, RELOAD_BUSY)));
        t.ev(done(1, TransferOutcome::Done));
        assert_eq!(t.act(Action::EditConfig), vec![Effect::EditConfig]);
    }

    #[test]
    fn export_refuses_archived() {
        let mut old = art('a', &["1.0"]);
        old.status = ArtifactStatus::Archived;
        let (mut t, _) = at_images(vec![old]);
        assert!(t.act(Action::ExportStart).is_empty());
        assert_eq!(
            t.s.status(),
            Some((StatusKind::Error, "archived images cannot be exported"))
        );
    }
}

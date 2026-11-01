//! App state and the read-only views the UI renders from.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use time::OffsetDateTime;
use url::Url;

use super::cache::{Cache, Entry};
use super::compare::{CompareModal, Side};
use super::copy::{CopyStage, CopyWizard};
use super::delete::{DelStage, DeleteModal};
use super::export::{ExportModal, ExportStage};
use super::mask::mask_config_json;
use super::push::{Stage, Wizard};
use super::regform::RegForm;
use super::transfer::TransferOutcome;
use super::write::{OpId, PendingFocus, PullModal, PushOutcome, WriteOp};
use crate::config::{AuthCfg, RegistryCfg, Theme};
use crate::config_edit::RegistryForm;
use crate::conntest::Check;
use crate::docker::PushStep;
use crate::domain::{
    ArtifactStatus, ArtifactSummary, Caps, DeletePlan, DeleteReport, DepSnapshot, Descriptor,
    Details, Digest, ImageConfig, LocalImage, ManifestDocument, ManifestKind, Marks, Reference,
    RegistryError, RegistryId, RepoPath, RepoSummary, SnapshotError, TagPolicy, TransferStep,
    image_ref,
};
use crate::reqlog::RequestEntry;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CacheKey {
    Repos(RegistryId),
    Inventory(RegistryId, RepoPath),
    /// Manifest by digest.
    Manifest(RegistryId, RepoPath, Digest),
    /// Config of the image manifest with this digest (resolved from that manifest).
    Config(RegistryId, RepoPath, Digest),
    Referrers(RegistryId, RepoPath, Digest),
    /// Mutability of this tag in the repository (push preflight).
    TagPolicy(RegistryId, RepoPath, String),
    RegistryDetails(RegistryId),
    RepoDetails(RegistryId, RepoPath),
    LocalImages,
}

impl CacheKey {
    pub fn registry(&self) -> Option<&RegistryId> {
        match self {
            CacheKey::Repos(r)
            | CacheKey::Inventory(r, _)
            | CacheKey::Manifest(r, _, _)
            | CacheKey::Config(r, _, _)
            | CacheKey::Referrers(r, _, _)
            | CacheKey::TagPolicy(r, _, _)
            | CacheKey::RegistryDetails(r)
            | CacheKey::RepoDetails(r, _) => Some(r),
            CacheKey::LocalImages => None,
        }
    }

    pub(super) fn repo(&self) -> Option<&RepoPath> {
        match self {
            CacheKey::Inventory(_, p)
            | CacheKey::Manifest(_, p, _)
            | CacheKey::Config(_, p, _)
            | CacheKey::Referrers(_, p, _)
            | CacheKey::TagPolicy(_, p, _)
            | CacheKey::RepoDetails(_, p) => Some(p),
            CacheKey::Repos(_) | CacheKey::RegistryDetails(_) | CacheKey::LocalImages => None,
        }
    }

    pub(super) fn digest(&self) -> Option<&Digest> {
        match self {
            CacheKey::Manifest(_, _, d)
            | CacheKey::Config(_, _, d)
            | CacheKey::Referrers(_, _, d) => Some(d),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CacheValue {
    Repos(Vec<RepoSummary>),
    Inventory(Vec<ArtifactSummary>),
    Manifest(ManifestDocument),
    Config(Option<ImageConfig>),
    /// `None` = unknown (referrers could not be determined).
    Referrers(Option<Vec<Descriptor>>),
    LocalImages(Vec<LocalImage>),
    TagPolicy(TagPolicy),
    Details(Details),
}

/// How the status line shows a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Ok,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pane {
    Registries,
    Repos,
    Images,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Layers,
    Config,
    History,
    Raw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Tag,
    Pushed,
    Size,
    Pulled,
    Name,
    Count,
}

#[derive(Clone, Debug)]
pub struct RegistryView {
    pub id: RegistryId,
    pub ref_host: String,
    pub endpoint: Url,
    pub provider: &'static str,
    pub auth: AuthCfg,
    pub caps: Caps,
    pub auth_failed: bool,
    /// The registry's credential can be given to `docker login`.
    pub can_login: bool,
    /// `docker login` run by the app this session (in memory only).
    pub login: Option<LoginState>,
    /// Config `pinned` repositories, in config order (always shown first).
    pub pinned: Vec<RepoSummary>,
}

/// A row of the Repos pane (`State::repo_rows`).
#[derive(Clone, Debug, PartialEq)]
pub enum RepoRow<'a> {
    Repo {
        repo: &'a RepoSummary,
        depth: usize,
        pinned: bool,
    },
    /// The repos under path prefix `prefix` (its identity); `label` is what is drawn.
    Group {
        prefix: String,
        label: String,
        count: usize,
        depth: usize,
        expanded: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum LoginState {
    Running,
    Done { expires_at: Option<OffsetDateTime> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    NextPane,
    PrevPane,
    /// `←`/`h`: collapse the selected repo group, else the previous pane.
    Left,
    /// `→`/`l`: expand the selected repo group, else the next pane.
    Right,
    Enter,
    Tab(Tab),
    PlatformNext,
    PlatformPrev,
    ScrollDown,
    ScrollUp,
    /// Not on a key: `main` bounds the scroll to what the detail pane can show.
    ClampScroll(u16),
    Refresh,
    FilterStart,
    FilterChar(char),
    FilterBackspace,
    FilterApply,
    FilterCancel,
    SortCycle,
    SortReverse,
    Copy,
    ToggleReveal,
    ToggleHelp,
    Login,
    /// `p`: open the push wizard (or a running push's progress).
    PushStart,
    /// `P`: pull the selected image into local Docker.
    PullStart,
    /// `C`: copy / retag the selected image (or show a hidden running copy).
    CopyStart,
    /// `X`: export the selected image to an OCI layout tarball (or show a hidden export).
    ExportStart,
    /// `a` in the export details: all platforms of an index, or just the selected one.
    ToggleAll,
    /// `e`: edit the config file in `$VISUAL`/`$EDITOR`, then reload.
    EditConfig,
    /// `t`: open the theme picker.
    ThemePick,
    /// Esc in a wizard list stage: one stage back.
    Back,
    /// `space`: mark/unmark the focused manifest (Images pane).
    Mark,
    /// `u`: mark untagged manifests as `Automatic`.
    MarkUntagged,
    /// `a`: open the glob mark prompt.
    MarkGlob,
    /// Esc on the Images pane: cancel the range anchor, else clear the repo's marks.
    ClearMarks,
    /// `=`: mark the compare base, or compare the cursor image with it.
    CompareMark,
    /// `v`: anchor a range, or mark every image between the anchor and the cursor.
    RangeMark,
    /// `d`: start the delete preview.
    DeleteStart,
    /// `w` in the preview: toggle whole-manifest deletion.
    ToggleWhole,
    /// `!`: open the request log overlay.
    Requests,
    /// `c` on the Registries pane: edit the selected registry in the form.
    RegistryEdit,
    /// `n` on the Registries pane: a new registry in the form.
    RegistryNew,
    /// `x` on the Registries pane: remove the selected registry (asks first).
    RegistryRemove,
    /// Tab / ↓ in the form.
    FormNext,
    /// BackTab / ↑ in the form.
    FormPrev,
    /// Ctrl-T in the form: connection test.
    FormTest,
    /// Ctrl-S in the form.
    FormSave,
    /// The answer to a y/n prompt.
    Confirm(bool),
    /// `q`: asks again while a push or delete is running.
    Quit,
    /// Ctrl-C: quits at once.
    ForceQuit,
}

/// Which key map applies (`keys::key_to_action`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMode {
    Normal,
    /// Typing: a filter or the wizard's image filter / tag field.
    Text,
    /// A wizard list stage.
    List,
    Help,
    /// The delete preview (`w` toggles whole manifests, no typing).
    Preview,
    /// A read-only scrolling view: ↑↓ scroll, `/` filter, `y` copy, Esc closes.
    Scroll,
    /// The registry form: typing, field moves, ←→ choices, Ctrl-S save.
    Form,
    /// A y/n prompt.
    Confirm,
    /// The export details: `a` toggles all platforms, Enter goes on, Esc closes.
    ExportDetails,
}

/// The request log overlay's copy of the log (`main` refreshes it while the overlay is open).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestsView {
    /// Oldest first.
    pub entries: Vec<RequestEntry>,
    pub filter: String,
    pub filter_editing: bool,
    /// Rows scrolled up from the newest entry.
    pub scroll: u16,
}

impl RequestsView {
    /// Entries whose registry, op, path or outcome contains the filter.
    pub fn visible(&self) -> Vec<&RequestEntry> {
        self.entries
            .iter()
            .filter(|e| {
                [&e.registry, &e.op, &e.path, &e.outcome]
                    .iter()
                    .any(|f| f.contains(&self.filter))
            })
            .collect()
    }
}

// CacheValue is large (manifest/config inline); one value per fetch, so size is irrelevant.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Loaded {
        key: CacheKey,
        req: RequestId,
        result: Result<CacheValue, RegistryError>,
    },
    Progress {
        key: CacheKey,
        pages: usize,
    },
    CopyResult(Result<(), String>),
    /// Exactly one per `Effect::Login`; `Ok` carries the credential's expiry.
    LoginDone {
        registry: RegistryId,
        result: Result<Option<OffsetDateTime>, RegistryError>,
    },
    /// Ignored unless `op` is the current write op.
    PushProgress {
        op: OpId,
        step: PushStep,
    },
    /// Exactly one per `Effect::Push`.
    PushDone {
        op: OpId,
        outcome: PushOutcome,
    },
    /// Ignored unless `op` is the running pull.
    PullProgress {
        op: OpId,
        step: TransferStep,
    },
    /// Exactly one per `Effect::Pull`; an abort is `Cancelled`.
    PullDone {
        op: OpId,
        outcome: TransferOutcome,
    },
    /// Ignored unless `op` is the running copy.
    CopyProgress {
        op: OpId,
        step: TransferStep,
    },
    /// Exactly one per `Effect::CopyImage`; an abort is `Cancelled`, or `Uncertain` once a
    /// manifest upload was dispatched.
    CopyDone {
        op: OpId,
        outcome: TransferOutcome,
    },
    /// Ignored unless `op` is the running export.
    ExportProgress {
        op: OpId,
        step: TransferStep,
    },
    /// Exactly one per `Effect::Export`; an abort is `Cancelled`, an existing target without
    /// `overwrite` is `Exists`.
    ExportDone {
        op: OpId,
        outcome: TransferOutcome,
    },
    /// Exactly one per `Effect::DeleteSnapshot`.
    DeleteSnapshot {
        op: OpId,
        result: Result<DepSnapshot, SnapshotError>,
    },
    /// Exactly one per `Effect::Delete`; `stopped` says why the remaining ops are `NotAttempted`.
    DeleteDone {
        op: OpId,
        report: DeleteReport,
        stopped: Option<RegistryError>,
    },
    /// One connection-test step (`conntest::STEPS[step]`); ignored unless `op` is the form's test.
    ConnCheck {
        op: OpId,
        step: usize,
        check: Check,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Fetch {
        key: CacheKey,
        req: RequestId,
    },
    /// Keys of the selected item and its visible tab, for fetch prioritisation.
    SetFocused(Vec<CacheKey>),
    Copy(String),
    /// `docker login` to the registry's `ref_host`.
    Login(RegistryId),
    /// `docker tag <image_id> <ref_host>/<repo>:<tag>`, then push exactly that tag.
    Push {
        op: OpId,
        registry: RegistryId,
        repo: RepoPath,
        image_id: String,
        tag: String,
    },
    /// Aborts the running op `op` (push, pull, copy, export); a no-op once it has ended.
    Cancel(OpId),
    /// Pull `image` (a full reference on `registry`) into local Docker, for `platform`
    /// (`os/arch[/variant]`) when given.
    Pull {
        op: OpId,
        registry: RegistryId,
        image: String,
        platform: Option<String>,
    },
    /// Copies `src/src_repo@digest` to `dst/dst_repo:tag` (a retag when both are the same).
    CopyImage {
        op: OpId,
        src: RegistryId,
        src_repo: RepoPath,
        digest: Digest,
        dst: RegistryId,
        dst_repo: RepoPath,
        tag: String,
    },
    /// Writes `repo@top` (or its index child `platform`) as an OCI layout tarball to `path`
    /// (`~/` expanded by the runtime) through `<path>.part`.
    Export {
        op: OpId,
        registry: RegistryId,
        repo: RepoPath,
        top: Digest,
        platform: Option<Digest>,
        tag: Option<String>,
        full_ref: String,
        path: String,
        overwrite: bool,
    },
    /// Fresh dependency snapshot of `repo` for these marked digests (spec §7.4).
    DeleteSnapshot {
        op: OpId,
        registry: RegistryId,
        repo: RepoPath,
        marked: BTreeSet<Digest>,
    },
    /// Runs `plan` (spec §7.4 execution 2–5). `parents` is restricted to the delete set.
    Delete {
        op: OpId,
        registry: RegistryId,
        repo: RepoPath,
        plan: DeletePlan,
        parents: BTreeMap<Digest, BTreeSet<Digest>>,
    },
    /// Handled by `main` like `Quit`: suspend the terminal, run the editor, reload.
    EditConfig,
    /// Handled by `main`: write the theme file (`config::save_theme`).
    SaveTheme(Theme),
    /// Handled by `main`: read the registry from the file (`None`: a new one) and open the form.
    OpenRegistryForm(Option<RegistryId>),
    /// Handled by `main`: write the form into the file (`original`: the registry it edits),
    /// then reload.
    SaveRegistry {
        original: Option<RegistryId>,
        form: RegistryForm,
    },
    /// Handled by `main`: drop the registry from the file, then reload.
    RemoveRegistry(RegistryId),
    /// Ctrl-T: steps 1..=4 of the connection test on a throwaway registry built from `cfg`.
    TestConnection {
        op: OpId,
        cfg: RegistryCfg,
    },
    Quit,
}

pub struct State {
    pub(super) registries: Vec<RegistryView>,
    pub(super) cache: Cache,
    pub(super) focus: Pane,
    /// Selection by identity, so re-sorting/reloading lists never moves it. The registry list is
    /// static, so its row is its identity. Missing repo/image → first visible row (pinned in `sync`).
    pub(super) sel_reg: usize,
    pub(super) sel_repo: Option<RepoPath>,
    /// Selected repo group (its prefix); at most one of `sel_group` / `sel_repo` is `Some`.
    pub(super) sel_group: Option<String>,
    /// Expanded repo groups.
    pub(super) expanded: HashSet<(RegistryId, String)>,
    pub(super) sel_img: Option<Digest>,
    pub(super) tab: Tab,
    pub(super) platform: usize,
    pub(super) scroll: u16,
    /// Revealed image; masked whenever it is not the selected image.
    pub(super) reveal: Option<Digest>,
    pub(super) help: bool,
    /// Help body rows scrolled off the top.
    pub(super) help_scroll: u16,
    /// Palette every frame is drawn with; `main` sets it from the theme file.
    pub theme: Theme,
    /// Open theme picker: the row under the cursor (previewed live) and the theme to restore on Esc.
    pub(super) theme_pick: Option<(usize, Theme)>,
    pub(super) repo_filter: String,
    pub(super) image_filter: String,
    pub(super) filter_editing: bool,
    pub(super) repo_sort: (SortKey, bool),
    /// `None` = the registry's default (see `image_sort`).
    pub(super) image_sort: Option<(SortKey, bool)>,
    /// Latest message, with the key whose success clears it (`None` = not tied to a key).
    pub(super) status: Option<(Option<CacheKey>, StatusKind, String)>,
    /// Load-failure messages, for in-block errors; only read while the entry is `Failed`.
    pub(super) failures: HashMap<CacheKey, String>,
    /// Last `SetFocused` sent.
    pub(super) focused: Option<Vec<CacheKey>>,
    /// The single write op (running or finished); runs on while the wizard is hidden.
    pub(super) write: Option<WriteOp>,
    /// The pull modal (details, running, result), also while hidden.
    pub(super) pull: Option<PullModal>,
    /// The copy wizard (picking through result), also while hidden.
    pub(super) copy: Option<CopyWizard>,
    /// The export modal (details through result), also while hidden.
    pub(super) export: Option<ExportModal>,
    /// Last `OpId` issued.
    pub(super) next_op: u64,
    /// Selected and focused once a post-push inventory contains it; dropped on navigation.
    pub(super) pending_focus: Option<PendingFocus>,
    /// The push wizard while open; `None` also when hidden during a running push.
    pub(super) wizard: Option<Wizard>,
    /// Delete marks per repo.
    pub(super) marks: HashMap<(RegistryId, RepoPath), Marks>,
    /// Registries that answered "deletion unsupported" this session.
    pub(super) delete_disabled: HashSet<RegistryId>,
    /// The glob mark prompt's pattern while open.
    pub(super) glob: Option<String>,
    /// The first row of a half-made `v` range.
    pub(super) range_anchor: Option<Digest>,
    /// The image `=` marked as the base of a comparison.
    pub(super) compare_base: Option<Side>,
    /// The compare modal while open.
    pub(super) compare: Option<CompareModal>,
    /// The delete modal (preview through report), also while hidden.
    pub(super) delete: Option<DeleteModal>,
    /// `q` was refused during a write; the next action, if `q`, quits.
    pub(super) quit_armed: bool,
    /// The request log overlay while open.
    pub(super) requests: Option<RequestsView>,
    /// The registry form while open.
    pub(super) form: Option<RegForm>,
    /// The form of the last `SaveRegistry`, restored if `main` cannot write it.
    pub(super) form_saved: Option<RegForm>,
    /// The registry the `x` prompt asks about.
    pub(super) remove: Option<RegistryId>,
}

impl State {
    /// Initial effects: `LocalImages` fetch, `Repos` of the first registry, `SetFocused`.
    pub fn new(registries: Vec<RegistryView>) -> (State, Vec<Effect>) {
        let mut s = State {
            registries,
            cache: Cache::default(),
            focus: Pane::Registries,
            sel_reg: 0,
            sel_repo: None,
            sel_group: None,
            expanded: HashSet::new(),
            sel_img: None,
            tab: Tab::Overview,
            platform: 0,
            scroll: 0,
            reveal: None,
            help: false,
            help_scroll: 0,
            theme: Theme::default(),
            theme_pick: None,
            repo_filter: String::new(),
            image_filter: String::new(),
            filter_editing: false,
            repo_sort: (SortKey::Name, false),
            image_sort: None,
            status: None,
            failures: HashMap::new(),
            focused: None,
            write: None,
            pull: None,
            copy: None,
            export: None,
            next_op: 0,
            pending_focus: None,
            wizard: None,
            marks: HashMap::new(),
            delete_disabled: HashSet::new(),
            glob: None,
            range_anchor: None,
            compare_base: None,
            compare: None,
            delete: None,
            quit_armed: false,
            requests: None,
            form: None,
            form_saved: None,
            remove: None,
        };
        let mut fx: Vec<Effect> = s.cache.ensure(CacheKey::LocalImages).into_iter().collect();
        fx.extend(s.sync());
        (s, fx)
    }

    // ---- views ----

    pub fn registries(&self) -> &[RegistryView] {
        &self.registries
    }

    pub fn focus(&self) -> Pane {
        self.focus
    }

    /// Selected row of `pane` in its visible list.
    pub fn selected(&self, pane: Pane) -> usize {
        let row = match pane {
            Pane::Registries => return self.sel_reg,
            Pane::Repos => self.repo_rows().iter().position(|r| self.is_selected(r)),
            Pane::Images => self
                .visible_images()
                .iter()
                .position(|a| Some(&a.digest) == self.sel_img.as_ref()),
        };
        row.unwrap_or(0)
    }

    pub fn tab(&self) -> Tab {
        self.tab
    }

    pub fn platform(&self) -> usize {
        self.platform
    }

    pub fn scroll(&self) -> u16 {
        self.scroll
    }

    pub fn reveal(&self) -> bool {
        self.reveal.is_some() && self.reveal.as_ref() == self.selected_image().map(|a| &a.digest)
    }

    pub fn help_shown(&self) -> bool {
        self.help
    }

    pub fn help_scroll(&self) -> u16 {
        self.help_scroll
    }

    /// The picker's cursor row in `Theme::ALL`, while it is open.
    pub fn theme_picker(&self) -> Option<usize> {
        self.theme_pick.map(|(i, _)| i)
    }

    pub fn filter_editing(&self) -> bool {
        self.filter_editing
    }

    pub fn requests(&self) -> Option<&RequestsView> {
        self.requests.as_ref()
    }

    pub fn requests_open(&self) -> bool {
        self.requests.is_some()
    }

    /// Replaces the overlay's entries (its filter and scroll stay); no-op while it is closed.
    pub fn set_requests(&mut self, entries: Vec<RequestEntry>) {
        if let Some(r) = &mut self.requests {
            r.entries = entries;
        }
    }

    /// Modals in the order `update` routes keys to them and `ui::render` draws them on top.
    pub fn key_mode(&self) -> KeyMode {
        if self.remove.is_some() || self.form.as_ref().is_some_and(|f| f.confirm_discard) {
            return KeyMode::Confirm;
        }
        if self.form.is_some() {
            return KeyMode::Form;
        }
        if self.compare.is_some() {
            return KeyMode::Scroll;
        }
        if let Some(r) = &self.requests {
            return if r.filter_editing {
                KeyMode::Text
            } else {
                KeyMode::Scroll
            };
        }
        if let Some(m) = self.delete.as_ref().filter(|m| !m.hidden) {
            return match m.stage {
                DelStage::Confirm => KeyMode::Text,
                DelStage::Running | DelStage::Report { .. } => KeyMode::List,
                _ => KeyMode::Preview,
            };
        }
        if self.pull.as_ref().is_some_and(|m| !m.hidden) {
            return KeyMode::List;
        }
        if let Some(w) = self.copy.as_ref().filter(|w| !w.hidden) {
            return match w.stage {
                CopyStage::PickRepo | CopyStage::Tag => KeyMode::Text,
                _ => KeyMode::List,
            };
        }
        if let Some(m) = self.export.as_ref().filter(|m| !m.hidden) {
            return match m.stage {
                ExportStage::Details => KeyMode::ExportDetails,
                ExportStage::Path => KeyMode::Text,
                ExportStage::Overwrite => KeyMode::Confirm,
                ExportStage::Running | ExportStage::Result(_) => KeyMode::List,
            };
        }
        match self.wizard.as_ref().map(|w| w.stage) {
            Some(Stage::PickImage | Stage::Tag) => KeyMode::Text,
            Some(_) => KeyMode::List,
            None if self.filter_editing || self.glob.is_some() => KeyMode::Text,
            None if self.theme_pick.is_some() => KeyMode::List,
            None if self.help => KeyMode::Help,
            None => KeyMode::Normal,
        }
    }

    pub fn filter(&self, pane: Pane) -> &str {
        match pane {
            Pane::Repos => &self.repo_filter,
            Pane::Images => &self.image_filter,
            Pane::Registries => "",
        }
    }

    pub fn status(&self) -> Option<(StatusKind, &str)> {
        self.status.as_ref().map(|(_, k, m)| (*k, m.as_str()))
    }

    /// A status not tied to a cache key, set from outside `update` (`main`).
    pub fn set_status(&mut self, kind: StatusKind, msg: String) {
        self.status = Some((None, kind, msg));
    }

    /// The message of a `Failed` entry.
    pub fn failure(&self, key: &CacheKey) -> Option<&str> {
        match self.cache.get(key) {
            Some(Entry::Failed { .. }) => self.failures.get(key).map(String::as_str),
            _ => None,
        }
    }

    pub fn entry(&self, key: &CacheKey) -> Option<&Entry> {
        self.cache.get(key)
    }

    pub fn value(&self, key: &CacheKey) -> Option<&CacheValue> {
        self.cache.value(key)
    }

    pub fn selected_registry(&self) -> Option<&RegistryView> {
        self.registries.get(self.sel_reg)
    }

    /// (key, descending). Repos default to name ascending.
    pub fn repo_sort(&self) -> (SortKey, bool) {
        self.repo_sort
    }

    /// (key, descending). Default: pushed desc when the registry reports it, else tag desc.
    pub fn image_sort(&self) -> (SortKey, bool) {
        let pushed = self.selected_registry().is_some_and(|r| r.caps.pushed_at);
        self.image_sort.unwrap_or(if pushed {
            (SortKey::Pushed, true)
        } else {
            (SortKey::Tag, true)
        })
    }

    /// Pinned repositories first, in config order (filtered, never sorted), then the rest
    /// sorted. A pinned repo found in the loaded list is borrowed from there (it has
    /// `created_at`).
    pub fn visible_repos(&self) -> Vec<&RepoSummary> {
        let Some(reg) = self.selected_registry() else {
            return vec![];
        };
        let all: &[RepoSummary] = match self.value(&CacheKey::Repos(reg.id.clone())) {
            Some(CacheValue::Repos(all)) => all,
            _ => &[],
        };
        let shown = |r: &&RepoSummary| r.path.0.contains(&self.repo_filter);
        let mut v: Vec<_> = reg
            .pinned
            .iter()
            .map(|p| all.iter().find(|r| r.path == p.path).unwrap_or(p))
            .filter(shown)
            .collect();
        let pinned = v.len();
        v.extend(
            all.iter()
                .filter(|r| !self.is_pinned(&r.path))
                .filter(shown),
        );
        let (key, desc) = self.repo_sort;
        v[pinned..].sort_by(|a, b| {
            let o = match key {
                SortKey::Count => self.repo_count(&a.path).cmp(&self.repo_count(&b.path)),
                _ => a.path.cmp(&b.path),
            };
            if desc { o.reverse() } else { o }
        });
        v
    }

    /// Configured `pinned` repository of the selected registry.
    pub fn is_pinned(&self, p: &RepoPath) -> bool {
        self.selected_registry()
            .is_some_and(|r| r.pinned.iter().any(|x| &x.path == p))
    }

    /// Its inventory failed with `NotFound`.
    pub fn repo_not_found(&self, p: &RepoPath) -> bool {
        self.selected_registry().is_some_and(|r| {
            matches!(
                self.entry(&CacheKey::Inventory(r.id.clone(), p.clone())),
                Some(Entry::Failed {
                    error: RegistryError::NotFound,
                    ..
                })
            )
        })
    }

    /// Repos pane rows: pinned flat on top, then the rest as a namespace tree (flat while a
    /// filter is set, or when no path nests below their common prefix).
    pub fn repo_rows(&self) -> Vec<RepoRow<'_>> {
        let v = self.visible_repos();
        let n = v.iter().take_while(|r| self.is_pinned(&r.path)).count();
        let (pinned, rest) = v.split_at(n);
        let mut rows: Vec<_> = pinned
            .iter()
            .map(|r| RepoRow::Repo {
                repo: r,
                depth: 0,
                pinned: true,
            })
            .collect();
        // Longest whole-segment prefix shared by every path's parent.
        let mut common: Vec<&str> = vec![];
        for (i, r) in rest.iter().enumerate() {
            let segs: Vec<&str> = r.path.0.split('/').collect();
            let parent = &segs[..segs.len() - 1];
            if i == 0 {
                common = parent.to_vec();
            }
            let same = common
                .iter()
                .zip(parent)
                .take_while(|(a, b)| a == b)
                .count();
            common.truncate(same);
        }
        let common = common.join("/");
        let nests = rest.iter().any(|r| under(&r.path.0, &common).contains('/'));
        match self.selected_registry() {
            Some(reg) if nests && self.repo_filter.is_empty() => {
                self.tree(&reg.id, rest, &common, 0, &mut rows);
            }
            _ => rows.extend(rest.iter().map(|r| RepoRow::Repo {
                repo: r,
                depth: 0,
                pinned: false,
            })),
        }
        rows
    }

    /// Rows of node `prefix` (`repos`: every repo below it, in the active sort): child groups
    /// by name (reversed with `O`), a group with no repos and one child group drawn as one row,
    /// then the node's own repos.
    fn tree<'a>(
        &self,
        reg: &RegistryId,
        repos: &[&'a RepoSummary],
        prefix: &str,
        depth: usize,
        rows: &mut Vec<RepoRow<'a>>,
    ) {
        let child = |p: &str, seg: &str| match p {
            "" => seg.to_owned(),
            _ => format!("{p}/{seg}"),
        };
        let groups = |p: &str| -> BTreeSet<String> {
            repos
                .iter()
                .filter_map(|r| under(&r.path.0, p).split_once('/'))
                .map(|(seg, _)| seg.to_owned())
                .collect()
        };
        let mut names: Vec<String> = groups(prefix).into_iter().collect();
        if self.repo_sort.1 {
            names.reverse();
        }
        for name in names {
            let (mut full, mut label) = (child(prefix, &name), name);
            loop {
                let sub = groups(&full);
                let own = repos.iter().any(|r| is_child(&r.path.0, &full));
                match sub.iter().next() {
                    Some(seg) if sub.len() == 1 && !own => {
                        full = child(&full, seg);
                        label = format!("{label}/{seg}");
                    }
                    _ => break,
                }
            }
            let sub: Vec<_> = repos
                .iter()
                .filter(|r| r.path.0.starts_with(&format!("{full}/")))
                .copied()
                .collect();
            let expanded = self.expanded.contains(&(reg.clone(), full.clone()));
            rows.push(RepoRow::Group {
                prefix: full.clone(),
                label,
                count: sub.len(),
                depth,
                expanded,
            });
            if expanded {
                self.tree(reg, &sub, &full, depth + 1, rows);
            }
        }
        rows.extend(
            repos
                .iter()
                .filter(|r| is_child(&r.path.0, prefix))
                .map(|r| RepoRow::Repo {
                    repo: r,
                    depth,
                    pinned: false,
                }),
        );
    }

    fn is_selected(&self, row: &RepoRow) -> bool {
        match row {
            RepoRow::Repo { repo, .. } => Some(&repo.path) == self.sel_repo.as_ref(),
            RepoRow::Group { prefix, .. } => Some(prefix) == self.sel_group.as_ref(),
        }
    }

    /// The selected Repos row (the first one when the selection is not shown).
    pub fn selected_row(&self) -> Option<RepoRow<'_>> {
        let rows = self.repo_rows();
        let i = rows.iter().position(|r| self.is_selected(r)).unwrap_or(0);
        rows.into_iter().nth(i)
    }

    /// `None` while a group is selected.
    pub fn selected_repo(&self) -> Option<&RepoSummary> {
        match self.selected_row()? {
            RepoRow::Repo { repo, .. } => Some(repo),
            RepoRow::Group { .. } => None,
        }
    }

    /// Image count once the repo's inventory has been loaded.
    pub fn repo_count(&self, repo: &RepoPath) -> Option<usize> {
        let reg = self.selected_registry()?;
        match self.value(&CacheKey::Inventory(reg.id.clone(), repo.clone()))? {
            CacheValue::Inventory(v) => Some(v.len()),
            _ => None,
        }
    }

    /// Images of the selected repo, filtered by tag substring and sorted.
    pub fn visible_images(&self) -> Vec<&ArtifactSummary> {
        let (Some(reg), Some(repo)) = (self.selected_registry(), self.selected_repo()) else {
            return vec![];
        };
        let key = CacheKey::Inventory(reg.id.clone(), repo.path.clone());
        let Some(CacheValue::Inventory(all)) = self.value(&key) else {
            return vec![];
        };
        let f = &self.image_filter;
        let mut v: Vec<_> = all
            .iter()
            .filter(|a| f.is_empty() || a.tags.iter().any(|t| t.contains(f.as_str())))
            .collect();
        let (key, desc) = self.image_sort();
        v.sort_by(|a, b| {
            let o = match key {
                SortKey::Pushed => a.pushed_at.cmp(&b.pushed_at),
                SortKey::Size => self.display_size(a).cmp(&self.display_size(b)),
                SortKey::Pulled => a.last_pulled.cmp(&b.last_pulled),
                _ => a.tags.cmp(&b.tags),
            };
            if desc { o.reverse() } else { o }
        });
        v
    }

    pub fn selected_image(&self) -> Option<&ArtifactSummary> {
        let v = self.visible_images();
        let found = v.iter().find(|a| Some(&a.digest) == self.sel_img.as_ref());
        found.or(v.first()).copied()
    }

    /// Ready manifest with this digest in the selected registry/repo.
    pub fn manifest(&self, digest: &Digest) -> Option<&ManifestDocument> {
        let (reg, repo) = (self.selected_registry()?, self.selected_repo()?);
        let key = CacheKey::Manifest(reg.id.clone(), repo.path.clone(), digest.clone());
        match self.value(&key)? {
            CacheValue::Manifest(m) => Some(m),
            _ => None,
        }
    }

    /// Reported size; generic OCI (size `None`): config + layers once the manifest is loaded;
    /// `None` for an index or before that (UI shows "—" / blank).
    pub fn display_size(&self, a: &ArtifactSummary) -> Option<u64> {
        a.size.or_else(|| match &self.manifest(&a.digest)?.kind {
            ManifestKind::Image { config, layers } => {
                Some(config.size + layers.iter().map(|l| l.size).sum::<u64>())
            }
            ManifestKind::Index { .. } => None,
        })
    }

    /// `<ref_host>/<repo>:<first tag>`, or `@<digest>` when untagged.
    pub fn image_reference(&self) -> Option<String> {
        let (reg, repo, img) = (
            self.selected_registry()?,
            self.selected_repo()?,
            self.selected_image()?,
        );
        let r = match img.tags.first() {
            Some(t) => Reference::Tag(t.clone()),
            None => Reference::Digest(img.digest.clone()),
        };
        Some(image_ref(&reg.ref_host, &repo.path, &r))
    }

    /// Raw tab text: pretty manifest(s) and config JSON, masked unless revealed.
    pub fn raw_text(&self) -> Option<String> {
        let parts: Vec<String> = self
            .content_keys()
            .iter()
            .filter_map(|k| match self.value(k)? {
                CacheValue::Manifest(m) => Some(m.raw.as_str()),
                CacheValue::Config(Some(c)) => Some(c.raw.as_str()),
                _ => None,
            })
            .map(|raw| {
                if self.reveal() {
                    serde_json::from_str::<serde_json::Value>(raw)
                        .and_then(|v| serde_json::to_string_pretty(&v))
                        .unwrap_or_else(|_| raw.to_owned())
                } else {
                    mask_config_json(raw)
                }
            })
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    }

    /// Number of platforms when the selected image is a loaded index.
    pub fn platform_count(&self) -> Option<usize> {
        match &self.manifest(&self.selected_image()?.digest)?.kind {
            ManifestKind::Index { manifests } => Some(manifests.len()),
            ManifestKind::Image { .. } => None,
        }
    }

    // ---- fetch planning ----

    /// Content keys of the selected active image in fetch order: its manifest; for an index the
    /// selected platform's manifest; then `Config` once the image manifest it hangs off is Ready.
    pub(super) fn content_keys(&self) -> Vec<CacheKey> {
        let Some(img) = self
            .selected_image()
            .filter(|a| a.status == ArtifactStatus::Active)
        else {
            return vec![];
        };
        self.image_keys(&img.digest, self.platform)
    }

    /// Keys for the selected item at the focused level and its visible tab.
    pub(super) fn focus_keys(&self) -> Vec<CacheKey> {
        let mut keys = vec![];
        let Some(reg) = self.selected_registry() else {
            return keys;
        };
        keys.push(CacheKey::Repos(reg.id.clone()));
        if self.focus == Pane::Registries {
            keys.push(CacheKey::RegistryDetails(reg.id.clone()));
            return keys;
        }
        let Some(repo) = self.selected_repo() else {
            return keys;
        };
        keys.push(CacheKey::Inventory(reg.id.clone(), repo.path.clone()));
        if self.focus == Pane::Repos {
            keys.push(CacheKey::RepoDetails(reg.id.clone(), repo.path.clone()));
            return keys;
        }
        let content = self.content_keys();
        if self.tab == Tab::Overview
            && let Some(CacheKey::Manifest(r, p, d)) = content.first()
        {
            keys.push(CacheKey::Referrers(r.clone(), p.clone(), d.clone()));
        }
        keys.extend(content);
        keys
    }

    /// Keys `r` refreshes: the focused level's key(s) and every cached key below it.
    pub(super) fn refresh_scope(&self) -> Vec<CacheKey> {
        let Some(reg) = self.selected_registry() else {
            return vec![];
        };
        let in_reg = |k: &&CacheKey| k.registry() == Some(&reg.id);
        let mut keys = match self.focus {
            Pane::Registries => vec![CacheKey::Repos(reg.id.clone())],
            Pane::Repos => match self.selected_repo() {
                Some(repo) => vec![CacheKey::Inventory(reg.id.clone(), repo.path.clone())],
                None => return vec![],
            },
            // Referrers (on Overview) and content keys of the selected active image; when there
            // are none (no image selected, or an archived one), the repo's inventory.
            Pane::Images => match self.focus_keys().into_iter().skip(2).collect::<Vec<_>>() {
                k if k.is_empty() => match self.selected_repo() {
                    Some(repo) => vec![CacheKey::Inventory(reg.id.clone(), repo.path.clone())],
                    None => return vec![],
                },
                k => k,
            },
        };
        if keys.is_empty() {
            return keys;
        }
        let below: Vec<_> = self
            .cache
            .keys()
            .filter(in_reg)
            .filter(|k| match self.focus {
                Pane::Registries => true,
                Pane::Repos => k.repo() == keys[0].repo(),
                Pane::Images => k.repo() == keys[0].repo() && k.digest() == keys[0].digest(),
            })
            .cloned()
            .collect();
        for k in below {
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        keys
    }

    pub(super) fn list_len(&self, pane: Pane) -> usize {
        match pane {
            Pane::Registries => self.registries.len(),
            Pane::Repos => self.repo_rows().len(),
            Pane::Images => self.visible_images().len(),
        }
    }

    /// Selects the item at visible row `row` of `pane`; resets lower panes, detail scroll,
    /// platform and reveal.
    pub(super) fn select(&mut self, pane: Pane, row: usize) {
        match pane {
            Pane::Registries => {
                self.range_anchor = None;
                self.compare_base = None;
                self.sel_reg = row;
                self.sel_repo = None;
                self.sel_group = None;
                self.sel_img = None;
                self.image_sort = None; // default depends on the registry's caps
            }
            Pane::Repos => {
                self.range_anchor = None;
                self.compare_base = None;
                (self.sel_repo, self.sel_group) = match self.repo_rows().get(row) {
                    Some(RepoRow::Repo { repo, .. }) => (Some(repo.path.clone()), None),
                    Some(RepoRow::Group { prefix, .. }) => (None, Some(prefix.clone())),
                    None => (None, None),
                };
                self.sel_img = None;
            }
            Pane::Images => {
                self.sel_img = self.visible_images().get(row).map(|a| a.digest.clone());
            }
        }
        self.scroll = 0;
        self.platform = 0;
        self.reveal = None;
    }

    /// Expands the collapsed groups hiding the selected repo, so the selection never jumps.
    fn reveal_selected_repo(&mut self) {
        while let Some(g) = self.group_hiding_selection() {
            let Some(reg) = self.selected_registry() else {
                return;
            };
            self.expanded.insert((reg.id.clone(), g));
        }
    }

    /// The collapsed group that hides the selected repo, while it is not shown.
    fn group_hiding_selection(&self) -> Option<String> {
        let path = format!("{}/", self.sel_repo.as_ref()?);
        let rows = self.repo_rows();
        if rows.iter().any(|r| self.is_selected(r)) {
            return None;
        }
        rows.into_iter().find_map(|r| match r {
            RepoRow::Group {
                prefix,
                expanded: false,
                ..
            } if path.starts_with(&format!("{prefix}/")) => Some(prefix),
            _ => None,
        })
    }

    /// Pins selections whose item left a non-empty list to row 0, ensures the focused keys and
    /// reports focus changes. An empty (e.g. reloading) list keeps the selection.
    pub(super) fn sync(&mut self) -> Vec<Effect> {
        self.reveal_selected_repo();
        let rows = self.repo_rows();
        if !rows.is_empty() && !rows.iter().any(|r| self.is_selected(r)) {
            self.select(Pane::Repos, 0);
        }
        if let Some(a) = self.selected_image()
            && Some(&a.digest) != self.sel_img.as_ref()
        {
            self.select(Pane::Images, 0);
        }
        let mut keys = self.focus_keys();
        for k in self.compare_keys() {
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        // Pinned inventories load in the background (not focused), once per key.
        let pinned: Vec<CacheKey> = self.selected_registry().map_or(vec![], |reg| {
            reg.pinned
                .iter()
                .map(|p| CacheKey::Inventory(reg.id.clone(), p.path.clone()))
                .collect()
        });
        let mut fx: Vec<Effect> = keys
            .iter()
            .chain(&pinned)
            .filter_map(|k| self.cache.ensure(k.clone()))
            .collect();
        if self.focused.as_ref() != Some(&keys) {
            fx.push(Effect::SetFocused(keys.clone()));
            self.focused = Some(keys);
        }
        fx
    }
}

/// `path` below `prefix` (`""` when it is not below it).
fn under<'p>(path: &'p str, prefix: &str) -> &'p str {
    match prefix {
        "" => path,
        _ => path
            .strip_prefix(prefix)
            .and_then(|r| r.strip_prefix('/'))
            .unwrap_or(""),
    }
}

/// `path` is a repo directly in node `prefix`.
fn is_child(path: &str, prefix: &str) -> bool {
    let rest = under(path, prefix);
    !rest.is_empty() && !rest.contains('/')
}

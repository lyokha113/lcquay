//! The push wizard (spec §7.3): pick a local image, the target repo and the tag, confirm with
//! warnings, then follow the write op. Plain state that `update` drives.

use super::cache::Entry;
use super::state::{Action, CacheKey, CacheValue, Effect, Pane, State, StatusKind};
use super::version::suggest;
use crate::docker::UNAVAILABLE;
use crate::domain::{LocalImage, Reference, RegistryId, RepoPath, TagPolicy, image_ref};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    PickImage,
    PickRepo,
    Tag,
    Confirm,
    /// Shows `State::write` (running or finished).
    Progress,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Wizard {
    pub stage: Stage,
    pub registry: RegistryId,
    /// Local image filter (`PickImage`).
    pub filter: String,
    /// Highlighted local image id in `PickImage`; the chosen one after it.
    pub image: Option<String>,
    /// Highlighted repo in `PickRepo` (defaults to the focused repo); the chosen one after it.
    pub repo: Option<RepoPath>,
    pub tag: String,
    /// The user edited the tag; the suggestion no longer replaces it.
    pub tag_edited: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Warning {
    CheckingTag,
    TagExistenceUnknown,
    TagWillMove,
    TagWillBeRejected,
    TagExistsPolicyUnknown,
    CheckingPolicy,
    LocalTagReplaced,
}

impl Wizard {
    /// Local images with a repo tag containing the filter, or an id starting with it.
    pub fn images<'a>(&self, s: &'a State) -> Vec<&'a LocalImage> {
        let Some(CacheValue::LocalImages(all)) = s.value(&CacheKey::LocalImages) else {
            return vec![];
        };
        let f = self.filter.as_str();
        all.iter()
            .filter(|i| {
                i.id.starts_with(f)
                    || i.id
                        .strip_prefix("sha256:")
                        .is_some_and(|h| h.starts_with(f))
                    || i.repo_tags.iter().any(|t| t.contains(f))
            })
            .collect()
    }

    pub fn selected_image<'a>(&self, s: &'a State) -> Option<&'a LocalImage> {
        let v = self.images(s);
        let found = v.iter().find(|i| Some(&i.id) == self.image.as_ref());
        found.or(v.first()).copied()
    }

    /// Repos of the target registry, by name.
    pub fn repos<'a>(&self, s: &'a State) -> Vec<&'a RepoPath> {
        let Some(CacheValue::Repos(all)) = s.value(&CacheKey::Repos(self.registry.clone())) else {
            return vec![];
        };
        let mut v: Vec<_> = all.iter().map(|r| &r.path).collect();
        v.sort();
        v
    }

    pub fn selected_repo<'a>(&self, s: &'a State) -> Option<&'a RepoPath> {
        let v = self.repos(s);
        let found = v.iter().find(|r| Some(**r) == self.repo.as_ref());
        found.or(v.first()).copied()
    }

    /// `Push to <registry> → <ref_host>/<repo|?>:<tag|?>`. The repo is the highlighted row in
    /// `PickRepo`; the tag is `?` until the `Tag` stage and while it is empty.
    pub fn title(&self, s: &State) -> String {
        let host = s.registries.iter().find(|r| r.id == self.registry);
        let repo = match self.stage {
            Stage::PickRepo => self.selected_repo(s),
            _ => self.repo.as_ref(),
        };
        let tag = match self.stage {
            Stage::PickImage | Stage::PickRepo => "",
            _ => self.tag.as_str(),
        };
        format!(
            "Push to {} → {}/{}:{}",
            self.registry,
            host.map_or("?", |r| r.ref_host.as_str()),
            repo.map_or_else(|| "?".into(), |r| r.to_string()),
            if tag.is_empty() { "?" } else { tag },
        )
    }

    /// `<ref_host>/<repo>:<tag>`.
    pub fn target(&self, s: &State) -> Option<String> {
        let reg = s.registries.iter().find(|r| r.id == self.registry)?;
        let tag = Reference::Tag(self.tag.clone());
        Some(image_ref(&reg.ref_host, self.repo.as_ref()?, &tag))
    }

    /// Whether `key` is one the wizard loads for its target (repos, inventory, tag policy).
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
        let repo = self.repo.clone()?;
        Some(CacheKey::TagPolicy(
            self.registry.clone(),
            repo,
            self.tag.clone(),
        ))
    }

    /// Whether the tag exists remotely; `None` until the target inventory is `Ready`.
    fn tag_exists(&self, s: &State) -> Option<bool> {
        match s.cache.get(&self.inventory_key()?)? {
            Entry::Ready {
                value: CacheValue::Inventory(v),
                ..
            } => Some(v.iter().any(|a| a.tags.contains(&self.tag))),
            _ => None,
        }
    }

    /// Confirm warnings from the live cache state (a refetch in flight counts as checking).
    pub fn warnings(&self, s: &State) -> Vec<Warning> {
        let mut w = vec![];
        let (Some(inv), Some(policy)) = (self.inventory_key(), self.policy_key()) else {
            return w;
        };
        match s.cache.get(&inv) {
            None | Some(Entry::Loading { .. }) => w.push(Warning::CheckingTag),
            Some(Entry::Failed { .. }) => w.push(Warning::TagExistenceUnknown),
            Some(Entry::Ready { .. }) if self.tag_exists(s) == Some(true) => {
                w.push(match s.cache.get(&policy) {
                    None | Some(Entry::Loading { .. }) => Warning::CheckingPolicy,
                    Some(Entry::Ready {
                        value: CacheValue::TagPolicy(p),
                        ..
                    }) => match p {
                        TagPolicy::Mutable => Warning::TagWillMove,
                        TagPolicy::Immutable => Warning::TagWillBeRejected,
                        TagPolicy::Unknown => Warning::TagExistsPolicyUnknown,
                    },
                    Some(_) => Warning::TagExistsPolicyUnknown,
                });
            }
            Some(Entry::Ready { .. }) => {}
        }
        if let (Some(target), Some(CacheValue::LocalImages(local))) =
            (self.target(s), s.value(&CacheKey::LocalImages))
            && local
                .iter()
                .any(|i| Some(&i.id) != self.image.as_ref() && i.repo_tags.contains(&target))
        {
            w.push(Warning::LocalTagReplaced);
        }
        w
    }
}

/// Docker's reference tag grammar: `[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}`.
pub(super) fn valid_tag(t: &str) -> bool {
    let ok = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut chars = t.chars();
    chars.next().is_some_and(ok) && t.len() <= 128 && chars.all(|c| ok(c) || c == '.' || c == '-')
}

/// The item after/before `cur` in `items` (`cur` missing → row 0).
pub(super) fn step<T: Clone + PartialEq>(items: &[T], cur: Option<&T>, down: bool) -> Option<T> {
    let i = items.iter().position(|x| Some(x) == cur).unwrap_or(0);
    let j = if down { i + 1 } else { i.saturating_sub(1) };
    items.get(j.min(items.len().saturating_sub(1))).cloned()
}

impl State {
    pub fn wizard(&self) -> Option<&Wizard> {
        self.wizard.as_ref()
    }

    /// `p`: a running push's progress, else a new wizard once the local images are known (a
    /// failed Docker check is retried, so the next `p` works once Docker is back).
    pub(super) fn push_start(&mut self) -> Vec<Effect> {
        if let Some(w) = self.write.as_ref().filter(|w| w.outcome.is_none()) {
            self.wizard = Some(Wizard {
                stage: Stage::Progress,
                registry: w.registry.clone(),
                filter: String::new(),
                image: None,
                repo: Some(w.repo.clone()),
                tag: w.tag.clone(),
                tag_edited: true,
            });
            return vec![];
        }
        if let Some(fx) = self.need_local_images() {
            return fx;
        }
        let Some(reg) = self.selected_registry() else {
            return vec![];
        };
        self.wizard = Some(Wizard {
            stage: Stage::PickImage,
            registry: reg.id.clone(),
            filter: String::new(),
            image: None,
            // Only a repo the user is on pre-selects; from Registries the picker starts at row 0.
            repo: (self.focus != Pane::Registries)
                .then(|| self.selected_repo().map(|r| r.path.clone()))
                .flatten(),
            tag: String::new(),
            tag_edited: false,
        });
        vec![]
    }

    /// `Some(effects)` (and a status) while the local images are not known: Docker failed
    /// (refetched) or the list is still loading.
    pub(super) fn need_local_images(&mut self) -> Option<Vec<Effect>> {
        if self.value(&CacheKey::LocalImages).is_some() {
            return None;
        }
        let (kind, msg, fx) = match self.cache.get(&CacheKey::LocalImages) {
            Some(Entry::Failed { .. }) => (
                StatusKind::Error,
                UNAVAILABLE,
                self.cache.refresh(CacheKey::LocalImages),
            ),
            _ => (StatusKind::Info, "checking local Docker images…", None),
        };
        self.status = Some((Some(CacheKey::LocalImages), kind, msg.to_owned()));
        Some(fx.into_iter().collect())
    }

    /// An action while the wizard is open (all but `Quit` / `ForceQuit` / `ClampScroll`).
    pub(super) fn wizard_action(&mut self, a: Action) -> Vec<Effect> {
        let Some(mut w) = self.wizard.take() else {
            return vec![];
        };
        let mut fx = vec![];
        let back = matches!(a, Action::Back | Action::FilterCancel);
        let enter = matches!(a, Action::Enter | Action::FilterApply);
        let (up, down) = (a == Action::Up, a == Action::Down);
        let open = match w.stage {
            Stage::PickImage => {
                match a {
                    Action::FilterChar(c) => w.filter.push(c),
                    Action::FilterBackspace => {
                        w.filter.pop();
                    }
                    _ if up || down => {
                        let ids: Vec<String> =
                            w.images(self).iter().map(|i| i.id.clone()).collect();
                        if let Some(id) = step(&ids, w.image.as_ref(), down) {
                            w.image = Some(id);
                        }
                    }
                    _ if enter => {
                        if let Some(id) = w.selected_image(self).map(|i| i.id.clone()) {
                            w.image = Some(id);
                            w.stage = Stage::PickRepo;
                            fx.extend(self.load(CacheKey::Repos(w.registry.clone())));
                        }
                    }
                    _ => {}
                }
                !back
            }
            Stage::PickRepo => {
                if up || down {
                    let repos: Vec<RepoPath> = w.repos(self).into_iter().cloned().collect();
                    if let Some(r) = step(&repos, w.repo.as_ref(), down) {
                        w.repo = Some(r);
                    }
                } else if enter && let Some(r) = w.selected_repo(self).cloned() {
                    w.repo = Some(r);
                    w.stage = Stage::Tag;
                    fx.extend(self.enter_tag(&mut w));
                } else if enter {
                    // Nothing to pick: retry a failed list.
                    fx.extend(self.load(CacheKey::Repos(w.registry.clone())));
                } else if back {
                    w.stage = Stage::PickImage;
                }
                true
            }
            Stage::Tag => {
                match a {
                    Action::FilterChar(c) if w.tag_edited => w.tag.push(c),
                    Action::FilterChar(c) => {
                        w.tag = c.to_string();
                        w.tag_edited = true;
                    }
                    Action::FilterBackspace => {
                        w.tag.pop();
                        w.tag_edited = true;
                    }
                    _ if enter && w.tag.is_empty() => {}
                    _ if enter && !valid_tag(&w.tag) => {
                        self.status = Some((None, StatusKind::Error, "invalid tag".into()));
                    }
                    _ if enter => {
                        w.stage = Stage::Confirm;
                        // Never trust a cached policy: the registry settings may have changed.
                        if w.tag_exists(self) == Some(true) {
                            fx.extend(w.policy_key().and_then(|k| self.cache.refresh(k)));
                        }
                    }
                    _ if back => w.stage = Stage::PickRepo,
                    _ => {}
                }
                true
            }
            Stage::Confirm => {
                if back {
                    w.stage = Stage::Tag;
                } else if enter {
                    let checking = w
                        .warnings(self)
                        .iter()
                        .any(|x| matches!(x, Warning::CheckingTag | Warning::CheckingPolicy));
                    if checking {
                        let msg = "still checking the target tag…".to_owned();
                        self.status = Some((None, StatusKind::Info, msg));
                    } else if let (Some(repo), Some(image)) = (w.repo.clone(), w.image.clone()) {
                        match self.start_push(w.registry.clone(), repo, image, w.tag.clone()) {
                            Ok(e) => {
                                fx.push(e);
                                w.stage = Stage::Progress;
                            }
                            Err(m) => self.status = Some((None, StatusKind::Error, m)),
                        }
                    }
                }
                true
            }
            Stage::Progress => match &self.write {
                Some(op) if op.outcome.is_none() && back => {
                    fx.push(Effect::Cancel(op.op));
                    true
                }
                Some(op) if op.outcome.is_none() && enter => {
                    // Hide; the push runs on and `p` brings it back.
                    let msg = format!("pushing {}…", op.target);
                    self.status = Some((None, StatusKind::Info, msg));
                    false
                }
                _ => !(back || enter),
            },
        };
        if open {
            self.wizard = Some(w);
        }
        fx
    }

    /// After an applied `Loaded` for `key`: the target inventory fills the suggested tag (while
    /// the user has not edited it) and, on `Confirm`, fetches the policy of an existing tag.
    pub(super) fn wizard_loaded(&mut self, key: &CacheKey) -> Vec<Effect> {
        let Some(mut w) = self.wizard.take() else {
            return vec![];
        };
        let mut fx = vec![];
        if w.inventory_key().as_ref() == Some(key) {
            match w.stage {
                // A failed inventory has no suggestion: keep what the field shows.
                Stage::Tag if !w.tag_edited => {
                    let s = self.suggestion(&w);
                    if !s.is_empty() {
                        w.tag = s;
                    }
                }
                Stage::Confirm if w.tag_exists(self) == Some(true) => {
                    fx.extend(w.policy_key().and_then(|k| self.cache.refresh(k)));
                }
                _ => {}
            }
        }
        self.wizard = Some(w);
        fx
    }

    /// Entering `Tag`: suggest from the cached inventory, then always refetch it: a stale
    /// inventory must not hide a tag that exists remotely. `wizard_loaded` re-suggests.
    fn enter_tag(&mut self, w: &mut Wizard) -> Option<Effect> {
        let key = w.inventory_key()?;
        if !w.tag_edited {
            w.tag = self.suggestion(w);
        }
        self.cache.refresh(key)
    }

    /// Spec §7.5 suggestion from the target inventory (stale data while refetching is fine).
    fn suggestion(&self, w: &Wizard) -> String {
        let Some(CacheValue::Inventory(v)) = w.inventory_key().and_then(|k| self.value(&k)) else {
            return String::new();
        };
        let tags: Vec<_> = v
            .iter()
            .flat_map(|a| a.tags.iter().map(|t| (t.as_str(), a.pushed_at)))
            .collect();
        suggest(&tags).unwrap_or_default()
    }

    /// Fetches `key` unless it is loading or loaded; a failed load is retried.
    pub(super) fn load(&mut self, key: CacheKey) -> Option<Effect> {
        match self.cache.get(&key) {
            Some(Entry::Failed { .. }) => self.cache.refresh(key),
            _ => self.cache.ensure(key),
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::app::state::{Event, KeyMode, Pane};
    use crate::app::update::tests::{T, art, at_images, dg, fetched, inv, repos, rid, rp};
    use crate::app::write::{OpId, PushOutcome};
    use crate::domain::{ArtifactSummary, RegistryError};
    use crate::keys::key_to_action;

    const ID1: &str = "sha256:1111aaaa";
    const ID2: &str = "sha256:2222bbbb";
    const ID3: &str = "sha256:3333cccc";

    fn local(id: &str, tags: &[&str]) -> LocalImage {
        LocalImage {
            id: id.into(),
            repo_tags: tags.iter().map(|t| t.to_string()).collect(),
            repo_digests: vec![],
            manifests: vec![],
            created: None,
            size: 1,
        }
    }

    /// Images pane of ecr/app (`h.example`) with `images`; three local images.
    fn ready(images: Vec<ArtifactSummary>) -> T {
        let (mut t, _) = at_images(images);
        let locals = vec![
            local(ID1, &["web:dev"]),
            local(ID2, &["app:latest", "h.example/app:0.9.0"]),
            local(ID3, &[]),
        ];
        t.ok(CacheKey::LocalImages, CacheValue::LocalImages(locals));
        t
    }

    fn wiz(t: &T) -> &Wizard {
        t.s.wizard().expect("wizard open")
    }

    fn stage(t: &T) -> Option<Stage> {
        t.s.wizard().map(|w| w.stage)
    }

    fn typ(t: &mut T, s: &str) {
        for c in s.chars() {
            t.act(Action::FilterChar(c));
        }
    }

    fn sel_image(t: &T) -> Option<String> {
        wiz(t).selected_image(&t.s).map(|i| i.id.clone())
    }

    fn sel_repo(t: &T) -> Option<RepoPath> {
        wiz(t).selected_repo(&t.s).cloned()
    }

    fn warnings(t: &T) -> Vec<Warning> {
        wiz(t).warnings(&t.s)
    }

    fn policy(repo: &str, tag: &str) -> CacheKey {
        CacheKey::TagPolicy(rid("ecr"), rp(repo), tag.into())
    }

    /// `Tag` stage for ecr/app with local image `ID1`; the inventory refresh entering it has
    /// completed with `images`.
    fn at_tag(images: Vec<ArtifactSummary>) -> T {
        let mut t = ready(images.clone());
        t.act(Action::PushStart);
        t.act(Action::FilterApply);
        t.act(Action::Enter);
        assert_eq!(stage(&t), Some(Stage::Tag));
        t.ok(inv("ecr", "app"), CacheValue::Inventory(images));
        t
    }

    /// `Confirm` stage for ecr/app:`tag` with local image `ID1`, and the effects of entering it.
    fn at_confirm(images: Vec<ArtifactSummary>, tag: &str) -> (T, Vec<Effect>) {
        let mut t = at_tag(images);
        typ(&mut t, tag);
        let fx = t.act(Action::FilterApply);
        assert_eq!(stage(&t), Some(Stage::Confirm));
        (t, fx)
    }

    fn pushes(fx: &[Effect]) -> Vec<&Effect> {
        fx.iter()
            .filter(|e| matches!(e, Effect::Push { .. }))
            .collect()
    }

    fn done(op: u64, outcome: PushOutcome) -> Event {
        Event::PushDone {
            op: OpId(op),
            outcome,
        }
    }

    #[test]
    fn push_without_docker_shows_unavailable() {
        for err in [
            RegistryError::Provider(UNAVAILABLE.into()),
            RegistryError::Timeout("docker".into()),
        ] {
            let (mut t, _) = at_images(vec![]);
            t.load(CacheKey::LocalImages, Err(err));
            let fx = t.act(Action::PushStart);
            assert_eq!(t.s.wizard(), None);
            assert_eq!(t.s.status(), Some((StatusKind::Error, UNAVAILABLE)));
            assert_eq!(fetched(&fx), [CacheKey::LocalImages], "retries the check");
            // Browsing continues.
            assert_eq!(t.s.key_mode(), KeyMode::Normal);
            t.act(Action::PrevPane);
            assert_eq!(t.s.focus(), Pane::Repos);
        }
        let (mut t, _) = at_images(vec![]);
        t.act(Action::PushStart);
        assert_eq!(t.s.wizard(), None);
        assert_eq!(
            t.s.status(),
            Some((StatusKind::Info, "checking local Docker images…"))
        );
    }

    #[test]
    fn push_recovers_once_docker_is_back() {
        let (mut t, _) = at_images(vec![]);
        t.load(
            CacheKey::LocalImages,
            Err(RegistryError::Provider(UNAVAILABLE.into())),
        );
        t.act(Action::PushStart);
        assert_eq!(t.s.wizard(), None);
        t.ok(CacheKey::LocalImages, CacheValue::LocalImages(vec![]));
        assert_eq!(t.s.status(), None);
        let fx = t.act(Action::PushStart);
        assert!(fx.is_empty(), "{fx:?}");
        assert_eq!(stage(&t), Some(Stage::PickImage));
    }

    #[test]
    fn push_start_opens_on_selected_registry_and_repo() {
        let mut t = ready(vec![]);
        t.act(Action::PrevPane);
        t.act(Action::Down);
        t.act(Action::PushStart);
        let w = wiz(&t);
        assert_eq!(
            (w.stage, &w.registry, w.repo.as_ref(), w.tag.as_str()),
            (Stage::PickImage, &rid("ecr"), Some(&rp("web")), "")
        );
        assert_eq!(t.s.key_mode(), KeyMode::Text);

        t.act(Action::FilterCancel);
        assert_eq!(t.s.wizard(), None);
        t.s.start_push(rid("ecr"), rp("app"), ID1.into(), "1.0.1".into())
            .unwrap();
        t.act(Action::PushStart);
        assert_eq!(stage(&t), Some(Stage::Progress));
        assert_eq!(t.s.key_mode(), KeyMode::List);
    }

    /// Registries pane of ecr (`h.example`), local images loaded, repos loaded.
    fn on_registries() -> T {
        let mut t = ready(vec![]);
        t.act(Action::PrevPane);
        t.act(Action::PrevPane);
        assert_eq!(t.s.focus(), Pane::Registries);
        t
    }

    #[test]
    fn push_from_registries_preselects_nothing() {
        let mut t = on_registries();
        t.act(Action::PushStart);
        assert_eq!(wiz(&t).repo, None);
        t.act(Action::FilterApply);
        assert_eq!(stage(&t), Some(Stage::PickRepo));
        assert_eq!(sel_repo(&t), Some(rp("app")), "first repo");
        t.act(Action::Enter);
        assert_eq!(stage(&t), Some(Stage::Tag));
        assert_eq!(wiz(&t).repo, Some(rp("app")));
    }

    #[test]
    fn push_from_repos_preselects_focused() {
        let mut t = ready(vec![]);
        t.act(Action::PrevPane);
        t.act(Action::Down);
        assert_eq!(t.s.focus(), Pane::Repos);
        t.act(Action::PushStart);
        assert_eq!(wiz(&t).repo, Some(rp("web")));
    }

    #[test]
    fn title_follows_stages() {
        let title = |t: &T| wiz(t).title(&t.s);
        let mut t = on_registries();
        t.act(Action::PushStart);
        assert_eq!(title(&t), "Push to ecr → h.example/?:?");
        t.act(Action::FilterApply);
        assert_eq!(title(&t), "Push to ecr → h.example/app:?");
        t.act(Action::Down);
        assert_eq!(title(&t), "Push to ecr → h.example/web:?");
        t.act(Action::Up);

        let mut t = at_tag(vec![art('a', &["0.1.0-dev"])]);
        assert_eq!(title(&t), "Push to ecr → h.example/app:0.1.1-dev");
        for _ in 0.."0.1.1-dev".len() {
            t.act(Action::FilterBackspace);
        }
        assert_eq!(title(&t), "Push to ecr → h.example/app:?");
    }

    #[test]
    fn pick_image_filters_and_selects() {
        let mut t = ready(vec![]);
        t.act(Action::PushStart);
        let ids =
            |t: &T| -> Vec<String> { wiz(t).images(&t.s).iter().map(|i| i.id.clone()).collect() };
        assert_eq!(ids(&t), [ID1, ID2, ID3]);
        assert_eq!(sel_image(&t).as_deref(), Some(ID1));
        t.act(Action::Down);
        t.act(Action::Down);
        t.act(Action::Down);
        assert_eq!(sel_image(&t).as_deref(), Some(ID3));
        t.act(Action::Up);
        assert_eq!(sel_image(&t).as_deref(), Some(ID2));

        for (f, want) in [
            ("app", vec![ID2]),
            ("333", vec![ID3]),
            ("sha256:1", vec![ID1]),
            ("zzz", vec![]),
        ] {
            typ(&mut t, f);
            assert_eq!(ids(&t), want, "{f}");
            if want.is_empty() {
                t.act(Action::FilterApply);
                assert_eq!(stage(&t), Some(Stage::PickImage), "no match: stays");
            }
            for _ in f.chars() {
                t.act(Action::FilterBackspace);
            }
        }
        typ(&mut t, "dev");
        assert_eq!(
            sel_image(&t).as_deref(),
            Some(ID1),
            "hidden selection → first"
        );
        t.act(Action::FilterApply);
        assert_eq!(stage(&t), Some(Stage::PickRepo));
        assert_eq!(wiz(&t).image.as_deref(), Some(ID1));
    }

    #[test]
    fn pick_repo_defaults_and_moves() {
        let mut t = ready(vec![]);
        t.act(Action::PrevPane);
        t.act(Action::Down);
        t.act(Action::PushStart);
        t.act(Action::FilterApply);
        assert_eq!(stage(&t), Some(Stage::PickRepo));
        assert_eq!(t.s.key_mode(), KeyMode::List);
        assert_eq!(wiz(&t).repos(&t.s), [&rp("app"), &rp("web")]);
        assert_eq!(sel_repo(&t), Some(rp("web")), "default: focused repo");
        t.act(Action::Up);
        t.act(Action::Up);
        assert_eq!(sel_repo(&t), Some(rp("app")));
        t.act(Action::Down);
        t.act(Action::Down);
        assert_eq!(sel_repo(&t), Some(rp("web")));
        t.act(Action::Up);
        t.act(Action::Enter);
        assert_eq!(stage(&t), Some(Stage::Tag));
        assert_eq!(wiz(&t).repo, Some(rp("app")));

        // Repos not loaded (failed): entering the picker fetches them.
        let mut t = ready(vec![]);
        t.act(Action::PrevPane);
        t.act(Action::PrevPane);
        t.act(Action::Refresh);
        t.load(
            CacheKey::Repos(rid("ecr")),
            Err(RegistryError::Network("down".into())),
        );
        t.act(Action::PushStart);
        let fx = t.act(Action::FilterApply);
        assert!(
            fetched(&fx).contains(&CacheKey::Repos(rid("ecr"))),
            "{fx:?}"
        );
        assert_eq!(stage(&t), Some(Stage::PickRepo));
        let fx = t.act(Action::Enter);
        assert!(fx.is_empty(), "already loading: {fx:?}");
        assert_eq!(stage(&t), Some(Stage::PickRepo), "nothing to pick yet");
        t.load(
            CacheKey::Repos(rid("ecr")),
            Err(RegistryError::Network("down".into())),
        );
        let fx = t.act(Action::Enter);
        assert!(
            fetched(&fx).contains(&CacheKey::Repos(rid("ecr"))),
            "Enter retries a failed list: {fx:?}"
        );
        t.ok(CacheKey::Repos(rid("ecr")), repos(&["web", "app"]));
        assert_eq!(sel_repo(&t), Some(rp("app")));
        t.act(Action::Enter);
        assert_eq!(stage(&t), Some(Stage::Tag));
    }

    #[test]
    fn tag_field_prefilled_and_editable() {
        let images = vec![art('a', &["0.1.0-dev"]), art('b', &["0.0.9", "latest"])];
        let mut t = at_tag(images.clone());
        assert_eq!(t.s.key_mode(), KeyMode::Text);
        assert_eq!(
            (wiz(&t).tag.as_str(), wiz(&t).tag_edited),
            ("0.1.1-dev", false)
        );
        t.act(Action::FilterChar('2'));
        assert_eq!((wiz(&t).tag.as_str(), wiz(&t).tag_edited), ("2", true));
        typ(&mut t, ".0.0x");
        t.act(Action::FilterBackspace);
        assert_eq!(wiz(&t).tag, "2.0.0");

        // Backspace edits the suggestion.
        let mut t = at_tag(images);
        t.act(Action::FilterBackspace);
        assert_eq!(
            (wiz(&t).tag.as_str(), wiz(&t).tag_edited),
            ("0.1.1-de", true)
        );
        for _ in 0..8 {
            t.act(Action::FilterBackspace);
        }
        t.act(Action::FilterApply);
        assert_eq!(stage(&t), Some(Stage::Tag), "empty tag stays");
        for bad in ["-x", ".x", "a:b", "a b", "é", &"a".repeat(129)] {
            typ(&mut t, bad);
            t.act(Action::FilterApply);
            assert_eq!(stage(&t), Some(Stage::Tag), "{bad}");
            assert_eq!(t.s.status(), Some((StatusKind::Error, "invalid tag")));
            for _ in bad.chars() {
                t.act(Action::FilterBackspace);
            }
        }
        typ(&mut t, &format!("_{}", "a.-".repeat(42) + "b"));
        assert_eq!(wiz(&t).tag.len(), 128);
        t.act(Action::FilterApply);
        assert_eq!(stage(&t), Some(Stage::Confirm));

        // Inventory not loaded: empty field, fetched, filled on load.
        let mut t = ready(vec![]);
        t.act(Action::PushStart);
        t.act(Action::FilterApply);
        t.act(Action::Down);
        let fx = t.act(Action::Enter);
        assert!(fetched(&fx).contains(&inv("ecr", "web")), "{fx:?}");
        assert_eq!(wiz(&t).tag, "");
        t.ok(
            inv("ecr", "web"),
            CacheValue::Inventory(vec![art('c', &["2.3.4"])]),
        );
        assert_eq!(wiz(&t).tag, "2.3.5");

        // ... but never over what the user typed.
        let mut t = ready(vec![]);
        t.act(Action::PushStart);
        t.act(Action::FilterApply);
        t.act(Action::Down);
        t.act(Action::Enter);
        typ(&mut t, "x");
        t.ok(
            inv("ecr", "web"),
            CacheValue::Inventory(vec![art('c', &["2.3.4"])]),
        );
        assert_eq!(wiz(&t).tag, "x");
    }

    #[test]
    fn enter_tag_refreshes_cached_inventory() {
        // Cached (stale) inventory lacks 1.2.4, which exists remotely by now.
        let mut t = ready(vec![art('a', &["1.2.3"])]);
        t.act(Action::PushStart);
        t.act(Action::FilterApply);
        let fx = t.act(Action::Enter);
        assert_eq!(fetched(&fx), [inv("ecr", "app")], "{fx:?}");
        assert_eq!(wiz(&t).tag, "1.2.4", "suggested from the cached value");
        let fx = t.act(Action::FilterApply);
        assert!(fetched(&fx).is_empty(), "{fx:?}");
        assert_eq!(warnings(&t), [Warning::CheckingTag]);

        let fx = t.ok(
            inv("ecr", "app"),
            CacheValue::Inventory(vec![art('a', &["1.2.3"]), art('b', &["1.2.4"])]),
        );
        assert!(fetched(&fx).contains(&policy("app", "1.2.4")), "{fx:?}");
        assert_eq!(warnings(&t), [Warning::CheckingPolicy]);

        // Still on `Tag`: the fresh result re-suggests.
        let mut t = ready(vec![art('a', &["1.2.3"])]);
        t.act(Action::PushStart);
        t.act(Action::FilterApply);
        t.act(Action::Enter);
        t.ok(
            inv("ecr", "app"),
            CacheValue::Inventory(vec![art('a', &["1.2.3"]), art('b', &["1.2.4"])]),
        );
        assert_eq!(wiz(&t).tag, "1.2.5");
    }

    #[test]
    fn wizard_load_failures_do_not_name_r() {
        let down = || Err(RegistryError::Network("down".into()));
        // Inventory failure during the Tag stage.
        let mut t = ready(vec![art('a', &["1.2.3"])]);
        t.act(Action::PushStart);
        t.act(Action::FilterApply);
        t.act(Action::Enter);
        t.load(inv("ecr", "app"), down());
        assert_eq!(
            t.s.status(),
            Some((StatusKind::Error, "network error: down"))
        );
        let m = t.s.failure(&inv("ecr", "app")).unwrap();
        assert!(!m.contains("press r"), "{m}");

        // Repos failure while picking the repo.
        let mut t = ready(vec![]);
        t.act(Action::PrevPane);
        t.act(Action::PrevPane);
        t.act(Action::Refresh);
        t.act(Action::PushStart);
        t.act(Action::FilterApply);
        t.load(CacheKey::Repos(rid("ecr")), down());
        let (_, m) = t.s.status().unwrap();
        assert!(!m.contains("press r"), "{m}");

        // Outside the wizard the hint stays.
        let (mut t, _) = at_images(vec![]);
        t.act(Action::PrevPane);
        t.act(Action::Refresh);
        t.load(inv("ecr", "app"), down());
        let (_, m) = t.s.status().unwrap();
        assert!(m.contains("press r"), "{m}");
    }

    #[test]
    fn failed_inventory_keeps_suggestion() {
        // `web`'s inventory is loading with a refetch queued: the first result fills the
        // suggestion and the refetch then fails.
        let mut t = ready(vec![]);
        t.act(Action::PrevPane);
        t.act(Action::Down);
        t.act(Action::Refresh);
        t.act(Action::PushStart);
        t.act(Action::FilterApply);
        t.act(Action::Enter);
        assert_eq!((stage(&t), wiz(&t).tag.as_str()), (Some(Stage::Tag), ""));
        t.ok(
            inv("ecr", "web"),
            CacheValue::Inventory(vec![art('c', &["2.3.4"])]),
        );
        assert_eq!(wiz(&t).tag, "2.3.5");
        t.load(
            inv("ecr", "web"),
            Err(RegistryError::Network("down".into())),
        );
        assert_eq!((wiz(&t).tag.as_str(), wiz(&t).tag_edited), ("2.3.5", false));
    }

    #[test]
    fn confirm_warnings() {
        // Inventory loading, then failed.
        let mut t = ready(vec![]);
        t.act(Action::PushStart);
        t.act(Action::FilterApply);
        t.act(Action::Down);
        t.act(Action::Enter);
        typ(&mut t, "1.0.0");
        t.act(Action::FilterApply);
        assert_eq!(t.s.key_mode(), KeyMode::List);
        assert_eq!(warnings(&t), [Warning::CheckingTag]);
        t.load(
            inv("ecr", "web"),
            Err(RegistryError::Network("down".into())),
        );
        assert_eq!(warnings(&t), [Warning::TagExistenceUnknown]);

        // Tag absent.
        let (t, fx) = at_confirm(vec![art('a', &["1.0.0"])], "9.9.9");
        assert_eq!(warnings(&t), []);
        assert!(!fetched(&fx).contains(&policy("app", "9.9.9")));

        // Tag present: the policy decides.
        for (p, want) in [
            (TagPolicy::Mutable, Warning::TagWillMove),
            (TagPolicy::Immutable, Warning::TagWillBeRejected),
            (TagPolicy::Unknown, Warning::TagExistsPolicyUnknown),
        ] {
            let (mut t, fx) = at_confirm(vec![art('a', &["1.0.0"])], "1.0.0");
            assert!(fetched(&fx).contains(&policy("app", "1.0.0")), "{fx:?}");
            assert_eq!(warnings(&t), [Warning::CheckingPolicy]);
            t.ok(policy("app", "1.0.0"), CacheValue::TagPolicy(p));
            assert_eq!(warnings(&t), [want]);

            // A Ready policy is refetched on every Confirm.
            t.act(Action::Back);
            let fx = t.act(Action::FilterApply);
            assert!(fetched(&fx).contains(&policy("app", "1.0.0")), "{fx:?}");
            assert_eq!(warnings(&t), [Warning::CheckingPolicy]);
        }
        let (mut t, _) = at_confirm(vec![art('a', &["1.0.0"])], "1.0.0");
        t.load(
            policy("app", "1.0.0"),
            Err(RegistryError::Network("down".into())),
        );
        assert_eq!(warnings(&t), [Warning::TagExistsPolicyUnknown]);

        // `h.example/app:0.9.0` is local on ID2; pushing ID1 there replaces it.
        let (t, _) = at_confirm(vec![art('a', &["1.0.0"])], "0.9.0");
        assert_eq!(warnings(&t), [Warning::LocalTagReplaced]);
        let mut t = ready(vec![art('a', &["1.0.0"])]);
        t.act(Action::PushStart);
        t.act(Action::Down);
        t.act(Action::FilterApply);
        t.act(Action::Enter);
        t.ok(
            inv("ecr", "app"),
            CacheValue::Inventory(vec![art('a', &["1.0.0"])]),
        );
        typ(&mut t, "0.9.0");
        t.act(Action::FilterApply);
        assert_eq!(wiz(&t).image.as_deref(), Some(ID2));
        assert_eq!(warnings(&t), [], "same image");
    }

    #[test]
    fn confirm_reevaluates_when_inventory_arrives() {
        let mut t = ready(vec![]);
        t.act(Action::PushStart);
        t.act(Action::FilterApply);
        t.act(Action::Down);
        t.act(Action::Enter);
        typ(&mut t, "1.0.0");
        t.act(Action::FilterApply);
        assert_eq!(warnings(&t), [Warning::CheckingTag]);
        let fx = t.act(Action::Enter);
        assert!(pushes(&fx).is_empty());
        assert_eq!(stage(&t), Some(Stage::Confirm));
        assert_eq!(
            t.s.status(),
            Some((StatusKind::Info, "still checking the target tag…"))
        );

        let fx = t.ok(
            inv("ecr", "web"),
            CacheValue::Inventory(vec![art('c', &["1.0.0"])]),
        );
        assert!(fetched(&fx).contains(&policy("web", "1.0.0")), "{fx:?}");
        assert_eq!(warnings(&t), [Warning::CheckingPolicy]);
        assert!(pushes(&t.act(Action::Enter)).is_empty());
        t.ok(
            policy("web", "1.0.0"),
            CacheValue::TagPolicy(TagPolicy::Immutable),
        );
        assert_eq!(warnings(&t), [Warning::TagWillBeRejected]);
        // Advisory: the registry's answer wins.
        assert_eq!(pushes(&t.act(Action::Enter)).len(), 1);
    }

    #[test]
    fn confirm_starts_push() {
        let (mut t, _) = at_confirm(vec![art('a', &["1.0.0"])], "2.0.0");
        let fx = t.act(Action::Enter);
        assert_eq!(
            pushes(&fx),
            [&Effect::Push {
                op: OpId(1),
                registry: rid("ecr"),
                repo: rp("app"),
                image_id: ID1.into(),
                tag: "2.0.0".into(),
            }]
        );
        assert_eq!(stage(&t), Some(Stage::Progress));
        assert!(t.s.write_running());

        let (mut t, _) = at_confirm(vec![art('a', &["1.0.0"])], "2.0.0");
        t.s.start_push(rid("ecr"), rp("web"), ID2.into(), "1.0.0".into())
            .unwrap();
        let fx = t.act(Action::Enter);
        assert!(pushes(&fx).is_empty());
        assert_eq!(stage(&t), Some(Stage::Confirm));
        assert_eq!(
            t.s.status(),
            Some((StatusKind::Error, crate::app::write::BUSY))
        );
    }

    #[test]
    fn progress_stage_hide_and_cancel() {
        let (mut t, _) = at_confirm(vec![], "2.0.0");
        t.act(Action::Enter);
        t.act(Action::Enter);
        assert_eq!(t.s.wizard(), None, "hidden");
        assert!(t.s.write_running());
        assert_eq!(
            t.s.status(),
            Some((StatusKind::Info, "pushing h.example/app:2.0.0…"))
        );
        t.act(Action::PushStart);
        assert_eq!(stage(&t), Some(Stage::Progress));
        let fx = t.act(Action::Back);
        assert!(fx.contains(&Effect::Cancel(OpId(1))), "{fx:?}");
        assert_eq!(stage(&t), Some(Stage::Progress), "open until the op ends");
        t.ev(done(1, PushOutcome::Cancelled));
        assert_eq!(stage(&t), Some(Stage::Progress));
        let fx = t.act(Action::Back);
        assert!(!fx.iter().any(|e| matches!(e, Effect::Cancel(_))));
        assert_eq!(t.s.wizard(), None);

        let (mut t, _) = at_confirm(vec![], "2.0.0");
        t.act(Action::Enter);
        t.ev(done(1, PushOutcome::Pushed(None)));
        t.act(Action::Enter);
        assert_eq!(t.s.wizard(), None);
    }

    #[test]
    fn back_walks_stages() {
        for back in [Action::Back, Action::FilterCancel] {
            let (mut t, _) = at_confirm(vec![], "2.0.0");
            for want in [
                Some(Stage::Tag),
                Some(Stage::PickRepo),
                Some(Stage::PickImage),
                None,
            ] {
                t.act(back);
                assert_eq!(stage(&t), want, "{back:?}");
            }
        }
    }

    fn press(t: &mut T, k: KeyEvent) -> Vec<Effect> {
        match key_to_action(k, t.s.key_mode()) {
            Some(a) => t.act(a),
            None => vec![],
        }
    }

    fn key(t: &mut T, code: KeyCode) -> Vec<Effect> {
        press(t, KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn keys_end_to_end() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let mut t = ready(vec![]);
        key(&mut t, KeyCode::Char('p'));
        assert_eq!(stage(&t), Some(Stage::PickImage));
        assert!(press(&mut t, ctrl_c).contains(&Effect::Quit));
        key(&mut t, KeyCode::Enter);
        assert_eq!(stage(&t), Some(Stage::PickRepo));
        assert!(press(&mut t, ctrl_c).contains(&Effect::Quit));
        key(&mut t, KeyCode::Char('j'));
        assert_eq!(sel_repo(&t), Some(rp("web")));
        key(&mut t, KeyCode::Enter);
        assert_eq!(stage(&t), Some(Stage::Tag));

        let mut fx = vec![];
        for c in ['q', 'r', 'L'] {
            fx.extend(key(&mut t, KeyCode::Char(c)));
        }
        assert_eq!(wiz(&t).tag, "qrL");
        assert!(
            !fx.iter()
                .any(|e| matches!(e, Effect::Quit | Effect::Login(_) | Effect::Fetch { .. })),
            "{fx:?}"
        );
        assert!(press(&mut t, ctrl_c).contains(&Effect::Quit));
        for _ in 0..3 {
            key(&mut t, KeyCode::Backspace);
        }
        for c in "1.0.0".chars() {
            key(&mut t, KeyCode::Char(c));
        }
        key(&mut t, KeyCode::Enter);
        assert_eq!(stage(&t), Some(Stage::Confirm));
        key(&mut t, KeyCode::Esc);
        assert_eq!(stage(&t), Some(Stage::Tag));
        key(&mut t, KeyCode::Enter);
        assert_eq!(stage(&t), Some(Stage::Confirm));
        assert!(press(&mut t, ctrl_c).contains(&Effect::Quit));

        t.ok(inv("ecr", "web"), CacheValue::Inventory(vec![]));
        let fx = key(&mut t, KeyCode::Enter);
        assert_eq!(pushes(&fx).len(), 1);
        assert_eq!(stage(&t), Some(Stage::Progress));
        assert!(press(&mut t, ctrl_c).contains(&Effect::Quit));
        key(&mut t, KeyCode::Enter);
        assert_eq!(t.s.wizard(), None);

        // Hidden wizard, push running: normal browsing keys.
        assert_eq!(t.s.key_mode(), KeyMode::Normal);
        key(&mut t, KeyCode::Char('h'));
        assert_eq!(t.s.focus(), Pane::Repos);
        key(&mut t, KeyCode::Char('j'));
        assert_eq!(t.s.selected_repo().unwrap().path, rp("web"));
        assert!(!fetched(&key(&mut t, KeyCode::Char('r'))).is_empty());
        assert!(t.s.write_running());
    }

    #[test]
    fn pending_focus_survives_wizard_close() {
        let mut t = ready(vec![art('a', &["1.0.0"])]);
        t.act(Action::PrevPane);
        t.act(Action::PushStart);
        t.act(Action::FilterApply);
        t.act(Action::Enter);
        t.ok(
            inv("ecr", "app"),
            CacheValue::Inventory(vec![art('a', &["1.0.0"])]),
        );
        typ(&mut t, "1.0.1");
        t.act(Action::FilterApply);
        assert_eq!(pushes(&t.act(Action::Enter)).len(), 1);
        t.ev(done(1, PushOutcome::Pushed(Some(dg('c')))));
        t.act(Action::Enter);
        assert_eq!(t.s.wizard(), None);
        assert_eq!(t.s.focus(), Pane::Repos);

        t.ok(
            inv("ecr", "app"),
            CacheValue::Inventory(vec![art('a', &["1.0.0"]), art('c', &["1.0.1"])]),
        );
        assert_eq!(t.s.focus(), Pane::Images);
        assert_eq!(
            t.s.selected_image().map(|a| a.digest.clone()),
            Some(dg('c'))
        );
    }
}

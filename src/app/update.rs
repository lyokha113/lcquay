//! `update`: the pure state machine. It never does I/O; it only emits `Effect`s.

use time::{OffsetDateTime, UtcOffset, macros::format_description};

use super::messages::{describe, user_message};
use super::state::{
    Action, Effect, Event, LoginState, Pane, RepoRow, RequestsView, SortKey, State, StatusKind, Tab,
};
use crate::config::{AuthCfg, Theme};
use crate::domain::RegistryError;

pub const QUIT_HINT: &str = "a push/delete/copy is running — press q again to quit";

// CacheValue is large (manifest/config inline); one value per fetch, so size is irrelevant.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Action(Action),
    Event(Event),
}

pub fn update(state: &mut State, input: Input, now: OffsetDateTime) -> Vec<Effect> {
    let mut fx = match input {
        Input::Action(a) => action(state, a, now),
        Input::Event(e) => event(state, e, now),
    };
    fx.extend(state.sync());
    fx
}

/// Theme picker keys: ↑↓ preview, Enter keeps and saves, Esc restores the theme it opened with.
fn theme_action(s: &mut State, a: Action) -> Vec<Effect> {
    let Some((row, original)) = s.theme_pick else {
        return vec![];
    };
    match a {
        Action::Up | Action::Down => {
            let row = if a == Action::Up {
                row.saturating_sub(1)
            } else {
                (row + 1).min(Theme::ALL.len() - 1)
            };
            s.theme_pick = Some((row, original));
            s.theme = Theme::ALL[row];
        }
        Action::Enter => {
            s.theme_pick = None;
            s.set_status(StatusKind::Ok, format!("theme: {}", s.theme.label()));
            return vec![Effect::SaveTheme(s.theme)];
        }
        Action::Back => {
            s.theme_pick = None;
            s.theme = original;
        }
        _ => {}
    }
    vec![]
}

/// Request overlay keys: ↑↓ scroll back in time / forward, `/` filters, Esc stops typing (and
/// clears the filter), then closes. Everything else is inert.
fn requests_action(s: &mut State, a: Action) -> Vec<Effect> {
    let Some(r) = s.requests.as_mut() else {
        return vec![];
    };
    match a {
        Action::Up => r.scroll = r.scroll.saturating_add(1),
        Action::Down => r.scroll = r.scroll.saturating_sub(1),
        Action::FilterStart => r.filter_editing = true,
        Action::FilterChar(c) if r.filter_editing => {
            r.filter.push(c);
            r.scroll = 0;
        }
        Action::FilterBackspace if r.filter_editing => {
            r.filter.pop();
            r.scroll = 0;
        }
        Action::FilterApply => r.filter_editing = false,
        Action::FilterCancel => {
            r.filter_editing = false;
            r.filter.clear();
            r.scroll = 0;
        }
        Action::Back => s.requests = None,
        Action::ForceQuit => return vec![Effect::Quit],
        _ => {}
    }
    vec![]
}

fn action(s: &mut State, a: Action, now: OffsetDateTime) -> Vec<Effect> {
    // Open modals take every key but quit (Ctrl-C), before the pending-focus clear below:
    // closing a finished push with Enter must not cancel focusing the pushed image.
    let captured = !matches!(a, Action::Quit | Action::ForceQuit | Action::ClampScroll(_));
    // `ClampScroll` follows every redraw; it must not disarm a refused `q`.
    let armed = !matches!(a, Action::ClampScroll(_)) && std::mem::take(&mut s.quit_armed);
    if a == Action::EditConfig
        && let Some(m) = s.reload_refusal()
    {
        s.set_status(StatusKind::Error, m);
        return vec![];
    }
    // Modals capture keys in the order `ui::render` draws them on top and `State::key_mode`
    // picks: remove, form, compare, requests, delete, pull, copy, export, wizard, glob, theme,
    // help.
    if s.remove.is_some() && captured {
        return s.remove_action(a);
    }
    if s.form.is_some() && captured {
        return s.form_action(a);
    }
    if s.compare.is_some() && captured {
        return s.compare_action(a);
    }
    if s.requests.is_some() && captured {
        return requests_action(s, a);
    }
    if s.delete.as_ref().is_some_and(|m| !m.hidden) && captured {
        return s.delete_action(a);
    }
    if s.pull.as_ref().is_some_and(|m| !m.hidden) && captured {
        return s.pull_action(a, now);
    }
    if s.copy.as_ref().is_some_and(|w| !w.hidden) && captured {
        return s.copy_action(a, now);
    }
    if s.export.as_ref().is_some_and(|m| !m.hidden) && captured {
        return s.export_action(a, now);
    }
    if s.wizard.is_some() && captured {
        return s.wizard_action(a);
    }
    if s.glob.is_some() && captured {
        s.glob_action(a);
        return vec![];
    }
    if s.theme_pick.is_some() && captured {
        return theme_action(s, a);
    }
    if s.help && matches!(a, Action::Up | Action::Down) {
        s.help_scroll = if a == Action::Up {
            s.help_scroll.saturating_sub(1)
        } else {
            s.help_scroll.saturating_add(1)
        };
        return vec![];
    }
    if matches!(
        a,
        Action::Up
            | Action::Down
            | Action::NextPane
            | Action::PrevPane
            | Action::Left
            | Action::Right
            | Action::Enter
    ) {
        // The user moved on; do not yank the selection to the pushed image later.
        s.pending_focus = None;
    }
    match a {
        Action::Up | Action::Down => {
            let (pane, len) = (s.focus, s.list_len(s.focus));
            let cur = s.selected(pane);
            let next = if a == Action::Up {
                cur.saturating_sub(1)
            } else {
                (cur + 1).min(len.saturating_sub(1))
            };
            if next != cur {
                s.select(pane, next);
            }
        }
        Action::Enter | Action::Left | Action::Right if fold(s, a) => {}
        Action::NextPane | Action::Enter | Action::Right => set_focus(s, deeper(s.focus)),
        Action::PrevPane | Action::Left => set_focus(s, shallower(s.focus)),
        Action::Tab(t) => {
            s.tab = t;
            s.scroll = 0;
        }
        Action::PlatformNext | Action::PlatformPrev => {
            if let Some(n) = s.platform_count().filter(|n| *n > 0) {
                let step = if a == Action::PlatformNext { 1 } else { n - 1 };
                s.platform = (s.platform + step) % n;
                s.scroll = 0;
            }
        }
        Action::ScrollDown => s.scroll = s.scroll.saturating_add(1),
        Action::ScrollUp => s.scroll = s.scroll.saturating_sub(1),
        Action::ClampScroll(max) => s.scroll = s.scroll.min(max),
        Action::Refresh => {
            return s
                .refresh_scope()
                .into_iter()
                .filter_map(|k| s.cache.refresh(k))
                .collect();
        }
        Action::FilterStart => s.filter_editing = s.focus != Pane::Registries,
        Action::FilterChar(c) if s.filter_editing => edit_filter(s, |f| f.push(c)),
        Action::FilterBackspace if s.filter_editing => edit_filter(s, |f| {
            f.pop();
        }),
        Action::FilterCancel if s.filter_editing => {
            s.filter_editing = false;
            edit_filter(s, String::clear);
        }
        Action::FilterApply => s.filter_editing = false,
        Action::FilterChar(_) | Action::FilterBackspace | Action::FilterCancel => {}
        Action::SortCycle => match s.focus {
            Pane::Repos => {
                let next = if s.repo_sort.0 == SortKey::Name {
                    SortKey::Count
                } else {
                    SortKey::Name
                };
                s.repo_sort.0 = next;
            }
            Pane::Images => {
                let caps = s.selected_registry().map(|r| r.caps);
                let cols: Vec<SortKey> = [
                    SortKey::Tag,
                    SortKey::Pushed,
                    SortKey::Size,
                    SortKey::Pulled,
                ]
                .into_iter()
                .filter(|k| match k {
                    SortKey::Pushed => caps.is_some_and(|c| c.pushed_at),
                    SortKey::Pulled => caps.is_some_and(|c| c.last_pulled),
                    _ => true,
                })
                .collect();
                let (key, desc) = s.image_sort();
                let i = cols.iter().position(|k| *k == key).map_or(0, |i| i + 1);
                s.image_sort = Some((cols[i % cols.len()], desc));
            }
            Pane::Registries => {}
        },
        Action::SortReverse => match s.focus {
            Pane::Repos => {
                s.repo_sort.1 = !s.repo_sort.1;
            }
            Pane::Images => {
                let (key, desc) = s.image_sort();
                s.image_sort = Some((key, !desc));
            }
            Pane::Registries => {}
        },
        Action::Copy if s.focus == Pane::Images => {
            let text = if s.tab == Tab::Raw {
                s.raw_text()
            } else {
                s.image_reference()
            };
            return text.map(Effect::Copy).into_iter().collect();
        }
        Action::Copy => {}
        Action::ToggleReveal => {
            let img = s.selected_image().map(|a| a.digest.clone());
            if s.focus == Pane::Images && img.is_some() {
                s.reveal = if s.reveal == img { None } else { img };
            }
        }
        Action::ToggleHelp => {
            s.help = !s.help;
            s.help_scroll = 0;
        }
        Action::ThemePick => {
            let row = Theme::ALL.iter().position(|t| *t == s.theme).unwrap_or(0);
            s.theme_pick = Some((row, s.theme));
        }
        Action::Login => return login(s),
        Action::PushStart => return s.push_start(),
        Action::PullStart => return s.pull_start(),
        Action::CopyStart => return s.copy_start(),
        Action::ExportStart => return s.export_start(),
        // Only the export details map `a` to it; they took it above.
        Action::ToggleAll => {}
        // `reload_refusal` was checked above.
        Action::EditConfig => return vec![Effect::EditConfig],
        Action::Mark | Action::MarkUntagged | Action::MarkGlob | Action::RangeMark => {
            s.mark_action(a)
        }
        Action::ClearMarks if s.focus == Pane::Images => s.esc_images(),
        Action::ClearMarks => {}
        Action::CompareMark => s.compare_mark(),
        Action::DeleteStart => return s.delete_start(),
        // Only from plain Normal mode; the modals above took their keys already.
        Action::Requests if !s.help && !s.filter_editing => {
            s.requests = Some(RequestsView::default())
        }
        Action::Requests => {}
        Action::RegistryEdit | Action::RegistryNew | Action::RegistryRemove => {
            return s.registry_key(a);
        }
        // Form and prompt keys; their modals took them above.
        Action::FormNext
        | Action::FormPrev
        | Action::FormTest
        | Action::FormSave
        | Action::Confirm(_) => {}
        Action::ToggleWhole | Action::Back => {}
        Action::Quit if s.write_running() && !armed => {
            s.quit_armed = true;
            s.set_status(StatusKind::Info, QUIT_HINT.into());
        }
        Action::Quit | Action::ForceQuit => return vec![Effect::Quit],
    }
    vec![]
}

/// On a selected repo group: Enter toggles it, → expands it, ← collapses it. `false` when the
/// key does nothing to a group (it then switches panes).
fn fold(s: &mut State, a: Action) -> bool {
    if s.focus != Pane::Repos {
        return false;
    }
    let Some(RepoRow::Group {
        prefix, expanded, ..
    }) = s.selected_row()
    else {
        return false;
    };
    let Some(reg) = s.selected_registry().map(|r| r.id.clone()) else {
        return false;
    };
    match (a, expanded) {
        (Action::Enter | Action::Right, false) => s.expanded.insert((reg, prefix)),
        (Action::Enter | Action::Left, true) => s.expanded.remove(&(reg, prefix)),
        _ => return false,
    };
    true
}

fn deeper(p: Pane) -> Pane {
    match p {
        Pane::Registries => Pane::Repos,
        _ => Pane::Images,
    }
}

fn shallower(p: Pane) -> Pane {
    match p {
        Pane::Images => Pane::Repos,
        _ => Pane::Registries,
    }
}

/// The detail pane shows a different item: reset its scroll and re-mask.
fn set_focus(s: &mut State, p: Pane) {
    if s.focus != p {
        s.focus = p;
        s.filter_editing = false;
        s.scroll = 0;
        s.reveal = None;
    }
}

/// Edits the focused pane's filter. The selection stays if still visible (else `sync` pins row 0).
fn edit_filter(s: &mut State, f: impl FnOnce(&mut String)) {
    match s.focus {
        Pane::Repos => f(&mut s.repo_filter),
        Pane::Images => f(&mut s.image_filter),
        Pane::Registries => {}
    }
}

fn event(s: &mut State, e: Event, now: OffsetDateTime) -> Vec<Effect> {
    match e {
        Event::Loaded { key, req, result } => {
            if !s.cache.is_pending(&key, req) {
                return vec![];
            }
            let reg = key
                .registry()
                .and_then(|id| s.registries.iter_mut().find(|r| &r.id == id));
            match &result {
                Ok(_) => {
                    // referrers() maps 401 to Ok(None) and details answer without the registry
                    // login (STS or nothing), so none of them says anything about auth health.
                    if let Some(r) = reg.filter(|_| {
                        !matches!(
                            key,
                            super::state::CacheKey::Referrers(..)
                                | super::state::CacheKey::RegistryDetails(_)
                                | super::state::CacheKey::RepoDetails(..)
                        )
                    }) {
                        r.auth_failed = false;
                    }
                    if s.status
                        .as_ref()
                        .is_some_and(|(k, _, _)| k.as_ref() == Some(&key))
                    {
                        s.status = None;
                    }
                }
                Err(err) => {
                    // A missing pinned repo: its row already says `not found`.
                    let quiet = *err == RegistryError::NotFound
                        && matches!((&key, &reg), (super::state::CacheKey::Inventory(_, p), Some(r))
                            if r.pinned.iter().any(|x| &x.path == p));
                    let msg = match reg {
                        Some(r) => {
                            r.auth_failed |= *err == RegistryError::Unauthorized;
                            user_message(err, r, &describe(&key))
                        }
                        // Local images: a Docker failure or timeout only disables local checks.
                        None => crate::docker::UNAVAILABLE.to_owned(),
                    };
                    // The wizards do not route `r`; their retry hint (Enter) is drawn by the UI.
                    let msg = if s.wizard.as_ref().is_some_and(|w| w.loads(&key))
                        || s.copy.as_ref().is_some_and(|w| w.loads(&key))
                    {
                        msg.split_once(" — press r")
                            .map_or(msg.as_str(), |(m, _)| m)
                            .to_owned()
                    } else {
                        msg
                    };
                    s.failures.insert(key.clone(), msg.clone());
                    if !quiet {
                        s.status = Some((Some(key.clone()), StatusKind::Error, msg));
                    }
                }
            }
            let mut fx: Vec<Effect> = s
                .cache
                .apply(key.clone(), req, result, now)
                .into_iter()
                .collect();
            s.focus_pushed(&key);
            s.delete_loaded(&key);
            fx.extend(s.wizard_loaded(&key));
            fx.extend(s.copy_loaded(&key));
            fx
        }
        Event::Progress { key, pages } => {
            s.cache.set_pages(&key, pages);
            vec![]
        }
        Event::CopyResult(r) => {
            s.status = Some(match r {
                Ok(()) => (None, StatusKind::Ok, "copied to clipboard".to_owned()),
                Err(e) => (
                    None,
                    StatusKind::Error,
                    format!("clipboard unavailable: {e}"),
                ),
            });
            vec![]
        }
        Event::LoginDone { registry, result } => {
            let Some(r) = s.registries.iter_mut().find(|r| r.id == registry) else {
                return vec![];
            };
            let (kind, msg) = match result {
                Ok(expires_at) => {
                    r.login = Some(LoginState::Done { expires_at });
                    let msg = match expires_at {
                        Some(t) => format!("logged in to {} (expires {} UTC)", r.id, hm_utc(t)),
                        None => format!("logged in to {}", r.id),
                    };
                    (StatusKind::Ok, msg)
                }
                // The last attempt failed, so an earlier login's state is unknown now.
                Err(e) => {
                    r.login = None;
                    // `r` refreshes browsing, not a login, so point at `L`.
                    let msg = match &e {
                        RegistryError::Timeout(_) => {
                            format!("docker login to {} timed out — press L to retry", r.id)
                        }
                        RegistryError::Network(m) => {
                            format!("network error: {m} — press L to retry")
                        }
                        _ => user_message(&e, r, "login"),
                    };
                    (StatusKind::Error, msg)
                }
            };
            s.status = Some((None, kind, msg));
            vec![]
        }
        Event::PushProgress { op, step } => {
            s.push_progress(op, step);
            vec![]
        }
        Event::PushDone { op, outcome } => s.push_done(op, outcome),
        Event::PullProgress { op, step } => {
            s.pull_progress(op, step, now);
            vec![]
        }
        Event::PullDone { op, outcome } => s.pull_done(op, outcome, now),
        Event::CopyProgress { op, step } => {
            s.copy_progress(op, step, now);
            vec![]
        }
        Event::CopyDone { op, outcome } => s.copy_done(op, outcome, now),
        Event::ExportProgress { op, step } => {
            s.export_progress(op, step, now);
            vec![]
        }
        Event::ExportDone { op, outcome } => {
            s.export_done(op, outcome, now);
            vec![]
        }
        Event::DeleteSnapshot { op, result } => s.delete_snapshot(op, result),
        Event::DeleteDone {
            op,
            report,
            stopped,
        } => s.delete_done(op, report, stopped),
        Event::ConnCheck { op, step, check } => {
            s.conn_check(op, step, check);
            vec![]
        }
    }
}

/// `docker login` to the selected registry, when its credential allows it.
fn login(s: &mut State) -> Vec<Effect> {
    let Some(r) = s.registries.get_mut(s.sel_reg) else {
        return vec![];
    };
    let (msg, fx) = if !r.can_login {
        let msg = match &r.auth {
            AuthCfg::Anonymous => format!("{} uses anonymous access; nothing to log in", r.id),
            _ => format!(
                "log in to {0} outside lcquay: `docker login {0}`",
                r.ref_host
            ),
        };
        (msg, vec![])
    } else if r.login == Some(LoginState::Running) {
        (format!("login to {} already running", r.id), vec![])
    } else {
        r.login = Some(LoginState::Running);
        (
            format!("logging in to {}…", r.id),
            vec![Effect::Login(r.id.clone())],
        )
    };
    s.status = Some((None, StatusKind::Info, msg));
    fx
}

/// `HH:MM` of `t` in UTC.
fn hm_utc(t: OffsetDateTime) -> String {
    t.to_offset(UtcOffset::UTC)
        .format(format_description!("[hour]:[minute]"))
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::{BTreeMap, HashMap};

    use time::macros::datetime;

    use super::*;
    use crate::app::cache::Entry;
    use crate::app::compare::Side;
    use crate::app::state::{
        CacheKey, CacheValue, LoginState, Pane, RegistryView, RepoRow, RequestId, SortKey, Tab,
    };
    use crate::config::AuthCfg;
    use crate::domain::{
        ArtifactStatus, ArtifactSummary, Caps, Descriptor, Details, Digest, ImageConfig, Inventory,
        ManifestDocument, ManifestKind, RegistryError, RegistryId, RepoPath, RepoSummary,
    };

    const NOW: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

    pub(crate) fn dg(c: char) -> Digest {
        Digest::parse(&format!("sha256:{}", c.to_string().repeat(64))).unwrap()
    }

    pub(crate) fn rid(id: &str) -> RegistryId {
        RegistryId(id.into())
    }

    pub(crate) fn rp(p: &str) -> RepoPath {
        RepoPath(p.into())
    }

    fn view(id: &str, auth: AuthCfg, pushed_at: bool) -> RegistryView {
        RegistryView {
            id: rid(id),
            ref_host: "h.example".into(),
            endpoint: "https://h.example".parse().unwrap(),
            provider: if pushed_at { "ecr" } else { "oci" },
            auth,
            caps: Caps {
                inventory: if pushed_at {
                    Inventory::Complete
                } else {
                    Inventory::TaggedOnly
                },
                pushed_at,
                last_pulled: pushed_at,
                delete_tag: false,
                delete_manifest: pushed_at,
            },
            auth_failed: false,
            can_login: true,
            login: None,
            pinned: vec![],
        }
    }

    pub(crate) fn art(c: char, tags: &[&str]) -> ArtifactSummary {
        ArtifactSummary {
            digest: dg(c),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            media_type: None,
            artifact_type: None,
            subject: None,
            size: None,
            pushed_at: None,
            last_pulled: None,
            status: ArtifactStatus::Active,
        }
    }

    pub(crate) fn desc(c: char, size: u64) -> Descriptor {
        Descriptor {
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            digest: dg(c),
            size,
            platform: None,
            artifact_type: None,
            annotations: BTreeMap::new(),
        }
    }

    pub(crate) fn manifest(kind: ManifestKind) -> CacheValue {
        CacheValue::Manifest(ManifestDocument {
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            raw: r#"{"schemaVersion":2}"#.into(),
            kind,
            subject: None,
            artifact_type: None,
            annotations: BTreeMap::new(),
        })
    }

    fn image(cfg: char, layers: &[u64]) -> CacheValue {
        manifest(ManifestKind::Image {
            config: desc(cfg, 10),
            layers: layers.iter().map(|s| desc('0', *s)).collect(),
        })
    }

    fn index(children: &[char]) -> CacheValue {
        manifest(ManifestKind::Index {
            manifests: children.iter().map(|c| desc(*c, 500)).collect(),
        })
    }

    fn config(raw: &str) -> CacheValue {
        CacheValue::Config(Some(ImageConfig {
            raw: raw.into(),
            created: None,
            platform: None,
            entrypoint: vec![],
            cmd: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            volumes: vec![],
            labels: BTreeMap::new(),
            env: vec![],
            history: vec![],
        }))
    }

    pub(crate) fn repos(names: &[&str]) -> CacheValue {
        CacheValue::Repos(
            names
                .iter()
                .map(|n| RepoSummary {
                    path: rp(n),
                    created_at: None,
                })
                .collect(),
        )
    }

    pub(crate) fn inv(r: &str, repo: &str) -> CacheKey {
        CacheKey::Inventory(rid(r), rp(repo))
    }

    fn rdet(r: &str, repo: &str) -> CacheKey {
        CacheKey::RepoDetails(rid(r), rp(repo))
    }

    pub(crate) fn man(r: &str, c: char) -> CacheKey {
        CacheKey::Manifest(rid(r), rp("app"), dg(c))
    }

    fn cfg(r: &str, c: char) -> CacheKey {
        CacheKey::Config(rid(r), rp("app"), dg(c))
    }

    fn refs(r: &str, c: char) -> CacheKey {
        CacheKey::Referrers(rid(r), rp("app"), dg(c))
    }

    /// State plus the fetches it has issued and not yet answered.
    pub(crate) struct T {
        pub(crate) s: State,
        pending: HashMap<CacheKey, RequestId>,
    }

    impl T {
        pub(crate) fn new(regs: Vec<RegistryView>) -> (T, Vec<Effect>) {
            let (s, fx) = State::new(regs);
            let mut t = T {
                s,
                pending: HashMap::new(),
            };
            t.track(&fx);
            (t, fx)
        }

        fn track(&mut self, fx: &[Effect]) {
            for e in fx {
                if let Effect::Fetch { key, req } = e {
                    self.pending.insert(key.clone(), *req);
                }
            }
        }

        pub(crate) fn act(&mut self, a: Action) -> Vec<Effect> {
            let fx = update(&mut self.s, Input::Action(a), NOW);
            self.track(&fx);
            fx
        }

        pub(crate) fn ev(&mut self, e: Event) -> Vec<Effect> {
            let fx = update(&mut self.s, Input::Event(e), NOW);
            self.track(&fx);
            fx
        }

        /// `ev` at `now` (progress timing).
        pub(crate) fn ev_at(&mut self, e: Event, now: OffsetDateTime) -> Vec<Effect> {
            let fx = update(&mut self.s, Input::Event(e), now);
            self.track(&fx);
            fx
        }

        pub(crate) fn load(
            &mut self,
            key: CacheKey,
            result: Result<CacheValue, RegistryError>,
        ) -> Vec<Effect> {
            let req = self
                .pending
                .remove(&key)
                .unwrap_or_else(|| panic!("no fetch for {key:?}"));
            self.ev(Event::Loaded { key, req, result })
        }

        pub(crate) fn ok(&mut self, key: CacheKey, v: CacheValue) -> Vec<Effect> {
            self.load(key, Ok(v))
        }

        fn tags(&self) -> Vec<String> {
            self.s
                .visible_images()
                .iter()
                .map(|a| a.tags.join(","))
                .collect()
        }
    }

    pub(crate) fn fetched(fx: &[Effect]) -> Vec<CacheKey> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Fetch { key, .. } => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    fn focused(fx: &[Effect]) -> Option<Vec<CacheKey>> {
        fx.iter().find_map(|e| match e {
            Effect::SetFocused(k) => Some(k.clone()),
            _ => None,
        })
    }

    pub(crate) fn ecr() -> RegistryView {
        view("ecr", AuthCfg::Ecr, true)
    }

    /// Focus on the images pane of ecr/app with `images` loaded.
    pub(crate) fn at_images(images: Vec<ArtifactSummary>) -> (T, Vec<Effect>) {
        let (mut t, _) = T::new(vec![ecr(), view("oci", AuthCfg::Anonymous, false)]);
        t.ok(CacheKey::Repos(rid("ecr")), repos(&["app", "web"]));
        t.act(Action::NextPane);
        t.ok(inv("ecr", "app"), CacheValue::Inventory(images));
        let fx = t.act(Action::NextPane);
        (t, fx)
    }

    /// Single-registry variant of `at_images`: focus on the images pane of `<reg>/app`.
    fn at_images_in(reg: RegistryView, images: Vec<ArtifactSummary>) -> (T, Vec<Effect>) {
        let id = reg.id.0.clone();
        let (mut t, _) = T::new(vec![reg]);
        t.ok(CacheKey::Repos(rid(&id)), repos(&["app"]));
        t.act(Action::NextPane);
        t.ok(inv(&id, "app"), CacheValue::Inventory(images));
        let fx = t.act(Action::NextPane);
        (t, fx)
    }

    fn manifests(fx: &[Effect]) -> Vec<CacheKey> {
        fetched(fx)
            .into_iter()
            .filter(|k| matches!(k, CacheKey::Manifest(..)))
            .collect()
    }

    #[test]
    fn reorder_keeps_selected_digest_and_reveal() {
        let oci = view("oci", AuthCfg::Anonymous, false);
        let (mut t, _) = at_images_in(oci, vec![art('a', &["1"]), art('b', &["2"])]);
        t.act(Action::SortCycle); // Size desc; all sizes unknown → inventory order
        assert_eq!(t.s.image_sort(), (SortKey::Size, true));
        assert_eq!(t.tags(), vec!["1", "2"]);
        t.act(Action::Down);
        assert_eq!(t.s.selected_image().unwrap().digest, dg('b'));
        t.act(Action::ToggleReveal);
        assert!(t.s.reveal());

        t.ok(man("oci", 'b'), image('c', &[100])); // b gets a size → moves to row 0
        assert_eq!(t.tags(), vec!["2", "1"]);
        assert_eq!(t.s.selected_image().unwrap().digest, dg('b'));
        assert_eq!(t.s.selected(Pane::Images), 0);
        assert!(t.s.reveal(), "still revealed for b");

        t.act(Action::Down);
        assert_eq!(t.s.selected_image().unwrap().digest, dg('a'));
        assert!(!t.s.reveal(), "reveal never applies to another image");
        t.act(Action::Up);
        assert!(!t.s.reveal(), "leaving the image re-masked it");
    }

    #[test]
    fn size_asc_sort_does_not_cascade_fetches() {
        let oci = view("oci", AuthCfg::Anonymous, false);
        let imgs = vec![art('a', &["1"]), art('b', &["2"]), art('c', &["3"])];
        let (mut t, fx) = at_images_in(oci, imgs);
        let mut seen = manifests(&fx);
        assert_eq!(seen, vec![man("oci", 'c')], "tag desc: c selected");
        seen.extend(manifests(&t.act(Action::SortCycle)));
        seen.extend(manifests(&t.act(Action::SortReverse)));
        assert_eq!(t.s.image_sort(), (SortKey::Size, false));
        seen.extend(manifests(&t.ok(man("oci", 'c'), image('d', &[1]))));
        assert_eq!(
            t.tags(),
            vec!["1", "2", "3"],
            "c (sized) sorts after unknowns"
        );
        assert_eq!(t.s.selected_image().unwrap().digest, dg('c'));
        assert_eq!(
            seen,
            vec![man("oci", 'c')],
            "only the selected image's manifest"
        );
    }

    #[test]
    fn refresh_reorder_keeps_selected_digest() {
        let mut imgs = vec![art('a', &["a"]), art('b', &["b"]), art('c', &["c"])];
        imgs[0].pushed_at = Some(datetime!(2024-01-01 0:00 UTC));
        imgs[1].pushed_at = Some(datetime!(2024-02-01 0:00 UTC));
        imgs[2].pushed_at = Some(datetime!(2024-03-01 0:00 UTC));
        let (mut t, _) = at_images(imgs.clone());
        assert_eq!(t.tags(), vec!["c", "b", "a"]);
        t.act(Action::Down);
        assert_eq!(t.s.selected_image().unwrap().digest, dg('b'));

        t.act(Action::PrevPane);
        assert_eq!(fetched(&t.act(Action::Refresh))[0], inv("ecr", "app"));
        imgs[1].pushed_at = Some(datetime!(2024-04-01 0:00 UTC));
        t.ok(inv("ecr", "app"), CacheValue::Inventory(imgs));
        t.act(Action::NextPane);
        assert_eq!(t.tags(), vec!["b", "c", "a"]);
        assert_eq!(t.s.selected_image().unwrap().digest, dg('b'));
        assert_eq!(t.s.selected(Pane::Images), 0);
    }

    #[test]
    fn startup_fetches_repos_of_first_registry_and_local_images() {
        let (t, fx) = T::new(vec![ecr(), view("oci", AuthCfg::Anonymous, false)]);
        let keys = fetched(&fx);
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&CacheKey::Repos(rid("ecr"))));
        assert!(keys.contains(&CacheKey::RegistryDetails(rid("ecr"))));
        assert!(keys.contains(&CacheKey::LocalImages));
        assert_eq!(
            focused(&fx),
            Some(vec![
                CacheKey::Repos(rid("ecr")),
                CacheKey::RegistryDetails(rid("ecr"))
            ])
        );
        assert_eq!(t.s.focus(), Pane::Registries);
    }

    #[test]
    fn focus_fetches_registry_details() {
        let (_, fx) = T::new(vec![ecr()]);
        assert_eq!(
            focused(&fx),
            Some(vec![
                CacheKey::Repos(rid("ecr")),
                CacheKey::RegistryDetails(rid("ecr"))
            ])
        );
        assert!(fetched(&fx).contains(&CacheKey::RegistryDetails(rid("ecr"))));
        assert!(fetched(&fx).contains(&CacheKey::Repos(rid("ecr"))));
    }

    #[test]
    fn focus_fetches_repo_details() {
        let (mut t, _) = T::new(vec![ecr()]);
        t.ok(CacheKey::Repos(rid("ecr")), repos(&["app"]));
        let fx = t.act(Action::NextPane);
        assert_eq!(
            focused(&fx),
            Some(vec![
                CacheKey::Repos(rid("ecr")),
                inv("ecr", "app"),
                rdet("ecr", "app")
            ])
        );
        assert!(fetched(&fx).contains(&rdet("ecr", "app")));
        t.ok(
            inv("ecr", "app"),
            CacheValue::Inventory(vec![art('a', &["1"])]),
        );
        let fx = t.act(Action::NextPane);
        let keys = focused(&fx).unwrap();
        assert!(!keys.iter().any(|k| matches!(k, CacheKey::RepoDetails(..))));
    }

    #[test]
    fn refresh_includes_details() {
        let (mut t, _) = T::new(vec![ecr()]);
        t.ok(CacheKey::Repos(rid("ecr")), repos(&["app"]));
        let reg = CacheKey::RegistryDetails(rid("ecr"));
        t.ok(reg.clone(), CacheValue::Details(Details::default()));
        t.act(Action::NextPane);
        t.ok(inv("ecr", "app"), CacheValue::Inventory(vec![]));
        t.ok(rdet("ecr", "app"), CacheValue::Details(Details::default()));

        let keys = fetched(&t.act(Action::Refresh));
        assert!(keys.contains(&rdet("ecr", "app")));
        assert!(!keys.contains(&reg));

        t.ok(inv("ecr", "app"), CacheValue::Inventory(vec![]));
        t.ok(rdet("ecr", "app"), CacheValue::Details(Details::default()));
        t.act(Action::PrevPane);
        let keys = fetched(&t.act(Action::Refresh));
        assert!(keys.contains(&reg));
        assert!(keys.contains(&rdet("ecr", "app")));
        assert!(keys.contains(&CacheKey::Repos(rid("ecr"))));
    }

    #[test]
    fn oci_details_are_silent() {
        let (mut t, _) = T::new(vec![view("oci", AuthCfg::Anonymous, false)]);
        t.ok(
            CacheKey::RegistryDetails(rid("oci")),
            CacheValue::Details(Details::default()),
        );
        assert_eq!(msg(&t.s), None);
    }

    #[test]
    fn details_ok_keeps_auth_warning() {
        let (mut t, _) = T::new(vec![ecr()]);
        t.load(
            CacheKey::Repos(rid("ecr")),
            Err(RegistryError::Unauthorized),
        );
        assert!(t.s.registries()[0].auth_failed);
        t.ok(
            CacheKey::RegistryDetails(rid("ecr")),
            CacheValue::Details(Details::default()),
        );
        assert!(t.s.registries()[0].auth_failed);
    }

    #[test]
    fn local_images_failure_says_docker_unavailable() {
        let (mut t, _) = T::new(vec![ecr()]);
        t.load(
            CacheKey::LocalImages,
            Err(RegistryError::Timeout("docker".into())),
        );
        assert_eq!(msg(&t.s), Some(crate::docker::UNAVAILABLE));
    }

    #[test]
    fn browse_cascade() {
        let (mut t, _) = T::new(vec![ecr()]);
        let fx = t.ok(CacheKey::Repos(rid("ecr")), repos(&["app", "web"]));
        assert!(fetched(&fx).is_empty(), "repos pane not focused yet");
        assert_eq!(t.s.visible_repos().len(), 2);

        let fx = t.act(Action::NextPane);
        assert_eq!(fetched(&fx), vec![inv("ecr", "app"), rdet("ecr", "app")]);
        assert_eq!(
            focused(&fx),
            Some(vec![
                CacheKey::Repos(rid("ecr")),
                inv("ecr", "app"),
                rdet("ecr", "app")
            ])
        );

        let fx = t.ok(
            inv("ecr", "app"),
            CacheValue::Inventory(vec![art('a', &["1.0"])]),
        );
        assert!(fetched(&fx).is_empty(), "images pane not focused yet");
        assert_eq!(t.s.repo_count(&rp("app")), Some(1));
        assert_eq!(t.s.display_size(&art('a', &[])), None, "blank before focus");

        let fx = t.act(Action::Enter);
        assert_eq!(t.s.focus(), Pane::Images);
        let keys = fetched(&fx);
        assert!(keys.contains(&man("ecr", 'a')));
        assert!(keys.contains(&refs("ecr", 'a')), "referrers on Overview");
        assert!(
            !keys.contains(&cfg("ecr", 'a')),
            "config waits for the manifest"
        );

        let fx = t.ok(man("ecr", 'a'), image('c', &[100, 200]));
        assert_eq!(fetched(&fx), vec![cfg("ecr", 'a')]);
        assert_eq!(t.s.display_size(t.s.selected_image().unwrap()), Some(310));

        let fx = t.ok(cfg("ecr", 'a'), config("{}"));
        assert!(fetched(&fx).is_empty());
        assert!(matches!(
            t.s.entry(&cfg("ecr", 'a')),
            Some(Entry::Ready { .. })
        ));
    }

    #[test]
    fn archived_image_fetches_no_content() {
        let mut a = art('a', &["old"]);
        a.status = ArtifactStatus::Archived;
        let (t, fx) = at_images(vec![a]);
        assert!(fetched(&fx).is_empty());
        assert_eq!(
            focused(&fx),
            Some(vec![CacheKey::Repos(rid("ecr")), inv("ecr", "app")]),
            "repo details are not focused on the images pane"
        );
        assert!(t.s.selected_image().is_some());
        assert_eq!(t.s.raw_text(), None);
    }

    #[test]
    fn empty_repo_list_no_panic_on_navigation() {
        let all = [
            Action::Up,
            Action::Down,
            Action::NextPane,
            Action::Enter,
            Action::Down,
            Action::Up,
            Action::NextPane,
            Action::Refresh,
            Action::Copy,
            Action::ToggleReveal,
            Action::PlatformNext,
            Action::PlatformPrev,
            Action::SortCycle,
            Action::SortReverse,
            Action::FilterStart,
            Action::FilterChar('x'),
            Action::FilterBackspace,
            Action::FilterApply,
            Action::Tab(Tab::Raw),
            Action::ScrollUp,
            Action::PrevPane,
            Action::PrevPane,
            Action::PrevPane,
            Action::Up,
        ];
        let (mut t, _) = T::new(vec![ecr()]);
        t.ok(CacheKey::Repos(rid("ecr")), repos(&[]));
        for a in all {
            t.act(a);
        }
        assert_eq!(t.s.selected_repo(), None);
        assert_eq!(t.s.selected_image(), None);

        let (mut t, fx) = T::new(vec![]);
        assert_eq!(fetched(&fx), vec![CacheKey::LocalImages]);
        for a in all {
            let fx = t.act(a);
            assert!(fetched(&fx).is_empty());
        }
    }

    #[test]
    fn focus_away_and_back_while_loading_resolves() {
        let (mut t, _) = T::new(vec![ecr()]);
        t.ok(CacheKey::Repos(rid("ecr")), repos(&["app", "web"]));
        assert_eq!(
            fetched(&t.act(Action::NextPane)),
            vec![inv("ecr", "app"), rdet("ecr", "app")]
        );
        assert_eq!(
            fetched(&t.act(Action::Down)),
            vec![inv("ecr", "web"), rdet("ecr", "web")]
        );
        let fx = t.act(Action::Up);
        assert!(
            fetched(&fx).is_empty(),
            "app is still loading: no second fetch"
        );
        assert_eq!(
            focused(&fx),
            Some(vec![
                CacheKey::Repos(rid("ecr")),
                inv("ecr", "app"),
                rdet("ecr", "app")
            ])
        );
        t.act(Action::PrevPane);
        assert!(fetched(&t.act(Action::NextPane)).is_empty());
        t.ok(
            inv("ecr", "web"),
            CacheValue::Inventory(vec![art('f', &["w1"])]),
        );
        t.ok(
            inv("ecr", "app"),
            CacheValue::Inventory(vec![art('a', &["a1"])]),
        );
        assert_eq!(t.tags(), vec!["a1"]);
        t.act(Action::Down);
        assert_eq!(t.tags(), vec!["w1"], "background result was cached");
    }

    #[test]
    fn refresh_while_loading_refetches_once() {
        let (mut t, _) = T::new(vec![ecr()]);
        t.ok(CacheKey::Repos(rid("ecr")), repos(&["app"]));
        t.act(Action::NextPane);
        let first = t.pending[&inv("ecr", "app")];
        assert!(fetched(&t.act(Action::Refresh)).is_empty());
        assert!(fetched(&t.act(Action::Refresh)).is_empty());
        let fx = t.ok(
            inv("ecr", "app"),
            CacheValue::Inventory(vec![art('a', &["1"])]),
        );
        assert_eq!(fetched(&fx), vec![inv("ecr", "app")]);
        assert_ne!(t.pending[&inv("ecr", "app")], first);
        assert_eq!(t.tags(), vec!["1"], "first result shown while refetching");
        let fx = t.ok(
            inv("ecr", "app"),
            CacheValue::Inventory(vec![art('a', &["2"])]),
        );
        assert!(fetched(&fx).is_empty());
        assert_eq!(t.tags(), vec!["2"]);
    }

    fn entry(registry: &str, op: &str, path: &str) -> crate::reqlog::RequestEntry {
        crate::reqlog::RequestEntry {
            at: NOW,
            registry: registry.into(),
            op: op.into(),
            path: path.into(),
            outcome: "200".into(),
            ms: 5,
            bytes: None,
        }
    }

    #[test]
    fn requests_overlay_keys() {
        use crate::app::state::KeyMode;
        let (mut t, _) = T::new(vec![ecr()]);
        // `!` opens it, empty, on any pane.
        t.ok(CacheKey::Repos(rid("ecr")), repos(&["app"]));
        for pane in [Pane::Registries, Pane::Repos] {
            assert_eq!(t.s.focus(), pane);
            t.act(Action::Requests);
            let r = t.s.requests().expect("open");
            assert!(r.entries.is_empty() && r.filter.is_empty() && !r.filter_editing);
            assert_eq!(t.s.key_mode(), KeyMode::Scroll);
            t.act(Action::Back);
            assert!(!t.s.requests_open());
            t.act(Action::NextPane);
        }
        t.act(Action::PrevPane);
        t.act(Action::Requests);
        t.s.set_requests(
            (0..5)
                .map(|i| entry("ecr", "GET", &format!("/v2/{i}")))
                .collect(),
        );
        assert_eq!(t.s.requests().unwrap().entries.len(), 5);

        // Scroll counts up from the newest entry.
        t.act(Action::Up);
        t.act(Action::Up);
        assert_eq!(t.s.requests().unwrap().scroll, 2);
        t.act(Action::Down);
        t.act(Action::Down);
        t.act(Action::Down);
        assert_eq!(t.s.requests().unwrap().scroll, 0);

        // Filter typing: chars and Backspace edit, Enter applies, Esc clears.
        t.act(Action::FilterStart);
        assert_eq!(t.s.key_mode(), KeyMode::Text);
        for c in "v2/3x".chars() {
            t.act(Action::FilterChar(c));
        }
        t.act(Action::FilterBackspace);
        assert_eq!(t.s.requests().unwrap().filter, "v2/3");
        t.act(Action::FilterApply);
        assert_eq!(t.s.key_mode(), KeyMode::Scroll);
        assert_eq!(t.s.requests().unwrap().filter, "v2/3", "Enter keeps it");
        assert_eq!(t.s.requests().unwrap().visible().len(), 1);
        t.act(Action::FilterStart);
        t.act(Action::FilterCancel);
        let r = t.s.requests().unwrap();
        assert!(r.filter.is_empty() && !r.filter_editing, "Esc clears");
        assert!(t.s.requests_open(), "the first Esc only stops typing");

        // Everything else is inert; the lists behind it do not move.
        let sel = t.s.selected(Pane::Repos);
        let before = t.s.requests().unwrap().clone();
        for a in [
            Action::Copy,
            Action::Refresh,
            Action::NextPane,
            Action::Enter,
            Action::ToggleHelp,
            Action::ThemePick,
            Action::PushStart,
            Action::DeleteStart,
            Action::Mark,
            Action::Requests,
            Action::Tab(Tab::Raw),
        ] {
            assert!(t.act(a).is_empty(), "{a:?}");
        }
        assert_eq!(t.s.requests(), Some(&before));
        assert_eq!(t.s.selected(Pane::Repos), sel);
        assert!(
            (t.s.focus(), t.s.tab(), t.s.help_shown(), t.s.theme_picker())
                == (Pane::Repos, Tab::Overview, false, None)
        );
        t.act(Action::Back);
        assert!(!t.s.requests_open());
        assert_eq!(t.s.key_mode(), KeyMode::Normal);
    }

    #[test]
    fn requests_not_opened_inside_other_modals() {
        let (mut t, _) = T::new(vec![ecr()]);
        t.act(Action::ToggleHelp);
        t.act(Action::Requests);
        assert!(!t.s.requests_open());
        t.act(Action::ToggleHelp);
        t.act(Action::ThemePick);
        t.act(Action::Requests);
        assert!(!t.s.requests_open());
        t.act(Action::Back);
        t.act(Action::NextPane);
        t.act(Action::FilterStart);
        t.act(Action::Requests);
        assert!(!t.s.requests_open(), "typing a filter");
        t.act(Action::FilterCancel);
        t.act(Action::Requests);
        assert!(t.s.requests_open());
    }

    #[test]
    fn requests_filter() {
        let (mut t, _) = T::new(vec![ecr()]);
        t.act(Action::Requests);
        t.s.set_requests(vec![
            entry("ecr", "GET manifest", "/v2/app/manifests/latest"),
            entry("local", "docker pull", "/images/create"),
        ]);
        assert_eq!(t.s.requests().unwrap().visible().len(), 2);
        t.act(Action::FilterStart);
        for c in "docker".chars() {
            t.act(Action::FilterChar(c));
        }
        let r = t.s.requests().unwrap();
        let v = r.visible();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].registry, "local");
        // Path and outcome match too.
        t.act(Action::FilterCancel);
        for c in "manifests/".chars() {
            t.act(Action::FilterChar(c));
        }
        assert_eq!(t.s.requests().unwrap().visible()[0].registry, "ecr");
    }

    #[test]
    fn theme_picker_previews_saves_and_restores() {
        use crate::app::state::KeyMode;
        let (mut t, _) = T::new(vec![ecr()]);
        assert_eq!(t.s.theme, Theme::MapleDark);
        t.act(Action::ThemePick);
        assert_eq!(t.s.key_mode(), KeyMode::List);
        assert_eq!(t.s.theme_picker(), Some(0));
        t.act(Action::Down);
        assert_eq!(t.s.theme, Theme::MapleDarkBlack, "previewed live");
        t.act(Action::Down);
        t.act(Action::Down);
        t.act(Action::Down);
        assert_eq!(t.s.theme, Theme::TokyoNightBlack, "stops at the last row");
        assert_eq!(
            t.s.selected(Pane::Registries),
            0,
            "keys do not reach the lists"
        );
        t.act(Action::Back);
        assert_eq!(
            (t.s.theme, t.s.theme_picker()),
            (Theme::MapleDark, None),
            "Esc restores"
        );

        t.act(Action::ThemePick);
        t.act(Action::Down);
        let fx = t.act(Action::Enter);
        assert!(
            fx.contains(&Effect::SaveTheme(Theme::MapleDarkBlack)),
            "{fx:?}"
        );
        assert_eq!(
            (t.s.theme, t.s.theme_picker()),
            (Theme::MapleDarkBlack, None)
        );
        assert_eq!(
            t.s.focus(),
            Pane::Registries,
            "Enter does not open the registry"
        );
        t.act(Action::ThemePick);
        assert_eq!(t.s.theme_picker(), Some(1), "opens on the current theme");
    }

    #[test]
    fn refresh_on_empty_images_pane_refetches_inventory() {
        let (mut t, _) = at_images(vec![]);
        t.ok(
            CacheKey::RepoDetails(rid("ecr"), rp("app")),
            CacheValue::Details(Default::default()),
        );
        let mut keys = fetched(&t.act(Action::Refresh));
        keys.sort_by_key(|k| format!("{k:?}"));
        assert_eq!(
            keys,
            vec![
                inv("ecr", "app"),
                CacheKey::RepoDetails(rid("ecr"), rp("app"))
            ]
        );
    }

    #[test]
    fn refresh_scope_covers_cached_keys_below() {
        let (mut t, _) = at_images(vec![art('a', &["1"])]);
        t.ok(man("ecr", 'a'), image('c', &[1]));
        t.ok(cfg("ecr", 'a'), config("{}"));
        t.ok(refs("ecr", 'a'), CacheValue::Referrers(Some(vec![])));

        let mut keys = fetched(&t.act(Action::Refresh));
        keys.sort_by_key(|k| format!("{k:?}"));
        let mut want = vec![man("ecr", 'a'), cfg("ecr", 'a'), refs("ecr", 'a')];
        want.sort_by_key(|k| format!("{k:?}"));
        assert_eq!(keys, want, "images pane: content of the selected image");
        for k in want {
            t.ok(
                k.clone(),
                if k == refs("ecr", 'a') {
                    CacheValue::Referrers(None)
                } else if k == man("ecr", 'a') {
                    image('c', &[1])
                } else {
                    config("{}")
                },
            );
        }

        t.act(Action::PrevPane);
        t.act(Action::PrevPane);
        let keys = fetched(&t.act(Action::Refresh));
        assert_eq!(
            keys.len(),
            5,
            "repos + inventory + 3 content keys: {keys:?}"
        );
        assert!(keys.contains(&CacheKey::Repos(rid("ecr"))));
        assert!(keys.contains(&inv("ecr", "app")));
        assert!(!keys.contains(&CacheKey::LocalImages));
    }

    #[test]
    fn unauthorized_sets_bang_and_ok_clears() {
        let (mut t, _) = T::new(vec![ecr()]);
        t.load(
            CacheKey::Repos(rid("ecr")),
            Err(RegistryError::Unauthorized),
        );
        assert!(t.s.registries()[0].auth_failed);
        assert_eq!(msg(&t.s), Some("auth failed for ecr — check AWS creds"));

        t.ok(CacheKey::LocalImages, CacheValue::LocalImages(vec![]));
        assert!(msg(&t.s).is_some(), "another key's success keeps the error");

        assert_eq!(
            fetched(&t.act(Action::Refresh)),
            vec![CacheKey::Repos(rid("ecr"))]
        );
        t.ok(CacheKey::Repos(rid("ecr")), repos(&["app"]));
        assert!(!t.s.registries()[0].auth_failed);
        assert_eq!(msg(&t.s), None);
    }

    #[test]
    fn referrers_ok_does_not_clear_auth_failed() {
        let (mut t, _) = at_images(vec![art('a', &["1"])]);
        t.load(man("ecr", 'a'), Err(RegistryError::Unauthorized));
        assert!(t.s.registries()[0].auth_failed);
        t.ok(refs("ecr", 'a'), CacheValue::Referrers(None));
        assert!(t.s.registries()[0].auth_failed);
    }

    #[test]
    fn filter_and_sort_images() {
        let mut imgs = vec![
            art('a', &["0.1.0"]),
            art('b', &["0.2.0"]),
            art('c', &["latest"]),
        ];
        imgs[0].pushed_at = Some(datetime!(2024-01-01 0:00 UTC));
        imgs[1].pushed_at = Some(datetime!(2024-02-01 0:00 UTC));
        imgs[2].pushed_at = Some(datetime!(2024-03-01 0:00 UTC));
        imgs[0].size = Some(30);
        imgs[1].size = Some(10);
        imgs[2].size = Some(20);
        let (mut t, _) = at_images(imgs.clone());
        assert_eq!(t.s.image_sort(), (SortKey::Pushed, true));
        assert_eq!(t.tags(), vec!["latest", "0.2.0", "0.1.0"]);

        t.act(Action::FilterStart);
        assert!(t.s.filter_editing());
        t.act(Action::FilterChar('0'));
        t.act(Action::FilterChar('.'));
        t.act(Action::FilterApply);
        assert!(!t.s.filter_editing());
        assert_eq!(t.s.filter(Pane::Images), "0.");
        assert_eq!(t.tags(), vec!["0.2.0", "0.1.0"]);
        t.act(Action::FilterChar('9'));
        assert_eq!(
            t.s.filter(Pane::Images),
            "0.",
            "chars ignored outside filter mode"
        );

        t.act(Action::SortCycle);
        assert_eq!(t.s.image_sort(), (SortKey::Size, true));
        assert_eq!(t.tags(), vec!["0.1.0", "0.2.0"]);
        t.act(Action::SortCycle);
        assert_eq!(t.s.image_sort().0, SortKey::Pulled);
        t.act(Action::SortCycle);
        assert_eq!(t.s.image_sort(), (SortKey::Tag, true));
        assert_eq!(t.tags(), vec!["0.2.0", "0.1.0"]);
        t.act(Action::SortReverse);
        assert_eq!(t.tags(), vec!["0.1.0", "0.2.0"]);

        t.act(Action::FilterStart);
        t.act(Action::FilterChar('x'));
        t.act(Action::FilterCancel);
        assert_eq!(t.s.filter(Pane::Images), "");
        assert_eq!(t.tags().len(), 3);

        // Registry without pushed_at / last_pulled: tag desc, cycle skips hidden columns.
        let (mut t, _) = T::new(vec![view("oci", AuthCfg::Anonymous, false)]);
        t.ok(CacheKey::Repos(rid("oci")), repos(&["web", "app"]));
        assert_eq!(t.s.repo_sort(), (SortKey::Name, false));
        let names: Vec<_> =
            t.s.visible_repos()
                .iter()
                .map(|r| r.path.0.clone())
                .collect();
        assert_eq!(names, vec!["app", "web"]);
        t.act(Action::NextPane);
        t.ok(inv("oci", "app"), CacheValue::Inventory(imgs));
        t.act(Action::NextPane);
        assert_eq!(t.s.image_sort(), (SortKey::Tag, true));
        assert_eq!(t.tags(), vec!["latest", "0.2.0", "0.1.0"]);
        t.act(Action::SortCycle);
        assert_eq!(t.s.image_sort().0, SortKey::Size);
        t.act(Action::SortCycle);
        assert_eq!(t.s.image_sort().0, SortKey::Tag);

        // Repos: name ↔ count.
        t.act(Action::PrevPane);
        t.act(Action::SortCycle);
        assert_eq!(t.s.repo_sort().0, SortKey::Count);
        t.act(Action::SortCycle);
        assert_eq!(t.s.repo_sort().0, SortKey::Name);
    }

    #[test]
    fn copy_reference_and_raw() {
        let (mut t, _) = at_images(vec![art('a', &["1.0", "latest"]), art('b', &[])]);
        assert_eq!(
            t.act(Action::Copy),
            vec![Effect::Copy("h.example/app:1.0".into())]
        );
        t.act(Action::Down);
        assert_eq!(
            t.act(Action::Copy),
            vec![Effect::Copy(format!("h.example/app@{}", dg('b')))]
        );
        t.act(Action::Up);

        t.act(Action::Tab(Tab::Raw));
        t.ok(man("ecr", 'a'), image('c', &[1]));
        t.ok(
            cfg("ecr", 'a'),
            config(r#"{"config":{"Env":["API_TOKEN=s3cr3t","PATH=/bin"]}}"#),
        );
        let masked = match t.act(Action::Copy).as_slice() {
            [Effect::Copy(s)] => s.clone(),
            other => panic!("{other:?}"),
        };
        assert!(masked.contains("schemaVersion"));
        assert!(masked.contains("API_TOKEN=••••••"));
        assert!(!masked.contains("s3cr3t"));
        assert_eq!(
            Some(masked),
            t.s.raw_text(),
            "copies exactly what is displayed"
        );

        t.act(Action::ToggleReveal);
        assert!(t.s.reveal());
        match t.act(Action::Copy).as_slice() {
            [Effect::Copy(s)] => assert!(s.contains("API_TOKEN=s3cr3t")),
            other => panic!("{other:?}"),
        }
        t.act(Action::Down);
        assert!(!t.s.reveal(), "leaving the image re-masks");

        t.ev(Event::CopyResult(Err("no display".into())));
        assert_eq!(msg(&t.s), Some("clipboard unavailable: no display"));
    }

    #[test]
    fn index_platform_cycle_fetches_child() {
        let (mut t, _) = at_images(vec![art('9', &["multi"])]);
        let fx = t.ok(man("ecr", '9'), index(&['b', 'c']));
        assert_eq!(fetched(&fx), vec![man("ecr", 'b')]);
        assert_eq!(
            t.s.display_size(t.s.selected_image().unwrap()),
            None,
            "index → —"
        );
        assert_eq!(t.s.platform_count(), Some(2));

        let fx = t.ok(man("ecr", 'b'), image('d', &[5]));
        assert_eq!(fetched(&fx), vec![cfg("ecr", 'b')]);

        let fx = t.act(Action::PlatformNext);
        assert_eq!(t.s.platform(), 1);
        assert_eq!(fetched(&fx), vec![man("ecr", 'c')]);
        assert!(focused(&fx).unwrap().contains(&man("ecr", 'c')));

        let fx = t.act(Action::PlatformNext);
        assert_eq!(t.s.platform(), 0);
        assert!(
            fetched(&fx).is_empty(),
            "platform 0 already cached / loading"
        );
        t.act(Action::PlatformPrev);
        assert_eq!(t.s.platform(), 1);
    }

    #[test]
    fn timeout_failed_then_refresh_retries() {
        let (mut t, _) = T::new(vec![ecr()]);
        t.ok(CacheKey::Repos(rid("ecr")), repos(&["app", "web"]));
        t.act(Action::NextPane);
        t.load(
            inv("ecr", "app"),
            Err(RegistryError::Timeout("request".into())),
        );
        assert!(matches!(
            t.s.entry(&inv("ecr", "app")),
            Some(Entry::Failed { .. })
        ));
        assert_eq!(
            msg(&t.s),
            Some("timed out loading images of app from ecr — press r to retry")
        );
        t.act(Action::Down);
        assert!(
            fetched(&t.act(Action::Up)).is_empty(),
            "failed is not retried on focus"
        );

        assert_eq!(fetched(&t.act(Action::Refresh)), vec![inv("ecr", "app")]);
        t.ok(inv("ecr", "app"), CacheValue::Inventory(vec![]));
        assert_eq!(msg(&t.s), None);
    }

    #[test]
    fn clamp_scroll_action() {
        let (mut t, _) = T::new(vec![ecr()]);
        for _ in 0..10 {
            t.act(Action::ScrollDown);
        }
        t.act(Action::ClampScroll(3));
        assert_eq!(t.s.scroll(), 3);
        t.act(Action::ScrollUp);
        assert_eq!(t.s.scroll(), 2);
        t.act(Action::ClampScroll(20));
        assert_eq!(t.s.scroll(), 2);
    }

    #[test]
    fn progress_scroll_help_and_quit() {
        let (mut t, _) = T::new(vec![ecr()]);
        t.ev(Event::Progress {
            key: CacheKey::Repos(rid("ecr")),
            pages: 3,
        });
        assert!(matches!(
            t.s.entry(&CacheKey::Repos(rid("ecr"))),
            Some(Entry::Loading { pages: 3, .. })
        ));
        t.act(Action::ScrollDown);
        t.act(Action::ScrollDown);
        t.act(Action::ScrollUp);
        assert_eq!(t.s.scroll(), 1);
        t.act(Action::ToggleHelp);
        assert!(t.s.help_shown());
        assert_eq!(t.act(Action::Quit), vec![Effect::Quit]);
    }

    fn company() -> RegistryView {
        view("company", AuthCfg::Ecr, true)
    }

    fn logins(fx: &[Effect]) -> Vec<RegistryId> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Login(r) => Some(r.clone()),
                _ => None,
            })
            .collect()
    }

    fn login_done(r: &str, result: Result<Option<OffsetDateTime>, RegistryError>) -> Event {
        Event::LoginDone {
            registry: rid(r),
            result,
        }
    }

    #[test]
    fn login_emits_effect_and_marks_running() {
        let (mut t, _) = at_images_in(company(), vec![art('a', &["1"])]);
        assert_eq!(t.s.focus(), Pane::Images, "any pane: the selected registry");
        let fx = t.act(Action::Login);
        assert_eq!(logins(&fx), vec![rid("company")]);
        assert_eq!(t.s.registries()[0].login, Some(LoginState::Running));
        assert_eq!(msg(&t.s), Some("logging in to company…"));
    }

    #[test]
    fn login_uses_selected_registry() {
        let (mut t, _) = T::new(vec![company(), view("other", AuthCfg::Ecr, true)]);
        t.act(Action::Down);
        assert_eq!(logins(&t.act(Action::Login)), vec![rid("other")]);
        assert_eq!(t.s.registries()[0].login, None);
        assert_eq!(t.s.registries()[1].login, Some(LoginState::Running));
    }

    #[test]
    fn second_login_while_running_is_ignored() {
        let (mut t, _) = T::new(vec![company()]);
        t.act(Action::Login);
        assert!(logins(&t.act(Action::Login)).is_empty());
        assert_eq!(msg(&t.s), Some("login to company already running"));
        assert_eq!(t.s.registries()[0].login, Some(LoginState::Running));
    }

    #[test]
    fn login_done_ok_records_expiry() {
        let (mut t, _) = T::new(vec![company()]);
        t.act(Action::Login);
        let at = datetime!(2026-10-29 23:12 UTC);
        t.ev(login_done("company", Ok(Some(at))));
        assert_eq!(
            t.s.registries()[0].login,
            Some(LoginState::Done {
                expires_at: Some(at)
            })
        );
        assert_eq!(msg(&t.s), Some("logged in to company (expires 23:12 UTC)"));
    }

    #[test]
    fn login_done_ok_without_expiry() {
        let (mut t, _) = T::new(vec![company()]);
        t.act(Action::Login);
        t.ev(login_done("company", Ok(None)));
        assert_eq!(
            t.s.registries()[0].login,
            Some(LoginState::Done { expires_at: None })
        );
        assert_eq!(msg(&t.s), Some("logged in to company"));
    }

    #[test]
    fn login_done_err_clears_and_reports() {
        let (mut t, _) = T::new(vec![company()]);
        t.act(Action::Login);
        t.ev(login_done("company", Ok(None)));
        t.act(Action::Login);
        assert_eq!(t.s.registries()[0].login, Some(LoginState::Running));
        let err = RegistryError::Provider("docker login failed: x".into());
        t.ev(login_done("company", Err(err.clone())));
        let r = &t.s.registries()[0];
        assert_eq!(r.login, None, "a previous Done is cleared too");
        assert!(!r.auth_failed, "login does not touch auth_failed");
        assert_eq!(msg(&t.s), Some(user_message(&err, r, "login").as_str()));
    }

    #[test]
    fn login_done_timeout_and_network_hint_login_key() {
        let cases = [
            (
                RegistryError::Timeout("docker login".into()),
                "docker login to company timed out — press L to retry",
            ),
            (
                RegistryError::Timeout("credentials".into()),
                "docker login to company timed out — press L to retry",
            ),
            (
                RegistryError::Network("dns failure".into()),
                "network error: dns failure — press L to retry",
            ),
        ];
        for (err, want) in cases {
            let (mut t, _) = T::new(vec![company()]);
            t.act(Action::Login);
            t.ev(login_done("company", Err(err)));
            assert_eq!(t.s.registries()[0].login, None);
            assert_eq!(msg(&t.s), Some(want));
        }
    }

    #[test]
    fn login_not_offered_for_docker_config() {
        let mut v = view("hub", AuthCfg::DockerConfig, false);
        v.can_login = false;
        let (mut t, _) = T::new(vec![v]);
        assert!(logins(&t.act(Action::Login)).is_empty());
        assert_eq!(t.s.registries()[0].login, None);
        assert_eq!(
            msg(&t.s),
            Some("log in to h.example outside lcquay: `docker login h.example`")
        );
    }

    #[test]
    fn login_not_offered_for_anonymous() {
        let mut v = view("pub", AuthCfg::Anonymous, false);
        v.can_login = false;
        let (mut t, _) = T::new(vec![v]);
        assert!(logins(&t.act(Action::Login)).is_empty());
        assert_eq!(
            msg(&t.s),
            Some("pub uses anonymous access; nothing to log in")
        );
    }

    #[test]
    fn login_done_for_unknown_registry_ignored() {
        let (mut t, _) = T::new(vec![company()]);
        t.act(Action::Login);
        let fx = t.ev(login_done("nope", Ok(None)));
        assert!(logins(&fx).is_empty());
        assert_eq!(t.s.registries()[0].login, Some(LoginState::Running));
        assert_eq!(msg(&t.s), Some("logging in to company…"));
    }

    fn msg(s: &State) -> Option<&str> {
        s.status().map(|(_, m)| m)
    }

    fn kind(s: &State) -> Option<StatusKind> {
        s.status().map(|(k, _)| k)
    }

    #[test]
    fn status_kind_set() {
        let (mut t, _) = T::new(vec![company()]);
        t.load(
            CacheKey::Repos(rid("company")),
            Err(RegistryError::Unauthorized),
        );
        assert_eq!(kind(&t.s), Some(StatusKind::Error));
        assert!(t.s.failure(&CacheKey::Repos(rid("company"))).is_some());
        t.act(Action::Login);
        assert_eq!(kind(&t.s), Some(StatusKind::Info), "logging in to …");
        t.ev(login_done("company", Ok(None)));
        assert_eq!(kind(&t.s), Some(StatusKind::Ok));
        t.act(Action::Login);
        t.ev(login_done(
            "company",
            Err(RegistryError::Timeout("x".into())),
        ));
        assert_eq!(kind(&t.s), Some(StatusKind::Error), "login failed");
        t.ev(Event::CopyResult(Ok(())));
        assert_eq!(kind(&t.s), Some(StatusKind::Ok));
        t.ev(Event::CopyResult(Err("no display".into())));
        assert_eq!(kind(&t.s), Some(StatusKind::Error));
    }

    #[test]
    fn edit_config_blocked_while_push_runs() {
        let (mut t, _) = at_images(vec![art('a', &["1.0.0"])]);
        assert_eq!(t.act(Action::EditConfig), vec![Effect::EditConfig]);
        t.s.start_push(rid("ecr"), rp("app"), "img".into(), "1.0.1".into())
            .unwrap();
        assert_eq!(t.act(Action::EditConfig), vec![]);
        assert_eq!(kind(&t.s), Some(StatusKind::Error));
        assert_eq!(msg(&t.s), Some(crate::app::write::BUSY));
    }

    #[test]
    fn reload_refused_while_pull_runs() {
        use crate::app::transfer::TransferOutcome;
        use crate::app::write::{OpId, RELOAD_BUSY};
        let (mut t, _) = at_images(vec![art('a', &["1.0.0"])]);
        t.ok(CacheKey::LocalImages, CacheValue::LocalImages(vec![]));
        t.act(Action::PullStart);
        assert_eq!(t.act(Action::EditConfig), vec![], "details stage");
        t.act(Action::Enter);
        for hide in [false, true] {
            if hide {
                t.act(Action::Enter);
            }
            assert_eq!(t.act(Action::EditConfig), vec![], "hidden: {hide}");
            assert_eq!(kind(&t.s), Some(StatusKind::Error));
            assert_eq!(msg(&t.s), Some(RELOAD_BUSY));
        }
        t.ev(Event::PullDone {
            op: OpId(1),
            outcome: TransferOutcome::Done,
        });
        assert_eq!(t.act(Action::EditConfig), vec![Effect::EditConfig]);
    }

    fn pinned_reg(names: &[&str]) -> RegistryView {
        RegistryView {
            pinned: names
                .iter()
                .map(|n| RepoSummary {
                    path: rp(n),
                    created_at: None,
                })
                .collect(),
            ..ecr()
        }
    }

    fn visible(t: &T) -> Vec<String> {
        t.s.visible_repos()
            .iter()
            .map(|r| r.path.0.clone())
            .collect()
    }

    #[test]
    fn pinned_first_in_config_order() {
        let (mut t, _) = T::new(vec![pinned_reg(&["z", "b"])]);
        t.ok(CacheKey::Repos(rid("ecr")), repos(&["a", "b", "c", "z"]));
        assert_eq!(visible(&t), ["z", "b", "a", "c"]);
        assert!(t.s.is_pinned(&rp("z")) && !t.s.is_pinned(&rp("a")));
        t.act(Action::NextPane);
        t.act(Action::SortReverse);
        assert_eq!(visible(&t), ["z", "b", "c", "a"]);
        t.act(Action::FilterStart);
        t.act(Action::FilterChar('b'));
        assert_eq!(visible(&t), ["b"]);
    }

    #[test]
    fn pinned_shown_before_list_loads() {
        let (t, _) = T::new(vec![pinned_reg(&["z", "b"])]);
        assert_eq!(visible(&t), ["z", "b"]);
    }

    #[test]
    fn pinned_inventories_prefetched_in_background() {
        let (mut t, fx) = T::new(vec![pinned_reg(&["z", "b"])]);
        let keys = fetched(&fx);
        assert!(keys.contains(&inv("ecr", "z")) && keys.contains(&inv("ecr", "b")));
        let focus = focused(&fx).unwrap();
        assert!(!focus.contains(&inv("ecr", "z")) && !focus.contains(&inv("ecr", "b")));
        let fx = t.act(Action::Down);
        assert!(fetched(&fx).is_empty(), "{fx:?}");
    }

    #[test]
    fn pinned_missing_not_found() {
        let (mut t, _) = T::new(vec![pinned_reg(&["z", "b"])]);
        t.load(inv("ecr", "z"), Err(RegistryError::NotFound));
        assert!(t.s.repo_not_found(&rp("z")) && !t.s.repo_not_found(&rp("b")));
        assert_eq!(visible(&t), ["z", "b"]);
        // The row says `not found`; no error status line for a background prefetch.
        assert_eq!(t.s.status(), None);
        // Other errors still report.
        t.load(inv("ecr", "b"), Err(RegistryError::Network("down".into())));
        assert_eq!(t.s.status().map(|(k, _)| k), Some(StatusKind::Error));
    }

    const TREE: &[&str] = &["api", "eng/api/fo", "eng/api/bo", "eng/api/x/y", "web"];

    /// ecr with `names` loaded, focus on the Repos pane.
    fn tree_at(reg: RegistryView, names: &[&str]) -> T {
        let (mut t, _) = T::new(vec![reg]);
        t.ok(CacheKey::Repos(rid("ecr")), repos(names));
        t.act(Action::NextPane);
        t
    }

    /// Rows as `<depth><mark><label or path>(count)`; marks: ▸/▾ group, ★ pinned.
    fn tree(t: &T) -> Vec<String> {
        t.s.repo_rows()
            .iter()
            .map(|r| match r {
                RepoRow::Group {
                    label,
                    count,
                    depth,
                    expanded,
                    ..
                } => format!(
                    "{depth}{}{label}({count})",
                    if *expanded { "▾" } else { "▸" }
                ),
                RepoRow::Repo {
                    repo,
                    depth,
                    pinned,
                } => format!("{depth}{}{}", if *pinned { "★" } else { " " }, repo.path),
            })
            .collect()
    }

    fn sel_path(t: &T) -> Option<String> {
        t.s.selected_repo().map(|r| r.path.0.clone())
    }

    #[test]
    fn tree_groups_and_compresses() {
        let mut t = tree_at(ecr(), TREE);
        assert_eq!(tree(&t), ["0▸eng/api(3)", "0 api", "0 web"]);
        assert!(
            matches!(&t.s.repo_rows()[0], RepoRow::Group { prefix, .. } if prefix == "eng/api")
        );
        t.act(Action::Enter);
        assert_eq!(
            tree(&t),
            [
                "0▾eng/api(3)",
                "1▸x(1)",
                "1 eng/api/bo",
                "1 eng/api/fo",
                "0 api",
                "0 web"
            ]
        );
        // Groups by name, reversed with O; repos in the active sort.
        t.act(Action::SortReverse);
        t.act(Action::Down);
        t.act(Action::Enter);
        assert_eq!(
            tree(&t),
            [
                "0▾eng/api(3)",
                "1▾x(1)",
                "2 eng/api/x/y",
                "1 eng/api/fo",
                "1 eng/api/bo",
                "0 web",
                "0 api"
            ]
        );
        // No tree when nothing nests.
        let t = tree_at(ecr(), &["b", "a"]);
        assert_eq!(tree(&t), ["0 a", "0 b"]);
    }

    #[test]
    fn tree_common_prefix_hidden() {
        let t = tree_at(ecr(), &["s4/a", "s4/b/c", "s4/b/d"]);
        assert_eq!(tree(&t), ["0▸b(2)", "0 s4/a"]);
        assert!(matches!(&t.s.repo_rows()[0], RepoRow::Group { prefix, .. } if prefix == "s4/b"));
        // Nothing nests below the common prefix: flat.
        let t = tree_at(ecr(), &["s4/a", "s4/b"]);
        assert_eq!(tree(&t), ["0 s4/a", "0 s4/b"]);
    }

    #[test]
    fn tree_keys() {
        let mut t = tree_at(ecr(), TREE);
        assert_eq!(t.s.sel_group.as_deref(), Some("eng/api"));
        assert_eq!(t.s.selected_repo(), None);
        t.act(Action::Enter);
        assert_eq!(tree(&t)[0], "0▾eng/api(3)");
        t.act(Action::Enter);
        assert_eq!(tree(&t)[0], "0▸eng/api(3)");
        assert_eq!(t.s.focus(), Pane::Repos);
        // → expands a collapsed group, then switches pane.
        t.act(Action::Right);
        assert_eq!(
            (tree(&t)[0].as_str(), t.s.focus()),
            ("0▾eng/api(3)", Pane::Repos)
        );
        t.act(Action::Right);
        assert_eq!(t.s.focus(), Pane::Images);
        t.act(Action::PrevPane);
        // ← collapses an expanded group, then switches pane.
        t.act(Action::Left);
        assert_eq!(
            (tree(&t)[0].as_str(), t.s.focus()),
            ("0▸eng/api(3)", Pane::Repos)
        );
        t.act(Action::Left);
        assert_eq!(t.s.focus(), Pane::Registries);
        // Tab always switches.
        t.act(Action::NextPane);
        t.act(Action::NextPane);
        assert_eq!(t.s.focus(), Pane::Images);
        t.act(Action::PrevPane);
        assert_eq!(t.s.sel_group.as_deref(), Some("eng/api"));
        // Repo rows: ←/→ switch panes.
        t.act(Action::Down);
        assert_eq!(sel_path(&t).as_deref(), Some("api"));
        assert_eq!(t.s.sel_group, None);
        t.act(Action::Left);
        assert_eq!(t.s.focus(), Pane::Registries);
        t.act(Action::Right);
        t.act(Action::Right);
        assert_eq!(t.s.focus(), Pane::Images);
        assert_eq!(tree(&t)[0], "0▸eng/api(3)");
    }

    #[test]
    fn tree_filter_keeps_selection() {
        let mut t = tree_at(ecr(), TREE);
        t.act(Action::Right);
        for _ in 0..3 {
            t.act(Action::Down);
        }
        assert_eq!(sel_path(&t).as_deref(), Some("eng/api/fo"));
        let expanded = tree(&t);
        t.act(Action::FilterStart);
        t.act(Action::FilterChar('f'));
        t.act(Action::FilterChar('o'));
        assert_eq!(tree(&t), ["0 eng/api/fo"]);
        assert_eq!(sel_path(&t).as_deref(), Some("eng/api/fo"));
        // No groups while filtering: ← switches panes, nothing collapses.
        t.act(Action::FilterApply);
        t.act(Action::Left);
        assert_eq!(t.s.focus(), Pane::Registries);
        t.act(Action::NextPane);
        t.act(Action::FilterStart);
        t.act(Action::FilterCancel);
        assert_eq!(tree(&t), expanded);
        assert_eq!(sel_path(&t).as_deref(), Some("eng/api/fo"));
        // Collapsed beforehand: clearing the filter expands the selected repo's group again.
        for _ in 0..3 {
            t.act(Action::Up);
        }
        t.act(Action::Left);
        assert_eq!(tree(&t)[0], "0▸eng/api(3)");
        t.act(Action::FilterStart);
        t.act(Action::FilterChar('f'));
        t.act(Action::FilterChar('o'));
        assert_eq!(sel_path(&t).as_deref(), Some("eng/api/fo"));
        t.act(Action::FilterCancel);
        assert_eq!(tree(&t), expanded);
        assert_eq!(sel_path(&t).as_deref(), Some("eng/api/fo"));
        // A selected group survives a reload; `r` on it refreshes nothing below it.
        for _ in 0..3 {
            t.act(Action::Up);
        }
        assert_eq!(t.s.sel_group.as_deref(), Some("eng/api"));
        let fx = t.act(Action::Refresh);
        assert!(
            !fetched(&fx)
                .iter()
                .any(|k| matches!(k, CacheKey::Inventory(..))),
            "{fx:?}"
        );
        t.act(Action::PrevPane);
        t.act(Action::Refresh);
        t.ok(CacheKey::Repos(rid("ecr")), repos(TREE));
        assert_eq!(t.s.sel_group.as_deref(), Some("eng/api"));
        assert_eq!(t.s.sel_repo, None);
    }

    #[test]
    fn tree_pinned_flat_on_top() {
        let t = tree_at(pinned_reg(&["eng/api/fo"]), TREE);
        assert_eq!(tree(&t), ["0★eng/api/fo", "0▸eng/api(2)", "0 api", "0 web"]);
    }

    fn two() -> T {
        at_images(vec![art('a', &["1"]), art('b', &["2"])]).0
    }

    /// `=` on `a`, Down, `=` on `b`.
    fn compare_ab(t: &mut T) {
        t.act(Action::CompareMark);
        t.act(Action::Down);
        t.act(Action::CompareMark);
    }

    #[test]
    fn compare_flow() {
        let mut t = two();
        t.act(Action::CompareMark);
        assert_eq!(
            t.s.compare_base(),
            Some(&Side {
                digest: dg('a'),
                platform: 0
            })
        );
        t.act(Action::CompareMark);
        assert_eq!(t.s.compare_base(), None, "= on the base clears it");

        compare_ab(&mut t);
        assert!(t.s.compare().is_some());
        assert_eq!(t.s.key_mode(), crate::app::state::KeyMode::Scroll);
        assert_eq!(t.s.compare_keys(), [man("ecr", 'a'), man("ecr", 'b')]);
        t.ok(man("ecr", 'a'), image('c', &[100]));
        t.ok(man("ecr", 'b'), image('d', &[100]));
        assert_eq!(
            t.s.compare_keys(),
            [
                man("ecr", 'a'),
                cfg("ecr", 'a'),
                man("ecr", 'b'),
                cfg("ecr", 'b')
            ]
        );
        assert_eq!(t.s.compare_text(), None, "loading until every key is Ready");
        assert_eq!(t.act(Action::Copy), vec![]);
        t.ok(cfg("ecr", 'a'), config("{}"));
        assert_eq!(t.s.compare_text(), None);
        t.ok(cfg("ecr", 'b'), config("{}"));
        let text = t.s.compare_text().unwrap();
        assert_eq!(text[0], "Summary (A | B)");
        assert_eq!(t.act(Action::Copy), vec![Effect::Copy(text.join("\n"))]);

        t.act(Action::Back);
        assert!(t.s.compare().is_none());
        assert_eq!(t.s.compare_base(), None, "Esc closes and clears the base");
    }

    #[test]
    fn compare_index_uses_each_sides_platform() {
        let mut t = two();
        t.ok(man("ecr", 'a'), index(&['c', 'd']));
        t.act(Action::PlatformNext);
        t.ok(man("ecr", 'd'), image('5', &[1]));
        t.act(Action::CompareMark);
        assert_eq!(t.s.compare_base().unwrap().platform, 1);
        t.act(Action::Down);
        t.ok(man("ecr", 'b'), index(&['e', 'f']));
        t.ok(man("ecr", 'e'), image('6', &[1]));
        t.act(Action::CompareMark);
        let keys = t.s.compare_keys();
        assert!(
            keys.contains(&cfg("ecr", 'd')) && keys.contains(&cfg("ecr", 'e')),
            "{keys:?}"
        );
        assert!(
            !keys.contains(&cfg("ecr", 'c')) && !keys.contains(&cfg("ecr", 'f')),
            "{keys:?}"
        );
    }

    #[test]
    fn compare_other_repo_resets() {
        let mut t = two();
        t.act(Action::CompareMark);
        t.act(Action::PrevPane);
        t.act(Action::Down);
        assert_eq!(t.s.compare_base(), None);
    }

    #[test]
    fn esc_order_anchor_base_marks() {
        let mut t = two();
        t.act(Action::Mark);
        t.act(Action::CompareMark);
        t.act(Action::Down);
        t.act(Action::RangeMark);
        assert!(
            t.s.range_anchor().is_some() && t.s.compare_base().is_some() && t.s.marks().is_some()
        );
        t.act(Action::ClearMarks);
        assert!(
            t.s.range_anchor().is_none() && t.s.compare_base().is_some() && t.s.marks().is_some()
        );
        t.act(Action::ClearMarks);
        assert!(t.s.compare_base().is_none() && t.s.marks().is_some());
        t.act(Action::ClearMarks);
        assert!(t.s.marks().is_none());
    }
}

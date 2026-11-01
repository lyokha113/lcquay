//! Rendering: reads `State`, never mutates it, does no I/O.

mod detail;
mod lists;
mod modals;
pub mod theme;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span, Text},
    widgets::{Block, Borders, ListState, Paragraph, TableState},
};
use time::{OffsetDateTime, macros::format_description};

use crate::app::cache::Entry;
use crate::app::delete::{DelStage, DeleteModal};
use crate::app::messages::describe;
use crate::app::push::Stage;
use crate::app::state::{Action, CacheKey, KeyMode, Pane, State, StatusKind, Tab};
use crate::domain::{ArtifactStatus, Inventory, fmt_size};

/// What the UI remembers between frames: list scroll offsets (ratatui keeps them in the widget
/// states) and the detail scroll range. Only `ui` writes it; `State` never sees it.
#[derive(Default)]
pub struct ViewState {
    registries: ListState,
    repos: ListState,
    images: TableState,
    /// Largest useful detail scroll, as of the last frame.
    pub detail_max_scroll: u16,
    /// Largest useful scroll of the shown delete modal (0 when none).
    pub delete_max_scroll: u16,
    /// Largest useful scroll of the requests overlay (0 when closed).
    pub requests_max_scroll: u16,
    /// Largest useful scroll of the compare modal (0 when closed).
    pub compare_max_scroll: u16,
    /// Largest useful scroll of the help modal (0 when closed).
    pub help_max_scroll: u16,
}

impl ViewState {
    /// A scroll key that would go past the end of the detail pane or of the shown delete modal;
    /// it is dropped, not stored.
    pub fn past_end(&self, state: &State, a: Action) -> bool {
        if let (Action::Down, Some(m)) = (a, state.compare()) {
            return m.scroll >= self.compare_max_scroll;
        }
        if let (Action::Up, Some(r)) = (a, state.requests()) {
            return r.scroll >= self.requests_max_scroll;
        }
        if a == Action::Down && state.key_mode() == KeyMode::Help {
            return state.help_scroll() >= self.help_max_scroll;
        }
        match (a, state.delete_modal().filter(|m| !m.hidden)) {
            (Action::Down, Some(m)) => m.scroll >= self.delete_max_scroll,
            (Action::ScrollDown, _) => state.scroll() >= self.detail_max_scroll,
            _ => false,
        }
    }
}

/// `now` only drives the login time left.
pub fn render(frame: &mut Frame, state: &State, now: OffsetDateTime, view: &mut ViewState) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    theme::set(state.theme);
    frame.render_widget(Block::new().style(theme::base()), area);
    let [top, det, hints, status] = Layout::vertical([
        Constraint::Percentage(40),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let [reg, repo, img] = Layout::horizontal([
        Constraint::Length(28),
        Constraint::Length(24),
        Constraint::Min(0),
    ])
    .areas(top);
    lists::registries(frame, reg, state, now, &mut view.registries);
    lists::repos(frame, repo, state, &mut view.repos);
    lists::images(frame, img, state, &mut view.images);
    view.detail_max_scroll = detail::render(frame, det, state, now);
    frame.render_widget(
        Paragraph::new(hint_line(state, hints.width)).style(theme::bar()),
        hints,
    );
    let line = push_status(state)
        .or_else(|| pull_status(state, now))
        .or_else(|| delete_status(state))
        .or_else(|| state.status().map(|(k, m)| status_line(k, m)));
    frame.render_widget(
        Paragraph::new(line.unwrap_or_default()).style(theme::bar()),
        status,
    );
    view.delete_max_scroll = 0;
    view.requests_max_scroll = 0;
    view.compare_max_scroll = 0;
    view.help_max_scroll = 0;
    // Above the hint bar and status line, which carry the modal's keys.
    let above = Rect {
        height: area.height.saturating_sub(2),
        ..area
    };
    // The order `update` routes keys in and `State::key_mode` picks.
    if let Some(id) = state.remove_confirm() {
        modals::confirm(frame, above, &format!("remove registry {id}? y/n"));
    } else if state.reg_form().is_some() {
        modals::registry_form(frame, above, state);
    } else if state.compare().is_some() {
        view.compare_max_scroll = modals::compare(frame, above, state);
    } else if state.requests_open() {
        view.requests_max_scroll = modals::requests(frame, above, state);
    } else if state.delete_modal().is_some_and(|m| !m.hidden) {
        view.delete_max_scroll = modals::delete(frame, above, state);
    } else if state.pull_modal().is_some_and(|m| !m.hidden) {
        modals::pull(frame, above, state, now);
    } else if state.copy_wizard().is_some_and(|w| !w.hidden) {
        modals::copy(frame, above, state, now);
    } else if state.export_modal().is_some_and(|m| !m.hidden) {
        modals::export(frame, above, state, now);
    } else if state.wizard().is_some() {
        modals::wizard(frame, area, state);
    } else if state.glob().is_some() {
        modals::glob(frame, area, state);
    } else if let Some(row) = state.theme_picker() {
        modals::theme_picker(frame, area, row);
    } else if state.help_shown() {
        view.help_max_scroll = modals::help(frame, above, state.help_scroll());
    }
}

/// A running push with its wizard hidden: live progress in place of an info message.
fn push_status(state: &State) -> Option<Line<'static>> {
    let op = state.write().filter(|w| w.outcome.is_none())?;
    let quiet = state.status().is_none_or(|(k, _)| k == StatusKind::Info);
    (state.wizard().is_none() && quiet).then(|| {
        let m = format!(
            "pushing {} · layers done {}",
            op.target, op.progress.layers_done
        );
        status_line(StatusKind::Info, &m)
    })
}

/// A hidden running pull: live progress in place of an info message.
fn pull_status(state: &State, now: OffsetDateTime) -> Option<Line<'static>> {
    let m = state.pull_status(now)?;
    let quiet = state.status().is_none_or(|(k, _)| k == StatusKind::Info);
    quiet.then(|| status_line(StatusKind::Info, &m))
}

/// A hidden delete: it shows over info messages while running, and always while it holds an
/// unread report; the message keeps the kind the report status had.
fn delete_status(state: &State) -> Option<Line<'static>> {
    let msg = state.delete_status()?;
    let cur = state.status();
    let report = matches!(
        state.delete_modal().map(|m| &m.stage),
        Some(DelStage::Report { .. })
    );
    if !report && cur.is_some_and(|(k, _)| k != StatusKind::Info) {
        return None;
    }
    let kind = cur
        .filter(|(_, m)| *m == msg)
        .map_or(StatusKind::Info, |(k, _)| k);
    Some(status_line(kind, &msg))
}

fn status_line(kind: StatusKind, msg: &str) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    match kind {
        StatusKind::Info => {}
        StatusKind::Ok => spans.push(Span::styled("✓ ", theme::ok())),
        StatusKind::Error => spans.push(Span::styled("! ", theme::bad())),
    }
    spans.push(Span::raw(msg.to_owned()));
    Line::from(spans)
}

/// `title` is the pane name (bold blue when focused); `fetched` (the fetched time) and `meta`
/// (filter, …) follow it, each starting with its own ` · `; `meta` is muted.
fn pane_block(title: &str, fetched: &str, meta: &str, focused: bool) -> Block<'static> {
    let (border, title_style) = if focused {
        (theme::border_focus(), theme::title())
    } else {
        (theme::border(), theme::title_idle())
    };
    Block::new()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(title.to_owned(), title_style),
            Span::styled(fetched.to_owned(), theme::time()),
            Span::styled(meta.to_owned(), theme::muted()),
            Span::raw(" "),
        ]))
}

/// Pane text with no value to show (`(empty)`, `loading…`), muted.
fn muted_text(s: &str) -> Text<'static> {
    Text::from(Span::styled(s.to_owned(), theme::muted()))
}

/// What a `Failed` entry shows in its own block: the reason, and `hint` on how to retry unless
/// the message already says so.
fn failure_text(state: &State, key: &CacheKey, hint: &'static str) -> Option<Text<'static>> {
    let m = state.failure(key)?;
    let mut lines = vec![Line::styled(
        format!("Couldn't load {}: {m}", describe(key)),
        theme::bad(),
    )];
    if !m.contains("press r") {
        lines.push(Line::styled(hint, theme::muted()));
    }
    Some(Text::from(lines))
}

/// Keys valid now; `? help  q quit` stay visible when the rest is cut to `width`, and an item is
/// either shown whole or dropped.
fn hint_line(state: &State, width: u16) -> Line<'static> {
    if state.key_mode() == KeyMode::Confirm {
        return hints_only(&[("y", "yes"), ("n/Esc", "no")]);
    }
    if state.reg_form().is_some() {
        return hints_only(&[
            ("Tab/↑↓", "field"),
            ("←→", "type/auth"),
            ("Ctrl-T", "test"),
            ("Ctrl-S", "save"),
            ("Esc", "cancel"),
        ]);
    }
    if state.compare().is_some() {
        return hints_only(&[("↑↓", "scroll"), ("y", "copy"), ("Esc", "close")]);
    }
    if let Some(r) = state.requests() {
        return hints_only(if r.filter_editing {
            &[("type", "to filter"), ("Enter", "apply"), ("Esc", "clear")]
        } else {
            &[("↑↓", "scroll"), ("/", "filter"), ("Esc", "close")]
        });
    }
    if let Some(w) = state.wizard() {
        return hints_only(wizard_keys(state, w.stage));
    }
    if let Some(m) = state.delete_modal().filter(|m| !m.hidden) {
        return hints_only(delete_keys(m));
    }
    if let Some(m) = state.pull_modal().filter(|m| !m.hidden) {
        return hints_only(modals::pull_keys(m));
    }
    if let Some(w) = state.copy_wizard().filter(|w| !w.hidden) {
        return hints_only(modals::copy_keys(state, w));
    }
    if let Some(m) = state.export_modal().filter(|m| !m.hidden) {
        return hints_only(modals::export_keys(m));
    }
    if state.glob().is_some() {
        return hints_only(&[
            ("type", "a pattern"),
            ("Enter", "add to marks"),
            ("Esc", "cancel"),
        ]);
    }
    if state.theme_picker().is_some() {
        return hints_only(&[("↑↓", "preview"), ("Enter", "save"), ("Esc", "cancel")]);
    }
    if state.key_mode() == KeyMode::Help {
        return hints_only(&[("↑↓", "scroll"), ("Esc/?", "close"), ("Ctrl-C", "quit")]);
    }
    if state.filter_editing() {
        return Line::from(vec![
            Span::styled(" type to filter  ", theme::muted()),
            Span::styled("Enter", theme::key()),
            Span::styled(" apply  ", theme::muted()),
            Span::styled("Esc", theme::key()),
            Span::styled(" cancel", theme::muted()),
        ]);
    }
    let mut keys = vec![("↑↓/jk", "move"), ("Tab/←→/hl", "pane")];
    // A shown pull modal returned above, so this one is hidden.
    if state.pull_modal().is_some() {
        keys.push(("P", "show pull"));
    }
    // Likewise a copy wizard here is hidden.
    if state.copy_wizard().is_some() {
        keys.push(("C", "show copy"));
    }
    // And an export modal.
    if state.export_modal().is_some() {
        keys.push(("X", "show export"));
    }
    let focus = state.focus();
    // Same rule as `p` itself: local images known (a refetch in flight keeps the old list).
    let local_known = state.value(&CacheKey::LocalImages).is_some();
    // `C`, `P`, `X` and `=` act on an Active image.
    let active = state
        .selected_image()
        .is_some_and(|a| a.status == ArtifactStatus::Active);
    if focus != Pane::Images {
        keys.push(("Enter", "open"));
    }
    if focus == Pane::Registries && state.selected_registry().is_some_and(|r| r.can_login) {
        keys.push(("L", "login"));
    }
    if focus == Pane::Images {
        keys.push(("1-5", "tabs"));
        if state.platform_count().is_some() {
            keys.push(("[ ]", "platform"));
        }
        if matches!(state.tab(), Tab::Config | Tab::History | Tab::Raw) {
            keys.push(("m", "reveal"));
        }
        keys.push(("y", "copy"));
        // Mid-flow keys: shown before the keys a narrow bar drops first.
        if state.can_delete() {
            let marked = state.marked_count() > 0;
            if marked {
                keys.push(("d", "delete"));
            }
            if state.range_anchor().is_some() {
                keys.extend([("v", "mark range"), ("Esc", "cancel")]);
            } else if marked {
                keys.push(("Esc", "clear"));
            }
        }
        if active && state.copy_wizard().is_none() {
            keys.push(("C", "copy"));
        }
        if active && local_known && state.pull_modal().is_none() {
            keys.push(("P", "pull"));
        }
    }
    // Images: refresh and push before filter and sort, so they stay next to `C`/`P` at 120.
    let browse = [("/", "filter"), ("o/O", "sort")];
    if focus == Pane::Repos {
        keys.extend(browse);
    }
    keys.push(("r", "refresh"));
    if local_known {
        keys.push(("p", "push"));
    }
    if focus == Pane::Registries {
        keys.push(("c/n/x", "edit/new/remove"));
    }
    if focus == Pane::Images {
        keys.extend(browse);
        if active && state.export_modal().is_none() {
            keys.push(("X", "export"));
        }
        if active {
            keys.push(("=", "compare"));
        }
        keys.push(("Ctrl-d/u", "scroll"));
    }
    if state.can_delete() {
        keys.push(("space", "mark"));
        if state
            .selected_registry()
            .is_some_and(|r| r.caps.inventory == Inventory::Complete)
        {
            keys.push(("u", "untagged"));
        }
        keys.push(("a", "glob"));
        if state.range_anchor().is_none() {
            keys.push(("v", "range"));
        }
    }
    keys.push(("t", "theme"));
    keys.push(("!", "requests"));
    // Always shown: a narrow bar drops the keys above first.
    const TAIL: [(&str, &str); 3] = [("e", "config"), ("?", "help"), ("q", "quit")];
    let len = |(k, l): &(&str, &str)| k.chars().count() + 1 + l.chars().count();
    let tail_len = TAIL.iter().map(len).sum::<usize>() + 2 * (TAIL.len() - 1);
    let room = (width as usize).saturating_sub(tail_len + 2);
    let mut spans = vec![];
    let mut used = 0;
    for k in keys {
        let sep = if used == 0 { 1 } else { 2 };
        // Skip a key that does not fit; a shorter later one may.
        if used + sep + len(&k) > room {
            continue;
        }
        spans.push(Span::raw(" ".repeat(sep)));
        spans.extend(key_hint(k));
        used += sep + len(&k);
    }
    spans.push(Span::raw(
        " ".repeat((width as usize).saturating_sub(used + tail_len)),
    ));
    for (i, k) in TAIL.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.extend(key_hint(k));
    }
    Line::from(spans)
}

/// The wizard's keys for `stage`.
fn wizard_keys(state: &State, stage: Stage) -> &'static [(&'static str, &'static str)] {
    match stage {
        Stage::PickImage => &[
            ("type", "to filter"),
            ("↑↓", "move"),
            ("Enter", "select"),
            ("Esc", "cancel"),
        ],
        Stage::PickRepo => &[("↑↓", "move"), ("Enter", "select"), ("Esc", "back")],
        Stage::Tag => &[("type", "a tag"), ("Enter", "next"), ("Esc", "back")],
        Stage::Confirm => &[("Enter", "push"), ("Esc", "back")],
        Stage::Progress if state.write_running() => &[("Enter", "hide"), ("Esc", "cancel")],
        Stage::Progress => &[("Enter", "close")],
    }
}

/// The delete modal's keys for its stage.
fn delete_keys(m: &DeleteModal) -> &'static [(&'static str, &'static str)] {
    match &m.stage {
        DelStage::Snapshot | DelStage::Revalidate => &[("Esc", "cancel")],
        DelStage::Blocked(_) => &[("Esc", "close")],
        DelStage::Preview => &[
            ("w", "whole-manifest"),
            ("↑↓", "scroll"),
            ("Enter", "confirm"),
            ("Esc", "cancel"),
        ],
        DelStage::Confirm if m.typed == m.repo.0 => &[("Enter", "delete"), ("Esc", "back")],
        DelStage::Confirm => &[
            ("type", "the repo name"),
            ("Enter", "delete"),
            ("Esc", "back"),
        ],
        DelStage::Running => &[("Enter/Esc", "hide")],
        DelStage::Report { .. } => &[("Enter", "close")],
    }
}

/// A hint bar of just `keys`.
fn hints_only(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![];
    for (i, k) in keys.iter().enumerate() {
        spans.push(Span::raw(if i == 0 { " " } else { "  " }));
        spans.extend(key_hint(*k));
    }
    Line::from(spans)
}

/// `key label`: the key bold, the label muted.
fn key_hint((key, label): (&str, &str)) -> [Span<'static>; 2] {
    [
        Span::styled(key.to_owned(), theme::key()),
        Span::styled(format!(" {label}"), theme::muted()),
    ]
}

// ---- shared helpers ----

/// `YYYY-MM-DD HH:MM`.
fn fmt_time(t: OffsetDateTime) -> String {
    t.to_offset(time::UtcOffset::UTC)
        .format(format_description!("[year]-[month]-[day] [hour]:[minute]"))
        .unwrap_or_default()
}

/// `fmt_time` with its zone, for places that have no column header to say it.
fn fmt_utc(t: OffsetDateTime) -> String {
    format!("{} UTC", fmt_time(t))
}

fn fmt_hm(t: OffsetDateTime) -> String {
    t.to_offset(time::UtcOffset::UTC)
        .format(format_description!("[hour]:[minute]"))
        .unwrap_or_default()
}

/// `HH:MM:SS.mmm` in UTC.
fn fmt_hmsms(t: OffsetDateTime) -> String {
    t.to_offset(time::UtcOffset::UTC)
        .format(format_description!(
            "[hour]:[minute]:[second].[subsecond digits:3]"
        ))
        .unwrap_or_default()
}

/// Login time left: `11h40m`, `40m`, `0m`; `expired` from `expires_at` on.
pub(crate) fn remaining(expires_at: OffsetDateTime, now: OffsetDateTime) -> String {
    let m = (expires_at - now).whole_minutes();
    if expires_at <= now {
        "expired".to_owned()
    } else if m >= 60 {
        format!("{}h{}m", m / 60, m % 60)
    } else {
        format!("{m}m")
    }
}

/// Truncates to `w` columns (chars) with `…`.
fn fit(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_owned();
    }
    let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
    if w > 0 {
        t.push('…');
    }
    t
}

fn fetched_at(e: &Entry) -> Option<OffsetDateTime> {
    match e {
        Entry::Ready { fetched_at, .. } => Some(*fetched_at),
        Entry::Loading { stale: Some(s), .. } => fetched_at(s),
        _ => None,
    }
}

/// `Some(pages)` while the key is being fetched (also a refetch showing a stale value).
fn loading_pages(state: &State, key: &CacheKey) -> Option<usize> {
    match state.entry(key)? {
        Entry::Loading { pages, .. } => Some(*pages),
        _ => None,
    }
}

/// ` · loading… N pages`, or empty when not loading.
fn loading_suffix(state: &State, key: &CacheKey) -> String {
    match loading_pages(state, key) {
        None => String::new(),
        Some(0) => " · loading…".to_owned(),
        Some(1) => " · loading… 1 page".to_owned(),
        Some(n) => format!(" · loading… {n} pages"),
    }
}

/// Text for a key with no value: blank when never requested (e.g. pane not focused yet).
fn placeholder(state: &State, key: &CacheKey) -> &'static str {
    match state.entry(key) {
        Some(Entry::Loading { .. }) => "loading…",
        Some(Entry::Failed { .. }) => "load failed — see status, r to retry",
        _ => "",
    }
}

/// Block inner width after borders (saturating).
fn inner_width(area: Rect) -> usize {
    area.width.saturating_sub(2) as usize
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use ratatui::{
        Terminal,
        backend::TestBackend,
        buffer::Buffer,
        style::{Color, Modifier, Style},
    };
    use time::{OffsetDateTime, macros::datetime};

    use super::*;
    use crate::app::state::{
        Action, CacheKey, CacheValue, Effect, Event, RegistryView, RequestId, Tab,
    };
    use crate::app::update::{Input, update};
    use crate::app::write::{OpId, PushOutcome};
    use crate::config::AuthCfg;
    use crate::config_edit::{AuthKind, Kind, RegistryForm};
    use crate::docker::PushStep;
    use crate::domain::{
        ArtifactStatus, ArtifactSummary, Caps, Descriptor, Details, Digest, HistoryEntry,
        ImageConfig, Inventory, LocalImage, ManifestDocument, ManifestKind, Platform,
        RegistryError, RegistryId, RepoPath, RepoSummary, TagPolicy,
    };

    const NOW: OffsetDateTime = datetime!(2024-06-17 14:02 UTC);
    /// `now` given to `render` (login time left).
    const RENDER_NOW: OffsetDateTime = datetime!(2026-10-29 11:32 UTC);

    fn dg(c: char) -> Digest {
        Digest::parse(&format!("sha256:{}", c.to_string().repeat(64))).unwrap()
    }

    fn rid(s: &str) -> RegistryId {
        RegistryId(s.into())
    }

    fn rp(s: &str) -> RepoPath {
        RepoPath(s.into())
    }

    fn ecr() -> RegistryView {
        RegistryView {
            id: rid("company"),
            ref_host: "123456789012.dkr.ecr.us-west-2.amazonaws.com".into(),
            endpoint: "https://123456789012.dkr.ecr.us-west-2.amazonaws.com"
                .parse()
                .unwrap(),
            provider: "ecr",
            auth: AuthCfg::Ecr,
            caps: Caps {
                inventory: Inventory::Complete,
                pushed_at: true,
                last_pulled: true,
                delete_tag: true,
                delete_manifest: true,
            },
            auth_failed: false,
            can_login: true,
            login: None,
            pinned: vec![],
        }
    }

    fn oci() -> RegistryView {
        RegistryView {
            id: rid("mirror"),
            ref_host: "registry.example.com".into(),
            endpoint: "https://registry.example.com".parse().unwrap(),
            provider: "oci",
            auth: AuthCfg::DockerConfig,
            caps: Caps {
                inventory: Inventory::TaggedOnly,
                pushed_at: false,
                last_pulled: false,
                delete_tag: false,
                delete_manifest: false,
            },
            auth_failed: true,
            can_login: false,
            login: None,
            pinned: vec![],
        }
    }

    fn art(c: char, tags: &[&str], size: Option<u64>) -> ArtifactSummary {
        ArtifactSummary {
            digest: dg(c),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            media_type: Some("application/vnd.docker.distribution.manifest.v2+json".into()),
            artifact_type: None,
            subject: None,
            size,
            pushed_at: Some(datetime!(2024-06-17 09:40 UTC)),
            last_pulled: None,
            status: ArtifactStatus::Active,
        }
    }

    fn desc(c: char, size: u64, platform: Option<(&str, &str)>) -> Descriptor {
        Descriptor {
            media_type: "application/vnd.docker.image.rootfs.diff.tar.gzip".into(),
            digest: dg(c),
            size,
            platform: platform.map(|(os, arch)| Platform {
                os: os.into(),
                architecture: arch.into(),
                variant: None,
            }),
            artifact_type: None,
            annotations: BTreeMap::new(),
        }
    }

    fn manifest(raw: &str, kind: ManifestKind) -> CacheValue {
        CacheValue::Manifest(ManifestDocument {
            media_type: "application/vnd.docker.distribution.manifest.v2+json".into(),
            raw: raw.into(),
            kind,
            subject: None,
            artifact_type: None,
            annotations: BTreeMap::new(),
        })
    }

    fn image_manifest(cfg: char) -> CacheValue {
        manifest(
            r#"{"schemaVersion":2,"config":{"digest":"x"}}"#,
            ManifestKind::Image {
                config: desc(cfg, 7_000, None),
                layers: vec![desc('1', 29_000_000, None), desc('2', 181_000_000, None)],
            },
        )
    }

    fn config() -> CacheValue {
        config_with(vec![
            HistoryEntry {
                created_by: "/bin/sh -c #(nop) ADD file:abc in /".into(),
                created: Some(datetime!(2024-06-17 09:30 UTC)),
                empty_layer: false,
            },
            HistoryEntry {
                created_by: "ENV API_TOKEN=abc123".into(),
                created: Some(datetime!(2024-06-17 09:31 UTC)),
                empty_layer: true,
            },
            HistoryEntry {
                created_by: "/bin/sh -c apk add curl".into(),
                created: Some(datetime!(2024-06-17 09:32 UTC)),
                empty_layer: false,
            },
        ])
    }

    fn config_with(history: Vec<HistoryEntry>) -> CacheValue {
        CacheValue::Config(Some(ImageConfig {
            raw: r#"{"architecture":"amd64","config":{"Env":["PATH=/usr/bin","DB_PASSWORD=hunter2"],"Cmd":["serve"]}}"#.into(),
            created: Some(datetime!(2024-06-17 09:38 UTC)),
            platform: Some(Platform { os: "linux".into(), architecture: "amd64".into(), variant: None }),
            entrypoint: vec!["/app/gw".into()],
            cmd: vec!["serve".into(), "--api-token".into(), "abc123".into()],
            working_dir: Some("/app".into()),
            user: Some("1000".into()),
            exposed_ports: vec!["8080/tcp".into()],
            volumes: vec!["/data".into()],
            labels: BTreeMap::from([
                ("org.opencontainers.image.title".into(), "api-svc".into()),
                ("auth.secret".into(), "s3cr3t".into()),
            ]),
            env: vec![
                ("PATH".into(), "/usr/bin".into()),
                ("DB_PASSWORD".into(), "hunter2".into()),
            ],
            history,
        }))
    }

    /// State plus the fetches it has issued and not yet answered, built only via the public API.
    struct Fx {
        s: State,
        pending: HashMap<CacheKey, RequestId>,
    }

    impl Fx {
        fn new(regs: Vec<RegistryView>) -> Fx {
            let (s, fx) = State::new(regs);
            let mut t = Fx {
                s,
                pending: HashMap::new(),
            };
            t.absorb(fx);
            t
        }

        fn absorb(&mut self, fx: Vec<Effect>) {
            for e in fx {
                if let Effect::Fetch { key, req } = e {
                    self.pending.insert(key, req);
                }
            }
        }

        fn act(&mut self, a: Action) {
            let fx = update(&mut self.s, Input::Action(a), NOW);
            self.absorb(fx);
        }

        fn load(&mut self, key: CacheKey, v: CacheValue) {
            self.finish_load(key, Ok(v));
        }

        fn fail(&mut self, key: CacheKey, e: RegistryError) {
            self.finish_load(key, Err(e));
        }

        fn finish_load(&mut self, key: CacheKey, result: Result<CacheValue, RegistryError>) {
            let req = self
                .pending
                .remove(&key)
                .unwrap_or_else(|| panic!("no fetch for {key:?}"));
            let ev = Event::Loaded { key, req, result };
            let fx = update(&mut self.s, Input::Event(ev), NOW);
            self.absorb(fx);
        }
    }

    fn repos() -> CacheValue {
        CacheValue::Repos(
            ["api-svc", "ttl"]
                .iter()
                .map(|n| RepoSummary {
                    path: rp(n),
                    created_at: Some(datetime!(2024-01-02 03:04 UTC)),
                })
                .collect(),
        )
    }

    /// ECR registry, repo `api-svc` with `images`, focus on the Images pane.
    fn with_images(images: Vec<ArtifactSummary>) -> Fx {
        let host = "123456789012.dkr.ecr.us-west-2.amazonaws.com";
        with_images_local(images, format!("{host}/api-svc@{}", dg('a')))
    }

    /// As `with_images`, with one local image carrying `repo_digest`.
    fn with_images_local(images: Vec<ArtifactSummary>, repo_digest: String) -> Fx {
        let mut f = Fx::new(vec![ecr(), oci()]);
        f.load(
            CacheKey::LocalImages,
            CacheValue::LocalImages(vec![LocalImage {
                id: "sha256:88d90c6be1e0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .into(),
                repo_tags: vec![],
                repo_digests: vec![repo_digest],
                manifests: vec![],
                created: None,
                size: 1,
            }]),
        );
        f.load(CacheKey::Repos(rid("company")), repos());
        f.act(Action::Enter);
        f.load(
            CacheKey::Inventory(rid("company"), rp("api-svc")),
            CacheValue::Inventory(images),
        );
        f.act(Action::Enter);
        f
    }

    fn key_man(c: char) -> CacheKey {
        CacheKey::Manifest(rid("company"), rp("api-svc"), dg(c))
    }

    fn key_cfg(c: char) -> CacheKey {
        CacheKey::Config(rid("company"), rp("api-svc"), dg(c))
    }

    /// One plain image `a`, fully loaded.
    fn loaded() -> Fx {
        loaded_with(config(), CacheValue::Referrers(Some(vec![])))
    }

    fn loaded_with(cfg: CacheValue, referrers: CacheValue) -> Fx {
        let mut a = art('a', &["0.1.0-dev"], Some(210_000_000));
        a.last_pulled = None;
        let mut b = art('b', &["0.0.9"], Some(150_000_000));
        b.pushed_at = Some(datetime!(2024-05-01 10:00 UTC));
        b.last_pulled = Some(datetime!(2024-06-10 08:00 UTC));
        let mut f = with_images(vec![a, b]);
        f.load(key_man('a'), image_manifest('c'));
        f.load(
            CacheKey::Referrers(rid("company"), rp("api-svc"), dg('a')),
            referrers,
        );
        f.load(key_cfg('a'), cfg);
        f
    }

    fn render_to(w: u16, h: u16, s: &State) -> String {
        render_view(w, h, s, &mut ViewState::default())
    }

    /// As `render_to`, keeping `view` across frames.
    fn render_view(w: u16, h: u16, s: &State, view: &mut ViewState) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render(f, s, RENDER_NOW, view)).unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn shot(f: &Fx) -> String {
        render_to(120, 40, &f.s)
    }

    #[test]
    fn main_screen_ecr_overview() {
        insta::assert_snapshot!(shot(&loaded()));
    }

    #[test]
    fn tagged_only_registry() {
        let mut f = Fx::new(vec![ecr(), oci()]);
        f.load(CacheKey::LocalImages, CacheValue::LocalImages(vec![]));
        f.load(CacheKey::Repos(rid("company")), repos());
        f.act(Action::Down);
        f.load(
            CacheKey::Repos(rid("mirror")),
            CacheValue::Repos(vec![RepoSummary {
                path: rp("team/api"),
                created_at: None,
            }]),
        );
        f.act(Action::Enter);
        f.act(Action::Enter);
        let inv = CacheKey::Inventory(rid("mirror"), rp("team/api"));
        f.load(
            inv,
            CacheValue::Inventory(vec![
                art('d', &["1.0", "latest"], None),
                art('e', &["2.0"], None),
            ]),
        );
        // Selected 'e' is an OCI index (size shown as —); 'd' is not loaded yet (blank).
        let d = CacheKey::Manifest(rid("mirror"), rp("team/api"), dg('e'));
        f.load(
            d,
            manifest(
                "{}",
                ManifestKind::Index {
                    manifests: vec![desc('f', 5, Some(("linux", "amd64")))],
                },
            ),
        );
        insta::assert_snapshot!(shot(&f));
    }

    #[test]
    fn layers_index_platforms() {
        let mut f = with_images(vec![art('a', &["0.2.0"], Some(210_000_000))]);
        f.load(
            key_man('a'),
            manifest(
                r#"{"manifests":[]}"#,
                ManifestKind::Index {
                    manifests: vec![
                        desc('c', 500, Some(("linux", "amd64"))),
                        desc('d', 500, Some(("linux", "arm64"))),
                    ],
                },
            ),
        );
        f.act(Action::Tab(Tab::Layers));
        f.load(key_man('c'), image_manifest('e'));
        f.act(Action::PlatformNext);
        f.load(key_man('d'), image_manifest('f'));
        insta::assert_snapshot!(shot(&f));
    }

    #[test]
    fn config_tab_masked() {
        let mut f = loaded();
        f.act(Action::Tab(Tab::Config));
        insta::assert_snapshot!(shot(&f));
    }

    #[test]
    fn config_tab_revealed() {
        let mut f = loaded();
        f.act(Action::Tab(Tab::Config));
        f.act(Action::ToggleReveal);
        insta::assert_snapshot!(shot(&f));
    }

    #[test]
    fn history_tab() {
        let mut f = loaded();
        f.act(Action::Tab(Tab::History));
        insta::assert_snapshot!(shot(&f));
    }

    fn steps(by: &[(&str, bool)]) -> Vec<HistoryEntry> {
        by.iter()
            .enumerate()
            .map(|(i, (by, empty))| HistoryEntry {
                created_by: (*by).into(),
                created: Some(datetime!(2024-06-17 09:30 UTC) + time::Duration::minutes(i as i64)),
                empty_layer: *empty,
            })
            .collect()
    }

    #[test]
    fn history_tab_sizes_mismatch() {
        let h = steps(&[
            ("RUN a", false),
            ("ENV B=1", true),
            ("RUN c", false),
            ("RUN d", false),
        ]);
        let mut f = loaded_with(config_with(h), CacheValue::Referrers(Some(vec![])));
        f.act(Action::Tab(Tab::History));
        insta::assert_snapshot!(shot(&f));
    }

    #[test]
    fn history_masks_after_cleanup() {
        let h = steps(&[("/bin/sh -c export API_TOKEN=abc", false)]);
        let mut f = loaded_with(config_with(h), CacheValue::Referrers(Some(vec![])));
        f.act(Action::Tab(Tab::History));
        let out = shot(&f);
        assert!(out.contains("RUN export API_TOKEN=••••••"), "{out}");
        assert!(!out.contains("abc"), "{out}");
    }

    fn referrer(
        artifact_type: Option<&str>,
        media: &str,
        d: char,
        size: u64,
        created: Option<&str>,
    ) -> Descriptor {
        Descriptor {
            media_type: media.into(),
            artifact_type: artifact_type.map(Into::into),
            annotations: created
                .map(|c| BTreeMap::from([("org.opencontainers.image.created".into(), c.into())]))
                .unwrap_or_default(),
            ..desc(d, size, None)
        }
    }

    #[test]
    fn referrers_list() {
        let oci = "application/vnd.oci.image.manifest.v1+json";
        let refs = vec![
            referrer(
                Some("application/vnd.dev.cosign.artifact.sig.v1+json"),
                oci,
                'a',
                1_200,
                Some("2026-10-30T09:00:00Z"),
            ),
            // No artifact type: falls back to the media type.
            referrer(
                None,
                "application/spdx+json",
                'b',
                48_000,
                Some("2026-10-30T10:30:00Z"),
            ),
            referrer(Some("application/x-custom"), oci, 'c', 300, None),
        ];
        let f = loaded_with(config(), CacheValue::Referrers(Some(refs)));
        let out = shot(&f);
        assert!(out.contains("Attached   3"), "{out}");
        insta::assert_snapshot!(out);
        for none in [
            CacheValue::Referrers(Some(vec![])),
            CacheValue::Referrers(None),
        ] {
            let out = shot(&loaded_with(config(), none));
            let rows = out.lines().filter(|l| l.contains("sha256:")).count();
            assert_eq!(rows, 1, "only the Digest line:\n{out}");
        }
    }

    #[test]
    fn raw_tab() {
        let mut f = loaded();
        f.act(Action::Tab(Tab::Raw));
        insta::assert_snapshot!(shot(&f));
    }

    #[test]
    fn non_image_artifact() {
        let mut f = with_images(vec![art('a', &["sbom"], Some(1_200))]);
        f.load(key_man('a'), image_manifest('c'));
        f.load(
            CacheKey::Referrers(rid("company"), rp("api-svc"), dg('a')),
            CacheValue::Referrers(None),
        );
        f.load(key_cfg('a'), CacheValue::Config(None));
        f.act(Action::Tab(Tab::Config));
        insta::assert_snapshot!(shot(&f));
    }

    #[test]
    fn archived_image() {
        let mut a = art('a', &["old"], Some(5_000_000));
        a.status = ArtifactStatus::Archived;
        let mut f = with_images(vec![a]);
        f.act(Action::Tab(Tab::Layers));
        insta::assert_snapshot!(shot(&f));
    }

    #[test]
    fn empty_registry() {
        let mut f = Fx::new(vec![ecr(), oci()]);
        f.load(CacheKey::LocalImages, CacheValue::LocalImages(vec![]));
        f.load(CacheKey::Repos(rid("company")), CacheValue::Repos(vec![]));
        insta::assert_snapshot!(shot(&f));
    }

    fn details(fields: &[(&str, &str)]) -> CacheValue {
        CacheValue::Details(Details {
            fields: fields
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        })
    }

    /// Registries pane, `company` focused, its repos loaded.
    fn registries() -> Fx {
        let mut f = Fx::new(vec![ecr(), oci()]);
        f.load(CacheKey::LocalImages, CacheValue::LocalImages(vec![]));
        f.load(CacheKey::Repos(rid("company")), repos());
        f
    }

    #[test]
    fn registry_details_ecr() {
        let mut f = registries();
        f.load(
            CacheKey::RegistryDetails(rid("company")),
            details(&[
                ("Registry", "123456789012"),
                ("Account", "111122223333"),
                ("Caller", "arn:aws:sts::111122223333:assumed-role/dev/me"),
            ]),
        );
        insta::assert_snapshot!(shot(&f));
    }

    #[test]
    fn repo_details_ecr() {
        let mut f = registries();
        f.act(Action::Enter);
        f.load(
            CacheKey::Inventory(rid("company"), rp("api-svc")),
            CacheValue::Inventory(vec![art('a', &["0.1.0-dev"], Some(1))]),
        );
        f.load(
            CacheKey::RepoDetails(rid("company"), rp("api-svc")),
            details(&[
                (
                    "ARN",
                    "arn:aws:ecr:us-west-2:123456789012:repository/api-svc",
                ),
                (
                    "URI",
                    "123456789012.dkr.ecr.us-west-2.amazonaws.com/api-svc",
                ),
                ("Mutability", "MUTABLE"),
                ("Encryption", "AES256"),
                ("Scan push", "no"),
            ]),
        );
        insta::assert_snapshot!(shot(&f));
    }

    #[test]
    fn details_failed_line() {
        let mut f = registries();
        f.fail(
            CacheKey::RegistryDetails(rid("company")),
            RegistryError::Unauthorized,
        );
        let s = shot(&f);
        assert!(
            s.contains("Details    unavailable \u{2014} press r to retry"),
            "{s}"
        );
        assert!(s.contains("Endpoint"), "{s}");
    }

    #[test]
    fn oci_registry_has_no_details_line() {
        let mut f = registries();
        f.act(Action::Down);
        f.load(
            CacheKey::RegistryDetails(rid("mirror")),
            CacheValue::Details(Details::default()),
        );
        let s = shot(&f);
        assert!(!s.contains("Details"), "{s}");
        assert!(s.contains("Endpoint"), "{s}");
    }

    #[test]
    fn times_are_converted_to_utc() {
        let t = datetime!(2026-10-29 13:00 +02:00);
        assert_eq!(fmt_time(t), "2026-10-29 11:00");
        assert_eq!(fmt_hm(t), "11:00");
    }

    #[test]
    fn theme_picker() {
        // Cursor stays on the default theme: rendering sets the process-wide palette, and other
        // tests running in parallel compare colours against it.
        let mut f = loaded();
        f.act(Action::ThemePick);
        insta::assert_snapshot!(shot(&f));
    }

    #[test]
    fn help_modal() {
        let mut f = loaded();
        f.act(Action::ToggleHelp);
        insta::assert_snapshot!(shot(&f));
    }

    fn req(
        at: OffsetDateTime,
        reg: &str,
        op: &str,
        path: &str,
        outcome: &str,
        ms: u64,
        bytes: Option<u64>,
    ) -> crate::reqlog::RequestEntry {
        crate::reqlog::RequestEntry {
            at,
            registry: reg.into(),
            op: op.into(),
            path: path.into(),
            outcome: outcome.into(),
            ms,
            bytes,
        }
    }

    #[test]
    fn requests_overlay() {
        let mut f = loaded();
        f.act(Action::Requests);
        f.s.set_requests(vec![
            req(
                datetime!(2026-10-31 09:15:02.123 UTC),
                "ecr",
                "GET manifest",
                "/v2/app/manifests/latest",
                "200",
                48,
                Some(1_234),
            ),
            req(
                datetime!(2026-10-31 09:15:03.007 +02:00),
                "local",
                "docker pull",
                "/images/create",
                "ok",
                1_820,
                None,
            ),
        ]);
        let out = render_to(120, 30, &f.s);
        assert!(out.contains(" Requests (2) "), "{out}");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn requests_overlay_scrolls_from_the_bottom() {
        let mut f = loaded();
        f.act(Action::Requests);
        let at = datetime!(2026-10-31 09:15:02 UTC);
        f.s.set_requests(
            (0..40)
                .map(|i| req(at, "ecr", "GET", &format!("/v2/p{i}"), "200", 1, None))
                .collect(),
        );
        let mut view = ViewState::default();
        let out = render_view(120, 20, &f.s, &mut view);
        assert!(out.contains("/v2/p39") && !out.contains("/v2/p0 "), "{out}");
        assert!(view.requests_max_scroll > 0);
        for _ in 0..100 {
            if !view.past_end(&f.s, Action::Up) {
                f.act(Action::Up);
            }
        }
        let out = render_view(120, 20, &f.s, &mut view);
        assert!(out.contains("/v2/p0 ") && !out.contains("/v2/p39"), "{out}");
        assert_eq!(f.s.requests().unwrap().scroll, view.requests_max_scroll);
    }

    #[test]
    fn requests_hint_bar() {
        let mut f = loaded();
        f.act(Action::Requests);
        let hints = row_text(&buf_of(120, 40, &f.s), 38);
        assert_eq!(hints.trim(), "↑↓ scroll  / filter  Esc close");
        f.act(Action::FilterStart);
        let hints = row_text(&buf_of(120, 40, &f.s), 38);
        assert_eq!(hints.trim(), "type to filter  Enter apply  Esc clear");
    }

    #[test]
    fn help_hint_bar_and_quit_rows() {
        let mut f = loaded();
        f.act(Action::ToggleHelp);
        let out = shot(&f);
        let hints = row_text(&buf_of(120, 40, &f.s), 38);
        assert_eq!(hints.trim(), "↑↓ scroll  Esc/? close  Ctrl-C quit");
        assert!(
            out.contains("  q               quit (main screen)"),
            "{out}"
        );
        assert!(out.contains("  Ctrl-C          quit (anywhere)"), "{out}");
        assert!(!out.contains("q / Ctrl-C"), "{out}");
    }

    #[test]
    fn help_scrolls_to_the_last_row() {
        let mut f = loaded();
        f.act(Action::ToggleHelp);
        let mut view = ViewState::default();
        let last = "? again closes this help";
        let out = render_view(120, 40, &f.s, &mut view);
        assert!(out.contains("Keys") && !out.contains(last), "{out}");
        assert!(view.help_max_scroll > 0);
        for _ in 0..100 {
            if !view.past_end(&f.s, Action::Down) {
                f.act(Action::Down);
            }
        }
        let out = render_view(120, 40, &f.s, &mut view);
        assert!(out.contains(last) && !out.contains("Keys"), "{out}");
        assert_eq!(f.s.help_scroll(), view.help_max_scroll);
        f.act(Action::ToggleHelp);
        f.act(Action::ToggleHelp);
        assert_eq!(f.s.help_scroll(), 0, "reopens at the top");
    }

    // ---- registry form ----

    fn ecr_form() -> RegistryForm {
        RegistryForm {
            id: "company".into(),
            kind: Kind::Ecr,
            account_id: "123456789012".into(),
            region: "us-west-2".into(),
            pinned: "api-svc, eng/api/fo".into(),
            ..RegistryForm::new_default()
        }
    }

    #[test]
    fn registry_form_ecr() {
        let mut f = registries();
        f.s.open_registry_form(Some(rid("company")), ecr_form());
        let derived = "https://123456789012.dkr.ecr.us-west-2.amazonaws.com (auto)";
        let out = render_to(100, 30, &f.s);
        assert!(out.contains(" Registry company "), "{out}");
        assert!(out.contains(derived), "{out}");
        let buf = buf_of(100, 30, &f.s);
        let (x, y) = find(&buf, derived);
        assert_eq!(
            cell_style(&buf, x, y).fg,
            theme::muted().fg,
            "derived is muted"
        );
        assert_eq!(
            row_text(&buf, 28).trim(),
            "Tab/↑↓ field  ←→ type/auth  Ctrl-T test  Ctrl-S save  Esc cancel"
        );
        insta::assert_snapshot!(out);
    }

    #[test]
    fn registry_form_oci_basic() {
        let mut f = registries();
        let form = RegistryForm {
            id: "ghcr".into(),
            kind: Kind::Oci,
            endpoint: "https://ghcr.io".into(),
            auth: AuthKind::Basic,
            username: "lyo".into(),
            // Always set; its value must never be shown.
            password_env: "PATH".into(),
            repositories: "lyo/app, lyo/tools".into(),
            ..RegistryForm::new_default()
        };
        f.s.open_registry_form(None, form);
        let out = render_to(100, 30, &f.s);
        assert!(out.contains(" New registry "), "{out}");
        assert!(out.contains("Password env       PATH"), "{out}");
        let path = std::env::var("PATH").unwrap();
        for part in path.split(':').filter(|p| p.len() > 4) {
            assert!(!out.contains(part), "{part} shown: {out}");
        }
        insta::assert_snapshot!(out);
    }

    #[test]
    fn registry_form_test() {
        use crate::conntest::{Check, CheckStatus};
        let mut f = registries();
        f.s.open_registry_form(Some(rid("company")), ecr_form());
        f.act(Action::FormTest);
        let op = f.s.reg_form().unwrap().test.as_ref().unwrap().op;
        let arn = "arn:aws:iam::123456789012:user/ci-robot";
        for (step, status, detail, ms) in [
            (1, CheckStatus::Ok, arn, 240),
            (2, CheckStatus::Ok, "HTTP 401", 182),
            (3, CheckStatus::Failed, "unauthorized", 95),
            (4, CheckStatus::Skipped, "", 0),
        ] {
            let check = Check {
                status,
                detail: detail.into(),
                ms,
            };
            let fx = update(
                &mut f.s,
                Input::Event(Event::ConnCheck { op, step, check }),
                NOW,
            );
            f.absorb(fx);
        }
        let out = render_to(100, 30, &f.s);
        for want in [
            "✓ config valid",
            &format!("✓ credentials         {arn} · 240 ms"),
            "✓ endpoint reachable  HTTP 401 · 182 ms",
            "✗ registry login      unauthorized · 95 ms",
            "· list repositories   skipped",
            "Not working — save anyway with Ctrl-S",
        ] {
            assert!(out.contains(want), "{want}: {out}");
        }
        insta::assert_snapshot!(out);
    }

    #[test]
    fn registry_form_discard_prompt() {
        let mut f = registries();
        f.s.open_registry_form(Some(rid("company")), ecr_form());
        f.act(Action::FilterChar('2'));
        f.act(Action::Back);
        let out = render_to(100, 30, &f.s);
        assert!(out.contains("discard changes? y/n"), "{out}");
        assert_eq!(
            row_text(&buf_of(100, 30, &f.s), 28).trim(),
            "y yes  n/Esc no"
        );
        insta::assert_snapshot!(out);
    }

    #[test]
    fn registry_remove_prompt() {
        let mut f = registries();
        f.act(Action::RegistryRemove);
        let out = render_to(100, 30, &f.s);
        assert!(out.contains("remove registry company? y/n"), "{out}");
    }

    #[test]
    fn registries_hint_bar_offers_form_keys() {
        let f = registries();
        let hints = row_text(&buf_of(160, 40, &f.s), 38);
        assert!(hints.contains("c/n/x edit/new/remove"), "{hints}");
        let f = loaded();
        let hints = row_text(&buf_of(160, 40, &f.s), 38);
        assert!(!hints.contains("c/n/x"), "images pane: {hints}");
    }

    // ---- push wizard ----

    const HOST: &str = "123456789012.dkr.ecr.us-west-2.amazonaws.com";
    const IMG1: &str = "sha256:88d90c6be1e0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const IMG2: &str = "sha256:b3f1d2c4e5f6bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const IMG3: &str = "sha256:0a1b2c3d4e5fcccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn local(id: &str, tags: &[&str], created: Option<OffsetDateTime>, size: u64) -> LocalImage {
        LocalImage {
            id: id.into(),
            repo_tags: tags.iter().map(|t| t.to_string()).collect(),
            repo_digests: vec![],
            manifests: vec![],
            created,
            size,
        }
    }

    /// Wizard open (`PickImage`) on company/api-svc, whose inventory holds `0.1.0-dev`; three
    /// local images, the second one already tagged `<host>/api-svc:0.1.0-dev`.
    fn at_wizard() -> Fx {
        let mut f = Fx::new(vec![ecr()]);
        let tagged = format!("{HOST}/api-svc:0.1.0-dev");
        f.load(
            CacheKey::LocalImages,
            CacheValue::LocalImages(vec![
                local(
                    IMG1,
                    &["api-svc:dev", "api-svc:latest"],
                    Some(datetime!(2026-10-28 09:15 UTC)),
                    210_000_000,
                ),
                local(
                    IMG2,
                    &[&tagged],
                    Some(datetime!(2026-10-20 18:02 UTC)),
                    1_830_000_000,
                ),
                local(IMG3, &[], None, 12_300),
            ]),
        );
        f.load(CacheKey::Repos(rid("company")), repos());
        f.act(Action::Enter);
        f.load(
            CacheKey::Inventory(rid("company"), rp("api-svc")),
            CacheValue::Inventory(vec![art('a', &["0.1.0-dev"], Some(210_000_000))]),
        );
        f.act(Action::PushStart);
        f
    }

    fn type_text(f: &mut Fx, t: &str) {
        for c in t.chars() {
            f.act(Action::FilterChar(c));
        }
    }

    /// `at_wizard()` advanced to `Tag`, image 1 chosen, suggestion `0.1.1-dev`; the inventory
    /// refresh on entering `Tag` has completed.
    fn at_tag() -> Fx {
        let mut f = at_wizard();
        f.act(Action::Enter);
        f.act(Action::Enter);
        f.load(
            CacheKey::Inventory(rid("company"), rp("api-svc")),
            CacheValue::Inventory(vec![art('a', &["0.1.0-dev"], Some(210_000_000))]),
        );
        f
    }

    /// `at_tag()` with `tag` typed over the suggestion, on `Confirm`.
    fn at_confirm(tag: &str) -> Fx {
        let mut f = at_tag();
        type_text(&mut f, tag);
        f.act(Action::Enter);
        f
    }

    fn policy_key(tag: &str) -> CacheKey {
        CacheKey::TagPolicy(rid("company"), rp("api-svc"), tag.into())
    }

    /// `at_tag()` pushed: stage `Progress`, op 1 running.
    fn pushing() -> Fx {
        let mut f = at_confirm("0.1.1-dev");
        f.act(Action::Enter);
        for step in [
            PushStep::Status("Preparing".into()),
            PushStep::LayerDone,
            PushStep::LayerDone,
            PushStep::LayerDone,
            PushStep::Bytes {
                current: 12_300_000,
                total: 20_500_000,
            },
            PushStep::Status("Waiting".into()),
        ] {
            let ev = Event::PushProgress { op: OpId(1), step };
            update(&mut f.s, Input::Event(ev), NOW);
        }
        f
    }

    fn finish(f: &mut Fx, outcome: PushOutcome) {
        let ev = Event::PushDone {
            op: OpId(1),
            outcome,
        };
        update(&mut f.s, Input::Event(ev), NOW);
    }

    #[test]
    fn wizard_pick_image() {
        let mut f = at_wizard();
        f.act(Action::Down);
        insta::assert_snapshot!(render_to(100, 30, &f.s));
    }

    #[test]
    fn wizard_title_from_registries() {
        // Opened on the Registries pane: no repo pre-selected, so the title shows `?:?`.
        let mut f = Fx::new(vec![ecr()]);
        f.load(
            CacheKey::LocalImages,
            CacheValue::LocalImages(vec![local(IMG1, &["api-svc:dev"], None, 210_000_000)]),
        );
        f.act(Action::PushStart);
        let out = render_to(100, 30, &f.s);
        assert!(
            out.contains(&format!("Push to company → {HOST}/?:?")),
            "{out}"
        );
        insta::assert_snapshot!(out);
    }

    #[test]
    fn wizard_pick_image_filter_and_empty() {
        let mut f = at_wizard();
        type_text(&mut f, "latest");
        let out = render_to(100, 30, &f.s);
        assert!(out.contains("filter: latest▏"), "{out}");
        assert!(out.contains("88d90c6be1e0"), "{out}");
        assert!(!out.contains("b3f1d2c4e5f6"), "{out}");
        type_text(&mut f, "zz");
        assert!(render_to(100, 30, &f.s).contains("(no matching images)"));
    }

    #[test]
    fn wizard_pick_repo() {
        let mut f = at_wizard();
        f.act(Action::Enter);
        insta::assert_snapshot!(render_to(100, 30, &f.s));
    }

    #[test]
    fn wizard_tag() {
        insta::assert_snapshot!(render_to(100, 30, &at_tag().s));
        // Typed over: no suggestion note.
        let mut f = at_tag();
        f.act(Action::FilterBackspace);
        let out = render_to(100, 30, &f.s);
        assert!(out.contains("Tag  0.1.1-de▏"), "{out}");
        assert!(!out.contains("suggested from"), "{out}");
    }

    #[test]
    fn wizard_confirm() {
        // The tag exists (policy fetched: mutable) and another local image holds the ref.
        let mut f = at_confirm("0.1.0-dev");
        let out = render_to(100, 30, &f.s);
        assert!(out.contains("checking tag policy…"), "{out}");
        f.load(
            policy_key("0.1.0-dev"),
            CacheValue::TagPolicy(TagPolicy::Mutable),
        );
        insta::assert_snapshot!(render_to(100, 30, &f.s));
    }

    #[test]
    fn wizard_confirm_warning_texts() {
        for (p, text) in [
            (
                TagPolicy::Immutable,
                "tag exists; push will be rejected (immutable)",
            ),
            (TagPolicy::Unknown, "tag exists; registry policy unknown"),
        ] {
            let mut f = at_confirm("0.1.0-dev");
            f.load(policy_key("0.1.0-dev"), CacheValue::TagPolicy(p));
            let out = render_to(100, 30, &f.s);
            assert!(out.contains(text), "{out}");
        }
        let out = render_to(100, 30, &at_confirm("0.1.0-dev").s);
        assert!(out.contains("local tag "), "{out}");
        // (the line wraps in the modal)
        assert!(
            out.contains(&format!("{HOST}/api-svc:0.1.0-dev will be")),
            "{out}"
        );
        assert!(out.contains("replaced"), "{out}");
        // A new tag: no warning at all, the hint names the keys.
        let out = render_to(100, 30, &at_confirm("9.9.9").s);
        assert!(
            !out.contains("tag exists") && !out.contains("will be"),
            "{out}"
        );
        assert!(out.contains("Enter push  Esc back"), "{out}");
    }

    #[test]
    fn wizard_running() {
        insta::assert_snapshot!(render_to(100, 30, &pushing().s));
    }

    #[test]
    fn status_line_while_hidden() {
        let mut f = pushing();
        f.act(Action::Enter);
        assert!(f.s.wizard().is_none());
        assert_eq!(
            row_text(&buf_of(100, 30, &f.s), 29).trim(),
            format!("pushing {HOST}/api-svc:0.1.1-dev · layers done 3")
        );
        // The wizard reopened shows the progress again.
        f.act(Action::PushStart);
        assert!(render_to(100, 30, &f.s).contains("layers done 3"));
    }

    #[test]
    fn wizard_finished_ok() {
        let mut f = pushing();
        finish(&mut f, PushOutcome::Pushed(Some(dg('d'))));
        insta::assert_snapshot!(render_to(100, 30, &f.s));
    }

    #[test]
    fn wizard_finished_failed() {
        let mut f = pushing();
        finish(
            &mut f,
            PushOutcome::Failed(RegistryError::Network("connection reset".into())),
        );
        let out = render_to(100, 30, &f.s);
        assert!(!out.contains("press r"), "{out}");
        // Modal and status line both say how to try again.
        assert_eq!(
            out.matches("network error: connection reset — press p to push again")
                .count(),
            2
        );
        insta::assert_snapshot!(out);
    }

    #[test]
    fn transfer_percent_is_clamped_and_hidden_without_total() {
        let transfer = |current, total| {
            let mut f = pushing();
            let step = PushStep::Bytes { current, total };
            update(
                &mut f.s,
                Input::Event(Event::PushProgress { op: OpId(1), step }),
                NOW,
            );
            render_to(100, 30, &f.s)
        };
        let out = transfer(5, 0);
        assert!(
            out.contains("layers done 3") && !out.contains("current transfer"),
            "{out}"
        );
        assert!(!out.contains('%'), "{out}");
        let out = transfer(30, 20);
        assert!(
            out.contains("[████████████████████] 100%  30B / 20B"),
            "{out}"
        );
    }

    #[test]
    fn wizard_finished_cancelled() {
        let mut f = pushing();
        finish(&mut f, PushOutcome::Cancelled);
        insta::assert_snapshot!(render_to(100, 30, &f.s));
    }

    #[test]
    fn wizard_hint_bar_shows_only_wizard_keys() {
        let mut f = at_wizard();
        let hints = |f: &Fx| row_text(&buf_of(100, 30, &f.s), 28).trim().to_owned();
        assert_eq!(
            hints(&f),
            "type to filter  ↑↓ move  Enter select  Esc cancel"
        );
        f.act(Action::Enter);
        assert_eq!(hints(&f), "↑↓ move  Enter select  Esc back");
        f.act(Action::Enter);
        assert_eq!(hints(&f), "type a tag  Enter next  Esc back");
        f.load(
            CacheKey::Inventory(rid("company"), rp("api-svc")),
            CacheValue::Inventory(vec![art('a', &["0.1.0-dev"], Some(210_000_000))]),
        );
        f.act(Action::Enter);
        assert_eq!(hints(&f), "Enter push  Esc back");
        f.act(Action::Enter);
        assert_eq!(hints(&f), "Enter hide  Esc cancel");
        finish(&mut f, PushOutcome::Cancelled);
        assert_eq!(hints(&f), "Enter close");
    }

    #[test]
    fn wizard_styles() {
        fn find_in(f: &Fx, needle: &str) -> (Buffer, (u16, u16)) {
            let buf = buf_of(100, 30, &f.s);
            let at = find(&buf, needle);
            (buf, at)
        }
        // Selected image row filled across the modal; the title bold.
        let (buf, at) = find_in(&at_wizard(), "88d90c6be1e0");
        assert!(all_bg(&buf, at, "88d90c6be1e0", theme::selected().bg));
        assert!(all_have(&buf, at, "88d90c6be1e0", Modifier::BOLD));
        // ... through the last cell inside the border (modal x = 8..92 at 100 columns).
        assert_eq!(cell_style(&buf, 90, at.1).bg, theme::selected().bg);
        let (buf, at) = find_in(&at_wizard(), "Push to company");
        assert!(all_have(&buf, at, "Push to company", Modifier::BOLD));
        assert!(all_fg(&buf, at, "Push to company", theme::title().fg));
        // Confirm: the target is bold, warnings yellow.
        let mut f = at_confirm("0.1.0-dev");
        f.load(
            policy_key("0.1.0-dev"),
            CacheValue::TagPolicy(TagPolicy::Mutable),
        );
        let (buf, at) = find_in(&f, "tag exists and will be moved");
        assert!(
            styles(&buf, at, "tag exists and will be moved")
                .iter()
                .all(|s| s.fg == theme::warn().fg)
        );
        let (buf, at) = find_in(&f, &format!("{HOST}/api-svc:0.1.0-dev"));
        assert!(all_have(&buf, at, HOST, Modifier::BOLD));
        assert!(all_fg(&buf, at, HOST, theme::title().fg));
        // Running: the last status is muted; outcomes are green / red.
        let mut f = pushing();
        let (buf, at) = find_in(&f, "Waiting");
        assert!(all_fg(&buf, at, "Waiting", theme::muted().fg));
        finish(&mut f, PushOutcome::Pushed(None));
        let (buf, at) = find_in(&f, "✓ pushed");
        assert_eq!(cell_style(&buf, at.0, at.1).fg, theme::ok().fg);
        let mut f = pushing();
        finish(
            &mut f,
            PushOutcome::Failed(RegistryError::Network("x".into())),
        );
        let (buf, at) = find_in(&f, "! ");
        assert_eq!(cell_style(&buf, at.0, at.1).fg, theme::bad().fg);
    }

    #[test]
    fn loading_pages_title() {
        // Inventory still loading with 3 pages so far; repos blank, images "loading…".
        let mut f = Fx::new(vec![ecr()]);
        f.load(CacheKey::Repos(rid("company")), repos());
        f.act(Action::Enter);
        f.act(Action::Enter);
        let inv = CacheKey::Inventory(rid("company"), rp("api-svc"));
        let req = f.pending[&inv];
        update(
            &mut f.s,
            Input::Event(Event::Progress { key: inv, pages: 3 }),
            NOW,
        );
        let _ = req;
        insta::assert_snapshot!(shot(&f));
    }

    #[test]
    fn stale_inventory_shown_while_refetching() {
        let mut f = Fx::new(vec![ecr()]);
        f.load(CacheKey::Repos(rid("company")), repos());
        f.act(Action::Enter);
        f.act(Action::Refresh); // inventory still loading: sets `refetch`
        let inv = CacheKey::Inventory(rid("company"), rp("api-svc"));
        f.load(
            inv,
            CacheValue::Inventory(vec![art('a', &["0.1.0-dev"], Some(210_000_000))]),
        );
        insta::assert_snapshot!(shot(&f));
    }

    #[test]
    fn filter_editing() {
        let mut f = loaded();
        for a in [
            Action::FilterStart,
            Action::FilterChar('0'),
            Action::FilterChar('.'),
            Action::FilterChar('0'),
        ] {
            f.act(a);
        }
        insta::assert_snapshot!(shot(&f));
    }

    #[test]
    fn local_presence_requires_same_registry_and_repo() {
        let imgs = || vec![art('a', &["0.1.0-dev"], Some(1))];
        // Same digest pulled from a mirror host, and from another repo of this registry.
        for rd in [
            format!("registry.example.com/api-svc@{}", dg('a')),
            format!(
                "123456789012.dkr.ecr.us-west-2.amazonaws.com/ttl@{}",
                dg('a')
            ),
        ] {
            let out = shot(&with_images_local(imgs(), rd));
            assert!(out.contains("Local      not present"), "{out}");
        }
        assert!(shot(&with_images(imgs())).contains("Local      ✓ present (88d90c6be1e0)"));
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for (w, h) in [(40, 10), (20, 5), (1, 1), (0, 0), (120, 3), (3, 40)] {
            render_to(w, h, &loaded().s);
            let mut f = loaded();
            f.act(Action::ToggleHelp);
            render_to(w, h, &f.s);
        }
    }

    #[test]
    fn remaining_formats() {
        use time::Duration as D;
        let n = RENDER_NOW;
        let r = |d: D| remaining(n + d, n);
        assert_eq!(r(D::hours(11) + D::minutes(40) + D::seconds(30)), "11h40m");
        assert_eq!(r(D::minutes(40)), "40m");
        assert_eq!(r(D::seconds(30)), "0m");
        assert_eq!(r(D::seconds(-1)), "expired");
        assert_eq!(r(D::ZERO), "expired");
    }

    /// Done with expiry, Done without, auth failed only, running, and Done + auth failed.
    fn login_marks() -> Fx {
        use crate::app::state::LoginState;
        let at = RENDER_NOW + time::Duration::minutes(11 * 60 + 40);
        let reg = |id: &str, login: Option<LoginState>, auth_failed: bool| RegistryView {
            id: rid(id),
            login,
            auth_failed,
            ..ecr()
        };
        let done = |t| Some(LoginState::Done { expires_at: t });
        Fx::new(vec![
            reg("company", done(Some(at)), false),
            reg("public", done(None), false),
            RegistryView {
                can_login: false,
                ..oci()
            },
            reg("harbor", Some(LoginState::Running), false),
            reg("both", done(Some(at)), true),
        ])
    }

    #[test]
    fn registries_login_marks() {
        insta::assert_snapshot!(render_to(100, 30, &login_marks().s));
    }

    #[test]
    fn registry_detail_login_field_and_hint() {
        let mut f = login_marks();
        let line = |f: &Fx, prefix: &str| {
            render_to(100, 30, &f.s).lines().find_map(|l| {
                l.split_once(prefix)
                    .map(|(_, r)| r.trim_end_matches('│').trim().to_owned())
            })
        };
        assert_eq!(
            line(&f, "│Login      ").as_deref(),
            Some("expires 2026-10-29 23:12 UTC (11h40m)")
        );
        assert!(render_to(100, 30, &f.s).contains("L login"));
        f.act(Action::Down);
        assert_eq!(
            line(&f, "│Login      ").as_deref(),
            Some("logged in (no expiry)")
        );
        f.act(Action::Down);
        assert_eq!(line(&f, "│Login      "), None, "no login: no field");
        assert!(
            !render_to(100, 30, &f.s).contains("L login"),
            "cannot log in"
        );
        f.act(Action::Down);
        assert_eq!(line(&f, "│Login      ").as_deref(), Some("running…"));
        f.act(Action::Enter);
        assert!(!render_to(100, 30, &f.s).contains("L login"), "Repos pane");
    }

    #[test]
    fn list_offset_kept_when_moving_up() {
        let mut f = Fx::new(vec![ecr()]);
        f.load(
            CacheKey::Repos(rid("company")),
            CacheValue::Repos(
                (0..30)
                    .map(|i| RepoSummary {
                        path: rp(&format!("repo-{i:02}")),
                        created_at: None,
                    })
                    .collect(),
            ),
        );
        f.act(Action::Enter);
        for _ in 0..25 {
            f.act(Action::Down);
        }
        let mut view = ViewState::default();
        // 25 rows: the top band is 10 high, so the repos list shows 8 rows (y = 1..=8).
        let last = |out: &str| out.lines().nth(8).unwrap().to_owned();
        let out = render_view(120, 25, &f.s, &mut view);
        assert!(last(&out).contains("repo-25"), "{out}");
        f.act(Action::Up);
        let out = render_view(120, 25, &f.s, &mut view);
        assert!(out.contains("repo-25"), "{out}");
        assert!(!last(&out).contains("repo-24"), "{out}");
    }

    #[test]
    fn detail_max_scroll_reported() {
        let raw = (0..30)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut f = with_images(vec![art('a', &["x"], Some(1))]);
        f.load(
            key_man('a'),
            manifest(
                &raw,
                ManifestKind::Image {
                    config: desc('c', 1, None),
                    layers: vec![],
                },
            ),
        );
        f.act(Action::Tab(Tab::Raw));
        let mut view = ViewState::default();
        // 40 rows: top band 16, detail 22 high, inner 20.
        render_view(120, 40, &f.s, &mut view);
        assert_eq!(view.detail_max_scroll, 30u16.saturating_sub(20));
        // Fewer lines than rows: nothing to scroll.
        render_view(120, 80, &f.s, &mut view);
        assert_eq!(view.detail_max_scroll, 0);
    }

    #[test]
    fn hint_bar_drops_whole_items() {
        let mut f = loaded();
        f.act(Action::Tab(Tab::Config));
        let full: Vec<&str> = [
            "↑↓/jk move",
            "Tab/←→/hl pane",
            "1-5 tabs",
            "m reveal",
            "y copy",
            "C copy",
            "P pull",
            "X export",
            "= compare",
            "/ filter",
            "o/O sort",
            "space mark",
            "u untagged",
            "a glob",
            "v range",
            "r refresh",
            "p push",
            "Ctrl-d/u scroll",
            "e config",
            "? help  q quit",
        ]
        .to_vec();
        for w in [40u16, 60, 80, 120] {
            let line = hint_line(&f.s, w).to_string();
            assert!(line.chars().count() <= w as usize, "{line:?}");
            let text = line.trim();
            assert!(text.ends_with("? help  q quit"), "{line:?}");
            let head = text.trim_end_matches("? help  q quit").trim_end();
            for chunk in head.split("  ").map(str::trim).filter(|c| !c.is_empty()) {
                assert!(full.contains(&chunk), "partial item {chunk:?} in {line:?}");
            }
        }
        let narrow = hint_line(&f.s, 40).to_string();
        assert!(
            narrow.contains("↑↓/jk move") && !narrow.contains("Tab/"),
            "{narrow:?}"
        );
        assert!(hint_line(&f.s, 200).to_string().contains("p push"));
        // Local images not known yet: no push.
        let f = Fx::new(vec![ecr()]);
        assert!(!hint_line(&f.s, 200).to_string().contains("p push"));
    }

    #[test]
    fn times_labelled_utc() {
        let f = loaded();
        let out = shot(&f);
        assert!(out.contains("Pushed UTC"), "{out}");
        assert!(out.contains("Pulled≈ UTC"), "{out}");
        assert!(
            out.lines().any(|l| l.contains("Images: api-svc · ")
                && l.contains(":")
                && l.contains(" UTC ")),
            "{out}"
        );
        assert!(out.contains("Pushed     2024-06-17 09:40 UTC"), "{out}");
    }

    // ---- style assertions: snapshots only hold text, so these read cell styles ----

    fn buf_of(w: u16, h: u16, s: &State) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render(f, s, RENDER_NOW, &mut ViewState::default()))
            .unwrap();
        term.backend().buffer().clone()
    }

    fn cell_style(buf: &Buffer, x: u16, y: u16) -> Style {
        buf[(x, y)].style()
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    /// (x, y) of the first `needle` in the first row containing `anchor`.
    fn find_row(buf: &Buffer, anchor: &str, needle: &str) -> (u16, u16) {
        let y = (0..buf.area.height)
            .find(|&y| row_text(buf, y).contains(anchor))
            .unwrap_or_else(|| panic!("no row with {anchor:?}"));
        let row = row_text(buf, y);
        let at = row
            .find(needle)
            .unwrap_or_else(|| panic!("no {needle:?} in {row:?}"));
        (row[..at].chars().count() as u16, y)
    }

    fn find(buf: &Buffer, needle: &str) -> (u16, u16) {
        find_row(buf, needle, needle)
    }

    /// The styles of the cells `needle` covers.
    fn styles(buf: &Buffer, (x, y): (u16, u16), needle: &str) -> Vec<Style> {
        (0..needle.chars().count() as u16)
            .map(|i| cell_style(buf, x + i, y))
            .collect()
    }

    fn all_have(buf: &Buffer, at: (u16, u16), needle: &str, m: Modifier) -> bool {
        styles(buf, at, needle)
            .iter()
            .all(|s| s.add_modifier.contains(m))
    }

    fn all_fg(buf: &Buffer, at: (u16, u16), needle: &str, fg: Option<Color>) -> bool {
        styles(buf, at, needle).iter().all(|s| s.fg == fg)
    }

    fn all_bg(buf: &Buffer, at: (u16, u16), needle: &str, bg: Option<Color>) -> bool {
        styles(buf, at, needle).iter().all(|s| s.bg == bg)
    }

    fn none_have(buf: &Buffer, at: (u16, u16), needle: &str, m: Modifier) -> bool {
        styles(buf, at, needle)
            .iter()
            .all(|s| !s.add_modifier.contains(m))
    }

    #[test]
    fn selected_row_is_filled_in_focused_pane() {
        let f = loaded();
        let out = shot(&f);
        assert!(!out.contains("> "), "{out}");
        let buf = buf_of(120, 40, &f.s);
        let img = find(&buf, "0.1.0-dev");
        assert!(all_bg(&buf, img, "0.1.0-dev", theme::selected().bg));
        assert!(all_have(&buf, img, "0.1.0-dev", Modifier::BOLD));
        assert!(!all_fg(&buf, img, "0.1.0-dev", theme::muted().fg));
        let other = find(&buf, "0.0.9");
        assert!(!all_bg(&buf, other, "0.0.9", theme::selected().bg));
        // The unfocused registries pane: the idle selection.
        let reg = find(&buf, "company");
        assert!(all_bg(&buf, reg, "company", theme::selected_idle().bg));
        assert!(none_have(&buf, reg, "company", Modifier::BOLD));
    }

    #[test]
    fn headers_sentence_case() {
        let out = shot(&loaded());
        assert!(!out.contains("TAG") && !out.contains("PUSHED"), "{out}");
    }

    #[test]
    fn headers_use_header_color() {
        let mut f = loaded();
        let buf = buf_of(120, 40, &f.s);
        for h in ["Tag", "Pushed UTC↓", "Size*", "Pulled≈ UTC"] {
            let at = find_row(&buf, "Pushed UTC", h);
            assert!(all_fg(&buf, at, h, theme::header().fg), "{h}");
        }
        // The History tab's header row, sort arrow included in the images header above.
        f.act(Action::Tab(Tab::History));
        let buf = buf_of(120, 40, &f.s);
        let at = find(&buf, "Created (UTC)");
        assert!(all_fg(&buf, at, "Created (UTC)", theme::header().fg));
    }

    #[test]
    fn size_right_aligned() {
        let f = with_images(vec![
            art('a', &["big"], Some(210_000_000)),
            art('b', &["tiny"], Some(999)),
        ]);
        let buf = buf_of(120, 40, &f.s);
        let end = |anchor: &str, n: &str| {
            let (x, y) = find_row(&buf, anchor, n);
            (x + n.chars().count() as u16, y)
        };
        assert_eq!(end("big", "210MB").0, end("tiny", "999B").0);
    }

    #[test]
    fn missing_values_are_muted_dashes() {
        let f = with_images(vec![art('a', &["x"], None)]);
        let buf = buf_of(120, 40, &f.s);
        let dash = find(&buf, "—");
        assert!(all_fg(&buf, dash, "—", theme::muted().fg));
        let never = find(&buf, "never");
        assert!(all_fg(&buf, never, "never", theme::muted().fg));
    }

    #[test]
    fn untagged_and_archived_are_muted() {
        let f = with_images(vec![art('a', &[], Some(1))]);
        let buf = buf_of(120, 40, &f.s);
        let at = find(&buf, "<untagged>");
        assert!(all_fg(&buf, at, "<untagged>", theme::muted().fg));

        let mut a = art('a', &["old"], Some(5_000_000));
        a.status = ArtifactStatus::Archived;
        let f = with_images(vec![a]);
        let out = shot(&f);
        assert!(!out.contains("[archived]"), "{out}");
        let buf = buf_of(120, 40, &f.s);
        let at = find(&buf, "old archived");
        assert!(all_fg(&buf, at, "old archived", theme::muted().fg));
        // The whole row is muted: size and pushed time give up their own colors.
        let size = find_row(&buf, "old archived", "5.0MB");
        assert!(all_fg(&buf, size, "5.0MB", theme::muted().fg));
        let pushed = find_row(&buf, "old archived", "2024-06-17 09:40");
        assert!(all_fg(&buf, pushed, "2024-06-17 09:40", theme::muted().fg));
    }

    #[test]
    fn local_marker_column() {
        let f = loaded();
        let buf = buf_of(120, 40, &f.s);
        let (x, y) = find(&buf, "●");
        assert_eq!(cell_style(&buf, x, y).fg, theme::accent().fg);
        // The row of the image that is not local has a space in the same column.
        assert_eq!(buf[(x, y + 1)].symbol(), " ");
        assert!(row_text(&buf, y + 1).contains("0.0.9"));
        assert_eq!(buf[(x + 2, y)].symbol(), "0", "tag column follows");
    }

    #[test]
    fn registry_marks_coloured() {
        let buf = buf_of(100, 30, &login_marks().s);
        let ok = find_row(&buf, "company", "✓ 11h40m");
        assert!(
            styles(&buf, ok, "✓ 11h40m")
                .iter()
                .all(|s| s.fg == theme::ok().fg)
        );
        let bad = find_row(&buf, "mirror", "!");
        assert_eq!(cell_style(&buf, bad.0, bad.1).fg, theme::bad().fg);
        let run = find_row(&buf, "harbor", "…");
        assert!(all_fg(&buf, run, "…", theme::muted().fg));
    }

    #[test]
    fn status_kinds_render() {
        let last = |f: &Fx| {
            let buf = buf_of(100, 30, &f.s);
            (row_text(&buf, 29), buf)
        };
        let mut f = Fx::new(vec![ecr()]);
        update(
            &mut f.s,
            Input::Event(Event::CopyResult(Err("no display".into()))),
            NOW,
        );
        let (row, buf) = last(&f);
        assert_eq!(row.trim_end(), " ! clipboard unavailable: no display");
        assert_eq!(cell_style(&buf, 1, 29).fg, theme::bad().fg);
        update(&mut f.s, Input::Event(Event::CopyResult(Ok(()))), NOW);
        let (row, buf) = last(&f);
        assert_eq!(row.trim_end(), " ✓ copied to clipboard");
        assert_eq!(cell_style(&buf, 1, 29).fg, theme::ok().fg);
        f.act(Action::Login);
        let (row, _) = last(&f);
        assert_eq!(row.trim_end(), " logging in to company…");
    }

    #[test]
    fn failure_shown_in_block() {
        let mut f = Fx::new(vec![ecr()]);
        f.load(CacheKey::Repos(rid("company")), repos());
        f.act(Action::Enter);
        let inv = CacheKey::Inventory(rid("company"), rp("api-svc"));
        let req = f.pending[&inv];
        update(
            &mut f.s,
            Input::Event(Event::Loaded {
                key: inv,
                req,
                result: Err(RegistryError::Provider("boom".into())),
            }),
            NOW,
        );
        let buf = buf_of(120, 40, &f.s);
        let msg = "Couldn't load images of api-svc: company: boom";
        let (x, y) = find(&buf, msg);
        assert!(
            styles(&buf, (x, y), msg)
                .iter()
                .all(|s| s.fg == theme::bad().fg)
        );
        let retry = find(&buf, "r retry");
        assert_eq!(retry.1, y + 1);
        assert!(all_fg(&buf, retry, "r retry", theme::muted().fg));
        assert!(row_text(&buf, y).contains('│'), "inside the block");
    }

    #[test]
    fn failure_message_with_retry_hint_is_not_doubled() {
        let mut f = Fx::new(vec![ecr()]);
        f.load(CacheKey::Repos(rid("company")), repos());
        f.act(Action::Enter);
        f.fail(
            CacheKey::Inventory(rid("company"), rp("api-svc")),
            RegistryError::Network("x".into()),
        );
        let out = render_to(200, 40, &f.s);
        assert!(
            out.contains("Couldn't load images of api-svc: network error: x — press r to retry"),
            "{out}"
        );
        assert!(!out.contains("r retry"), "{out}");
        assert_eq!(out.matches("press r to retry").count(), 2, "block + status");
    }

    #[test]
    fn wizard_repo_failure_says_enter_retry() {
        let down = || RegistryError::Network("down".into());
        let mut f = Fx::new(vec![ecr()]);
        f.load(
            CacheKey::LocalImages,
            CacheValue::LocalImages(vec![local(IMG1, &[], None, 1)]),
        );
        f.fail(CacheKey::Repos(rid("company")), down());
        f.act(Action::PushStart);
        f.act(Action::Enter);
        f.fail(CacheKey::Repos(rid("company")), down());
        let out = render_to(100, 30, &f.s);
        assert!(
            out.contains("Couldn't load repositories: network error: down"),
            "{out}"
        );
        // The modal spans x = 8..92 here; the panes behind it keep their own `r` hint.
        let modal: String = out
            .lines()
            .map(|l| l.chars().skip(8).take(84).collect::<String>() + "\n")
            .collect();
        assert!(modal.contains("Enter retry"), "{out}");
        assert!(
            !modal.contains("press r") && !modal.contains("│r retry"),
            "{out}"
        );
    }

    #[test]
    fn overview_unknown_size_is_muted_dash() {
        // Manifest not loaded yet, no reported size.
        let f = with_images(vec![art('a', &["0.1.0-dev"], None)]);
        let buf = buf_of(120, 40, &f.s);
        let at = find_row(&buf, "│Size*", "—");
        assert!(all_fg(&buf, at, "—", theme::muted().fg));
    }

    #[test]
    fn hint_keys_bold_labels_muted() {
        let buf = buf_of(120, 40, &loaded().s);
        let (x, y) = find(&buf, "r refresh");
        assert!(buf[(x, y)].style().add_modifier.contains(Modifier::BOLD));
        assert!(all_fg(&buf, (x, y), "r", theme::key().fg));
        assert!(all_fg(&buf, (x + 2, y), "refresh", theme::muted().fg));
    }

    #[test]
    fn detail_labels_styled_groups_spaced() {
        let buf = buf_of(120, 40, &loaded().s);
        let (_, y0) = find(&buf, "│Ref ");
        assert!(all_fg(&buf, (1, y0), "Ref", theme::label().fg));
        assert!(all_fg(&buf, (12, y0), "123456789012", theme::base().fg));
        let first_words: Vec<String> = (y0..y0 + 15)
            .map(|y| {
                let r = row_text(&buf, y);
                let inner = r.trim_matches('│');
                inner.split_whitespace().next().unwrap_or("").to_owned()
            })
            .collect();
        assert_eq!(
            first_words,
            [
                "Ref",
                "Tags",
                "Digest",
                "",
                "Status",
                "Media",
                "Size*",
                "Platform",
                "Created",
                "",
                "Pushed",
                "Pulled≈",
                "",
                "Local",
                "Attached"
            ]
        );
    }

    #[test]
    fn tab_bar_active_underlined() {
        let f = loaded();
        let out = shot(&f);
        assert!(
            out.contains("1 Overview  2 Layers  3 Config  4 History  5 Raw"),
            "{out}"
        );
        assert!(!out.contains('['), "{out}");
        let buf = buf_of(120, 40, &f.s);
        let on = find(&buf, "1 Overview");
        assert!(all_have(
            &buf,
            on,
            "1 Overview",
            Modifier::BOLD | Modifier::UNDERLINED
        ));
        let off = find(&buf, "2 Layers");
        assert!(all_fg(&buf, on, "1 Overview", theme::title().fg));
        assert!(all_fg(&buf, off, "2 Layers", theme::muted().fg));
        assert!(none_have(&buf, off, "2 Layers", Modifier::UNDERLINED));
    }

    #[test]
    fn base_background_painted() {
        // A cell nothing draws on: inside the detail pane, far from any text.
        let buf = buf_of(120, 40, &Fx::new(vec![ecr()]).s);
        assert_eq!(cell_style(&buf, 100, 30).bg, theme::base().bg);
        assert_eq!(cell_style(&buf, 100, 30).bg, Some(theme::bg()));
    }

    #[test]
    fn labels_use_label_color() {
        let buf = buf_of(120, 40, &loaded().s);
        for (anchor, label) in [
            ("│Ref ", "Ref"),
            ("│Digest", "Digest"),
            ("│Status", "Status"),
        ] {
            let (x, y) = find(&buf, anchor);
            assert!(
                all_fg(&buf, (x + 1, y), label, theme::label().fg),
                "{label}"
            );
        }
    }

    #[test]
    fn sizes_use_number_color() {
        let buf = buf_of(120, 40, &loaded().s);
        let table = find_row(&buf, "0.1.0-dev", "210MB");
        assert!(all_fg(&buf, table, "210MB", theme::number().fg));
        let overview = find_row(&buf, "│Size*", "210MB");
        assert!(all_fg(&buf, overview, "210MB", theme::number().fg));
        // Repo image count.
        let repo = find_row(&buf, "│api-svc", "2");
        assert!(all_fg(&buf, repo, "2", theme::number().fg));
        // Layers tab.
        let mut f = loaded();
        f.act(Action::Tab(Tab::Layers));
        let buf = buf_of(120, 40, &f.s);
        let (x, y) = find_row(&buf, "sha256:", "B");
        assert_eq!(cell_style(&buf, x, y).fg, theme::number().fg);
    }

    #[test]
    fn times_use_time_color() {
        let buf = buf_of(120, 40, &loaded().s);
        let pushed = find_row(&buf, "0.1.0-dev", "2024-06-17 09:40");
        assert!(all_fg(&buf, pushed, "2024-06-17 09:40", theme::time().fg));
        let pulled = find_row(&buf, "0.0.9", "2024-06-10 08:00");
        assert!(all_fg(&buf, pulled, "2024-06-10 08:00", theme::time().fg));
        let detail = find_row(&buf, "│Pushed", "2024-06-17 09:40 UTC");
        assert!(all_fg(
            &buf,
            detail,
            "2024-06-17 09:40 UTC",
            theme::time().fg
        ));
        let created = find_row(&buf, "│Created", "2024-06-17 09:38 UTC");
        assert!(all_fg(
            &buf,
            created,
            "2024-06-17 09:38 UTC",
            theme::time().fg
        ));
        // Pane-title fetched time.
        let title = find_row(&buf, "Images: api-svc", "UTC");
        assert!(all_fg(&buf, title, "UTC", theme::time().fg));
        // `never` stays muted.
        let never = find_row(&buf, "0.1.0-dev", "never");
        assert!(all_fg(&buf, never, "never", theme::muted().fg));
    }

    #[test]
    fn bars_use_bar_background() {
        let buf = buf_of(120, 40, &loaded().s);
        for y in [38, 39] {
            for x in [0, 60, 119] {
                assert_eq!(cell_style(&buf, x, y).bg, theme::bar().bg, "({x},{y})");
            }
        }
        assert_eq!(cell_style(&buf, 60, 37).bg, theme::base().bg);
    }

    // ---- delete ----

    use crate::app::delete::{CHANGED, DelStage, DeleteModal};
    use crate::app::planner::{TAGGED_ONLY_BANNER, UNREADABLE_INDEX};
    use crate::domain::{
        DeleteOp, DeletePlan, DeleteReport, DepSnapshot, ExcludeReason, OpOutcome, SideEffect,
    };

    /// `images` of ecr `company/api-svc`, focus on the Images pane, `reg` as the registry.
    fn images_in(reg: RegistryView, images: Vec<ArtifactSummary>) -> Fx {
        let mut f = Fx::new(vec![reg]);
        f.load(CacheKey::LocalImages, CacheValue::LocalImages(vec![]));
        f.load(CacheKey::Repos(rid("company")), repos());
        f.act(Action::Enter);
        f.load(
            CacheKey::Inventory(rid("company"), rp("api-svc")),
            CacheValue::Inventory(images),
        );
        f.act(Action::Enter);
        f
    }

    /// Adds the tags matching `pattern` to the marks through the glob prompt.
    fn mark_glob(f: &mut Fx, pattern: &str) {
        f.act(Action::MarkGlob);
        while f.s.glob().is_some_and(|g| !g.is_empty()) {
            f.act(Action::FilterBackspace);
        }
        type_text(f, pattern);
        f.act(Action::FilterApply);
    }

    fn marked() -> Fx {
        let mut f = with_images(vec![
            art('a', &["0.1.0", "0.1.1"], Some(1_000)),
            art('b', &["0.2.0", "0.2.1"], Some(2_000)),
            art('c', &["0.3.0"], Some(3_000)),
        ]);
        mark_glob(&mut f, "0.1.*");
        mark_glob(&mut f, "0.2.0");
        f
    }

    #[test]
    fn images_mark_column() {
        let f = marked();
        let buf = buf_of(100, 30, &f.s);
        let full = find_row(&buf, "0.1.0, 0.1.1", "✗");
        assert_eq!(full.0, 53, "first column of the images pane");
        assert!(all_fg(&buf, full, "✗", theme::bad().fg));
        let part = find_row(&buf, "0.2.0, 0.2.1", "~");
        assert_eq!(part.0, 53, "first column of the images pane");
        assert!(all_fg(&buf, part, "~", theme::warn().fg));
        let unmarked = row_text(&buf, find_row(&buf, "0.3.0", "0.3.0").1);
        assert!(
            !unmarked.contains('✗') && !unmarked.contains('~'),
            "{unmarked}"
        );
        let title = row_text(&buf, 0);
        assert!(title.contains("· 3 marked"), "{title}");
    }

    #[test]
    fn glob_prompt() {
        let mut f = with_images(vec![
            art('a', &["0.1.0"], Some(1)),
            art('b', &["0.2.0"], Some(1)),
        ]);
        f.act(Action::MarkGlob);
        f.act(Action::FilterBackspace);
        type_text(&mut f, "0.1.*");
        let out = render_to(100, 30, &f.s);
        insta::assert_snapshot!(out);
        let buf = buf_of(100, 30, &f.s);
        let (x, y) = find(&buf, "matches 1 tag");
        assert!(all_fg(&buf, (x + 8, y), "1", theme::number().fg));
        let hint = find_row(&buf, "* any run", "* any run");
        assert!(all_fg(&buf, hint, "* any run", theme::muted().fg));
        f.act(Action::FilterChar('x'));
        assert!(render_to(100, 30, &f.s).contains("matches 0 tags"));
        f.act(Action::FilterCancel);
        f.act(Action::MarkGlob);
        assert!(
            render_to(100, 30, &f.s).contains("matches 2 tags"),
            "prefilled `*`"
        );
    }

    fn dmodal(stage: DelStage) -> DeleteModal {
        DeleteModal {
            registry: rid("company"),
            repo: rp("app"),
            op: OpId(1),
            stage,
            whole: false,
            typed: String::new(),
            snapshot: None,
            plan: None,
            changed: false,
            scroll: 0,
            hidden: false,
        }
    }

    /// A plan with every section; ops `a` tag `1`, manifests `b c` then `d`.
    fn full_plan() -> (DeletePlan, DepSnapshot) {
        let plan = DeletePlan {
            remove_tags: vec![DeleteOp::RemoveTag {
                tag: "1".into(),
                expected: dg('a'),
            }],
            layers: vec![vec![dg('b'), dg('c')], vec![dg('d')]],
            collateral: vec![(dg('e'), vec!["y".into(), "x".into()])],
            excluded: vec![
                (dg('1'), ExcludeReason::DependencyUnknown),
                (dg('2'), ExcludeReason::ChildOfKeptIndex(dg('3'))),
                (dg('4'), ExcludeReason::AttachedToKeptManifest(dg('5'))),
                (dg('6'), ExcludeReason::NeedsWholeManifest),
            ],
            side_effects: vec![
                SideEffect::UnreferencedChild {
                    child: dg('c'),
                    index: dg('b'),
                },
                SideEffect::OrphanedReferrer {
                    referrer: dg('7'),
                    subject: dg('d'),
                },
                SideEffect::FallbackIndexStale {
                    tag: "sha256-dddd".into(),
                },
                SideEffect::ReferrersUnknown { digest: dg('d') },
            ],
            banner: None,
        };
        let snap = DepSnapshot {
            inventory: vec![
                art('b', &["2", "3"], Some(100_000_000)),
                art('c', &[], Some(10_000_000)),
                art('d', &["4"], Some(100_000_000)),
            ],
            ..DepSnapshot::default()
        };
        (plan, snap)
    }

    fn preview_modal(stage: DelStage) -> DeleteModal {
        let (plan, snap) = full_plan();
        DeleteModal {
            snapshot: Some(snap),
            plan: Some(plan),
            ..dmodal(stage)
        }
    }

    fn with_modal(m: DeleteModal) -> Fx {
        let mut f = loaded();
        f.s.set_delete_modal(m);
        f
    }

    #[test]
    fn delete_preview_all_sections() {
        let f = with_modal(preview_modal(DelStage::Preview));
        let out = render_to(100, 40, &f.s);
        insta::assert_snapshot!(out);
        let buf = buf_of(100, 40, &f.s);
        for h in [
            "Tags removed (1)",
            "Manifests deleted (3)",
            "Collateral tags (2)",
            "Excluded (4)",
            "Side effects (4)",
        ] {
            let at = find(&buf, h);
            assert!(all_fg(&buf, at, h, theme::title().fg), "{h}");
        }
        assert!(out.contains("Totals  4 tags · 3 manifests · 210MB reported"));
        assert!(out.contains("eeeeeeeeeeee  x, y"), "tag names, sorted");
        let at = find(&buf, "x, y");
        assert!(all_fg(&buf, at, "x, y", theme::number().fg));
        assert!(out.contains("w whole-manifest: off   Enter confirm   Esc cancel"));
        assert!(out.contains(" w whole-manifest  ↑↓ scroll  Enter confirm  Esc cancel"));
    }

    #[test]
    fn delete_preview_scrolls_and_clamps() {
        let mut m = preview_modal(DelStage::Preview);
        let f = with_modal(m.clone());
        assert!(render_to(100, 24, &f.s).contains("Tags removed"));
        m.scroll = 3;
        let f = with_modal(m.clone());
        let out = render_to(100, 24, &f.s);
        assert!(!out.contains("Tags removed (1)"), "{out}");
        // Past the end: stops at the last row.
        m.scroll = 500;
        let out = render_to(100, 24, &with_modal(m).s);
        assert!(out.contains("Totals  4 tags"), "{out}");
    }

    #[test]
    fn delete_preview_banners() {
        let mut m = preview_modal(DelStage::Preview);
        m.plan.as_mut().unwrap().banner = Some(TAGGED_ONLY_BANNER.into());
        m.changed = true;
        let f = with_modal(m);
        let out = render_to(100, 40, &f.s);
        insta::assert_snapshot!(out);
        let buf = buf_of(100, 40, &f.s);
        for b in [TAGGED_ONLY_BANNER, CHANGED] {
            let at = find(&buf, b);
            assert!(all_fg(&buf, at, b, theme::warn().fg), "{b}");
        }
        assert!(out.find(CHANGED) < out.find("Tags removed"));
    }

    #[test]
    fn delete_preview_blocked() {
        let f = with_modal(dmodal(DelStage::Blocked(UNREADABLE_INDEX.into())));
        let out = render_to(100, 30, &f.s);
        insta::assert_snapshot!(out);
        let buf = buf_of(100, 30, &f.s);
        let at = find(&buf, "repository contains");
        assert!(all_fg(&buf, at, "repository contains", theme::bad().fg));
        assert!(row_text(&buf, 28).contains("Esc close"));
    }

    #[test]
    fn delete_snapshot_and_revalidate_bodies() {
        let f = with_modal(dmodal(DelStage::Snapshot));
        let out = render_to(100, 30, &f.s);
        assert!(out.contains("taking dependency snapshot…") && out.contains("Delete from app"));
        let f = with_modal(preview_modal(DelStage::Revalidate));
        let out = render_to(100, 30, &f.s);
        assert!(out.contains("revalidating…"), "{out}");
        assert!(row_text(&buf_of(100, 30, &f.s), 28).contains("Esc cancel"));
    }

    #[test]
    fn delete_confirm_typing() {
        let mut m = preview_modal(DelStage::Confirm);
        m.typed = "ap".into();
        let f = with_modal(m.clone());
        let out = render_to(100, 40, &f.s);
        insta::assert_snapshot!(out);
        assert!(out.contains("Type app to confirm: ap▏"));
        assert!(
            row_text(&buf_of(100, 40, &f.s), 38)
                .contains("type the repo name  Enter delete  Esc back")
        );

        m.typed = "app".into();
        let f = with_modal(m);
        let buf = buf_of(100, 40, &f.s);
        let at = find(&buf, "app▏");
        assert!(all_fg(&buf, at, "app", theme::ok().fg));
        let hint = row_text(&buf, 38);
        assert!(
            hint.contains("Enter delete  Esc back") && !hint.contains("type the"),
            "{hint}"
        );
    }

    #[test]
    fn delete_running() {
        let f = with_modal(preview_modal(DelStage::Running));
        let out = render_to(100, 30, &f.s);
        insta::assert_snapshot!(out);
        assert!(out.contains("Deleting from app… 4 operations"));
        assert!(row_text(&buf_of(100, 30, &f.s), 28).contains("Enter/Esc hide"));
    }

    #[test]
    fn pull_hint_and_status_line() {
        let mut f = loaded();
        assert!(hint_line(&f.s, 200).to_string().contains("P pull"));
        f.act(Action::PullStart);
        assert_eq!(
            row_text(&buf_of(100, 30, &f.s), 28).trim(),
            "Enter pull  Esc cancel"
        );
        f.act(Action::Enter);
        assert_eq!(
            row_text(&buf_of(100, 30, &f.s), 28).trim(),
            "Enter hide  Esc cancel"
        );
        f.act(Action::Enter);
        let buf = buf_of(100, 30, &f.s);
        assert_eq!(
            row_text(&buf, 29).trim(),
            "pulling api-svc:0.1.0-dev 0% 0.0 MB/s — press P to show"
        );
        assert!(row_text(&buf, 28).contains("P show pull"));
        f.act(Action::PrevPane);
        assert!(hint_line(&f.s, 200).to_string().contains("P show pull"));
    }

    // ---- copy ----

    /// `C` on `api-svc:0.1.0-dev`, registry `company` picked: stage `PickRepo`.
    fn copy_fixture() -> Fx {
        let mut f = with_images(vec![art('a', &["0.1.0-dev"], Some(10_000_000))]);
        f.act(Action::CopyStart);
        f.act(Action::Enter);
        f
    }

    /// `copy_fixture` to `ttl`:`tag`, whose inventory holds `ttl`: stage `Confirm`.
    fn copy_confirm(tag: &str, ttl: Vec<ArtifactSummary>) -> Fx {
        let mut f = copy_fixture();
        type_text(&mut f, "ttl");
        f.act(Action::FilterApply);
        for _ in 0.."0.1.0-dev".len() {
            f.act(Action::FilterBackspace);
        }
        type_text(&mut f, tag);
        f.load(
            CacheKey::Inventory(rid("company"), rp("ttl")),
            CacheValue::Inventory(ttl),
        );
        f.act(Action::FilterApply);
        f
    }

    #[test]
    fn copy_pick_repo_new_name() {
        let mut f = copy_fixture();
        type_text(&mut f, "new/x");
        let out = render_to(100, 30, &f.s);
        assert!(
            out.contains("not in the repository list — ECR rejects"),
            "{out}"
        );
        assert!(hint_line(&f.s, 200).to_string().contains("Esc back"));
        insta::assert_snapshot!(out);
    }

    #[test]
    fn copy_confirm_warnings() {
        let mut f = copy_confirm("0.1.0-dev", vec![art('b', &["0.1.0-dev"], None)]);
        f.load(
            CacheKey::TagPolicy(rid("company"), rp("ttl"), "0.1.0-dev".into()),
            CacheValue::TagPolicy(crate::domain::TagPolicy::Mutable),
        );
        let out = render_to(100, 30, &f.s);
        assert!(
            out.contains("tag exists on bbbbbbbbbbbb and will be moved"),
            "{out}"
        );
        insta::assert_snapshot!(out);
    }

    #[test]
    fn copy_progress() {
        use crate::domain::ItemStep as S;
        let mut f = copy_confirm("1.0", vec![]);
        let s = |x: i64| RENDER_NOW - time::Duration::seconds(4) + time::Duration::seconds(x);
        update(&mut f.s, Input::Action(Action::Enter), s(0));
        let step = |c: char, step| Event::CopyProgress {
            op: OpId(1),
            step: crate::domain::TransferStep {
                id: c.to_string().repeat(12),
                step,
            },
        };
        let sizes = [3_000_000, 2_000_000, 1_500_000, 1_000_000, 500_000];
        for (c, n) in ('1'..='5').zip(sizes) {
            pull_ev(&mut f, step(c, S::Waiting { total: Some(n) }), s(0));
        }
        for (c, st) in [
            ('3', S::Mounted),
            ('4', S::Exists),
            ('5', S::Done),
            (
                '2',
                S::Bytes {
                    current: 1_500_000,
                    total: 2_000_000,
                },
            ),
        ] {
            pull_ev(&mut f, step(c, st), s(4));
        }
        let out = render_to(100, 30, &f.s);
        for want in [
            "· waiting",
            "↑ uploading",
            "⇄ mounted",
            "✓ exists",
            "✓ done",
        ] {
            assert!(out.contains(want), "{want}\n{out}");
        }
        assert!(
            hint_line(&f.s, 200)
                .to_string()
                .contains("Enter hide  Esc cancel")
        );
        insta::assert_snapshot!(out);

        // Hidden: the hint bar offers it back.
        f.act(Action::Enter);
        assert!(hint_line(&f.s, 200).to_string().contains("C show copy"));
    }

    #[test]
    fn copy_progress_placeholder() {
        // Another repo: blobs may follow once the source manifest is read.
        let mut f = copy_confirm("1.0", vec![]);
        f.act(Action::Enter);
        let out = render_to(100, 30, &f.s);
        assert!(
            out.contains("reading the source manifest…") && !out.contains("tagging"),
            "{out}"
        );
        // Same registry and repo: a retag copies no blobs.
        let mut f = copy_fixture();
        f.act(Action::FilterApply);
        type_text(&mut f, "-b");
        f.load(
            CacheKey::Inventory(rid("company"), rp("api-svc")),
            CacheValue::Inventory(vec![art('a', &["0.1.0-dev"], None)]),
        );
        f.act(Action::FilterApply);
        f.act(Action::Enter);
        let out = render_to(100, 30, &f.s);
        assert!(out.contains("tagging (no blobs to copy)…"), "{out}");
    }

    // ---- export ----

    #[test]
    fn export_details_all() {
        let mut f = with_images(vec![art('a', &["0.2.0"], Some(210_000_000))]);
        f.load(
            key_man('a'),
            manifest(
                r#"{"manifests":[]}"#,
                ManifestKind::Index {
                    manifests: vec![
                        desc('c', 500, Some(("linux", "amd64"))),
                        desc('d', 500, Some(("linux", "arm64"))),
                    ],
                },
            ),
        );
        f.act(Action::ExportStart);
        let out = render_to(100, 30, &f.s);
        assert!(out.contains("Platform  linux/amd64"), "{out}");
        assert!(
            hint_line(&f.s, 200)
                .to_string()
                .contains("a all platforms  Enter next  Esc cancel")
        );
        f.act(Action::ToggleAll);
        let out = render_to(100, 30, &f.s);
        for want in [
            "Platform  all platforms",
            "Size      —",
            "Path      ~/Downloads/api-svc_0.2.0.tar",
            crate::app::export::ALL_NOTE,
        ] {
            assert!(out.contains(want), "{want}\n{out}");
        }
        insta::assert_snapshot!(out);
    }

    #[test]
    fn export_path() {
        let mut f = pull_fixture();
        f.act(Action::ExportStart);
        let out = render_to(100, 30, &f.s);
        assert!(out.contains("Platform  linux/amd64"), "{out}");
        assert!(out.contains("Size      10.0MB compressed"), "{out}");
        assert!(!out.contains("all platforms"), "single image: {out}");
        f.act(Action::Enter);
        for c in "x".chars() {
            f.act(Action::FilterChar(c));
        }
        let out = render_to(100, 30, &f.s);
        assert!(
            out.contains("Path      ~/Downloads/api-svc_0.1.0-dev.tarx▏"),
            "{out}"
        );
        assert!(
            hint_line(&f.s, 200)
                .to_string()
                .contains("type a path  Enter export  Esc back")
        );
        insta::assert_snapshot!(out);

        // The target exists: the y/n prompt.
        let fx = update(&mut f.s, Input::Action(Action::FilterApply), RENDER_NOW);
        f.absorb(fx);
        pull_ev(
            &mut f,
            Event::ExportDone {
                op: OpId(1),
                outcome: crate::app::transfer::TransferOutcome::Exists,
            },
            RENDER_NOW,
        );
        let out = render_to(100, 30, &f.s);
        assert!(
            out.contains("~/Downloads/api-svc_0.1.0-dev.tarx exists — overwrite? y/n"),
            "{out}"
        );
        assert!(hint_line(&f.s, 200).to_string().contains("y yes  n/Esc no"));
    }

    #[test]
    fn export_running() {
        use crate::domain::ItemStep as S;
        let mut f = pull_fixture();
        let s = |x: i64| RENDER_NOW - time::Duration::seconds(4) + time::Duration::seconds(x);
        f.act(Action::ExportStart);
        f.act(Action::Enter);
        update(&mut f.s, Input::Action(Action::FilterApply), s(0));
        let step = |c: char, step| Event::ExportProgress {
            op: OpId(1),
            step: crate::domain::TransferStep {
                id: c.to_string().repeat(12),
                step,
            },
        };
        for (c, n) in [('c', 7_000), ('1', 3_000_000), ('2', 2_000_000)] {
            pull_ev(&mut f, step(c, S::Waiting { total: Some(n) }), s(0));
        }
        pull_ev(&mut f, step('c', S::Done), s(1));
        let half = S::Bytes {
            current: 1_500_000,
            total: 3_000_000,
        };
        pull_ev(&mut f, step('1', half), s(4));
        let out = render_to(100, 30, &f.s);
        for want in ["Downloading", "↓ downloading", "✓ done", "· waiting"] {
            assert!(out.contains(want), "{want}\n{out}");
        }
        assert!(
            hint_line(&f.s, 200)
                .to_string()
                .contains("Enter hide  Esc cancel")
        );
        insta::assert_snapshot!(out);

        // Hidden: the hint bar offers it back.
        f.act(Action::Enter);
        assert!(hint_line(&f.s, 200).to_string().contains("X show export"));
    }

    // ---- pull ----

    /// Image `a` (10.0MB in eight layers `1`..`8`), fully loaded and present locally.
    fn pull_fixture() -> Fx {
        let mut f = with_images(vec![art('a', &["0.1.0-dev"], Some(10_000_000))]);
        let sizes = [
            3_000_000, 2_000_000, 1_500_000, 1_000_000, 1_000_000, 800_000, 500_000, 200_000,
        ];
        f.load(
            key_man('a'),
            manifest(
                "{}",
                ManifestKind::Image {
                    config: desc('c', 7_000, None),
                    layers: ('1'..='8')
                        .zip(sizes)
                        .map(|(c, s)| desc(c, s, None))
                        .collect(),
                },
            ),
        );
        f.load(
            CacheKey::Referrers(rid("company"), rp("api-svc"), dg('a')),
            CacheValue::Referrers(Some(vec![])),
        );
        f.load(key_cfg('a'), config());
        f
    }

    fn pull_ev(f: &mut Fx, e: Event, now: OffsetDateTime) {
        let fx = update(&mut f.s, Input::Event(e), now);
        f.absorb(fx);
    }

    fn pull_step(c: char, step: crate::domain::ItemStep) -> Event {
        Event::PullProgress {
            op: OpId(1),
            step: crate::domain::TransferStep {
                id: c.to_string().repeat(12),
                step,
            },
        }
    }

    #[test]
    fn pull_details() {
        let mut f = pull_fixture();
        f.act(Action::PullStart);
        insta::assert_snapshot!(render_to(100, 30, &f.s));
    }

    #[test]
    fn pull_details_fields() {
        let mut f = loaded();
        f.act(Action::PullStart);
        let out = render_to(100, 30, &f.s);
        for want in [
            &format!("Ref       {HOST}/api-svc:0.1.0-dev"),
            "Digest    sha256:aaaaaaaaaaaa…   Platform linux/amd64",
            "Size      210MB compressed · 2 layers   Created 2024-06-17 09:38 UTC",
            "Local     ✓ present (88d90c6be1e0)",
            "already local — pulling again checks for changes",
        ] {
            assert!(out.contains(want), "{want}\n{out}");
        }
        let mut f = with_images_local(
            vec![art('a', &["0.1.0-dev"], Some(210_000_000))],
            format!("{HOST}/api-svc@{}", dg('f')),
        );
        f.act(Action::PullStart);
        let out = render_to(100, 30, &f.s);
        assert!(out.contains("Local     not present"), "{out}");
        assert!(!out.contains("already local"), "{out}");
        // The manifest is still loading.
        assert!(out.contains("Platform loading…"), "{out}");
        assert!(out.contains("Size      loading…"), "{out}");
    }

    #[test]
    fn pull_running() {
        use crate::domain::ItemStep as S;
        let mut f = pull_fixture();
        let s = |x: i64| RENDER_NOW - time::Duration::seconds(4) + time::Duration::seconds(x);
        f.act(Action::PullStart);
        update(&mut f.s, Input::Action(Action::Enter), s(0));
        pull_ev(
            &mut f,
            pull_step(
                '1',
                S::Bytes {
                    current: 100_000,
                    total: 3_000_000,
                },
            ),
            s(2),
        );
        for (c, st) in [
            ('1', S::Done),
            (
                '2',
                S::Bytes {
                    current: 1_500_000,
                    total: 2_000_000,
                },
            ),
            (
                '3',
                S::Bytes {
                    current: 500_000,
                    total: 1_500_000,
                },
            ),
            ('6', S::Extracting),
            ('7', S::Done),
            ('8', S::Exists),
        ] {
            pull_ev(&mut f, pull_step(c, st), s(4));
        }
        let out = render_to(100, 30, &f.s);
        for want in [
            "6.5MB / 10.0MB  65%",
            "3.1 MB/s · ETA 1s · elapsed 4s",
            "…2 more",
        ] {
            assert!(out.contains(want), "{want}\n{out}");
        }
        insta::assert_snapshot!(out);
    }

    #[test]
    fn pull_result_ok() {
        use crate::domain::ItemStep as S;
        let mut f = pull_fixture();
        let s = |x: f64| RENDER_NOW - time::Duration::seconds(10) + time::Duration::seconds_f64(x);
        f.act(Action::PullStart);
        update(&mut f.s, Input::Action(Action::Enter), s(0.0));
        for c in '1'..='8' {
            pull_ev(&mut f, pull_step(c, S::Done), s(3.0));
        }
        pull_ev(
            &mut f,
            Event::PullDone {
                op: OpId(1),
                outcome: crate::app::transfer::TransferOutcome::Done,
            },
            s(4.0),
        );
        assert!(render_to(100, 30, &f.s).contains("Local     checking…"));
        f.load(
            CacheKey::LocalImages,
            CacheValue::LocalImages(
                vec![local(IMG1, &[], None, 1)]
                    .into_iter()
                    .map(|mut l| {
                        l.repo_digests = vec![format!("{HOST}/api-svc@{}", dg('a'))];
                        l
                    })
                    .collect(),
            ),
        );
        let out = render_to(100, 30, &f.s);
        assert!(
            out.contains("✓ pulled · 10.0MB in 4.0s (avg 2.5 MB/s) · image 88d90c6be1e0"),
            "{out}"
        );
        assert_eq!(row_text(&buf_of(100, 30, &f.s), 28).trim(), "Enter close");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn status_line_while_delete_hidden() {
        let mut m = preview_modal(DelStage::Running);
        m.hidden = true;
        let f = with_modal(m);
        let out = render_to(100, 30, &f.s);
        assert!(
            !out.contains("Deleting from") && !out.contains("Delete from"),
            "{out}"
        );
        let buf = buf_of(100, 30, &f.s);
        assert!(row_text(&buf, 28).contains("? help  q quit"));
        assert_eq!(
            row_text(&buf, 29).trim(),
            "deleting in app… — press d to show"
        );
    }

    fn report() -> DeleteReport {
        let tag = |t: &str, c| DeleteOp::RemoveTag {
            tag: t.into(),
            expected: dg(c),
        };
        let man = |c| DeleteOp::DeleteManifest { digest: dg(c) };
        DeleteReport {
            results: vec![
                (tag("2", 'a'), OpOutcome::Done),
                (man('b'), OpOutcome::Done),
                (
                    man('c'),
                    OpOutcome::Failed("ImageReferencedByManifestList: still referenced".into()),
                ),
                (man('d'), OpOutcome::Skipped("parent not deleted".into())),
                (
                    tag("1", 'e'),
                    OpOutcome::Uncertain("network error: timed out".into()),
                ),
                (man('f'), OpOutcome::NotAttempted),
            ],
            aborted_unsupported: false,
        }
    }

    fn report_modal(report: DeleteReport, stopped: Option<RegistryError>, refreshed: bool) -> Fx {
        with_modal(dmodal(DelStage::Report {
            report,
            stopped,
            refreshed,
        }))
    }

    #[test]
    fn delete_report() {
        let stopped = Some(RegistryError::Network("connection reset".into()));
        let f = report_modal(report(), stopped, true);
        let out = render_to(100, 30, &f.s);
        insta::assert_snapshot!(out);
        for want in [
            " Delete result ",
            "2 done  1 failed  1 skipped  1 uncertain  1 not attempted",
            "manifest cccccccccccc  failed: ImageReferencedByManifestList: still referenced",
            "manifest dddddddddddd  skipped: parent not deleted",
            "tag 1 (eeeeeeeeeeee)  uncertain: network error: timed out — tag gone",
            "manifest ffffffffffff  not attempted",
            "run stopped: network",
            "Enter close",
        ] {
            assert!(out.contains(want), "{want}\n{out}");
        }
        assert!(!out.contains("press r"), "{out}");
        // The stop reason is its kind only: no error text, no retry hint.
        for (e, want) in [
            (
                RegistryError::AwsCredentials("expired".into()),
                "credentials",
            ),
            (RegistryError::Unauthorized, "credentials"),
            (RegistryError::Timeout("x".into()), "timeout"),
            (RegistryError::Internal("x".into()), "internal"),
        ] {
            let out = render_to(100, 30, &report_modal(report(), Some(e), false).s);
            let line = out.lines().find(|l| l.contains("run stopped")).unwrap();
            assert_eq!(
                line.split('│').nth(2).unwrap().trim(),
                format!("run stopped: {want}")
            );
        }
        // The observed state is only known once the inventory was refetched.
        let f = report_modal(report(), None, false);
        let out = render_to(100, 30, &f.s);
        assert!(out.contains("uncertain: network error: timed out") && !out.contains("tag gone"));
        assert!(!out.contains("run stopped"));

        let mut r = report();
        r.aborted_unsupported = true;
        let out = render_to(100, 30, &report_modal(r, None, false).s);
        assert!(out.contains("deletion disabled on company"), "{out}");
    }

    #[test]
    fn status_line_while_report_hidden() {
        let mut m = dmodal(DelStage::Report {
            report: report(),
            stopped: None,
            refreshed: false,
        });
        m.hidden = true;
        let mut f = with_modal(m);
        let want = "delete finished: 2 done, 1 failed, 1 skipped, 1 uncertain, 1 not attempted — press d for the report";
        // Over another message, and with the report's own kind.
        update(&mut f.s, Input::Event(Event::CopyResult(Ok(()))), NOW);
        let buf = buf_of(140, 30, &f.s);
        assert_eq!(row_text(&buf, 29).trim(), want);
    }

    #[test]
    fn hint_bar_delete_keys() {
        let imgs = || vec![art('a', &["1"], Some(1)), art('b', &[], Some(1))];
        let has = |f: &Fx| hint_line(&f.s, 300).to_string();
        let mut f = images_in(ecr(), imgs());
        let h = has(&f);
        for k in ["space mark", "u untagged", "a glob"] {
            assert!(h.contains(k), "{h}");
        }
        assert!(!h.contains("d delete") && !h.contains("Esc clear"), "{h}");
        mark_glob(&mut f, "1");
        let h = has(&f);
        for k in [
            "space mark",
            "u untagged",
            "a glob",
            "d delete",
            "Esc clear",
        ] {
            assert!(h.contains(k), "{h}");
        }

        let mut tagged = ecr();
        tagged.caps.inventory = Inventory::TaggedOnly;
        let mut f = images_in(tagged, imgs());
        mark_glob(&mut f, "1");
        let h = has(&f);
        assert!(h.contains("space mark") && h.contains("d delete"), "{h}");
        assert!(!h.contains("u untagged"), "{h}");

        // Deletion refused by the registry: no delete keys.
        let mut f = images_in(ecr(), imgs());
        f.s.set_delete_modal(dmodal(DelStage::Running));
        let done = Event::DeleteDone {
            op: OpId(1),
            report: DeleteReport {
                results: vec![],
                aborted_unsupported: true,
            },
            stopped: None,
        };
        update(&mut f.s, Input::Event(done), NOW);
        f.act(Action::Enter);
        assert!(f.s.delete_modal().is_none());
        let h = has(&f);
        for k in [
            "space mark",
            "u untagged",
            "a glob",
            "d delete",
            "Esc clear",
        ] {
            assert!(!h.contains(k), "{h}");
        }
    }

    #[test]
    fn range_anchor_marker() {
        let mut f = images_in(
            ecr(),
            vec![art('a', &["1"], Some(1)), art('b', &["2"], Some(1))],
        );
        let h = hint_line(&f.s, 300).to_string();
        assert!(h.contains("v range") && !h.contains("v mark range"), "{h}");
        f.act(Action::RangeMark);
        let buf = buf_of(140, 30, &f.s);
        let lines: Vec<String> = (0..30).map(|y| row_text(&buf, y)).collect();
        let marked: Vec<&String> = lines.iter().filter(|l| l.contains('▏')).collect();
        assert_eq!(marked.len(), 1, "{lines:#?}");
        // The first image row (tag 1) is the anchor and the cursor; its mark slot is `▏`.
        assert!(
            marked[0].contains("▏ ") && marked[0].contains(" 1"),
            "{}",
            marked[0]
        );
        let h = hint_line(&f.s, 300).to_string();
        assert!(h.contains("v mark range  Esc cancel"), "{h}");
        assert!(!h.contains("v range"), "{h}");
        f.act(Action::ClearMarks);
        let h = hint_line(&f.s, 300).to_string();
        assert!(h.contains("v range"), "{h}");
    }

    #[test]
    fn hint_bar_delete_modal_and_prompt() {
        let f = with_modal(preview_modal(DelStage::Preview));
        assert_eq!(
            hint_line(&f.s, 100).to_string().trim(),
            "w whole-manifest  ↑↓ scroll  Enter confirm  Esc cancel"
        );
        for (stage, want) in [
            (DelStage::Snapshot, "Esc cancel"),
            (DelStage::Blocked("x".into()), "Esc close"),
            (DelStage::Running, "Enter/Esc hide"),
        ] {
            let f = with_modal(dmodal(stage));
            assert_eq!(hint_line(&f.s, 100).to_string().trim(), want);
        }
        let mut f = loaded();
        f.act(Action::MarkGlob);
        assert_eq!(
            hint_line(&f.s, 100).to_string().trim(),
            "type a pattern  Enter add to marks  Esc cancel"
        );
    }

    #[test]
    fn hint_bar_keeps_delete_and_core_keys_at_120() {
        // Marks: `d delete` is visible on a 100 and a 120 column bar.
        for w in [100, 120] {
            let h = hint_line(&marked().s, w).to_string();
            assert!(
                h.contains("d delete") && h.contains("Esc clear"),
                "{w}: {h}"
            );
        }
        // No marks: the bar keeps `r refresh` and `p push` as before the delete keys.
        let h = hint_line(&loaded().s, 120).to_string();
        assert!(h.contains("r refresh") && h.contains("p push"), "{h}");
    }

    #[test]
    fn hint_bar_new_keys() {
        // Images pane at 120 columns: copy and pull survive next to the core keys.
        let h = hint_line(&loaded().s, 120).to_string();
        for want in ["y copy", "C copy", "P pull", "r refresh", "p push"] {
            assert!(h.contains(want), "{want}: {h}");
        }
        // Wide enough: export and compare too.
        let h = hint_line(&loaded().s, 200).to_string();
        for want in ["X export", "= compare"] {
            assert!(h.contains(want), "{want}: {h}");
        }
        // Registries pane: the registry keys come after `p push` and do not push it off.
        let mut f = loaded();
        f.act(Action::PrevPane);
        f.act(Action::PrevPane);
        assert_eq!(f.s.focus(), Pane::Registries);
        let h = hint_line(&f.s, 120).to_string();
        for want in ["L login", "r refresh", "p push", "c/n/x edit/new/remove"] {
            assert!(h.contains(want), "{want}: {h}");
        }
        assert!(h.find("p push") < h.find("c/n/x"), "{h}");
        let h = hint_line(&f.s, 200).to_string();
        assert!(h.contains("t theme") && h.contains("! requests"), "{h}");
        assert!(!h.contains("C copy") && !h.contains("X export"), "{h}");
    }

    #[test]
    fn hint_bar_skips_an_item_that_does_not_fit() {
        let items = |w| {
            let l = hint_line(&loaded().s, w).to_string();
            let head = l.split("e config").next().unwrap_or_default().to_owned();
            head.split("  ")
                .map(|c| c.trim().to_owned())
                .filter(|c| !c.is_empty())
                .collect::<Vec<_>>()
        };
        // 96 columns: `r refresh` does not fit after `P pull`; `p push` does.
        let (shown, all) = (items(96), items(400));
        let skipped = all.iter().position(|c| !shown.contains(c)).unwrap();
        assert!(
            all[skipped..].iter().any(|c| shown.contains(c)),
            "{shown:?}"
        );
    }

    #[test]
    fn delete_preview_scrolls_to_the_end_when_wrapped() {
        // 80 columns: a manifest with many tags and long side-effect lines wrap.
        let (mut plan, mut snap) = full_plan();
        // 15-char tags: a wrapped line breaks early at a word, so it needs more rows than
        // width / columns suggests.
        let tags: Vec<String> = (0..60).map(|n| format!("rc-{n:03}-stable-x")).collect();
        let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        snap.inventory[0] = art('b', &tag_refs, Some(100_000_000));
        plan.side_effects
            .extend((0..6).map(|n| SideEffect::OrphanedReferrer {
                referrer: dg('7'),
                subject: dg(char::from(b'a' + n)),
            }));
        let mut f = with_modal(DeleteModal {
            snapshot: Some(snap),
            plan: Some(plan),
            ..dmodal(DelStage::Preview)
        });
        let mut view = ViewState::default();
        let mut out = render_view(80, 24, &f.s, &mut view);
        assert!(
            view.delete_max_scroll > 0 && !out.contains("Totals"),
            "{out}"
        );
        // Down as `main` sends it: dropped once at the end.
        for _ in 0..200 {
            if !view.past_end(&f.s, Action::Down) {
                f.act(Action::Down);
            }
            out = render_view(80, 24, &f.s, &mut view);
        }
        assert_eq!(f.s.delete_modal().unwrap().scroll, view.delete_max_scroll);
        assert!(out.contains("Totals  62 tags"), "{out}");
        // One Up moves the view at once.
        f.act(Action::Up);
        assert!(!render_view(80, 24, &f.s, &mut view).is_empty());
        assert_eq!(
            f.s.delete_modal().unwrap().scroll,
            view.delete_max_scroll - 1
        );
    }

    #[test]
    fn repos_pinned() {
        let pin = |n: &str| RepoSummary {
            path: rp(n),
            created_at: None,
        };
        let mut f = Fx::new(vec![RegistryView {
            pinned: vec![pin("ttl"), pin("ghost")],
            ..ecr()
        }]);
        f.load(CacheKey::LocalImages, CacheValue::LocalImages(vec![]));
        f.load(CacheKey::Repos(rid("company")), repos());
        f.load(
            CacheKey::Inventory(rid("company"), rp("ttl")),
            CacheValue::Inventory(vec![art('a', &["1"], Some(1)), art('b', &["2"], Some(1))]),
        );
        f.fail(
            CacheKey::Inventory(rid("company"), rp("ghost")),
            RegistryError::NotFound,
        );
        insta::assert_snapshot!(render_to(100, 30, &f.s));
    }

    #[test]
    fn repos_tree() {
        let mut f = Fx::new(vec![ecr()]);
        f.load(CacheKey::LocalImages, CacheValue::LocalImages(vec![]));
        let names = ["api", "eng/api/fo", "eng/api/bo", "eng/api/x/y", "web"];
        f.load(
            CacheKey::Repos(rid("company")),
            CacheValue::Repos(
                names
                    .iter()
                    .map(|n| RepoSummary {
                        path: rp(n),
                        created_at: None,
                    })
                    .collect(),
            ),
        );
        f.act(Action::NextPane);
        let collapsed = render_to(100, 30, &f.s);
        assert!(collapsed.contains("▸ eng/api (3)"), "{collapsed}");
        // Expanded, with the collapsed child group `x`; the detail shows the group.
        f.act(Action::Right);
        insta::assert_snapshot!(render_to(100, 30, &f.s));
    }

    #[test]
    fn compare_base_mark_and_modal() {
        let mut f = loaded();
        f.act(Action::CompareMark);
        let out = shot(&f);
        assert!(out.contains('≡'), "{out}");
        f.act(Action::Down);
        f.act(Action::CompareMark);
        let out = shot(&f);
        assert!(
            out.contains("loading…") && !out.contains("hunter2"),
            "{out}"
        );
        f.load(
            key_man('b'),
            manifest(
                "{}",
                ManifestKind::Image {
                    config: desc('d', 7_000, None),
                    layers: vec![desc('1', 29_000_000, None), desc('3', 12_300_000, None)],
                },
            ),
        );
        f.load(key_cfg('b'), config_with(vec![]));
        let out = shot(&f);
        assert!(!out.contains("hunter2") && !out.contains("abc123"), "{out}");
        insta::assert_snapshot!("compare_modal", out);
    }

    #[test]
    fn compare_failure_replaces_loading() {
        let open = || {
            let mut f = loaded();
            f.act(Action::CompareMark);
            f.act(Action::Down);
            f.act(Action::CompareMark);
            f
        };
        let mut f = open();
        f.fail(key_man('b'), RegistryError::Network("reset".into()));
        let out = shot(&f);
        assert!(
            out.contains("cannot compare: network error: reset") && !out.contains("loading…"),
            "{out}"
        );
        // Not an image (a non-image config).
        let mut f = open();
        f.load(
            key_man('b'),
            manifest(
                "{}",
                ManifestKind::Image {
                    config: desc('d', 7_000, None),
                    layers: vec![],
                },
            ),
        );
        f.load(key_cfg('b'), CacheValue::Config(None));
        let out = shot(&f);
        assert!(
            out.contains("cannot compare: bbbbbbbbbbbb is not an image")
                && !out.contains("loading…"),
            "{out}"
        );
    }
}

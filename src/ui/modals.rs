//! Modal overlays.

use std::collections::HashMap;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use time::OffsetDateTime;

use super::{
    failure_text, fit, fmt_hmsms, fmt_size, fmt_time, fmt_utc, key_hint, muted_text, placeholder,
    theme,
};
use crate::app::cache::Entry;
use crate::app::copy::{CopyStage, CopyWarning, CopyWizard, NEW_REPO};
use crate::app::delete::{CHANGED, DelStage, DeleteModal, observed};
use crate::app::export::{ALL_NOTE, ExportModal, ExportStage};
use crate::app::planner::totals;
use crate::app::push::{Stage, Warning, Wizard};
use crate::app::regform::RegForm;
use crate::app::state::{CacheKey, CacheValue, State, StatusKind};
use crate::app::transfer::{Item, Transfer, TransferKind, fmt_speed};
use crate::app::version::suggest;
use crate::app::write::{
    CANCELLED, PullModal, PullStage, PushOutcome, WriteOp, push_failure_message,
};
use crate::config::Theme;
use crate::conntest::{Check, CheckStatus, STEPS};
use crate::domain::{
    ArtifactSummary, DeleteOp, DeletePlan, DeleteReport, DepSnapshot, Digest, ExcludeReason,
    ItemStep, LocalImage, OpOutcome, RegistryError, SideEffect,
};

/// (key, text) rows; an empty key continues the text above. Keys are padded to `KEY_W`.
const KEYS: &[(&str, &str)] = &[
    ("↑↓ / j k", "move"),
    (
        "Tab ← → / h l",
        "switch pane (Enter opens; on a repo group ← → fold it)",
    ),
    ("1-5", "detail tabs: Overview Layers Config History Raw"),
    ("[ ]", "previous / next platform (multi-platform image)"),
    ("Ctrl-d / Ctrl-u", "scroll the detail pane"),
    ("L", "docker login (ecr, ecr-public, basic)"),
    ("", "docker-config registries: run docker login yourself"),
    ("r", "refresh the focused level and everything below"),
    ("p", "push a local image to the selected registry"),
    ("P", "pull the selected image into local Docker"),
    ("C", "copy / retag the image to another repo or registry"),
    ("X", "export the image to an OCI layout tarball"),
    ("e", "edit config in $VISUAL / $EDITOR, then reload"),
    (
        "c / n / x",
        "edit / add / remove a registry (Registries pane)",
    ),
    (
        "t",
        "pick the colour theme (previewed live, saved on Enter)",
    ),
    ("=", "mark a base image; = on another compares the two"),
    ("space", "mark / unmark all tags of the image (images pane)"),
    ("u", "mark untagged images (registries that list them)"),
    ("a", "mark tags by glob (* any, ? one char), adds to marks"),
    ("v", "mark a range: v, move, v again (images pane)"),
    (
        "Esc",
        "cancel the range, else the compare base, else clear marks",
    ),
    (
        "d",
        "delete preview (w whole manifest, type repo name to confirm);",
    ),
    ("", "reopens a hidden running delete or its report"),
    ("/", "filter the focused list (Enter apply, Esc cancel)"),
    ("o / O", "cycle sort column / reverse"),
    ("y", "copy reference (Raw tab: the displayed JSON)"),
    ("m", "reveal masked values of this image"),
    ("!", "request log (↑↓ scroll, / filter, Esc close)"),
    ("?", "this help"),
    ("q", "quit (main screen)"),
    ("Ctrl-C", "quit (anywhere)"),
];

/// Caveat rows, padded to `CAVEAT_W`.
const CAVEATS: &[(&str, &str)] = &[
    (
        "Size*",
        "compressed size as reported by the registry, not storage use;",
    ),
    ("", "shared layers are not de-duplicated."),
    (
        "Pulled≈",
        "approximate (ECR refreshes it at least every 24 h).",
    ),
    (
        "Masking",
        "best effort: secrets in env, labels, commands and JSON are",
    ),
    (
        "",
        "hidden by key name only; do not rely on it for screen sharing.",
    ),
];

const KEY_W: usize = 16;
const CAVEAT_W: usize = 9;

fn rows(v: &[(&str, &str)], w: usize) -> impl Iterator<Item = Line<'static>> {
    v.iter().map(move |(k, t)| {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{k:<w$}"), theme::key()),
            Span::raw((*t).to_owned()),
        ])
    })
}

fn help_lines() -> Vec<Line<'static>> {
    let mut v = vec![Line::styled("Keys", theme::title())];
    v.extend(rows(KEYS, KEY_W));
    v.push(Line::from(""));
    v.push(Line::styled("Caveats", theme::title()));
    v.extend(rows(CAVEATS, CAVEAT_W));
    v.push(Line::styled("  ? again closes this help", theme::muted()));
    v
}

/// `w` × `h` (cut to the screen), centred.
fn centered(screen: Rect, w: u16, h: u16) -> Rect {
    let (w, h) = (w.min(screen.width), h.min(screen.height));
    Rect {
        x: screen.x + (screen.width - w) / 2,
        y: screen.y + (screen.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Scrolled down by `scroll` rows when it does not fit; returns the largest useful scroll.
pub fn help(frame: &mut Frame, screen: Rect, scroll: u16) -> u16 {
    let lines = help_lines();
    let widest = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let area = centered(screen, widest + 2, lines.len() as u16 + 2);
    let max = lines
        .len()
        .saturating_sub(area.height.saturating_sub(2) as usize) as u16;
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).scroll((scroll.min(max), 0)).block(
            Block::new()
                .style(theme::base())
                .borders(Borders::ALL)
                .border_style(theme::border_focus())
                .title(Line::styled(" Help ", theme::title())),
        ),
        area,
    );
    max
}

// Request log column widths; the path takes the rest.
const REQ_TIME_W: usize = 12;
const REQ_REG_W: usize = 10;
const REQ_OP_W: usize = 14;
const REQ_OUTCOME_W: usize = 8;
const REQ_MS_W: usize = 7;
const REQ_BYTES_W: usize = 8;

/// The request log, newest at the bottom; `scroll` counts rows up from there. Returns the largest
/// useful scroll. ponytail: new entries shift the rows while scrolled up; follow-by-anchor if
/// that annoys.
pub fn requests(frame: &mut Frame, screen: Rect, state: &State) -> u16 {
    let Some(r) = state.requests() else { return 0 };
    let area = centered(
        screen,
        screen.width.saturating_sub(4),
        screen.height.saturating_sub(2),
    );
    let width = area.width.saturating_sub(2) as usize;
    let path_w = width
        .saturating_sub(
            REQ_TIME_W + REQ_REG_W + REQ_OP_W + REQ_OUTCOME_W + REQ_MS_W + REQ_BYTES_W + 6,
        )
        .max(8);
    let visible = r.visible();
    let shows_filter = r.filter_editing || !r.filter.is_empty();
    let h = (area.height.saturating_sub(2) as usize).saturating_sub(1 + usize::from(shows_filter));
    let max = visible.len().saturating_sub(h);
    let end = visible.len() - usize::from(r.scroll).min(max);
    let header = format!(
        "{:<REQ_TIME_W$} {:<REQ_REG_W$} {:<REQ_OP_W$} {:<path_w$} {:<REQ_OUTCOME_W$} {:>REQ_MS_W$} {:>REQ_BYTES_W$}",
        "time (UTC)", "registry", "op", "path", "outcome", "ms", "bytes"
    );
    let mut lines = vec![];
    if shows_filter {
        let caret = if r.filter_editing { "▏" } else { "" };
        lines.push(field("filter:", Span::raw(format!("{}{caret}", r.filter))));
    }
    lines.push(Line::styled(header, theme::header()));
    if visible.is_empty() {
        lines.push(muted_line(if r.entries.is_empty() {
            "no requests yet"
        } else {
            "no request matches the filter"
        }));
    }
    for e in &visible[end.saturating_sub(h)..end] {
        let good = e.outcome == "ok" || e.outcome.starts_with(['2', '3']);
        lines.push(Line::from(vec![
            Span::styled(format!("{:<REQ_TIME_W$} ", fmt_hmsms(e.at)), theme::time()),
            Span::raw(format!("{:<REQ_REG_W$} ", fit(&e.registry, REQ_REG_W))),
            Span::raw(format!("{:<REQ_OP_W$} ", fit(&e.op, REQ_OP_W))),
            Span::raw(format!("{:<path_w$} ", fit(&e.path, path_w))),
            Span::styled(
                format!("{:<REQ_OUTCOME_W$} ", fit(&e.outcome, REQ_OUTCOME_W)),
                if good { theme::ok() } else { theme::bad() },
            ),
            Span::styled(format!("{:>REQ_MS_W$} ", e.ms), theme::number()),
            Span::raw(format!(
                "{:>REQ_BYTES_W$}",
                e.bytes.map_or_else(|| "—".to_owned(), fmt_size)
            )),
        ]));
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .style(theme::base())
                .borders(Borders::ALL)
                .border_style(theme::border_focus())
                .title(Line::styled(
                    format!(" Requests ({}) ", visible.len()),
                    theme::title(),
                )),
        ),
        area,
    );
    max as u16
}

/// The comparison of the marked base (A) and the cursor image (B). Returns the largest useful
/// scroll.
pub fn compare(frame: &mut Frame, screen: Rect, state: &State) -> u16 {
    let Some(m) = state.compare() else { return 0 };
    let area = centered(
        screen,
        screen.width.saturating_sub(4),
        screen.height.saturating_sub(2),
    );
    let h = area.height.saturating_sub(2) as usize;
    let rows: Vec<Line> = match state.compare_text() {
        None => match state.compare_failure() {
            Some(e) => vec![Line::styled(format!("cannot compare: {e}"), theme::bad())],
            None => vec![muted_line("loading…")],
        },
        Some(t) => t
            .into_iter()
            .map(|l| {
                let style = match l.trim_start().chars().next() {
                    _ if !l.starts_with(' ') => theme::header(),
                    Some('+') => theme::ok(),
                    Some('−') => theme::bad(),
                    Some('~') => theme::warn(),
                    _ => Style::new(),
                };
                Line::styled(l, style)
            })
            .collect(),
    };
    let max = rows.len().saturating_sub(h);
    let top = usize::from(m.scroll).min(max);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(rows.into_iter().skip(top).take(h).collect::<Vec<_>>()).block(
            Block::new()
                .style(theme::base())
                .borders(Borders::ALL)
                .border_style(theme::border_focus())
                .title(Line::styled(
                    format!(" Compare {} ⇄ {} ", m.a.digest.short(), m.b.digest.short()),
                    theme::title(),
                )),
        ),
        area,
    );
    max as u16
}

pub fn theme_picker(frame: &mut Frame, screen: Rect, row: usize) {
    let lines: Vec<Line> = Theme::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mark = if i == row { '›' } else { ' ' };
            let line = Line::from(format!(" {mark} {} ", t.label()));
            if i == row {
                line.style(theme::selected())
            } else {
                line
            }
        })
        .collect();
    let area = centered(screen, 48, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .style(theme::base())
                .borders(Borders::ALL)
                .border_style(theme::border_focus())
                .title(Line::styled(" Theme ", theme::title())),
        ),
        area,
    );
}

// ---- registry form ----

const FORM_LABEL_W: usize = 19;
/// `list repositories` plus a gap.
const TEST_STEP_W: usize = 20;

/// The registry form (`c` / `n`); the discard prompt is drawn over it.
pub fn registry_form(frame: &mut Frame, screen: Rect, state: &State) {
    let Some(f) = state.reg_form() else { return };
    let mut lines: Vec<Line> = f
        .fields()
        .into_iter()
        .map(|field| {
            let focused = field == f.focus;
            let label = RegForm::label(field, f.values.kind);
            let label_style = if focused {
                theme::key()
            } else {
                theme::muted()
            };
            let mut spans = vec![
                Span::raw(" "),
                Span::styled(format!("{label:<FORM_LABEL_W$}"), label_style),
            ];
            if let Some((options, i)) = f.choice(field) {
                spans.push(Span::raw(format!("‹ {} ›", options[i])));
                spans.push(Span::styled(
                    format!("  {}", options.join(" · ")),
                    theme::muted(),
                ));
            } else {
                spans.push(Span::raw(f.text(field).unwrap_or_default().to_owned()));
                if focused {
                    spans.push(Span::raw("▏"));
                }
            }
            if let Some(h) = f.hint(field) {
                spans.push(Span::styled(format!("  {h}"), theme::muted()));
            }
            selectable(spans, focused)
        })
        .collect();
    lines.push(Line::from(""));
    if let Some(e) = &f.error {
        lines.push(Line::styled(format!(" {e}"), theme::bad()));
    }
    if let Some(t) = &f.test {
        lines.push(Line::styled(" Test", theme::title()));
        lines.extend(
            STEPS
                .iter()
                .zip(&t.checks)
                .map(|(name, c)| test_line(name, c.as_ref())),
        );
        if let Some(text) = t.verdict_text() {
            let style = if t.verdict() == Some(true) {
                theme::ok()
            } else {
                theme::bad()
            };
            lines.push(Line::styled(format!("  {text}"), style));
        }
    }
    let title = match &f.original {
        Some(id) => format!(" Registry {id} "),
        None => " New registry ".to_owned(),
    };
    let area = centered(screen, 84, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .style(theme::base())
                .borders(Borders::ALL)
                .border_style(theme::border_focus())
                .title(Line::styled(title, theme::title())),
        ),
        area,
    );
    if f.confirm_discard {
        confirm(frame, screen, "discard changes? y/n");
    }
}

/// One connection-test row: `✓` / `✗` / `· skipped`, or `…` while the step runs.
fn test_line(name: &str, c: Option<&Check>) -> Line<'static> {
    let row = |icon: &str, style, rest: String| {
        Line::from(vec![
            Span::styled(format!("  {icon} "), style),
            Span::raw(format!("{name:<TEST_STEP_W$}")),
            Span::styled(rest, theme::muted()),
        ])
    };
    let Some(c) = c else {
        return row("…", theme::muted(), String::new());
    };
    let rest = match c.ms {
        0 => c.detail.clone(),
        ms => format!("{} · {ms} ms", c.detail),
    };
    match c.status {
        CheckStatus::Ok => row("✓", theme::ok(), rest),
        CheckStatus::Failed => row("✗", theme::bad(), rest),
        CheckStatus::Skipped => row("·", theme::muted(), "skipped".into()),
    }
}

/// A one-line y/n prompt.
pub fn confirm(frame: &mut Frame, screen: Rect, text: &str) {
    let area = centered(screen, text.chars().count() as u16 + 6, 3);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Line::styled(format!("  {text}"), theme::title())).block(
            Block::new()
                .style(theme::base())
                .borders(Borders::ALL)
                .border_style(theme::border_focus()),
        ),
        area,
    );
}

// ---- push wizard ----

/// First 12 hex digits of a local image id.
fn short_id(id: &str) -> String {
    id.strip_prefix("sha256:")
        .unwrap_or(id)
        .chars()
        .take(12)
        .collect()
}

/// `label  value`, the label muted.
fn field(label: &str, value: Span<'static>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}  "), theme::muted()),
        value,
    ])
}

/// The rows that fit in `h`, scrolled just far enough to show row `sel`.
fn window(rows: Vec<Line<'static>>, sel: usize, h: usize) -> Vec<Line<'static>> {
    let skip = (sel + 1).saturating_sub(h);
    rows.into_iter().skip(skip).take(h).collect()
}

fn selectable(spans: Vec<Span<'static>>, selected: bool) -> Line<'static> {
    let line = Line::from(spans);
    if selected {
        line.style(theme::selected())
    } else {
        line
    }
}

fn image_rows(state: &State, w: &Wizard, width: usize, h: usize) -> Vec<Line<'static>> {
    let mut v = vec![Line::from(vec![
        Span::raw("filter: ".to_owned()).style(theme::muted()),
        Span::raw(format!("{}▏", w.filter)),
    ])];
    let tag_w = width.saturating_sub(12 + 2 + 2 + 16 + 2 + 8);
    v.push(Line::styled(
        format!(
            "{:<12}  {:<tag_w$}  {:<16}  {:>8}",
            "Image", "Tag", "Created UTC", "Size"
        ),
        theme::muted(),
    ));
    let images = w.images(state);
    if images.is_empty() {
        v.push(Line::styled("(no matching images)", theme::muted()));
        return v;
    }
    let sel_id = w.selected_image(state).map(|i| &i.id);
    let sel = images
        .iter()
        .position(|i| Some(&i.id) == sel_id)
        .unwrap_or(0);
    let rows = images
        .iter()
        .enumerate()
        .map(|(n, i)| {
            let (tag, tag_style) = match i.repo_tags.first() {
                Some(t) => (fit(t, tag_w), Default::default()),
                None => ("<none>".to_owned(), theme::muted()),
            };
            let (created, created_style) = match i.created {
                Some(t) => (fmt_time(t), theme::time()),
                None => ("—".to_owned(), theme::muted()),
            };
            selectable(
                vec![
                    Span::raw(format!("{:<12}  ", short_id(&i.id))),
                    Span::styled(format!("{tag:<tag_w$}"), tag_style),
                    Span::raw("  ".to_owned()),
                    Span::styled(format!("{created:<16}"), created_style),
                    // Padded so the fill reaches the right border.
                    Span::raw("  ".to_owned()),
                    Span::styled(format!("{:>8}", fmt_size(i.size)), theme::number()),
                ],
                n == sel,
            )
        })
        .collect();
    v.extend(window(rows, sel, h.saturating_sub(v.len())));
    v
}

fn repo_rows(state: &State, w: &Wizard, h: usize) -> Vec<Line<'static>> {
    let mut v = vec![Line::styled("Target repository", theme::muted())];
    let repos = w.repos(state);
    if repos.is_empty() {
        let key = CacheKey::Repos(w.registry.clone());
        let text = failure_text(state, &key, "Enter retry").unwrap_or_else(|| {
            match placeholder(state, &key) {
                "" => muted_text("(empty)"),
                p => muted_text(p),
            }
        });
        v.extend(text.lines);
        return v;
    }
    let sel_repo = w.selected_repo(state);
    let sel = repos.iter().position(|r| Some(*r) == sel_repo).unwrap_or(0);
    let rows = repos
        .iter()
        .enumerate()
        .map(|(n, r)| selectable(vec![Span::raw(r.to_string())], n == sel))
        .collect();
    v.extend(window(rows, sel, h.saturating_sub(1)));
    v
}

/// The existing tag the suggestion was derived from (§7.5): the one whose own next version it is.
fn suggested_from(state: &State, w: &Wizard) -> Option<String> {
    let key = CacheKey::Inventory(w.registry.clone(), w.repo.clone()?);
    let CacheValue::Inventory(v) = state.value(&key)? else {
        return None;
    };
    v.iter()
        .flat_map(|a| &a.tags)
        .find(|t| suggest(&[(t.as_str(), None)]).as_deref() == Some(w.tag.as_str()))
        .cloned()
}

fn tag_lines(state: &State, w: &Wizard) -> Vec<Line<'static>> {
    let repo = w.repo.as_ref().map(ToString::to_string).unwrap_or_default();
    let mut v = vec![
        field("Repository", Span::raw(repo)),
        Line::from(""),
        field("Tag", Span::raw(format!("{}▏", w.tag))),
    ];
    if !w.tag_edited
        && let Some(from) = suggested_from(state, w)
    {
        v.push(Line::styled(
            format!("suggested from {from}"),
            theme::muted(),
        ));
    }
    v
}

fn warning_text(w: &Warning, target: &str) -> String {
    match w {
        Warning::CheckingTag => "checking tag…".to_owned(),
        Warning::TagExistenceUnknown => "could not check whether the tag exists".to_owned(),
        Warning::TagWillMove => "tag exists and will be moved".to_owned(),
        Warning::TagWillBeRejected => "tag exists; push will be rejected (immutable)".to_owned(),
        Warning::TagExistsPolicyUnknown => "tag exists; registry policy unknown".to_owned(),
        Warning::CheckingPolicy => "checking tag policy…".to_owned(),
        Warning::LocalTagReplaced => format!("local tag {target} will be replaced"),
    }
}

fn confirm_lines(state: &State, w: &Wizard) -> Vec<Line<'static>> {
    let target = w.target(state).unwrap_or_default();
    let image: Option<&LocalImage> = match state.value(&CacheKey::LocalImages) {
        Some(CacheValue::LocalImages(all)) => all.iter().find(|i| Some(&i.id) == w.image.as_ref()),
        _ => None,
    };
    let source = image.map_or(String::new(), |i| {
        let tag = i.repo_tags.first().map_or("<none>", String::as_str);
        format!("{} ({tag})", short_id(&i.id))
    });
    let mut v = vec![
        field("Target", Span::styled(target.clone(), theme::title())),
        field("Source", Span::raw(source)),
        Line::from(""),
    ];
    v.extend(
        w.warnings(state)
            .iter()
            .map(|x| Line::styled(warning_text(x, &target), theme::warn())),
    );
    v
}

/// `[██████····]` of `width` cells.
fn gauge(current: u64, total: u64, width: usize) -> String {
    let done = (current as u128 * width as u128 / total.max(1) as u128).min(width as u128) as usize;
    format!("[{}{}]", "█".repeat(done), "·".repeat(width - done))
}

fn progress_lines(state: &State, op: &WriteOp) -> Vec<Line<'static>> {
    let p = &op.progress;
    let mut v = vec![];
    match &op.outcome {
        None => {
            v.push(Line::styled(
                format!("Pushing {}", op.target),
                theme::title(),
            ));
            v.push(Line::from(vec![
                Span::raw("layers done "),
                Span::styled(p.layers_done.to_string(), theme::number()),
            ]));
            // No total (0): the engine has not said how big the transfer is.
            if let Some((cur, total)) = p.bytes.filter(|&(_, t)| t > 0) {
                let pct = (cur as u128 * 100 / total as u128).min(100);
                v.push(Line::from(vec![
                    Span::raw(format!("current transfer  {} ", gauge(cur, total, 20))),
                    Span::styled(format!("{pct}%"), theme::number()),
                    Span::raw(format!("  {} / {}", fmt_size(cur), fmt_size(total))),
                ]));
            }
            if !p.last_status.is_empty() {
                v.push(Line::from(""));
                v.push(Line::styled(p.last_status.clone(), theme::muted()));
            }
        }
        Some(PushOutcome::Pushed(digest)) => {
            v.push(Line::styled(format!("✓ pushed {}", op.target), theme::ok()));
            if let Some(d) = digest.as_ref().or(p.digest.as_ref()) {
                v.push(field("digest", Span::styled(d.to_string(), theme::muted())));
            }
        }
        Some(PushOutcome::Failed(e)) => {
            let reg = state.registries().iter().find(|r| r.id == op.registry);
            let msg = push_failure_message(e, reg);
            v.push(Line::styled(format!("! {msg}"), theme::bad()));
        }
        Some(PushOutcome::Cancelled) => v.push(Line::from(CANCELLED)),
    }
    v
}

pub fn wizard(frame: &mut Frame, screen: Rect, state: &State) {
    let Some(w) = state.wizard() else { return };
    let area = centered(screen, 84, 22);
    let block = Block::new()
        .style(theme::base())
        .borders(Borders::ALL)
        .border_style(theme::border_focus())
        .title(Line::styled(
            format!(" {} ", w.title(state)),
            theme::title(),
        ));
    let inner = block.inner(area);
    let (width, h) = (inner.width as usize, inner.height as usize);
    let lines = match w.stage {
        Stage::PickImage => image_rows(state, w, width, h),
        Stage::PickRepo => repo_rows(state, w, h),
        Stage::Tag => tag_lines(state, w),
        Stage::Confirm => confirm_lines(state, w),
        Stage::Progress => state
            .write()
            .map(|op| progress_lines(state, op))
            .unwrap_or_default(),
    };
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

// ---- pull ----

/// Item rows shown by `transfer_lines`; the rest are counted in `…N more`.
const TRANSFER_ROWS: usize = 6;

/// `1s`, `2m05s` (whole seconds).
fn fmt_secs(d: time::Duration) -> String {
    let s = d.whole_seconds().max(0);
    if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// `█████░░░` of `width` cells.
fn bar(current: u64, total: u64, width: usize) -> String {
    let done = (current as u128 * width as u128 / total.max(1) as u128).min(width as u128) as usize;
    format!("{}{}", "█".repeat(done), "░".repeat(width - done))
}

fn item_line(kind: TransferKind, i: &Item) -> Line<'static> {
    let size = i.size.map(fmt_size).unwrap_or_default();
    let (state, style, right) = match i.state {
        ItemStep::Waiting { .. } => ("· waiting".to_owned(), theme::muted(), String::new()),
        ItemStep::Bytes { current, total } => {
            let verb = if kind == TransferKind::Copy {
                "↑ uploading"
            } else {
                "↓ downloading"
            };
            let right = format!("{}/{}", fmt_size(current), fmt_size(total));
            (
                format!("{verb} {}", bar(current, total, 13)),
                theme::accent(),
                right,
            )
        }
        ItemStep::Downloaded => ("✓ downloaded".to_owned(), theme::ok(), size),
        ItemStep::Extracting => ("⇣ extracting".to_owned(), theme::accent(), size),
        ItemStep::Done => ("✓ done".to_owned(), theme::ok(), size),
        ItemStep::Exists => ("✓ exists".to_owned(), theme::ok(), size),
        ItemStep::Mounted => ("⇄ mounted".to_owned(), theme::ok(), size),
    };
    Line::from(vec![
        Span::raw(format!("  {:<12}  ", fit(&i.id, 12))),
        Span::styled(format!("{state:<28}"), style),
        Span::styled(format!("{right:>13}"), theme::number()),
    ])
}

/// A transfer (pull, copy, export): while it runs, the phase with the overall bar, bytes and
/// percent, then speed, ETA and elapsed time; then its items, unfinished first. A finished
/// transfer shows only its items (the caller shows its result line instead).
pub(super) fn transfer_lines(
    t: &Transfer,
    now: OffsetDateTime,
    width: usize,
) -> Vec<Line<'static>> {
    let mut v = vec![];
    if t.finished.is_none() {
        let (done, total) = (t.done_bytes(), t.total_bytes());
        let mut first = vec![
            Span::styled(format!("{:<13}", t.phase()), theme::title()),
            Span::styled(
                bar(done, total, width.saturating_sub(40).clamp(8, 28)),
                theme::accent(),
            ),
            Span::raw(format!("  {} / {}", fmt_size(done), fmt_size(total))),
        ];
        if let Some(p) = t.percent() {
            first.push(Span::raw("  "));
            first.push(Span::styled(format!("{p}%"), theme::number()));
        }
        v.push(Line::from(first));
        let mut second = format!("{:13}{}", "", fmt_speed(t.speed(now)));
        if let Some(eta) = t.eta(now) {
            second += &format!(" · ETA {}", fmt_secs(eta));
        }
        second += &format!(" · elapsed {}", fmt_secs(now - t.started));
        v.push(Line::styled(second, theme::muted()));
        v.push(Line::from(""));
    }
    let (rows, more) = t.rows(TRANSFER_ROWS);
    v.extend(rows.into_iter().map(|i| item_line(t.kind, i)));
    if more > 0 {
        v.push(Line::styled(format!("  …{more} more"), theme::muted()));
    }
    v
}

/// `label` padded to the pull header's label column.
fn pull_field(label: &str, value: Vec<Span<'static>>) -> Line<'static> {
    let mut v = vec![Span::styled(format!("{label:<10}"), theme::muted())];
    v.extend(value);
    Line::from(v)
}

fn pull_header(state: &State, m: &PullModal) -> Vec<Line<'static>> {
    let loading = || Span::styled("loading…", theme::muted());
    let cfg = m.config(state);
    let platform = match (cfg.and_then(|c| c.platform.as_ref()), &m.platform) {
        (Some(p), _) | (None, Some(p)) => Span::raw(p.to_string()),
        (None, None) if cfg.is_some() => Span::raw("—"),
        (None, None) => loading(),
    };
    let mut size = match (m.size(state), m.layers(state)) {
        (Some(b), Some(l)) => vec![
            Span::styled(fmt_size(b), theme::number()),
            Span::raw(format!(
                " compressed · {} {}",
                l.len(),
                if l.len() == 1 { "layer" } else { "layers" }
            )),
        ],
        _ => vec![loading()],
    };
    if let Some(t) = cfg.and_then(|c| c.created) {
        size.push(Span::styled("   Created ", theme::muted()));
        size.push(Span::styled(fmt_utc(t), theme::time()));
    }
    let local = m.local(state);
    let local_span = match (local, state.entry(&CacheKey::LocalImages)) {
        (Some(l), _) => Span::styled(format!("✓ present ({})", short_id(&l.id)), theme::ok()),
        (None, Some(Entry::Loading { .. })) => Span::styled("checking…", theme::muted()),
        (None, Some(Entry::Ready { .. })) => Span::raw("not present"),
        _ => Span::styled("unknown (Docker unavailable)", theme::muted()),
    };
    let mut v = vec![
        pull_field("Ref", vec![Span::styled(m.image.clone(), theme::title())]),
        pull_field(
            "Digest",
            vec![
                Span::raw(format!("{}:{}…", m.digest.algorithm(), m.digest.short())),
                Span::styled("   Platform ", theme::muted()),
                platform,
            ],
        ),
        pull_field("Size", size),
        pull_field("Local", vec![local_span]),
    ];
    if m.stage == PullStage::Details && local.is_some() {
        v.push(Line::styled(
            "already local — pulling again checks for changes",
            theme::warn(),
        ));
    }
    v
}

/// The pull modal's keys for its stage.
pub(super) fn pull_keys(m: &PullModal) -> &'static [(&'static str, &'static str)] {
    match m.stage {
        PullStage::Details => &[("Enter", "pull"), ("Esc", "cancel")],
        PullStage::Running => &[("Enter", "hide"), ("Esc", "cancel")],
        PullStage::Result(_) => &[("Enter", "close")],
    }
}

/// Details, then live progress, then the result line (spec §7.7).
pub fn pull(frame: &mut Frame, screen: Rect, state: &State, now: OffsetDateTime) {
    let Some(m) = state.pull_modal() else { return };
    transfer_modal(
        frame,
        screen,
        TransferBox {
            width: 84,
            title: format!(" Pull from {} ", m.registry),
            lines: pull_header(state, m),
            result: state.pull_result(),
            transfer: m.transfer.as_ref(),
            keys: pull_keys(m),
        },
        now,
    );
}

/// `✓ text` / `! text` / `text`, styled by kind.
fn result_line(kind: StatusKind, text: &str) -> Line<'static> {
    let (mark, style) = match kind {
        StatusKind::Ok => ("✓ ", theme::ok()),
        StatusKind::Error => ("! ", theme::bad()),
        StatusKind::Info => ("", theme::base()),
    };
    Line::styled(format!("{mark}{text}"), style)
}

/// A pull / export modal: its header `lines`, then the result line and the transfer.
struct TransferBox<'a> {
    width: u16,
    title: String,
    lines: Vec<Line<'static>>,
    result: Option<(StatusKind, String)>,
    transfer: Option<&'a Transfer>,
    keys: &'static [(&'static str, &'static str)],
}

/// Draws `b` centred, sized to its lines, with `keys` on the bottom border.
fn transfer_modal(frame: &mut Frame, screen: Rect, b: TransferBox, now: OffsetDateTime) {
    let mut lines = b.lines;
    if let Some((kind, text)) = &b.result {
        lines.push(Line::from(""));
        lines.push(result_line(*kind, text));
    }
    if let Some(t) = b.transfer {
        lines.push(Line::from(""));
        lines.extend(transfer_lines(t, now, b.width as usize - 2));
    }
    let mut bottom = vec![Span::raw(" ")];
    for (i, k) in b.keys.iter().enumerate() {
        if i > 0 {
            bottom.push(Span::raw("  "));
        }
        bottom.extend(key_hint(*k));
    }
    bottom.push(Span::raw(" "));
    let area = centered(screen, b.width, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .style(theme::base())
                .borders(Borders::ALL)
                .border_style(theme::border_focus())
                .title(Line::styled(b.title, theme::title()))
                .title_bottom(Line::from(bottom)),
        ),
        area,
    );
}

// ---- copy ----

pub(super) fn copy_keys(state: &State, w: &CopyWizard) -> &'static [(&'static str, &'static str)] {
    match w.stage {
        CopyStage::PickRegistry => &[("↑↓", "move"), ("Enter", "select"), ("Esc", "cancel")],
        CopyStage::PickRepo => &[
            ("type", "to filter or name a new repo"),
            ("↑↓", "move"),
            ("Enter", "select"),
            ("Esc", "back"),
        ],
        CopyStage::Tag => &[("type", "a tag"), ("Enter", "next"), ("Esc", "back")],
        CopyStage::Confirm => &[("Enter", "copy"), ("Esc", "back")],
        CopyStage::Progress if state.write_running() && w.outcome.is_none() => {
            &[("Enter", "hide"), ("Esc", "cancel")]
        }
        CopyStage::Progress => &[("Enter", "close")],
    }
}

fn copy_warning_text(w: &CopyWarning) -> String {
    match w {
        CopyWarning::TagWillMove(d) => format!("tag exists on {} and will be moved", d.short()),
        CopyWarning::TagWillBeRejected => "tag exists; copy will be rejected (immutable)".into(),
        CopyWarning::TagExistenceUnknown => "could not check whether the tag exists".into(),
        CopyWarning::TagExistsPolicyUnknown => "tag exists; registry policy unknown".into(),
        CopyWarning::NewRepo => NEW_REPO.into(),
        CopyWarning::CheckingTag => "checking tag…".into(),
    }
}

fn copy_registry_rows(state: &State, w: &CopyWizard, h: usize) -> Vec<Line<'static>> {
    let mut v = vec![Line::styled("Target registry", theme::muted())];
    let regs = state.registries();
    let sel = regs.iter().position(|r| r.id == w.registry).unwrap_or(0);
    let rows = regs
        .iter()
        .enumerate()
        .map(|(n, r)| {
            let spans = vec![
                Span::raw(format!("{:<20}  ", r.id)),
                Span::styled(r.ref_host.clone(), theme::muted()),
            ];
            selectable(spans, n == sel)
        })
        .collect();
    v.extend(window(rows, sel, h.saturating_sub(1)));
    v
}

fn copy_repo_rows(state: &State, w: &CopyWizard, h: usize) -> Vec<Line<'static>> {
    let mut v = vec![Line::from(vec![
        Span::styled("repository: ", theme::muted()),
        Span::raw(format!("{}▏", w.filter)),
    ])];
    if w.new_repo(state) {
        v.push(Line::from(""));
        v.push(Line::styled(
            format!("new repository {}", w.filter),
            theme::title(),
        ));
        v.push(Line::styled(NEW_REPO, theme::warn()));
        return v;
    }
    let repos = w.repos(state);
    if repos.is_empty() {
        let key = CacheKey::Repos(w.registry.clone());
        let text = failure_text(state, &key, "Enter retry").unwrap_or_else(|| {
            match placeholder(state, &key) {
                "" => muted_text("(no matching repository)"),
                p => muted_text(p),
            }
        });
        v.extend(text.lines);
        return v;
    }
    let target = w.target_repo(state);
    let sel = repos
        .iter()
        .position(|r| Some(*r) == target.as_ref())
        .unwrap_or(0);
    let rows = repos
        .iter()
        .enumerate()
        .map(|(n, r)| selectable(vec![Span::raw(r.to_string())], n == sel))
        .collect();
    v.extend(window(rows, sel, h.saturating_sub(1)));
    v
}

/// Picking, then live progress, then the result line. A retag moves no blobs, so its
/// progress has no items.
pub fn copy(frame: &mut Frame, screen: Rect, state: &State, now: OffsetDateTime) {
    let Some(w) = state.copy_wizard() else { return };
    let area = centered(screen, 84, 22);
    let block = Block::new()
        .style(theme::base())
        .borders(Borders::ALL)
        .border_style(theme::border_focus())
        .title(Line::styled(
            format!(" {} ", w.title(state)),
            theme::title(),
        ));
    let inner = block.inner(area);
    let (width, h) = (inner.width as usize, inner.height as usize);
    let source = field("Source", Span::raw(w.source(state)));
    let lines = match w.stage {
        CopyStage::PickRegistry => copy_registry_rows(state, w, h),
        CopyStage::PickRepo => copy_repo_rows(state, w, h),
        CopyStage::Tag => vec![
            field(
                "Repository",
                Span::raw(w.target_repo(state).map(|r| r.0).unwrap_or_default()),
            ),
            Line::from(""),
            field("Tag", Span::raw(format!("{}▏", w.tag))),
        ],
        CopyStage::Confirm => {
            let target = w.target(state).unwrap_or_default();
            let mut v = vec![
                source,
                field("Target", Span::styled(target, theme::title())),
                Line::from(""),
            ];
            v.extend(
                w.warnings(state)
                    .iter()
                    .map(|x| Line::styled(copy_warning_text(x), theme::warn())),
            );
            v
        }
        CopyStage::Progress => {
            let mut v = vec![source];
            if let Some((kind, text)) = state.copy_result() {
                v.push(Line::from(""));
                v.push(result_line(kind, &text));
            }
            match &w.transfer {
                Some(t) if t.items.is_empty() && w.outcome.is_none() => {
                    let retag =
                        w.registry == w.src_registry && w.repo.as_ref() == Some(&w.src_repo);
                    let m = if retag {
                        "tagging (no blobs to copy)…"
                    } else {
                        "reading the source manifest…"
                    };
                    v.push(Line::from(""));
                    v.push(Line::styled(m, theme::muted()));
                }
                Some(t) => {
                    v.push(Line::from(""));
                    v.extend(transfer_lines(t, now, width));
                }
                None => {}
            }
            v
        }
    };
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

// ---- export ----

pub(super) fn export_keys(m: &ExportModal) -> &'static [(&'static str, &'static str)] {
    match m.stage {
        ExportStage::Details if m.child.is_some() => {
            &[("a", "all platforms"), ("Enter", "next"), ("Esc", "cancel")]
        }
        ExportStage::Details => &[("Enter", "next"), ("Esc", "cancel")],
        ExportStage::Path => &[("type", "a path"), ("Enter", "export"), ("Esc", "back")],
        ExportStage::Overwrite => &[("y", "yes"), ("n/Esc", "no")],
        ExportStage::Running => &[("Enter", "hide"), ("Esc", "cancel")],
        ExportStage::Result(_) => &[("Enter", "close")],
    }
}

/// Details and path, then live progress and the result line; the overwrite prompt alone.
pub fn export(frame: &mut Frame, screen: Rect, state: &State, now: OffsetDateTime) {
    let Some(m) = state.export_modal() else {
        return;
    };
    if m.stage == ExportStage::Overwrite {
        confirm(
            frame,
            screen,
            &format!("{} exists — overwrite? y/n", m.path),
        );
        return;
    }
    let platform = match (&m.platform, m.all) {
        (_, true) => "all platforms".to_owned(),
        (Some(p), false) => p.to_string(),
        // A single image: its config's platform, once loaded.
        (None, false) => {
            let key = CacheKey::Config(m.registry.clone(), m.repo.clone(), m.digest.clone());
            match state.value(&key) {
                Some(CacheValue::Config(Some(c))) => c.platform.as_ref().map(ToString::to_string),
                _ => None,
            }
            .unwrap_or_else(|| "—".to_owned())
        }
    };
    let size = match m.size(state) {
        Some(b) => vec![
            Span::styled(fmt_size(b), theme::number()),
            Span::raw(" compressed"),
        ],
        None => vec![Span::raw("—")],
    };
    let cursor = if m.stage == ExportStage::Path {
        "▏"
    } else {
        ""
    };
    let mut lines = vec![
        pull_field("Ref", vec![Span::styled(m.image.clone(), theme::title())]),
        pull_field("Platform", vec![Span::raw(platform)]),
        pull_field("Size", size),
        pull_field("Path", vec![Span::raw(format!("{}{cursor}", m.path))]),
    ];
    if m.all && m.stage == ExportStage::Details {
        lines.push(Line::from(""));
        lines.push(Line::styled(ALL_NOTE, theme::warn()));
    }
    transfer_modal(
        frame,
        screen,
        TransferBox {
            width: 96,
            title: format!(" Export from {} ", m.registry),
            lines,
            result: state.export_result(),
            transfer: m.transfer.as_ref(),
            keys: export_keys(m),
        },
        now,
    );
}

// ---- delete ----

/// Mark tags by pattern (the `a` prompt).
pub fn glob(frame: &mut Frame, screen: Rect, state: &State) {
    let Some(g) = state.glob() else { return };
    let n = state.glob_matches();
    let lines = vec![
        field("Pattern", Span::raw(format!("{g}▏"))),
        Line::from(vec![
            Span::raw("matches "),
            Span::styled(n.to_string(), theme::number()),
            Span::raw(if n == 1 { " tag" } else { " tags" }),
        ]),
        Line::from(""),
        Line::styled(
            "* any run, ? one char; matched against the whole tag",
            theme::muted(),
        ),
    ];
    let area = centered(screen, 62, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .style(theme::base())
                .borders(Borders::ALL)
                .border_style(theme::border_focus())
                .title(Line::styled(" Mark tags by pattern ", theme::title())),
        ),
        area,
    );
}

fn section(v: &mut Vec<Line<'static>>, name: &str, n: usize, rows: Vec<Line<'static>>) {
    if rows.is_empty() {
        return;
    }
    if !v.is_empty() {
        v.push(Line::from(""));
    }
    v.push(Line::styled(format!("{name} ({n})"), theme::title()));
    v.extend(rows);
}

fn exclude_text(r: &ExcludeReason) -> String {
    match r {
        ExcludeReason::DependencyUnknown => {
            "dependency unknown (manifest could not be read)".into()
        }
        ExcludeReason::ChildOfKeptIndex(i) => format!("child of kept index {}", i.short()),
        ExcludeReason::AttachedToKeptManifest(m) => {
            format!("attached to kept manifest {}", m.short())
        }
        ExcludeReason::NeedsWholeManifest => "press w to delete the whole manifest".into(),
    }
}

fn side_effect_text(e: &SideEffect) -> String {
    match e {
        SideEffect::UnreferencedChild { child, index } => format!(
            "{} becomes unreferenced (child of deleted index {})",
            child.short(),
            index.short()
        ),
        SideEffect::OrphanedReferrer { referrer, subject } => format!(
            "referrer {} of {} may remain or be removed by the registry",
            referrer.short(),
            subject.short()
        ),
        SideEffect::FallbackIndexStale { tag } => format!("fallback index {tag} becomes stale"),
        SideEffect::ReferrersUnknown { digest } => {
            format!("referrers of {} unknown", digest.short())
        }
    }
}

/// A manifest row: `<short>  <tags or <untagged>>  <size>`; `a` is its inventory row.
fn manifest_row(a: Option<&ArtifactSummary>, d: &Digest) -> Line<'static> {
    let tags = match a {
        Some(a) if !a.tags.is_empty() => Span::raw(a.tags.join(", ")),
        _ => Span::styled("<untagged>", theme::muted()),
    };
    let size = a.and_then(|a| a.size).map_or_else(
        || Span::styled("—", theme::muted()),
        |n| Span::styled(fmt_size(n), theme::number()),
    );
    Line::from(vec![
        Span::raw(format!("    {}  ", d.short())),
        tags,
        Span::raw("  "),
        size,
    ])
}

fn plan_lines(m: &DeleteModal, plan: &DeletePlan, snap: &DepSnapshot) -> Vec<Line<'static>> {
    let mut v: Vec<Line<'static>> = plan
        .banner
        .iter()
        .map(String::as_str)
        .chain(m.changed.then_some(CHANGED))
        .map(|b| Line::styled(b.to_owned(), theme::warn()))
        .collect();
    let tags = plan
        .remove_tags
        .iter()
        .filter_map(|op| match op {
            DeleteOp::RemoveTag { tag, expected } => {
                Some(Line::from(format!("  {tag}  {}", expected.short())))
            }
            DeleteOp::DeleteManifest { .. } => None,
        })
        .collect::<Vec<_>>();
    section(&mut v, "Tags removed", tags.len(), tags);
    let rows: HashMap<&Digest, &ArtifactSummary> =
        snap.inventory.iter().map(|a| (&a.digest, a)).collect();
    let mut manifests = vec![];
    for (i, layer) in plan.layers.iter().enumerate() {
        manifests.push(Line::styled(format!("  layer {i}"), theme::muted()));
        manifests.extend(layer.iter().map(|d| manifest_row(rows.get(d).copied(), d)));
    }
    section(
        &mut v,
        "Manifests deleted",
        plan.layers.iter().map(Vec::len).sum(),
        manifests,
    );
    let collateral = plan
        .collateral
        .iter()
        .map(|(d, tags)| {
            let mut tags = tags.clone();
            tags.sort();
            Line::from(vec![
                Span::raw(format!("  {}  ", d.short())),
                Span::styled(tags.join(", "), theme::number()),
            ])
        })
        .collect::<Vec<_>>();
    section(
        &mut v,
        "Collateral tags",
        plan.collateral.iter().map(|(_, t)| t.len()).sum(),
        collateral,
    );
    let excluded = plan
        .excluded
        .iter()
        .map(|(d, r)| {
            Line::from(vec![
                Span::raw(format!("  {}  ", d.short())),
                Span::styled(exclude_text(r), theme::muted()),
            ])
        })
        .collect::<Vec<_>>();
    section(&mut v, "Excluded", excluded.len(), excluded);
    let effects = plan
        .side_effects
        .iter()
        .map(|e| Line::from(format!("  {}", side_effect_text(e))))
        .collect::<Vec<_>>();
    section(&mut v, "Side effects", effects.len(), effects);
    let t = totals(plan, snap);
    v.push(Line::from(""));
    let mut totals_line = vec![
        Span::styled("Totals  ", theme::muted()),
        Span::styled(t.tags.to_string(), theme::number()),
        Span::raw(if t.tags == 1 { " tag · " } else { " tags · " }),
        Span::styled(t.manifests.to_string(), theme::number()),
        Span::raw(if t.manifests == 1 {
            " manifest · "
        } else {
            " manifests · "
        }),
    ];
    match t.size {
        Some(n) => totals_line.extend([
            Span::styled(fmt_size(n), theme::number()),
            Span::raw(" reported"),
        ]),
        None => totals_line.push(Span::styled("size not reported", theme::muted())),
    }
    v.push(Line::from(totals_line));
    v
}

fn op_label(op: &DeleteOp) -> String {
    match op {
        DeleteOp::RemoveTag { tag, expected } => format!("tag {tag} ({})", expected.short()),
        DeleteOp::DeleteManifest { digest } => format!("manifest {}", digest.short()),
    }
}

fn report_lines(
    state: &State,
    m: &DeleteModal,
    report: &DeleteReport,
    stopped: Option<&RegistryError>,
    refreshed: bool,
) -> Vec<Line<'static>> {
    let reg = state.registries().iter().find(|r| r.id == m.registry);
    let n = |f: fn(&OpOutcome) -> bool| report.results.iter().filter(|(_, o)| f(o)).count();
    let counts = [
        (n(|o| *o == OpOutcome::Done), "done", theme::ok()),
        (
            n(|o| matches!(o, OpOutcome::Failed(_))),
            "failed",
            theme::bad(),
        ),
        (
            n(|o| matches!(o, OpOutcome::Skipped(_))),
            "skipped",
            theme::muted(),
        ),
        (
            n(|o| matches!(o, OpOutcome::Uncertain(_))),
            "uncertain",
            theme::warn(),
        ),
        (
            n(|o| *o == OpOutcome::NotAttempted),
            "not attempted",
            theme::muted(),
        ),
    ];
    let mut head = vec![];
    for (i, (c, name, style)) in counts.into_iter().enumerate() {
        if i > 0 {
            head.push(Span::raw("  "));
        }
        head.push(Span::styled(format!("{c} {name}"), style));
    }
    let mut v = vec![Line::from(head), Line::from("")];
    let key = CacheKey::Inventory(m.registry.clone(), m.repo.clone());
    let inventory = match state.value(&key) {
        Some(CacheValue::Inventory(a)) if refreshed => a.as_slice(),
        _ => &[],
    };
    for (op, outcome) in &report.results {
        let (text, style) = match outcome {
            OpOutcome::Done => continue,
            OpOutcome::Failed(e) => (format!("failed: {e}"), theme::bad()),
            OpOutcome::Skipped(e) => (format!("skipped: {e}"), theme::muted()),
            OpOutcome::Uncertain(e) => {
                let seen = reg
                    .filter(|_| refreshed)
                    .and_then(|r| observed(op, inventory, r.caps.inventory));
                let tail = seen.map(|s| format!(" — {s}")).unwrap_or_default();
                (format!("uncertain: {e}{tail}"), theme::warn())
            }
            OpOutcome::NotAttempted => ("not attempted".to_owned(), theme::muted()),
        };
        v.push(Line::from(vec![
            Span::raw(format!("{}  ", op_label(op))),
            Span::styled(text, style),
        ]));
    }
    if let Some(e) = stopped {
        v.push(Line::from(""));
        v.push(Line::styled(
            format!("run stopped: {}", e.kind()),
            theme::bad(),
        ));
    }
    if report.aborted_unsupported {
        v.push(Line::styled(
            format!("deletion disabled on {}", m.registry),
            theme::bad(),
        ));
    }
    v
}

/// The preview through the report; nothing while hidden. Returns the largest useful scroll.
pub fn delete(frame: &mut Frame, screen: Rect, state: &State) -> u16 {
    let Some(m) = state.delete_modal().filter(|m| !m.hidden) else {
        return 0;
    };
    let title = match m.stage {
        DelStage::Report { .. } => " Delete result ".to_owned(),
        _ => format!(" Delete from {} ", m.repo),
    };
    let area = centered(screen, 90, 32);
    let block = Block::new()
        .style(theme::base())
        .borders(Borders::ALL)
        .border_style(theme::border_focus())
        .title(Line::styled(title, theme::title()));
    let inner = block.inner(area);
    let preview = |m: &DeleteModal| match (&m.plan, &m.snapshot) {
        (Some(p), Some(s)) => plan_lines(m, p, s),
        _ => vec![],
    };
    let (body, footer) = match &m.stage {
        DelStage::Snapshot => (vec![muted_line("taking dependency snapshot…")], None),
        DelStage::Revalidate => (vec![muted_line("revalidating…")], None),
        DelStage::Blocked(reason) => (vec![Line::styled(reason.clone(), theme::bad())], None),
        DelStage::Preview => {
            let mut f = key_hint((
                "w",
                &format!("whole-manifest: {}", if m.whole { "on" } else { "off" }),
            ))
            .to_vec();
            for k in [("Enter", "confirm"), ("Esc", "cancel")] {
                f.push(Span::raw("   "));
                f.extend(key_hint(k));
            }
            (preview(m), Some(Line::from(f)))
        }
        DelStage::Confirm => {
            let typed = if m.typed == m.repo.0 {
                theme::ok()
            } else {
                Default::default()
            };
            let f = Line::from(vec![
                Span::styled(format!("Type {} to confirm: ", m.repo), theme::muted()),
                Span::styled(m.typed.clone(), typed),
                Span::raw("▏"),
            ]);
            (preview(m), Some(f))
        }
        DelStage::Running => {
            let n = m.plan.as_ref().map_or(0, |p| p.ops().len());
            let l = Line::from(vec![
                Span::styled(format!("Deleting from {}… ", m.repo), theme::title()),
                Span::styled(n.to_string(), theme::number()),
                Span::raw(if n == 1 { " operation" } else { " operations" }),
            ]);
            (vec![l], None)
        }
        DelStage::Report {
            report,
            stopped,
            refreshed,
        } => (
            report_lines(state, m, report, stopped.as_ref(), *refreshed),
            None,
        ),
    };
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let foot_h = if footer.is_some() { 2 } else { 0 };
    let [b, f] = Layout::vertical([Constraint::Min(0), Constraint::Length(foot_h)]).areas(inner);
    let body = Paragraph::new(body).wrap(Wrap { trim: false });
    let max = body.line_count(b.width).saturating_sub(b.height as usize) as u16;
    frame.render_widget(body.scroll((m.scroll.min(max), 0)), b);
    if let Some(footer) = footer {
        frame.render_widget(Paragraph::new(vec![Line::from(""), footer]), f);
    }
    max
}

fn muted_line(s: &str) -> Line<'static> {
    Line::styled(s.to_owned(), theme::muted())
}

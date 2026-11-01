//! The three list panes.

use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::{Block, Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap},
};
use time::OffsetDateTime;

use super::{
    failure_text, fetched_at, fit, fmt_hm, fmt_size, fmt_time, inner_width, loading_suffix,
    muted_text, pane_block, placeholder, remaining, theme,
};
use crate::app::state::{CacheKey, CacheValue, LoginState, Pane, RepoRow, SortKey, State};
use crate::domain::{ArtifactStatus, ArtifactSummary, Inventory};

/// `left`, then `right` flush right (right wins when tight), over `w` columns.
fn two_cols(left: &str, right: Vec<Span<'static>>, w: usize) -> Line<'static> {
    let r: usize = right.iter().map(|s| s.content.chars().count()).sum();
    let l = fit(left, w.saturating_sub(r + usize::from(r > 0)));
    let pad = w.saturating_sub(l.chars().count() + r);
    let mut spans = vec![Span::raw(l), Span::raw(" ".repeat(pad))];
    spans.extend(right);
    Line::from(spans)
}

fn selected(focused: bool) -> Style {
    if focused {
        theme::selected()
    } else {
        theme::selected_idle()
    }
}

fn draw_list(
    frame: &mut Frame,
    area: Rect,
    block: Block<'static>,
    items: Vec<ListItem<'static>>,
    empty: Text<'static>,
    highlight: Style,
    view: &mut ListState,
) {
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(empty)
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
        return;
    }
    let list = List::new(items).block(block).highlight_style(highlight);
    frame.render_stateful_widget(list, area, view);
}

pub fn registries(
    frame: &mut Frame,
    area: Rect,
    state: &State,
    now: OffsetDateTime,
    view: &mut ListState,
) {
    let w = inner_width(area);
    let items = state
        .registries()
        .iter()
        .map(|r| {
            let login = match &r.login {
                None => None,
                Some(LoginState::Running) => Some(Span::styled("…", theme::muted())),
                Some(LoginState::Done { expires_at: None }) => Some(Span::styled("✓", theme::ok())),
                Some(LoginState::Done {
                    expires_at: Some(t),
                }) => Some(Span::styled(
                    format!("✓ {}", remaining(*t, now)),
                    theme::ok(),
                )),
            };
            let marks: Vec<Span> = login
                .into_iter()
                .chain(r.auth_failed.then(|| Span::styled("!", theme::bad())))
                .flat_map(|m| [Span::raw(" "), m])
                .skip(1)
                .collect();
            ListItem::new(two_cols(&r.id.0, marks, w))
        })
        .collect();
    let focused = state.focus() == Pane::Registries;
    let block = pane_block("Registries", "", "", focused);
    view.select(Some(state.selected(Pane::Registries)));
    draw_list(
        frame,
        area,
        block,
        items,
        muted_text("(empty)"),
        selected(focused),
        view,
    );
}

pub fn repos(frame: &mut Frame, area: Rect, state: &State, view: &mut ListState) {
    let w = inner_width(area);
    let mut meta = String::new();
    let mut empty = muted_text("");
    if let Some(reg) = state.selected_registry() {
        let key = CacheKey::Repos(reg.id.clone());
        meta += &loading_suffix(state, &key);
        empty = match state.value(&key) {
            Some(_) if state.filter(Pane::Repos).is_empty() => muted_text("(empty)"),
            Some(_) => muted_text("(no match)"),
            None => failure_text(state, &key, "r retry")
                .unwrap_or_else(|| muted_text(placeholder(state, &key))),
        };
    }
    meta += &filter_suffix(state, Pane::Repos);
    let rows = state.repo_rows();
    let any_pinned = rows
        .iter()
        .any(|r| matches!(r, RepoRow::Repo { pinned: true, .. }));
    let pad = if any_pinned { "  " } else { "" };
    // Prefix of the group open at each depth: a repo in a group shows its path below it.
    let mut open: Vec<&str> = vec![];
    let items = rows
        .iter()
        .map(|row| {
            let (repo, depth, pinned) = match row {
                RepoRow::Group {
                    prefix,
                    label,
                    count,
                    depth,
                    expanded,
                } => {
                    open.truncate(*depth);
                    open.push(prefix);
                    let arrow = if *expanded { '▾' } else { '▸' };
                    let indent = "  ".repeat(*depth);
                    let name = format!("{pad}{indent}{arrow} {label} ({count})");
                    return ListItem::new(two_cols(&name, vec![], w));
                }
                RepoRow::Repo {
                    repo,
                    depth,
                    pinned,
                } => (repo, *depth, *pinned),
            };
            let path = &repo.path.0;
            let shown = depth
                .checked_sub(1)
                .and_then(|d| open.get(d))
                .and_then(|p| path.strip_prefix(&format!("{p}/")))
                .unwrap_or(path);
            let lead = if pinned { "★ " } else { pad };
            let name = format!("{lead}{}{shown}", "  ".repeat(depth));
            if state.repo_not_found(&repo.path) {
                let nf = vec![Span::raw("not found")];
                return ListItem::new(two_cols(&name, nf, w).patch_style(theme::muted()));
            }
            let n = state
                .repo_count(&repo.path)
                .map(|n| Span::styled(n.to_string(), theme::number()))
                .into_iter()
                .collect();
            ListItem::new(two_cols(&name, n, w))
        })
        .collect();
    let focused = state.focus() == Pane::Repos;
    let block = pane_block("Repos", "", &meta, focused);
    view.select(Some(state.selected(Pane::Repos)));
    draw_list(frame, area, block, items, empty, selected(focused), view);
}

fn filter_suffix(state: &State, pane: Pane) -> String {
    let f = state.filter(pane);
    let editing = state.filter_editing() && state.focus() == pane;
    match (f.is_empty(), editing) {
        (true, false) => String::new(),
        (_, true) => format!(" · /{f}_"),
        _ => format!(" · /{f}"),
    }
}

/// Tags, dim when there are none; a status other than active is appended (the row is dimmed).
fn tags_cell(a: &ArtifactSummary) -> Span<'static> {
    if a.tags.is_empty() {
        return Span::styled("<untagged>", theme::muted());
    }
    let mut s = a.tags.join(", ");
    match &a.status {
        ArtifactStatus::Active => {}
        ArtifactStatus::Archived => s += " archived",
        ArtifactStatus::Other(o) => s += &format!(" {}", o.to_lowercase()),
    }
    Span::raw(s)
}

/// A muted `—` for a value that is not known.
fn dash() -> Span<'static> {
    Span::styled("—", theme::muted())
}

/// Reported size, else computed from the manifest; `—` while neither is known (an unloaded
/// manifest, or an index).
fn size_cell(state: &State, a: &ArtifactSummary, style: Style) -> Span<'static> {
    state
        .display_size(a)
        .map_or_else(dash, |n| Span::styled(fmt_size(n), style))
}

pub fn images(frame: &mut Frame, area: Rect, state: &State, view: &mut TableState) {
    let focused = state.focus() == Pane::Images;
    let (Some(reg), Some(repo)) = (state.selected_registry(), state.selected_repo()) else {
        frame.render_widget(
            Paragraph::new("").block(pane_block("Images", "", "", focused)),
            area,
        );
        return;
    };
    let key = CacheKey::Inventory(reg.id.clone(), repo.path.clone());
    let mut meta = String::new();
    let fetched = state
        .entry(&key)
        .and_then(fetched_at)
        .map_or(String::new(), |t| format!(" · {} UTC", fmt_hm(t)));
    if reg.caps.inventory == Inventory::TaggedOnly {
        meta += " · tagged only";
    }
    match state.marked_count() {
        0 => {}
        n => meta += &format!(" · {n} marked"),
    }
    meta += &loading_suffix(state, &key);
    meta += &filter_suffix(state, Pane::Images);
    let block = pane_block(&format!("Images: {}", repo.path), &fetched, &meta, focused);

    let rows = state.visible_images();
    if rows.is_empty() {
        let msg = match state.value(&key) {
            Some(CacheValue::Inventory(_)) if state.filter(Pane::Images).is_empty() => {
                muted_text("(empty)")
            }
            Some(_) => muted_text("(no match)"),
            None => failure_text(state, &key, "r retry")
                .unwrap_or_else(|| muted_text(placeholder(state, &key))),
        };
        frame.render_widget(
            Paragraph::new(msg).wrap(Wrap { trim: false }).block(block),
            area,
        );
        return;
    }

    let (sort, desc) = state.image_sort();
    let head = |name: &str, k: SortKey| {
        if sort == k {
            format!("{name}{}", if desc { "↓" } else { "↑" })
        } else {
            name.to_owned()
        }
    };
    let show_pushed = reg.caps.pushed_at && area.width >= 64;
    // 1-column slots for the range anchor / delete mark and the local marker, then the tags.
    let mut widths = vec![
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ];
    let mut header = vec![
        Cell::from(""),
        Cell::from(""),
        Cell::from(head("Tag", SortKey::Tag)),
    ];
    if show_pushed {
        widths.push(Constraint::Length(16));
        header.push(Cell::from(head("Pushed UTC", SortKey::Pushed)));
    }
    widths.push(Constraint::Length(8));
    header.push(Cell::from(
        Line::from(head("Size*", SortKey::Size)).right_aligned(),
    ));
    if reg.caps.last_pulled {
        widths.push(Constraint::Length(16));
        header.push(Cell::from(head("Pulled≈ UTC", SortKey::Pulled)));
    }
    let local = match state.value(&CacheKey::LocalImages) {
        Some(CacheValue::LocalImages(all)) => all.as_slice(),
        _ => &[],
    };
    let cell = |s: Span<'static>| Cell::from(Line::from(s));
    let body: Vec<Row> = rows
        .iter()
        .map(|a| {
            // An archived row stays entirely muted, so value colors give way to the row style.
            let archived = a.status != ArtifactStatus::Active;
            let (time, number) = if archived {
                (Style::new(), Style::new())
            } else {
                (theme::time(), theme::number())
            };
            let is_local =
                crate::docker::local_match(local, &reg.ref_host, &repo.path, &a.digest).is_some();
            let mut c = vec![
                cell(match state.mark_state(a) {
                    _ if state.range_anchor() == Some(&a.digest) => {
                        Span::styled("▏", theme::accent())
                    }
                    _ if state.compare_base().is_some_and(|b| b.digest == a.digest) => {
                        Span::styled("≡", theme::accent())
                    }
                    Some(true) => Span::styled("✗", theme::bad()),
                    Some(false) => Span::styled("~", theme::warn()),
                    None => Span::raw(" "),
                }),
                cell(if is_local {
                    Span::styled("●", theme::accent())
                } else {
                    Span::raw(" ")
                }),
                cell(tags_cell(a)),
            ];
            if show_pushed {
                c.push(cell(
                    a.pushed_at
                        .map_or_else(dash, |t| Span::styled(fmt_time(t), time)),
                ));
            }
            c.push(Cell::from(
                Line::from(size_cell(state, a, number)).right_aligned(),
            ));
            if reg.caps.last_pulled {
                c.push(cell(a.last_pulled.map_or_else(
                    || Span::styled("never", theme::muted()),
                    |t| Span::styled(fmt_time(t), time),
                )));
            }
            let row = Row::new(c);
            if archived {
                row.style(theme::muted())
            } else {
                row
            }
        })
        .collect();
    let table = Table::new(body, widths)
        .header(Row::new(header).style(theme::header()))
        .block(block)
        .row_highlight_style(selected(focused));
    view.select(Some(state.selected(Pane::Images)));
    frame.render_stateful_widget(table, area, view);
}

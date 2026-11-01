//! Detail pane: registry / repository / image (tabbed).

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{fmt_size, fmt_time, fmt_utc, placeholder, remaining, theme};
use crate::app::cache::Entry;
use crate::app::mask::{mask_command, mask_map, mask_pairs};
use crate::app::state::{
    CacheKey, CacheValue, LoginState, Pane, RegistryView, RepoRow, State, Tab,
};
use crate::config::AuthCfg;
use crate::domain::instruction;
use crate::domain::{
    ArtifactStatus, ArtifactSummary, Descriptor, Digest, HistoryEntry, ImageConfig, Inventory,
    ManifestDocument, ManifestKind, Reference, RepoSummary, image_ref,
};

const TABS: [(Tab, &str); 5] = [
    (Tab::Overview, "1 Overview"),
    (Tab::Layers, "2 Layers"),
    (Tab::Config, "3 Config"),
    (Tab::History, "4 History"),
    (Tab::Raw, "5 Raw"),
];

/// Returns the largest useful scroll, for `ViewState`.
pub fn render(frame: &mut Frame, area: Rect, state: &State, now: OffsetDateTime) -> u16 {
    let (title, lines) = content(state, now);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(theme::border())
        .title(title);
    // Clamp here: the state's scroll is unbounded. Long lines are cut, not wrapped (keeps the
    // line count, and so the scroll range, exact).
    let inner_h = area.height.saturating_sub(2) as usize;
    let max = lines.len().saturating_sub(inner_h);
    let scroll = (state.scroll() as usize).min(max) as u16;
    frame.render_widget(Paragraph::new(lines).block(block).scroll((scroll, 0)), area);
    max.min(u16::MAX as usize) as u16
}

/// Block title: ` name ` (bold) followed by `rest` spans.
fn title(name: String, rest: Vec<Span<'static>>) -> Line<'static> {
    let mut v = vec![Span::raw(" "), Span::styled(name, theme::title())];
    v.extend(rest);
    v.push(Span::raw(" "));
    Line::from(v)
}

fn content(state: &State, now: OffsetDateTime) -> (Line<'static>, Vec<Line<'static>>) {
    let Some(reg) = state.selected_registry() else {
        return (title("Detail".into(), vec![]), vec![]);
    };
    let focus = state.focus();
    if focus == Pane::Images
        && let (Some(repo), Some(img)) = (state.selected_repo(), state.selected_image())
    {
        return image(state, reg, repo, img);
    }
    if focus != Pane::Registries
        && let Some(RepoRow::Group { prefix, count, .. }) = state.selected_row()
    {
        let n = Span::raw(format!(" · {count} repos"));
        return (title(format!("Group {prefix}"), vec![n]), vec![]);
    }
    if focus != Pane::Registries
        && let Some(repo) = state.selected_repo()
    {
        return (
            title(format!("Repo {}", repo.path), vec![]),
            repo_lines(state, reg, repo, now),
        );
    }
    (
        title(format!("Registry {}", reg.id), vec![]),
        registry_view(state, reg, now),
    )
}

fn kv(label: &str, value: impl Into<String>) -> Line<'static> {
    kv_span(label, Span::raw(value.into()))
}

fn kv_span(label: &str, value: Span<'static>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<11}"), theme::label()),
        value,
    ])
}

fn text(s: &str) -> Vec<Line<'static>> {
    vec![Line::from(s.to_owned())]
}

/// Provider details of `key`, or the one-line reason there are none (nothing while loading).
fn details_lines(state: &State, key: &CacheKey) -> Vec<Line<'static>> {
    if let Some(CacheValue::Details(d)) = state.value(key) {
        return d.fields.iter().map(|(k, v)| kv(k, v.clone())).collect();
    }
    if let Some(Entry::Failed { .. }) = state.entry(key) {
        return vec![kv_span(
            "Details",
            Span::styled("unavailable — press r to retry", theme::muted()),
        )];
    }
    vec![]
}

fn registry_view(state: &State, reg: &RegistryView, now: OffsetDateTime) -> Vec<Line<'static>> {
    let mut v = details_lines(state, &CacheKey::RegistryDetails(reg.id.clone()));
    v.extend(registry_lines(reg, now));
    v
}

fn registry_lines(reg: &RegistryView, now: OffsetDateTime) -> Vec<Line<'static>> {
    let auth = match &reg.auth {
        AuthCfg::Ecr => "ecr".to_owned(),
        AuthCfg::DockerConfig => "docker-config".to_owned(),
        AuthCfg::Anonymous => "anonymous".to_owned(),
        AuthCfg::Basic {
            username,
            password_env,
        } => format!("basic (user {username}, password from ${password_env})"),
    };
    let c = &reg.caps;
    let caps: Vec<&str> = [
        (c.pushed_at, "pushed-at"),
        (c.last_pulled, "last-pulled"),
        (c.delete_tag, "delete-tag"),
        (c.delete_manifest, "delete-manifest"),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, n)| *n)
    .collect();
    let inv = match c.inventory {
        Inventory::Complete => "complete",
        Inventory::TaggedOnly => "tagged only",
    };
    let mut v = vec![
        kv("Endpoint", reg.endpoint.as_str()),
        kv("Provider", reg.provider),
        kv("Auth", auth),
    ];
    let login = match &reg.login {
        None => None,
        Some(LoginState::Running) => Some("running…".to_owned()),
        Some(LoginState::Done { expires_at: None }) => Some("logged in (no expiry)".to_owned()),
        Some(LoginState::Done {
            expires_at: Some(t),
        }) => Some(format!("expires {} ({})", fmt_utc(*t), remaining(*t, now))),
    };
    v.extend(login.map(|l| kv("Login", l)));
    v.extend([
        kv("Inventory", inv),
        kv(
            "Caps",
            if caps.is_empty() {
                "none".to_owned()
            } else {
                caps.join(", ")
            },
        ),
    ]);
    if reg.auth_failed {
        v.push(kv("Warning", "authentication failed on the last request"));
    }
    v
}

fn repo_lines(
    state: &State,
    reg: &RegistryView,
    repo: &RepoSummary,
    now: OffsetDateTime,
) -> Vec<Line<'static>> {
    let mut v = vec![kv("Repository", repo.path.0.clone())];
    if let Some(t) = repo.created_at {
        v.push(kv_span("Created", Span::styled(fmt_utc(t), theme::time())));
    }
    if let Some(n) = state.repo_count(&repo.path) {
        v.push(kv_span(
            "Images",
            Span::styled(n.to_string(), theme::number()),
        ));
    }
    v.extend(details_lines(
        state,
        &CacheKey::RepoDetails(reg.id.clone(), repo.path.clone()),
    ));
    v.extend(registry_lines(reg, now));
    v
}

/// Everything the image tabs are drawn from.
struct Img<'a> {
    state: &'a State,
    reg: &'a RegistryView,
    repo: &'a RepoSummary,
    img: &'a ArtifactSummary,
    top: Option<&'a ManifestDocument>,
    /// Selected platform of an index.
    child: Option<&'a Descriptor>,
    /// Digest of the image manifest (the platform's for an index).
    target: Digest,
    image_manifest: Option<&'a ManifestDocument>,
}

impl<'a> Img<'a> {
    fn new(
        state: &'a State,
        reg: &'a RegistryView,
        repo: &'a RepoSummary,
        img: &'a ArtifactSummary,
    ) -> Self {
        let top = state.manifest(&img.digest);
        let child = match top.map(|m| &m.kind) {
            Some(ManifestKind::Index { manifests }) => manifests.get(state.platform()),
            _ => None,
        };
        let target = child.map_or_else(|| img.digest.clone(), |c| c.digest.clone());
        let image_manifest = state
            .manifest(&target)
            .filter(|m| matches!(m.kind, ManifestKind::Image { .. }));
        Img {
            state,
            reg,
            repo,
            img,
            top,
            child,
            target,
            image_manifest,
        }
    }

    fn manifest_key(&self, d: &Digest) -> CacheKey {
        CacheKey::Manifest(self.reg.id.clone(), self.repo.path.clone(), d.clone())
    }

    fn config_key(&self) -> CacheKey {
        CacheKey::Config(
            self.reg.id.clone(),
            self.repo.path.clone(),
            self.target.clone(),
        )
    }

    fn config_value(&self) -> Option<&'a ImageConfig> {
        match self.state.value(&self.config_key()) {
            Some(CacheValue::Config(Some(c))) => Some(c),
            _ => None,
        }
    }

    /// Why the image manifest / config is not shown yet (blank if never requested).
    fn waiting(&self) -> &'static str {
        let key = if self.image_manifest.is_none() {
            self.manifest_key(&self.target)
        } else {
            self.config_key()
        };
        placeholder(self.state, &key)
    }
}

fn image(
    state: &State,
    reg: &RegistryView,
    repo: &RepoSummary,
    img: &ArtifactSummary,
) -> (Line<'static>, Vec<Line<'static>>) {
    let c = Img::new(state, reg, repo, img);
    let name = img
        .tags
        .first()
        .cloned()
        .unwrap_or_else(|| format!("@{}", img.digest.short()));
    let mut rest = vec![Span::raw(" ─ ")];
    for (i, (t, n)) in TABS.iter().enumerate() {
        if i > 0 {
            rest.push(Span::raw("  "));
        }
        let style = if *t == state.tab() {
            theme::title().add_modifier(Modifier::UNDERLINED)
        } else {
            theme::muted()
        };
        rest.push(Span::styled(*n, style));
    }
    if matches!(state.tab(), Tab::Config | Tab::History | Tab::Raw) && state.reveal() {
        rest.push(Span::styled(" ─ REVEALED", theme::title()));
    }
    let heading = title(format!("Image {name}"), rest);
    let lines = if state.tab() == Tab::Overview {
        overview(&c)
    } else if img.status != ArtifactStatus::Active {
        text("content unavailable while archived")
    } else {
        match state.tab() {
            Tab::Layers => layers(&c),
            Tab::Config => config_tab(&c),
            Tab::History => history(&c),
            Tab::Raw => raw(&c),
            Tab::Overview => vec![],
        }
    };
    (heading, lines)
}

/// Reference, Content, Activity and Local, a blank line between groups.
fn overview(c: &Img) -> Vec<Line<'static>> {
    let (state, img) = (c.state, c.img);
    let host = &c.reg.ref_host;
    let mut reference = vec![];
    if img.tags.is_empty() {
        let r = Reference::Digest(img.digest.clone());
        reference.push(kv("Ref", image_ref(host, &c.repo.path, &r)));
    }
    for (i, t) in img.tags.iter().enumerate() {
        let r = image_ref(host, &c.repo.path, &Reference::Tag(t.clone()));
        reference.push(kv(if i == 0 { "Ref" } else { "" }, r));
    }
    if !img.tags.is_empty() {
        reference.push(kv("Tags", img.tags.join(", ")));
    }
    reference.push(kv("Digest", img.digest.to_string()));

    let mut content = vec![kv(
        "Status",
        match &img.status {
            ArtifactStatus::Active => "active".to_owned(),
            ArtifactStatus::Archived => "archived".to_owned(),
            ArtifactStatus::Other(s) => s.to_lowercase(),
        },
    )];
    if let Some(m) = img
        .media_type
        .as_deref()
        .or(c.top.map(|m| m.media_type.as_str()))
    {
        content.push(kv("Media", m));
    }
    if let Some(a) = img
        .artifact_type
        .as_deref()
        .or(c.top.and_then(|m| m.artifact_type.as_deref()))
    {
        content.push(kv("Artifact", a));
    }
    if let Some(s) = &img.subject {
        content.push(kv("Subject", s.to_string()));
    }
    let size = state.display_size(img).map_or_else(
        || Span::styled("—", theme::muted()),
        |n| Span::styled(fmt_size(n), theme::number()),
    );
    content.push(kv_span("Size*", size));
    if img.status == ArtifactStatus::Active {
        let cfg = c.config_value();
        let platform = cfg
            .and_then(|c| c.platform.as_ref())
            .or(c.child.and_then(|d| d.platform.as_ref()));
        if let Some(p) = platform {
            content.push(kv("Platform", p.to_string()));
        }
        if let Some(t) = cfg.and_then(|c| c.created) {
            content.push(kv_span("Created", Span::styled(fmt_utc(t), theme::time())));
        }
    }

    let mut activity = vec![];
    if let Some(t) = img.pushed_at {
        activity.push(kv_span("Pushed", Span::styled(fmt_utc(t), theme::time())));
    }
    if c.reg.caps.last_pulled {
        activity.push(kv_span(
            "Pulled≈",
            img.last_pulled.map_or_else(
                || Span::styled("never", theme::muted()),
                |t| Span::styled(fmt_utc(t), theme::time()),
            ),
        ));
    }

    let mut local = vec![kv("Local", local(c))];
    if img.status == ArtifactStatus::Active {
        let key = CacheKey::Referrers(c.reg.id.clone(), c.repo.path.clone(), img.digest.clone());
        let refs = match state.value(&key) {
            Some(CacheValue::Referrers(Some(r))) => r.len().to_string(),
            Some(_) => "unknown".to_owned(),
            None => match placeholder(state, &key) {
                "" => String::new(),
                "loading…" => "loading…".to_owned(),
                _ => "unknown".to_owned(),
            },
        };
        local.push(kv("Attached", refs));
        if let Some(CacheValue::Referrers(Some(r))) = state.value(&key) {
            local.extend(r.iter().map(referrer_row));
        }
    }

    let mut v = vec![];
    for g in [reference, content, activity, local] {
        if g.is_empty() {
            continue;
        }
        if !v.is_empty() {
            v.push(Line::from(""));
        }
        v.extend(g);
    }
    v
}

fn referrer_kind(artifact_type: &str) -> String {
    match artifact_type {
        "application/vnd.dev.cosign.artifact.sig.v1+json"
        | "application/vnd.dev.cosign.simplesigning.v1+json" => "cosign signature",
        "application/spdx+json" => "SBOM SPDX",
        "application/vnd.cyclonedx+json" => "SBOM CycloneDX",
        "application/vnd.in-toto+json" => "in-toto attestation",
        other => other,
    }
    .to_owned()
}

/// `  kind  sha256:… size created`, the date only when the descriptor says when.
fn referrer_row(d: &Descriptor) -> Line<'static> {
    let kind = referrer_kind(d.artifact_type.as_deref().unwrap_or(&d.media_type));
    let mut spans = vec![
        Span::raw(format!("  {kind}  {}  ", short(&d.digest))),
        Span::styled(fmt_size(d.size), theme::number()),
    ];
    let created = d
        .annotations
        .get("org.opencontainers.image.created")
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok());
    if let Some(t) = created {
        spans.push(Span::styled(format!("  {}", fmt_time(t)), theme::time()));
    }
    Line::from(spans)
}

fn local(c: &Img) -> String {
    let key = CacheKey::LocalImages;
    match c.state.value(&key) {
        Some(CacheValue::LocalImages(all)) => {
            match crate::docker::local_match(all, &c.reg.ref_host, &c.repo.path, &c.img.digest) {
                Some(l) => {
                    let id = l.id.strip_prefix("sha256:").unwrap_or(&l.id);
                    format!("✓ present ({})", id.chars().take(12).collect::<String>())
                }
                None => "not present".to_owned(),
            }
        }
        _ => match c.state.entry(&key) {
            Some(Entry::Loading { .. }) => "checking…".to_owned(),
            Some(Entry::Failed { .. }) => "unknown (Docker unavailable)".to_owned(),
            _ => String::new(),
        },
    }
}

fn short(d: &Digest) -> String {
    format!("{}:{}", d.algorithm(), d.short())
}

fn bold(s: String) -> Line<'static> {
    Line::from(s).style(Style::new().add_modifier(Modifier::BOLD))
}

/// A size right-aligned to 8 columns, in the number color.
fn size_span(size: u64) -> Span<'static> {
    Span::styled(format!("{:>8}", fmt_size(size)), theme::number())
}

fn layer_rows(layers: &[Descriptor]) -> Vec<Line<'static>> {
    layers
        .iter()
        .enumerate()
        .map(|(i, l)| {
            Line::from(vec![
                Span::raw(format!("{:>3}  {}  ", i + 1, short(&l.digest))),
                size_span(l.size),
                Span::raw(format!("  {}", l.media_type)),
            ])
        })
        .collect()
}

fn layers(c: &Img) -> Vec<Line<'static>> {
    let Some(top) = c.top else {
        return text(placeholder(c.state, &c.manifest_key(&c.img.digest)));
    };
    let ManifestKind::Index { manifests } = &top.kind else {
        return match &top.kind {
            ManifestKind::Image { layers, .. } => layer_rows(layers),
            ManifestKind::Index { .. } => vec![],
        };
    };
    let platform = |d: &Descriptor| {
        d.platform
            .as_ref()
            .map_or("unknown".to_owned(), |p| p.to_string())
    };
    let mut v = vec![bold("Platforms ([ ] to select)".into())];
    for (i, m) in manifests.iter().enumerate() {
        let mark = if i == c.state.platform() { ">" } else { " " };
        v.push(Line::from(vec![
            Span::raw(format!("{mark} {:<16} {}  ", platform(m), short(&m.digest))),
            size_span(m.size),
        ]));
    }
    if let Some(child) = c.child {
        v.push(Line::from(""));
        v.push(bold(format!("Layers of {}", platform(child))));
        match c.image_manifest.map(|m| &m.kind) {
            Some(ManifestKind::Image { layers, .. }) => v.extend(layer_rows(layers)),
            _ => v.extend(text(c.waiting())),
        }
    }
    v
}

/// The config, or the one-line reason it is not available.
fn config_or_msg<'a>(c: &Img<'a>) -> Result<&'a ImageConfig, Vec<Line<'static>>> {
    if let Some(cfg) = c.config_value() {
        return Ok(cfg);
    }
    Err(match c.state.value(&c.config_key()) {
        Some(CacheValue::Config(None)) => text("not an image config"),
        _ => text(c.waiting()),
    })
}

fn config_tab(c: &Img) -> Vec<Line<'static>> {
    let cfg = match config_or_msg(c) {
        Ok(cfg) => cfg,
        Err(m) => return m,
    };
    let reveal = c.state.reveal();
    let cmd = |a: &[String]| {
        let s = a.join(" ");
        if reveal { s } else { mask_command(&s) }
    };
    let mut v = vec![
        kv("Entrypoint", cmd(&cfg.entrypoint)),
        kv("Cmd", cmd(&cfg.cmd)),
        kv("WorkingDir", cfg.working_dir.clone().unwrap_or_default()),
        kv("User", cfg.user.clone().unwrap_or_default()),
        kv("Ports", cfg.exposed_ports.join(" ")),
        kv("Volumes", cfg.volumes.join(" ")),
    ];
    let labels = if reveal {
        cfg.labels.clone()
    } else {
        mask_map(&cfg.labels)
    };
    v.push(kv("Labels", if labels.is_empty() { "-" } else { "" }));
    v.extend(
        labels
            .iter()
            .map(|(k, val)| Line::from(format!("  {k}={val}"))),
    );
    let env = if reveal {
        cfg.env.clone()
    } else {
        mask_pairs(&cfg.env)
    };
    v.push(kv("Env", if env.is_empty() { "-" } else { "" }));
    v.extend(
        env.iter()
            .map(|(k, val)| Line::from(format!("  {k}={val}"))),
    );
    v
}

/// The layer size of each step: non-empty steps take the layers in order, empty steps get
/// `None`. `None` overall when the counts differ.
fn step_sizes(history: &[HistoryEntry], layers: &[Descriptor]) -> Option<Vec<Option<u64>>> {
    if history.iter().filter(|h| !h.empty_layer).count() != layers.len() {
        return None;
    }
    let mut sizes = layers.iter().map(|l| l.size);
    Some(
        history
            .iter()
            .map(|h| if h.empty_layer { None } else { sizes.next() })
            .collect(),
    )
}

fn history(c: &Img) -> Vec<Line<'static>> {
    let cfg = match config_or_msg(c) {
        Ok(cfg) => cfg,
        Err(m) => return m,
    };
    if cfg.history.is_empty() {
        return text("(empty)");
    }
    let layers = match c.image_manifest.map(|m| &m.kind) {
        Some(ManifestKind::Image { layers, .. }) => Some(layers),
        _ => None,
    };
    let sizes = layers.and_then(|l| step_sizes(&cfg.history, l));
    let head = Line::styled(
        format!(
            "{:>3}  {:<16}  {:>8}  {:>8}  {}",
            "#", "Created (UTC)", "Size", "Total", "Instruction"
        ),
        theme::header(),
    );
    let mut total = 0;
    let mut v = vec![head];
    for (i, h) in cfg.history.iter().enumerate() {
        let by = instruction(&h.created_by);
        let by = if c.state.reveal() {
            by
        } else {
            mask_command(&by)
        };
        let when = h.created.map_or(String::new(), fmt_time);
        let size = sizes.as_ref().and_then(|s| s[i]);
        total += size.unwrap_or(0);
        let (size, total) = match size {
            Some(n) => (size_span(n), size_span(total)),
            None => (
                Span::raw(format!("{:8}", "")),
                Span::raw(format!("{:8}", "")),
            ),
        };
        let line = Line::from(vec![
            Span::raw(format!("{:>3}  ", i + 1)),
            Span::styled(format!("{when:<16}"), theme::time()),
            Span::raw("  "),
            size,
            Span::raw("  "),
            total,
            Span::raw(format!("  {by}")),
        ]);
        v.push(if h.empty_layer {
            line.style(theme::muted())
        } else {
            line
        });
    }
    if let (Some(layers), None) = (layers, &sizes) {
        let n = cfg.history.iter().filter(|h| !h.empty_layer).count();
        v.push(Line::from(""));
        v.push(Line::styled(
            format!(
                "layer sizes not shown: {n} non-empty steps, {} layers",
                layers.len()
            ),
            theme::muted(),
        ));
    }
    v
}

fn raw(c: &Img) -> Vec<Line<'static>> {
    match c.state.raw_text() {
        Some(t) => t.lines().map(|l| Line::from(l.to_owned())).collect(),
        None => text(placeholder(c.state, &c.manifest_key(&c.img.digest))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Caps, HistoryEntry, RegistryId};

    #[test]
    fn registry_lines_basic_auth_label() {
        let reg = RegistryView {
            id: RegistryId("harbor".into()),
            ref_host: "h.example".into(),
            endpoint: "https://h.example".parse().unwrap(),
            provider: "oci",
            auth: AuthCfg::Basic {
                username: "ci".into(),
                password_env: "HARBOR_PW".into(),
            },
            caps: Caps {
                inventory: Inventory::TaggedOnly,
                pushed_at: false,
                last_pulled: false,
                delete_tag: false,
                delete_manifest: false,
            },
            auth_failed: false,
            can_login: false,
            login: None,
            pinned: vec![],
        };
        let lines: Vec<String> = registry_lines(&reg, OffsetDateTime::UNIX_EPOCH)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(
            lines.contains(&"Auth       basic (user ci, password from $HARBOR_PW)".to_owned()),
            "{lines:?}"
        );
    }

    fn step(by: &str, empty: bool) -> HistoryEntry {
        HistoryEntry {
            created_by: by.into(),
            created: None,
            empty_layer: empty,
        }
    }

    fn layer(size: u64) -> Descriptor {
        Descriptor {
            media_type: String::new(),
            digest: Digest::parse(&format!("sha256:{}", "a".repeat(64))).unwrap(),
            size,
            platform: None,
            artifact_type: None,
            annotations: Default::default(),
        }
    }

    #[test]
    fn instruction_cleanup() {
        for (from, to) in [
            (r#"/bin/sh -c #(nop)  CMD ["sh"]"#, r#"CMD ["sh"]"#),
            ("/bin/sh -c apk add curl", "RUN apk add curl"),
            (
                "RUN |2 A=1 B=2 /bin/sh -c make",
                "RUN |2 A=1 B=2 /bin/sh -c make",
            ),
            ("COPY . . # buildkit", "COPY . . # buildkit"),
        ] {
            assert_eq!(instruction(from), to);
        }
    }

    #[test]
    fn step_sizes_maps_in_order() {
        let h = [step("a", false), step("b", true), step("c", false)];
        assert_eq!(
            step_sizes(&h, &[layer(10), layer(20)]),
            Some(vec![Some(10), None, Some(20)])
        );
        assert_eq!(step_sizes(&h, &[layer(10), layer(20), layer(30)]), None);
    }

    #[test]
    fn referrer_kind_known_and_raw() {
        for (t, k) in [
            (
                "application/vnd.dev.cosign.artifact.sig.v1+json",
                "cosign signature",
            ),
            (
                "application/vnd.dev.cosign.simplesigning.v1+json",
                "cosign signature",
            ),
            ("application/spdx+json", "SBOM SPDX"),
            ("application/vnd.cyclonedx+json", "SBOM CycloneDX"),
            ("application/vnd.in-toto+json", "in-toto attestation"),
            ("application/x-foo", "application/x-foo"),
        ] {
            assert_eq!(referrer_kind(t), k);
        }
    }
}

//! Compare two images of one repo (`=`): the base mark, the modal's state and its masked text
//! (Summary, Layers, Config, History). The text is built here so `y` copies what is shown.

use std::collections::{BTreeMap, BTreeSet};

use time::{OffsetDateTime, UtcOffset, macros::format_description};

use super::mask::{mask_command, mask_map};
use super::state::{Action, CacheKey, CacheValue, Effect, Pane, State};
use crate::domain::{
    ArtifactStatus, Descriptor, Digest, ImageConfig, ManifestKind, fmt_size, instruction,
};

/// One side of a comparison: an image of the repo and, for an index, its platform row.
#[derive(Clone, Debug, PartialEq)]
pub struct Side {
    pub digest: Digest,
    pub platform: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompareModal {
    pub a: Side,
    pub b: Side,
    pub scroll: u16,
}

pub struct SideData<'a> {
    pub digest: &'a Digest,
    pub size: Option<u64>,
    pub layers: &'a [Descriptor],
    pub config: &'a ImageConfig,
}

const NONE: &str = "—";

fn created(c: &ImageConfig) -> String {
    c.created.map_or_else(
        || NONE.to_owned(),
        |t: OffsetDateTime| {
            t.to_offset(UtcOffset::UTC)
                .format(format_description!("[year]-[month]-[day] [hour]:[minute]"))
                .unwrap_or_else(|_| NONE.to_owned())
        },
    )
}

fn pair(out: &mut Vec<String>, name: &str, a: String, b: String) {
    out.push(format!("  {name:<9} {a} | {b}"));
}

/// `~ k a → b` / `+ k=b` / `− k=a` over the union of keys; changes are decided on the raw maps,
/// shown from the masked ones.
fn map_diff(
    out: &mut Vec<String>,
    title: &str,
    raw: (&BTreeMap<String, String>, &BTreeMap<String, String>),
    shown: (&BTreeMap<String, String>, &BTreeMap<String, String>),
) {
    let keys: BTreeSet<&String> = raw.0.keys().chain(raw.1.keys()).collect();
    let mut rows = vec![];
    for k in keys {
        match (raw.0.get(k), raw.1.get(k)) {
            (Some(x), Some(y)) if x != y => {
                rows.push(format!("    ~ {k} {} → {}", shown.0[k], shown.1[k]));
            }
            (None, Some(_)) => rows.push(format!("    + {k}={}", shown.1[k])),
            (Some(_), None) => rows.push(format!("    − {k}={}", shown.0[k])),
            _ => {}
        }
    }
    if !rows.is_empty() {
        out.push(format!("  {title}"));
        out.extend(rows);
    }
}

fn scalar(out: &mut Vec<String>, name: &str, a: Option<String>, b: Option<String>) {
    match (a, b) {
        (Some(x), Some(y)) if x != y => out.push(format!("  ~ {name} {x} → {y}")),
        (None, Some(y)) => out.push(format!("  + {name} {y}")),
        (Some(x), None) => out.push(format!("  − {name} {x}")),
        _ => {}
    }
}

fn command(v: &[String]) -> Option<String> {
    (!v.is_empty()).then(|| mask_command(&v.join(" ")))
}

fn env_map(c: &ImageConfig) -> BTreeMap<String, String> {
    c.env.iter().cloned().collect()
}

/// The comparison text, one line per entry; every value is masked.
pub fn compare_lines(a: &SideData, b: &SideData) -> Vec<String> {
    let mut v = vec!["Summary (A | B)".to_owned()];
    pair(&mut v, "Digest", a.digest.short(), b.digest.short());
    let size = |s: &SideData| s.size.map_or_else(|| NONE.to_owned(), fmt_size);
    pair(&mut v, "Size", size(a), size(b));
    pair(&mut v, "Created", created(a.config), created(b.config));
    let plat = |s: &SideData| {
        s.config
            .platform
            .as_ref()
            .map_or_else(|| NONE.to_owned(), |p| p.to_string())
    };
    pair(&mut v, "Platform", plat(a), plat(b));

    v.push(String::new());
    v.push("Layers".to_owned());
    let in_a: BTreeSet<&Digest> = a.layers.iter().map(|l| &l.digest).collect();
    let in_b: BTreeSet<&Digest> = b.layers.iter().map(|l| &l.digest).collect();
    let (mut added, mut removed) = (0u64, 0u64);
    for l in b.layers {
        if in_a.contains(&l.digest) {
            v.push(format!("  shared  {}", l.digest.short()));
        } else {
            added += l.size;
            v.push(format!(
                "  + added   {} {}",
                l.digest.short(),
                fmt_size(l.size)
            ));
        }
    }
    for l in a.layers.iter().filter(|l| !in_b.contains(&l.digest)) {
        removed += l.size;
        v.push(format!(
            "  − removed {} {}",
            l.digest.short(),
            fmt_size(l.size)
        ));
    }
    v.push(format!("  +{} / −{}", fmt_size(added), fmt_size(removed)));

    v.push(String::new());
    v.push("Config".to_owned());
    let n = v.len();
    let (ca, cb) = (a.config, b.config);
    let (ea, eb) = (env_map(ca), env_map(cb));
    map_diff(&mut v, "env", (&ea, &eb), (&mask_map(&ea), &mask_map(&eb)));
    map_diff(
        &mut v,
        "labels",
        (&ca.labels, &cb.labels),
        (&mask_map(&ca.labels), &mask_map(&cb.labels)),
    );
    scalar(
        &mut v,
        "entrypoint",
        command(&ca.entrypoint),
        command(&cb.entrypoint),
    );
    scalar(&mut v, "cmd", command(&ca.cmd), command(&cb.cmd));
    scalar(&mut v, "user", ca.user.clone(), cb.user.clone());
    scalar(
        &mut v,
        "workdir",
        ca.working_dir.clone(),
        cb.working_dir.clone(),
    );
    let (pa, pb): (BTreeSet<_>, BTreeSet<_>) = (
        ca.exposed_ports.iter().collect(),
        cb.exposed_ports.iter().collect(),
    );
    v.extend(pb.difference(&pa).map(|p| format!("  + port {p}")));
    v.extend(pa.difference(&pb).map(|p| format!("  − port {p}")));
    if v.len() == n {
        v.push("  no differences".to_owned());
    }

    v.push(String::new());
    v.push("History".to_owned());
    let old: BTreeSet<&str> = ca.history.iter().map(|h| h.created_by.as_str()).collect();
    let n = v.len();
    for h in cb
        .history
        .iter()
        .filter(|h| !old.contains(h.created_by.as_str()))
    {
        v.push(format!("  + {}", mask_command(&instruction(&h.created_by))));
    }
    if v.len() == n {
        v.push("  no new steps".to_owned());
    }
    v
}

impl State {
    /// The marked base image, until the repo changes, Esc or a second `=` on it.
    pub fn compare_base(&self) -> Option<&Side> {
        self.compare_base.as_ref()
    }

    pub fn compare(&self) -> Option<&CompareModal> {
        self.compare.as_ref()
    }

    /// Manifest keys of `side` (an index adds its platform's child), then the child's `Config`
    /// once that manifest is a loaded image.
    pub(super) fn image_keys(&self, digest: &Digest, platform: usize) -> Vec<CacheKey> {
        let (Some(reg), Some(repo)) = (self.selected_registry(), self.selected_repo()) else {
            return vec![];
        };
        let (id, path) = (&reg.id, &repo.path);
        let mut keys = vec![CacheKey::Manifest(id.clone(), path.clone(), digest.clone())];
        let mut target = digest;
        if let Some(ManifestKind::Index { manifests }) = self.manifest(target).map(|m| &m.kind) {
            let Some(child) = manifests.get(platform) else {
                return keys;
            };
            target = &child.digest;
            keys.push(CacheKey::Manifest(id.clone(), path.clone(), target.clone()));
        }
        if let Some(ManifestKind::Image { .. }) = self.manifest(target).map(|m| &m.kind) {
            keys.push(CacheKey::Config(id.clone(), path.clone(), target.clone()));
        }
        keys
    }

    pub(super) fn compare_keys(&self) -> Vec<CacheKey> {
        let Some(m) = &self.compare else {
            return vec![];
        };
        [&m.a, &m.b]
            .into_iter()
            .flat_map(|s| self.image_keys(&s.digest, s.platform))
            .collect()
    }

    /// Layers and config of `side`, once every key it needs is `Ready`.
    fn side_data<'a>(&'a self, side: &'a Side) -> Option<SideData<'a>> {
        let Some(CacheKey::Config(r, p, d)) = self.image_keys(&side.digest, side.platform).pop()
        else {
            return None;
        };
        let CacheValue::Config(Some(config)) =
            self.value(&CacheKey::Config(r.clone(), p.clone(), d.clone()))?
        else {
            return None;
        };
        let layers = match self.value(&CacheKey::Manifest(r.clone(), p.clone(), d.clone()))? {
            CacheValue::Manifest(m) => match &m.kind {
                ManifestKind::Image { layers, .. } => layers.as_slice(),
                ManifestKind::Index { .. } => return None,
            },
            _ => return None,
        };
        let size = self
            .visible_images()
            .into_iter()
            .find(|a| a.digest == side.digest)
            .and_then(|a| self.display_size(a));
        Some(SideData {
            digest: &side.digest,
            size,
            layers,
            config,
        })
    }

    /// The modal's text; `None` while any side is still loading.
    pub fn compare_text(&self) -> Option<Vec<String>> {
        let m = self.compare.as_ref()?;
        Some(compare_lines(
            &self.side_data(&m.a)?,
            &self.side_data(&m.b)?,
        ))
    }

    /// Why the modal can never show its text: a side's fetch failed, or a side is not an image.
    pub fn compare_failure(&self) -> Option<String> {
        let m = self.compare.as_ref()?;
        [&m.a, &m.b].into_iter().find_map(|side| {
            self.image_keys(&side.digest, side.platform)
                .iter()
                .find_map(|k| match self.failure(k) {
                    // `r` does not reach the modal.
                    Some(e) => Some(e.split_once(" — press r").map_or(e, |(e, _)| e).to_owned()),
                    None => matches!(self.value(k), Some(CacheValue::Config(None)))
                        .then(|| format!("{} is not an image", side.digest.short())),
                })
        })
    }

    /// `=` on the Images pane: marks the base; on another image (or platform) opens the modal;
    /// on the base clears it.
    pub(super) fn compare_mark(&mut self) {
        if self.focus != Pane::Images {
            return;
        }
        let Some(img) = self
            .selected_image()
            .filter(|a| a.status == ArtifactStatus::Active)
        else {
            return;
        };
        let here = Side {
            digest: img.digest.clone(),
            platform: self.platform,
        };
        match self.compare_base.take() {
            Some(base) if base == here => {}
            Some(base) => {
                self.compare_base = Some(base.clone());
                self.compare = Some(CompareModal {
                    a: base,
                    b: here,
                    scroll: 0,
                });
            }
            None => self.compare_base = Some(here),
        }
    }

    /// The modal's keys: ↑↓ scroll (`ui` drops a Down past the end), `y` copies the text, Esc
    /// closes it and clears the base.
    pub(super) fn compare_action(&mut self, a: Action) -> Vec<Effect> {
        let Some(m) = self.compare.as_mut() else {
            return vec![];
        };
        match a {
            Action::Up => m.scroll = m.scroll.saturating_sub(1),
            Action::Down => m.scroll = m.scroll.saturating_add(1),
            Action::Copy => {
                return self
                    .compare_text()
                    .map(|t| Effect::Copy(t.join("\n")))
                    .into_iter()
                    .collect();
            }
            Action::Back => {
                self.compare = None;
                self.compare_base = None;
            }
            Action::ForceQuit => return vec![Effect::Quit],
            _ => {}
        }
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::app::update::tests::{desc, dg};
    use crate::domain::HistoryEntry;

    fn cfg() -> ImageConfig {
        ImageConfig {
            raw: String::new(),
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
        }
    }

    type Img = (Vec<Descriptor>, ImageConfig);

    fn lines(a: &Img, b: &Img) -> Vec<String> {
        let (da, db) = (dg('a'), dg('b'));
        fn side<'a>(digest: &'a Digest, s: &'a Img) -> SideData<'a> {
            SideData {
                digest,
                size: None,
                layers: &s.0,
                config: &s.1,
            }
        }
        compare_lines(&side(&da, a), &side(&db, b))
    }

    fn has(v: &[String], s: &str) -> bool {
        v.iter().any(|l| l.trim() == s)
    }

    #[test]
    fn layers_diff_and_totals() {
        let a = (vec![desc('1', 10), desc('2', 20)], cfg());
        let b = (vec![desc('1', 10), desc('3', 12_300_000)], cfg());
        let v = lines(&a, &b);
        assert!(has(&v, "shared  111111111111"), "{v:#?}");
        assert!(has(&v, "+ added   333333333333 12.3MB"), "{v:#?}");
        assert!(has(&v, "− removed 222222222222 20B"), "{v:#?}");
        assert!(has(&v, "+12.3MB / −20B"), "{v:#?}");
    }

    #[test]
    fn config_diff_masks_env() {
        let env = |p: &[(&str, &str)]| {
            p.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let (mut ca, mut cb) = (cfg(), cfg());
        ca.env = env(&[("API_TOKEN", "a"), ("X", "1")]);
        cb.env = env(&[("API_TOKEN", "b"), ("X", "2"), ("Y", "3")]);
        let v = lines(&(vec![], ca), &(vec![], cb));
        assert!(has(&v, "~ X 1 → 2"), "{v:#?}");
        assert!(has(&v, "+ Y=3"), "{v:#?}");
        assert!(has(&v, "~ API_TOKEN •••••• → ••••••"), "{v:#?}");
        let token: Vec<_> = v.iter().filter(|l| l.contains("API_TOKEN")).collect();
        assert_eq!(token.len(), 1, "{v:#?}");
    }

    #[test]
    fn history_new_steps() {
        let step = |s: &str| HistoryEntry {
            created_by: s.into(),
            created: None,
            empty_layer: false,
        };
        let (mut ca, mut cb) = (cfg(), cfg());
        ca.history = vec![step("/bin/sh -c #(nop) CMD [\"sh\"]")];
        cb.history = vec![
            step("/bin/sh -c #(nop) CMD [\"sh\"]"),
            step("/bin/sh -c make"),
        ];
        let v = lines(&(vec![], ca), &(vec![], cb));
        let plus: Vec<_> = v.iter().filter(|l| l.starts_with("  + RUN")).collect();
        assert_eq!(plus, ["  + RUN make"], "{v:#?}");
    }
}

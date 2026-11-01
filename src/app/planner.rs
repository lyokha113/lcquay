//! Delete planning: pure dependency-snapshot assembly and planner (no I/O, no clock).

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::{
    ArtifactStatus, ArtifactSummary, Caps, DeleteOp, DeletePlan, DepSnapshot, Descriptor, Digest,
    ExcludeReason, Inventory, ManifestDocument, ManifestKind, MarkKind, Marks, SideEffect,
    SnapshotError,
};

pub const UNREADABLE_INDEX: &str =
    "repository contains an unreadable/archived index; nothing can be deleted safely";
pub const TAGGED_ONLY_BANNER: &str = "untagged parents are not visible on this registry";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Totals {
    pub tags: usize,
    pub manifests: usize,
    /// Sum of the known sizes; `None` when no deleted manifest reports one.
    pub size: Option<u64>,
}

/// Fail closed: anything but a known single-image manifest type (or none) may be an index.
pub fn may_be_index(a: &ArtifactSummary) -> bool {
    !matches!(
        a.media_type.as_deref(),
        Some(
            "application/vnd.oci.image.manifest.v1+json"
                | "application/vnd.docker.distribution.manifest.v2+json"
                | "application/vnd.docker.distribution.manifest.v1+prettyjws"
                | "application/vnd.docker.distribution.manifest.v1+json"
        )
    )
}

/// `sha256-<64 lowercase hex>` → `sha256:<hex>`.
pub fn fallback_subject(tag: &str) -> Option<Digest> {
    let hex = tag.strip_prefix("sha256-")?;
    let ok = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    ok.then(|| Digest::parse(&format!("sha256:{hex}")).ok())?
}

/// Children of a row's manifest, when it was read and is an index.
fn index_children<'a>(
    a: &ArtifactSummary,
    manifests: &'a BTreeMap<Digest, ManifestDocument>,
) -> Option<&'a [Descriptor]> {
    match &manifests.get(&a.digest)?.kind {
        ManifestKind::Index { manifests } => Some(manifests),
        ManifestKind::Image { .. } => None,
    }
}

/// Children of readable indexes that carry a fallback-shaped tag and have not been read yet.
pub fn fallback_targets(
    inventory: &[ArtifactSummary],
    manifests: &BTreeMap<Digest, ManifestDocument>,
) -> BTreeSet<Digest> {
    inventory
        .iter()
        .filter(|a| a.tags.iter().any(|t| fallback_subject(t).is_some()))
        .filter_map(|a| index_children(a, manifests))
        .flatten()
        .map(|c| c.digest.clone())
        .filter(|c| !manifests.contains_key(c))
        .collect()
}

pub fn assemble(
    inventory: Vec<ArtifactSummary>,
    manifests: &BTreeMap<Digest, ManifestDocument>,
    referrers: BTreeMap<Digest, Option<Vec<Digest>>>,
    marked: &BTreeSet<Digest>,
) -> Result<DepSnapshot, SnapshotError> {
    let readable = |a: &ArtifactSummary| {
        a.status == ArtifactStatus::Active && manifests.contains_key(&a.digest)
    };
    if inventory.iter().any(|a| may_be_index(a) && !readable(a)) {
        return Err(SnapshotError::UnreadableIndex);
    }
    let mut snap = DepSnapshot {
        referrers,
        ..Default::default()
    };
    for a in &inventory {
        if let Some(s) = &a.subject {
            snap.subjects.insert(a.digest.clone(), s.clone());
        }
    }
    for (digest, doc) in manifests {
        if let Some(s) = &doc.subject {
            snap.subjects.insert(digest.clone(), s.digest.clone());
        }
    }
    for a in &inventory {
        let Some(children) = index_children(a, manifests) else {
            continue;
        };
        // Fallback index: every child was read and is attached to that tag's subject.
        let fallback = a.tags.iter().find_map(|t| {
            let s = fallback_subject(t)?;
            let attached = children.iter().all(|c| {
                manifests
                    .get(&c.digest)
                    .and_then(|m| m.subject.as_ref())
                    .is_some_and(|m| m.digest == s)
            });
            attached.then(|| (t.clone(), s))
        });
        if let Some((tag, s)) = fallback {
            snap.fallback_indexes.insert(tag, s);
            continue;
        }
        for c in children {
            snap.parents
                .entry(c.digest.clone())
                .or_default()
                .insert(a.digest.clone());
        }
    }
    let archived = |m: &Digest| {
        inventory
            .iter()
            .any(|a| &a.digest == m && a.status != ArtifactStatus::Active)
    };
    snap.unreadable = marked
        .iter()
        .filter(|m| !manifests.contains_key(m) || archived(m))
        .cloned()
        .collect();
    snap.inventory = inventory;
    Ok(snap)
}

fn find<'a>(snap: &'a DepSnapshot, digest: &Digest) -> Option<&'a ArtifactSummary> {
    snap.inventory.iter().find(|a| &a.digest == digest)
}

/// Spec §7.4 planner steps 0–5. Marks count only while they still match the snapshot
/// (tag still on that digest, untagged manifest still untagged); the rest are ignored.
pub fn plan(marks: &Marks, snap: &DepSnapshot, caps: Caps, whole: bool) -> DeletePlan {
    let rows: BTreeMap<&Digest, &ArtifactSummary> =
        snap.inventory.iter().map(|a| (&a.digest, a)).collect();
    let mut p = DeletePlan::default();
    let mut excluded = BTreeMap::new();
    let mut set = BTreeSet::new();
    let mut automatic = BTreeSet::new();
    let mut collateral = BTreeMap::new();

    // Steps 0 and 1.
    for (digest, marked) in &marks.tags {
        let Some(a) = rows.get(digest) else { continue };
        let sel: BTreeSet<&String> = marked.iter().filter(|t| a.tags.contains(t)).collect();
        if sel.is_empty() {
            continue;
        }
        let mut rest: Vec<String> = a
            .tags
            .iter()
            .filter(|t| !sel.contains(t))
            .cloned()
            .collect();
        rest.sort();
        if snap.unreadable.contains(digest) {
            excluded.insert(digest.clone(), ExcludeReason::DependencyUnknown);
        } else if rest.is_empty() {
            set.insert(digest.clone());
        } else if whole {
            set.insert(digest.clone());
            collateral.insert(digest.clone(), rest);
        } else if caps.delete_tag {
            p.remove_tags
                .extend(sel.into_iter().map(|t| DeleteOp::RemoveTag {
                    tag: t.clone(),
                    expected: digest.clone(),
                }));
        } else {
            excluded.insert(digest.clone(), ExcludeReason::NeedsWholeManifest);
        }
    }
    for (digest, kind) in &marks.untagged {
        if !rows.get(digest).is_some_and(|a| a.tags.is_empty()) {
            continue;
        }
        if snap.unreadable.contains(digest) {
            excluded.insert(digest.clone(), ExcludeReason::DependencyUnknown);
        } else {
            set.insert(digest.clone());
            if *kind == MarkKind::Automatic {
                automatic.insert(digest.clone());
            }
        }
    }

    // Step 2: fixpoint.
    loop {
        let out: Vec<(Digest, ExcludeReason)> = set
            .iter()
            .filter_map(|m| {
                let parents = snap.parents.get(m).into_iter().flatten();
                if let Some(p) = parents.into_iter().find(|p| !set.contains(*p)) {
                    return Some((m.clone(), ExcludeReason::ChildOfKeptIndex(p.clone())));
                }
                // A subject missing from the inventory is gone: its orphaned referrers may go.
                let s = snap.subjects.get(m)?;
                (automatic.contains(m) && rows.contains_key(s) && !set.contains(s))
                    .then(|| (m.clone(), ExcludeReason::AttachedToKeptManifest(s.clone())))
            })
            .collect();
        if out.is_empty() {
            break;
        }
        for (m, reason) in out {
            set.remove(&m);
            excluded.insert(m, reason);
        }
    }

    // Step 3: every parent of a set member is in the set (fixpoint).
    let mut placed = BTreeSet::new();
    while placed.len() < set.len() {
        let layer: Vec<Digest> = set
            .iter()
            .filter(|m| !placed.contains(*m))
            .filter(|m| {
                snap.parents
                    .get(*m)
                    .into_iter()
                    .flatten()
                    .all(|p| placed.contains(p))
            })
            .cloned()
            .collect();
        // Content addressing rules out cycles; stop rather than spin if one appears.
        if layer.is_empty() {
            break;
        }
        placed.extend(layer.iter().cloned());
        p.layers.push(layer);
    }
    debug_assert_eq!(placed.len(), set.len());

    // Step 4: excluded manifests carry no ops and lose no tags.
    p.collateral = collateral
        .into_iter()
        .filter(|(d, _)| set.contains(d))
        .collect();
    p.excluded = excluded.into_iter().collect();

    // Step 5: side effects. A tagged child stays referenced by its tag.
    let tagged = |c: &Digest| rows.get(c).is_some_and(|a| !a.tags.is_empty());
    for (child, parents) in &snap.parents {
        if !set.contains(child) && !tagged(child) && parents.iter().all(|i| set.contains(i)) {
            p.side_effects
                .extend(parents.iter().map(|i| SideEffect::UnreferencedChild {
                    child: child.clone(),
                    index: i.clone(),
                }));
        }
    }
    for s in &set {
        let listed = snap.referrers.get(s).and_then(|r| r.as_deref());
        let attached = snap
            .subjects
            .iter()
            .filter(|(_, t)| *t == s)
            .map(|(a, _)| a);
        let kept: BTreeSet<&Digest> = listed
            .unwrap_or_default()
            .iter()
            .chain(attached)
            .filter(|r| !set.contains(*r))
            .collect();
        p.side_effects
            .extend(kept.into_iter().map(|r| SideEffect::OrphanedReferrer {
                referrer: r.clone(),
                subject: s.clone(),
            }));
    }
    for (tag, s) in &snap.fallback_indexes {
        let deleted = snap
            .inventory
            .iter()
            .any(|a| a.tags.contains(tag) && set.contains(&a.digest));
        if deleted {
            continue;
        }
        if set.contains(s) || set.iter().any(|r| snap.subjects.get(r) == Some(s)) {
            p.side_effects
                .push(SideEffect::FallbackIndexStale { tag: tag.clone() });
        }
    }
    // A digest missing from `referrers` was never fetched: unknown too.
    for s in &set {
        if !matches!(snap.referrers.get(s), Some(Some(_))) {
            p.side_effects
                .push(SideEffect::ReferrersUnknown { digest: s.clone() });
        }
    }

    if caps.inventory == Inventory::TaggedOnly {
        p.banner = Some(TAGGED_ONLY_BANNER.into());
    }
    p
}

/// Revalidation: same plan, and every manifest to delete still has the same tag set.
pub fn same_plan(a: &DeletePlan, sa: &DepSnapshot, b: &DeletePlan, sb: &DepSnapshot) -> bool {
    let tags = |s, d| find(s, d).map(|r| r.tags.iter().collect::<BTreeSet<_>>());
    a == b
        && a.layers
            .iter()
            .flatten()
            .all(|d| tags(sa, d) == tags(sb, d))
}

pub fn totals(p: &DeletePlan, snap: &DepSnapshot) -> Totals {
    let rows: BTreeMap<&Digest, &ArtifactSummary> =
        snap.inventory.iter().map(|a| (&a.digest, a)).collect();
    let deleted: Vec<&ArtifactSummary> = p
        .layers
        .iter()
        .flatten()
        .filter_map(|d| rows.get(d).copied())
        .collect();
    Totals {
        tags: p.remove_tags.len() + deleted.iter().map(|a| a.tags.len()).sum::<usize>(),
        manifests: p.layers.iter().map(Vec::len).sum(),
        size: deleted.iter().filter_map(|a| a.size).reduce(|a, b| a + b),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::update::tests::art;
    use crate::domain::{
        Caps, DeleteOp, DeletePlan, ExcludeReason::*, Inventory, MarkKind, MarkKind::*, Marks,
        SideEffect::*,
    };

    const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
    const OCI_IMAGE: &str = "application/vnd.oci.image.manifest.v1+json";

    pub(crate) fn d(n: u64) -> Digest {
        Digest::parse(&format!("sha256:{n:064x}")).unwrap()
    }

    fn row(digest: Digest, tags: &[&str], media: Option<&str>) -> ArtifactSummary {
        ArtifactSummary {
            digest,
            media_type: media.map(Into::into),
            ..art('0', tags)
        }
    }

    fn desc(digest: Digest) -> Descriptor {
        Descriptor {
            media_type: OCI_IMAGE.into(),
            digest,
            size: 1,
            platform: None,
            artifact_type: None,
            annotations: BTreeMap::new(),
        }
    }

    fn doc(kind: ManifestKind, subject: Option<Digest>) -> ManifestDocument {
        ManifestDocument {
            media_type: OCI_IMAGE.into(),
            raw: String::new(),
            kind,
            subject: subject.map(desc),
            artifact_type: None,
            annotations: BTreeMap::new(),
        }
    }

    fn image(subject: Option<Digest>) -> ManifestDocument {
        doc(
            ManifestKind::Image {
                config: desc(d(900)),
                layers: vec![],
            },
            subject,
        )
    }

    fn index(children: &[&Digest]) -> ManifestDocument {
        doc(
            ManifestKind::Index {
                manifests: children.iter().map(|c| desc((*c).clone())).collect(),
            },
            None,
        )
    }

    fn run(
        inv: Vec<ArtifactSummary>,
        docs: Vec<(Digest, ManifestDocument)>,
        marked: &[&Digest],
    ) -> Result<DepSnapshot, SnapshotError> {
        let m: BTreeMap<_, _> = docs.into_iter().collect();
        let marked = marked.iter().map(|d| (*d).clone()).collect();
        assemble(inv, &m, BTreeMap::new(), &marked)
    }

    fn set(ds: &[&Digest]) -> BTreeSet<Digest> {
        ds.iter().map(|d| (*d).clone()).collect()
    }

    fn fb_tag(s: &Digest) -> String {
        format!("sha256-{}", s.hex())
    }

    #[test]
    fn plan_ops_order() {
        let (a, b) = (d(1), d(2));
        let t1 = DeleteOp::RemoveTag {
            tag: "t1".into(),
            expected: d(3),
        };
        let plan = DeletePlan {
            remove_tags: vec![t1.clone()],
            layers: vec![vec![a.clone()], vec![b.clone()]],
            ..Default::default()
        };
        assert_eq!(
            plan.ops(),
            vec![
                t1,
                DeleteOp::DeleteManifest { digest: a },
                DeleteOp::DeleteManifest { digest: b }
            ]
        );
    }

    #[test]
    fn fallback_subject_parses() {
        let hex = "ab".repeat(32);
        assert_eq!(
            fallback_subject(&format!("sha256-{hex}")),
            Digest::parse(&format!("sha256:{hex}")).ok()
        );
        assert_eq!(
            fallback_subject(&format!("sha256-{}", hex.to_uppercase())),
            None
        );
        assert_eq!(fallback_subject(&format!("sha256-{}", &hex[..63])), None);
        assert_eq!(
            fallback_subject(&format!("sha512-{}", "a".repeat(128))),
            None
        );
        assert_eq!(fallback_subject("latest"), None);
    }

    #[test]
    fn index_unreadable_blocks() {
        let r = run(vec![row(d(1), &[], Some(OCI_INDEX))], vec![], &[]);
        assert_eq!(r, Err(SnapshotError::UnreadableIndex));
    }

    #[test]
    fn archived_index_blocks() {
        let mut a = row(d(1), &[], Some(OCI_INDEX));
        a.status = ArtifactStatus::Archived;
        let r = run(vec![a], vec![(d(1), index(&[]))], &[]);
        assert_eq!(r, Err(SnapshotError::UnreadableIndex));
    }

    #[test]
    fn unknown_media_type_unreadable_blocks() {
        let odd = Some("application/vnd.example.odd+json");
        for media in [None, odd] {
            let r = run(vec![row(d(1), &[], media)], vec![], &[]);
            assert_eq!(r, Err(SnapshotError::UnreadableIndex), "{media:?}");
            let r = run(vec![row(d(1), &[], media)], vec![(d(1), image(None))], &[]);
            assert!(r.is_ok(), "{media:?}");
        }
        // Known single-image types are never fetched for the index check.
        for media in [
            OCI_IMAGE,
            "application/vnd.docker.distribution.manifest.v2+json",
            "application/vnd.docker.distribution.manifest.v1+prettyjws",
            "application/vnd.docker.distribution.manifest.v1+json",
        ] {
            assert!(!may_be_index(&row(d(1), &[], Some(media))), "{media}");
        }
    }

    #[test]
    fn parents_from_index() {
        let (i, c1, c2) = (d(1), d(2), d(3));
        let snap = run(
            vec![
                row(i.clone(), &[], Some(OCI_INDEX)),
                row(c1.clone(), &[], Some(OCI_IMAGE)),
                row(c2.clone(), &[], Some(OCI_IMAGE)),
            ],
            vec![
                (i.clone(), index(&[&c1, &c2])),
                (c1.clone(), image(None)),
                (c2.clone(), image(None)),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(snap.parents[&c1], set(&[&i]));
        assert_eq!(snap.parents[&c2], set(&[&i]));
    }

    fn fallback_fixture(subject_of_r: Option<Digest>, read_r: bool, tag: String) -> DepSnapshot {
        let (f, r, s) = (d(1), d(2), d(3));
        let mut docs = vec![(f.clone(), index(&[&r]))];
        if read_r {
            docs.push((r.clone(), image(subject_of_r)));
        }
        run(
            vec![
                row(f, &[&tag], Some(OCI_INDEX)),
                row(r, &[], Some(OCI_IMAGE)),
                row(s, &["v1"], Some(OCI_IMAGE)),
            ],
            docs,
            &[],
        )
        .unwrap()
    }

    #[test]
    fn fallback_index_recognized() {
        let (r, s) = (d(2), d(3));
        let tag = fb_tag(&s);
        let snap = fallback_fixture(Some(s.clone()), true, tag.clone());
        assert_eq!(snap.subjects[&r], s);
        assert_eq!(snap.fallback_indexes[&tag], s);
        assert!(!snap.parents.contains_key(&r));
    }

    #[test]
    fn fallback_tag_non_matching_subject_is_parent() {
        let (f, r, s) = (d(1), d(2), d(3));
        let snap = fallback_fixture(Some(d(4)), true, fb_tag(&s));
        assert_eq!(snap.parents[&r], set(&[&f]));
        assert!(snap.fallback_indexes.is_empty());
    }

    #[test]
    fn fallback_child_unread_is_parent() {
        let (f, r, s) = (d(1), d(2), d(3));
        let snap = fallback_fixture(None, false, fb_tag(&s));
        assert_eq!(snap.parents[&r], set(&[&f]));
        assert!(snap.fallback_indexes.is_empty());
    }

    #[test]
    fn non_sha256_fallback_is_parent() {
        let (f, r, s) = (d(1), d(2), d(3));
        let snap = fallback_fixture(Some(s), true, format!("sha512-{}", "a".repeat(128)));
        assert_eq!(snap.parents[&r], set(&[&f]));
        assert!(snap.fallback_indexes.is_empty());
    }

    #[test]
    fn subjects_from_inventory_and_manifests() {
        let (a, b, s) = (d(1), d(2), d(3));
        let mut ra = row(a.clone(), &[], Some(OCI_IMAGE));
        ra.subject = Some(s.clone());
        let snap = run(
            vec![ra, row(b.clone(), &[], Some(OCI_IMAGE))],
            vec![(b.clone(), image(Some(s.clone())))],
            &[],
        )
        .unwrap();
        assert_eq!(snap.subjects[&a], s);
        assert_eq!(snap.subjects[&b], s);
    }

    #[test]
    fn marked_unreadable_is_dependency_unknown() {
        let (m, n, k) = (d(1), d(2), d(3));
        let mut arch = row(n.clone(), &[], Some(OCI_IMAGE));
        arch.status = ArtifactStatus::Archived;
        let snap = run(
            vec![
                row(m.clone(), &[], Some(OCI_IMAGE)),
                arch,
                row(k.clone(), &[], Some(OCI_IMAGE)),
            ],
            vec![(k.clone(), image(None))],
            &[&m, &n, &k],
        )
        .unwrap();
        assert_eq!(snap.unreadable, set(&[&m, &n]));
    }

    #[test]
    fn referrers_passed_through() {
        let mut refs = BTreeMap::new();
        refs.insert(d(1), Some(vec![d(2)]));
        refs.insert(d(3), None);
        let snap = assemble(vec![], &BTreeMap::new(), refs.clone(), &BTreeSet::new()).unwrap();
        assert_eq!(snap.referrers, refs);
    }

    #[test]
    fn fallback_targets_dedup() {
        let (f1, f2, r) = (d(1), d(2), d(3));
        let inv = vec![
            row(f1.clone(), &[&fb_tag(&d(7))], Some(OCI_INDEX)),
            row(f2.clone(), &[&fb_tag(&d(8))], Some(OCI_INDEX)),
        ];
        let mut m = BTreeMap::new();
        m.insert(f1, index(&[&r]));
        m.insert(f2, index(&[&r]));
        assert_eq!(fallback_targets(&inv, &m), set(&[&r]));
        m.insert(r, image(None));
        assert!(fallback_targets(&inv, &m).is_empty());
    }

    #[test]
    fn fallback_needs_every_child() {
        let (f, r1, r2, s) = (d(1), d(2), d(3), d(4));
        let tag = fb_tag(&s);
        for r2_doc in [Some(image(Some(d(5)))), None] {
            let mut docs = vec![
                (f.clone(), index(&[&r1, &r2])),
                (r1.clone(), image(Some(s.clone()))),
            ];
            docs.extend(r2_doc.map(|m| (r2.clone(), m)));
            let snap = run(
                vec![
                    row(f.clone(), &[&tag], Some(OCI_INDEX)),
                    row(r1.clone(), &[], Some(OCI_IMAGE)),
                    row(r2.clone(), &[], Some(OCI_IMAGE)),
                    row(s.clone(), &["v1"], Some(OCI_IMAGE)),
                ],
                docs,
                &[],
            )
            .unwrap();
            assert_eq!(snap.parents[&r1], set(&[&f]));
            assert_eq!(snap.parents[&r2], set(&[&f]));
            assert!(snap.fallback_indexes.is_empty());
        }
    }

    #[test]
    fn other_status_index_blocks() {
        let mut a = row(d(1), &[], Some(OCI_INDEX));
        a.status = ArtifactStatus::Other("ACTIVATING".into());
        let r = run(vec![a], vec![(d(1), index(&[]))], &[]);
        assert_eq!(r, Err(SnapshotError::UnreadableIndex));
    }

    // --- planner ---

    const CAPS: Caps = Caps {
        inventory: Inventory::Complete,
        pushed_at: true,
        last_pulled: true,
        delete_tag: true,
        delete_manifest: true,
    };

    fn snap_of(rows: &[(&Digest, &[&str])]) -> DepSnapshot {
        DepSnapshot {
            inventory: rows
                .iter()
                .map(|(x, t)| row((*x).clone(), t, None))
                .collect(),
            ..Default::default()
        }
    }

    fn edge(snap: &mut DepSnapshot, parent: &Digest, child: &Digest) {
        snap.parents
            .entry(child.clone())
            .or_default()
            .insert(parent.clone());
    }

    fn marks(tagged: &[(&Digest, &[&str])], untagged: &[(&Digest, MarkKind)]) -> Marks {
        Marks {
            tags: tagged
                .iter()
                .map(|(x, t)| ((*x).clone(), t.iter().map(|t| t.to_string()).collect()))
                .collect(),
            untagged: untagged.iter().map(|(x, k)| ((*x).clone(), *k)).collect(),
        }
    }

    fn rt(tag: &str, x: &Digest) -> DeleteOp {
        DeleteOp::RemoveTag {
            tag: tag.into(),
            expected: x.clone(),
        }
    }

    #[test]
    fn partial_selection_with_delete_tag() {
        let a = d(1);
        let snap = snap_of(&[(&a, &["1", "2"])]);
        let p = plan(&marks(&[(&a, &["1"])], &[]), &snap, CAPS, false);
        assert_eq!(p.remove_tags, vec![rt("1", &a)]);
        assert!(p.layers.is_empty() && p.excluded.is_empty() && p.collateral.is_empty());
    }

    #[test]
    fn partial_selection_without_delete_tag() {
        let a = d(1);
        let snap = snap_of(&[(&a, &["1", "2"])]);
        let caps = Caps {
            delete_tag: false,
            ..CAPS
        };
        let p = plan(&marks(&[(&a, &["1"])], &[]), &snap, caps, false);
        assert_eq!(p.excluded, vec![(a, NeedsWholeManifest)]);
        assert!(p.ops().is_empty());
    }

    #[test]
    fn partial_on_dependency_unknown_no_remove_tag() {
        let a = d(1);
        let mut snap = snap_of(&[(&a, &["1", "2"])]);
        snap.unreadable.insert(a.clone());
        let p = plan(&marks(&[(&a, &["1"])], &[]), &snap, CAPS, false);
        assert!(p.remove_tags.is_empty());
        assert_eq!(p.excluded, vec![(a, DependencyUnknown)]);
    }

    #[test]
    fn whole_manifest_collateral() {
        let a = d(1);
        let snap = snap_of(&[(&a, &["1", "2"])]);
        let p = plan(&marks(&[(&a, &["1"])], &[]), &snap, CAPS, true);
        assert_eq!(p.layers, vec![vec![a.clone()]]);
        assert_eq!(p.collateral, vec![(a, vec!["2".to_string()])]);
        assert!(p.remove_tags.is_empty());
    }

    #[test]
    fn untagged_and_full_selection_start_set() {
        let (b, c) = (d(2), d(3));
        let snap = snap_of(&[(&b, &["1", "2"]), (&c, &[])]);
        let m = marks(&[(&b, &["1", "2"])], &[(&c, Explicit)]);
        let p = plan(&m, &snap, CAPS, false);
        assert_eq!(p.layers, vec![vec![b, c]]);
        assert!(p.collateral.is_empty() && p.remove_tags.is_empty() && p.excluded.is_empty());
    }

    #[test]
    fn child_of_kept_index() {
        let (i, c) = (d(1), d(2));
        let mut snap = snap_of(&[(&i, &["v1"]), (&c, &[])]);
        edge(&mut snap, &i, &c);
        let p = plan(&marks(&[], &[(&c, Explicit)]), &snap, CAPS, false);
        assert_eq!(p.excluded, vec![(c, ChildOfKeptIndex(i))]);
        assert!(p.layers.is_empty());
    }

    #[test]
    fn shared_index_children() {
        let (i1, i2, c) = (d(1), d(2), d(3));
        let mut snap = snap_of(&[(&i1, &["a"]), (&i2, &["b"]), (&c, &[])]);
        edge(&mut snap, &i1, &c);
        edge(&mut snap, &i2, &c);
        let m = marks(&[(&i1, &["a"])], &[(&c, Explicit)]);
        let p = plan(&m, &snap, CAPS, false);
        assert_eq!(p.excluded, vec![(c.clone(), ChildOfKeptIndex(i2.clone()))]);
        assert_eq!(p.layers, vec![vec![i1.clone()]]);
        let m = marks(&[(&i1, &["a"]), (&i2, &["b"])], &[(&c, Explicit)]);
        let p = plan(&m, &snap, CAPS, false);
        assert_eq!(p.layers, vec![vec![i1, i2], vec![c]]);
        assert!(p.excluded.is_empty());
    }

    #[test]
    fn exclusion_through_two_levels() {
        let (i0, i1, c) = (d(1), d(2), d(3));
        let mut snap = snap_of(&[(&i0, &["v1"]), (&i1, &[]), (&c, &[])]);
        edge(&mut snap, &i0, &i1);
        edge(&mut snap, &i1, &c);
        let m = marks(&[], &[(&i1, Explicit), (&c, Explicit)]);
        let p = plan(&m, &snap, CAPS, false);
        assert_eq!(
            p.excluded,
            vec![
                (i1.clone(), ChildOfKeptIndex(i0)),
                (c, ChildOfKeptIndex(i1))
            ]
        );
        assert!(p.layers.is_empty());
    }

    #[test]
    fn automatic_vs_explicit_attached() {
        let (s, r1, r2) = (d(1), d(2), d(3));
        let mut snap = snap_of(&[(&s, &["v1"]), (&r1, &[]), (&r2, &[])]);
        snap.subjects.insert(r1.clone(), s.clone());
        snap.subjects.insert(r2.clone(), s.clone());
        let m = marks(&[], &[(&r1, Automatic), (&r2, Explicit)]);
        let p = plan(&m, &snap, CAPS, false);
        assert_eq!(
            p.excluded,
            vec![(r1.clone(), AttachedToKeptManifest(s.clone()))]
        );
        assert_eq!(p.layers, vec![vec![r2.clone()]]);
        let m = marks(&[(&s, &["v1"])], &[(&r1, Automatic), (&r2, Explicit)]);
        let p = plan(&m, &snap, CAPS, false);
        assert!(p.excluded.is_empty());
        assert_eq!(p.layers, vec![vec![s, r1, r2]]);
    }

    #[test]
    fn excluded_leaves_tags_untouched() {
        let (i, a, u) = (d(1), d(2), d(3));
        let mut snap = snap_of(&[(&i, &["v1"]), (&a, &["1", "2"]), (&u, &["3", "4"])]);
        edge(&mut snap, &i, &a);
        snap.unreadable.insert(u.clone());
        let m = marks(&[(&a, &["1"]), (&u, &["3"])], &[]);
        let p = plan(&m, &snap, CAPS, true);
        assert_eq!(
            p.excluded,
            vec![(a.clone(), ChildOfKeptIndex(i)), (u, DependencyUnknown)]
        );
        assert!(p.remove_tags.is_empty() && p.collateral.is_empty() && p.layers.is_empty());
        let p = plan(&m, &snap, CAPS, false);
        assert_eq!(p.remove_tags, vec![rt("1", &a)]);
    }

    #[test]
    fn layer_assignment() {
        let (i0, i1, c, c2) = (d(5), d(4), d(3), d(1));
        let mut snap = snap_of(&[(&i0, &[]), (&i1, &[]), (&c, &[]), (&c2, &[])]);
        edge(&mut snap, &i0, &i1);
        edge(&mut snap, &i1, &c);
        edge(&mut snap, &i1, &c2);
        let all = [&i0, &i1, &c, &c2].map(|x| (x, Explicit));
        let p = plan(&marks(&[], &all), &snap, CAPS, false);
        assert_eq!(p.layers, vec![vec![i0], vec![i1], vec![c2, c]]);
    }

    #[test]
    fn side_effects_list() {
        // Deleted index whose child has no other parent.
        let (i, u, k) = (d(1), d(2), d(3));
        let mut snap = snap_of(&[(&i, &["v1"]), (&u, &[]), (&k, &["v2"])]);
        edge(&mut snap, &i, &u);
        snap.referrers.insert(i.clone(), Some(vec![]));
        let m = marks(&[(&i, &["v1"])], &[]);
        let p = plan(&m, &snap, CAPS, false);
        let child = UnreferencedChild {
            child: u.clone(),
            index: i,
        };
        assert_eq!(p.side_effects, vec![child]);
        // A second, kept parent keeps it referenced.
        edge(&mut snap, &k, &u);
        assert!(plan(&m, &snap, CAPS, false).side_effects.is_empty());

        // Deleted subject with a kept referrer.
        let (s, r) = (d(1), d(2));
        let mut snap = snap_of(&[(&s, &["v1"]), (&r, &[])]);
        snap.referrers.insert(s.clone(), Some(vec![r.clone()]));
        let m = marks(&[(&s, &["v1"])], &[]);
        let p = plan(&m, &snap, CAPS, false);
        let orphan = OrphanedReferrer {
            referrer: r,
            subject: s.clone(),
        };
        assert_eq!(p.side_effects, vec![orphan]);

        // Fallback index of a deleted subject.
        let mut snap = snap_of(&[(&s, &["v1"])]);
        snap.referrers.insert(s.clone(), Some(vec![]));
        snap.fallback_indexes.insert(fb_tag(&s), s.clone());
        let p = plan(&m, &snap, CAPS, false);
        assert_eq!(p.side_effects, vec![FallbackIndexStale { tag: fb_tag(&s) }]);

        // Fallback index of a kept subject still listing a deleted referrer.
        let (t, r) = (d(1), d(2));
        let mut snap = snap_of(&[(&t, &["v1"]), (&r, &[])]);
        snap.subjects.insert(r.clone(), t.clone());
        snap.referrers.insert(r.clone(), Some(vec![]));
        snap.fallback_indexes.insert(fb_tag(&t), t.clone());
        let p = plan(&marks(&[], &[(&r, Explicit)]), &snap, CAPS, false);
        assert_eq!(p.layers, vec![vec![r]]);
        assert_eq!(p.side_effects, vec![FallbackIndexStale { tag: fb_tag(&t) }]);
    }

    #[test]
    fn referrers_failure_only_warns() {
        let s = d(1);
        let mut snap = snap_of(&[(&s, &["v1"])]);
        snap.referrers.insert(s.clone(), None);
        let p = plan(&marks(&[(&s, &["v1"])], &[]), &snap, CAPS, false);
        assert_eq!(p.side_effects, vec![ReferrersUnknown { digest: s.clone() }]);
        assert_eq!(p.layers, vec![vec![s]]);
        assert!(p.excluded.is_empty());
    }

    #[test]
    fn tagged_only_banner() {
        let caps = Caps {
            inventory: Inventory::TaggedOnly,
            ..CAPS
        };
        let (m, snap) = (Marks::default(), DepSnapshot::default());
        let p = plan(&m, &snap, caps, false);
        assert_eq!(p.banner.as_deref(), Some(TAGGED_ONLY_BANNER));
        assert_eq!(plan(&m, &snap, CAPS, false).banner, None);
    }

    #[test]
    fn stale_marks_ignored() {
        let (a, b, c, x) = (d(1), d(2), d(3), d(4));
        let snap = snap_of(&[(&a, &[]), (&b, &["latest"]), (&c, &["x"])]);
        let m = marks(
            &[(&a, &["latest"]), (&x, &["v"])],
            &[(&c, Explicit), (&x, Explicit)],
        );
        for whole in [false, true] {
            assert_eq!(plan(&m, &snap, CAPS, whole), DeletePlan::default());
        }
    }

    #[test]
    fn same_plan_detects_tag_set_change() {
        let (a, b) = (d(1), d(2));
        let sa = snap_of(&[(&a, &["1"]), (&b, &["2", "3"])]);
        let m = marks(&[(&a, &["1"]), (&b, &["2"])], &[]);
        let pa = plan(&m, &sa, CAPS, false);
        assert_eq!(pa.layers, vec![vec![a.clone()]]);
        assert!(same_plan(&pa, &sa, &pa.clone(), &sa.clone()));
        let sb = snap_of(&[(&a, &["1", "new"]), (&b, &["2", "3"])]);
        assert!(!same_plan(&pa, &sa, &pa, &sb));
        let mut pc = pa.clone();
        pc.excluded.push((d(9), NeedsWholeManifest));
        assert!(!same_plan(&pa, &sa, &pc, &sa));
    }

    #[test]
    fn totals_counts() {
        let (a, b, c) = (d(1), d(2), d(3));
        let mut snap = snap_of(&[(&a, &["1", "2"]), (&b, &[]), (&c, &["x", "y"])]);
        snap.inventory[0].size = Some(10);
        snap.inventory[2].size = Some(99);
        let m = marks(&[(&a, &["1", "2"]), (&c, &["x"])], &[(&b, Explicit)]);
        let p = plan(&m, &snap, CAPS, false);
        let want = Totals {
            tags: 3,
            manifests: 2,
            size: Some(10),
        };
        assert_eq!(totals(&p, &snap), want);
        // No deleted manifest reports a size: unknown, not zero.
        snap.inventory[0].size = None;
        assert_eq!(totals(&plan(&m, &snap, CAPS, false), &snap).size, None);
    }

    #[test]
    fn automatic_orphan_referrer_deletable() {
        let (s, r) = (d(1), d(2));
        let mut snap = snap_of(&[(&r, &[])]);
        snap.subjects.insert(r.clone(), s);
        let p = plan(&marks(&[], &[(&r, Automatic)]), &snap, CAPS, false);
        assert_eq!(p.layers, vec![vec![r]]);
        assert!(p.excluded.is_empty());
    }

    #[test]
    fn deleted_fallback_index_not_stale() {
        let (s, f) = (d(1), d(2));
        let tag = fb_tag(&s);
        let mut snap = snap_of(&[(&s, &["v1"]), (&f, &[&tag])]);
        snap.referrers.insert(s.clone(), Some(vec![]));
        snap.referrers.insert(f.clone(), Some(vec![]));
        snap.fallback_indexes.insert(tag.clone(), s.clone());
        let m = marks(&[(&s, &["v1"]), (&f, &[&tag])], &[]);
        let p = plan(&m, &snap, CAPS, false);
        assert_eq!(p.layers, vec![vec![s, f]]);
        assert!(p.side_effects.is_empty());
    }

    #[test]
    fn missing_referrers_key_is_unknown() {
        let s = d(1);
        let snap = snap_of(&[(&s, &["v1"])]);
        let p = plan(&marks(&[(&s, &["v1"])], &[]), &snap, CAPS, false);
        assert_eq!(p.side_effects, vec![ReferrersUnknown { digest: s }]);
    }

    #[test]
    fn tagged_child_not_unreferenced() {
        let (i, c) = (d(1), d(2));
        let mut snap = snap_of(&[(&i, &["v1"]), (&c, &["v2"])]);
        edge(&mut snap, &i, &c);
        snap.referrers.insert(i.clone(), Some(vec![]));
        let p = plan(&marks(&[(&i, &["v1"])], &[]), &snap, CAPS, false);
        assert_eq!(p.layers, vec![vec![i]]);
        assert!(p.side_effects.is_empty());
    }

    #[test]
    fn exclusion_propagates_across_rules() {
        let (i, s, r) = (d(1), d(2), d(3));
        let mut snap = snap_of(&[(&i, &["v1"]), (&s, &[]), (&r, &[])]);
        edge(&mut snap, &i, &s);
        snap.subjects.insert(r.clone(), s.clone());
        let m = marks(&[], &[(&s, Explicit), (&r, Automatic)]);
        let p = plan(&m, &snap, CAPS, false);
        assert_eq!(
            p.excluded,
            vec![
                (s.clone(), ChildOfKeptIndex(i)),
                (r, AttachedToKeptManifest(s))
            ]
        );
        assert!(p.layers.is_empty());
    }
}

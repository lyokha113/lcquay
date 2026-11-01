use std::collections::{BTreeMap, BTreeSet};

use super::{ArtifactSummary, Digest, RegistryError};

/// Per-repo delete selection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Marks {
    /// Selected tags per manifest.
    pub tags: BTreeMap<Digest, BTreeSet<String>>,
    /// Untagged manifests.
    pub untagged: BTreeMap<Digest, MarkKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkKind {
    Explicit,
    /// From `u`.
    Automatic,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DepSnapshot {
    /// All accessible pages exhausted (not `Inventory::Complete`); partial listings never reach the planner.
    pub inventory: Vec<ArtifactSummary>,
    /// Child → index parents.
    pub parents: BTreeMap<Digest, BTreeSet<Digest>>,
    /// Artifact → subject.
    pub subjects: BTreeMap<Digest, Digest>,
    /// `sha256-<hex>` tag → subject.
    pub fallback_indexes: BTreeMap<String, Digest>,
    /// Marked manifests whose own manifest failed to load.
    pub unreadable: BTreeSet<Digest>,
    /// `None` = referrers unknown.
    pub referrers: BTreeMap<Digest, Option<Vec<Digest>>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeleteOp {
    RemoveTag { tag: String, expected: Digest },
    DeleteManifest { digest: Digest },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeletePlan {
    /// `RemoveTag` only.
    pub remove_tags: Vec<DeleteOp>,
    /// `DeleteManifest`, layer 0 first.
    pub layers: Vec<Vec<Digest>>,
    pub collateral: Vec<(Digest, Vec<String>)>,
    pub excluded: Vec<(Digest, ExcludeReason)>,
    pub side_effects: Vec<SideEffect>,
    /// E.g. `TaggedOnly` notice.
    pub banner: Option<String>,
}

impl DeletePlan {
    /// Canonical op order for reports: tag removals, then manifests layer by layer.
    pub fn ops(&self) -> Vec<DeleteOp> {
        let manifests = self
            .layers
            .iter()
            .flatten()
            .map(|d| DeleteOp::DeleteManifest { digest: d.clone() });
        self.remove_tags.iter().cloned().chain(manifests).collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExcludeReason {
    /// Own manifest unreadable.
    DependencyUnknown,
    ChildOfKeptIndex(Digest),
    AttachedToKeptManifest(Digest),
    /// Partial selection without `delete_tag`.
    NeedsWholeManifest,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SideEffect {
    UnreferencedChild {
        child: Digest,
        index: Digest,
    },
    OrphanedReferrer {
        referrer: Digest,
        subject: Digest,
    },
    /// Fallback index for a deleted subject, or listing a deleted referrer.
    FallbackIndexStale {
        tag: String,
    },
    ReferrersUnknown {
        digest: Digest,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeleteReport {
    /// Every planned op appears exactly once.
    pub results: Vec<(DeleteOp, OpOutcome)>,
    /// Deletion reported unsupported; later ops `NotAttempted`.
    pub aborted_unsupported: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OpOutcome {
    Done,
    Failed(String),
    /// Parent not deleted.
    Skipped(String),
    /// The call may have been dispatched but returned no per-op answer (network error, timeout).
    Uncertain(String),
    /// Known not dispatched.
    NotAttempted,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SnapshotError {
    Inventory(RegistryError),
    UnreadableIndex,
}

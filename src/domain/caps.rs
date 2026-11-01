#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inventory {
    /// Untagged manifests and metadata are listed.
    Complete,
    TaggedOnly,
}

/// Whether the registry lets an existing tag be moved (push preflight, advisory).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagPolicy {
    Mutable,
    Immutable,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub inventory: Inventory,
    pub pushed_at: bool,
    /// Approximate where available.
    pub last_pulled: bool,
    /// Guarded removal of one tag, keeping the manifest.
    pub delete_tag: bool,
    pub delete_manifest: bool,
}

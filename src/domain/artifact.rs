use std::collections::BTreeMap;
use std::fmt;

use time::OffsetDateTime;

use super::{Digest, RepoPath};

#[derive(Clone, Debug, PartialEq)]
pub enum ArtifactStatus {
    Active,
    Archived,
    Other(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RepoSummary {
    pub path: RepoPath,
    pub created_at: Option<OffsetDateTime>,
}

/// Provider-reported label/value rows for a registry or repository, in display order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Details {
    pub fields: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactSummary {
    pub digest: Digest,
    pub tags: Vec<String>,
    pub media_type: Option<String>,
    pub artifact_type: Option<String>,
    /// When the provider reports it (ECR subjectManifestDigest).
    pub subject: Option<Digest>,
    /// Compressed, as reported; None = computed on focus.
    pub size: Option<u64>,
    pub pushed_at: Option<OffsetDateTime>,
    pub last_pulled: Option<OffsetDateTime>,
    pub status: ArtifactStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    pub variant: Option<String>,
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)?;
        if let Some(v) = &self.variant {
            write!(f, "/{v}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Descriptor {
    pub media_type: String,
    pub digest: Digest,
    pub size: u64,
    pub platform: Option<Platform>,
    pub artifact_type: Option<String>,
    pub annotations: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ManifestKind {
    Image {
        config: Descriptor,
        layers: Vec<Descriptor>,
    },
    Index {
        manifests: Vec<Descriptor>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ManifestDocument {
    pub media_type: String,
    pub raw: String,
    pub kind: ManifestKind,
    pub subject: Option<Descriptor>,
    pub artifact_type: Option<String>,
    pub annotations: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    pub created_by: String,
    pub created: Option<OffsetDateTime>,
    pub empty_layer: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageConfig {
    pub raw: String,
    pub created: Option<OffsetDateTime>,
    pub platform: Option<Platform>,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub exposed_ports: Vec<String>,
    pub volumes: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub env: Vec<(String, String)>,
    pub history: Vec<HistoryEntry>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LocalImage {
    pub id: String,
    pub repo_tags: Vec<String>,
    pub repo_digests: Vec<String>,
    /// Digests of the platform image manifests held locally, when the image is an index
    /// (multi-platform image store, e.g. containerd). Empty otherwise.
    pub manifests: Vec<String>,
    pub created: Option<OffsetDateTime>,
    pub size: u64,
}

/// Decimal units, as registries report them.
pub fn fmt_size(b: u64) -> String {
    const U: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if b < 1000 {
        return format!("{b}B");
    }
    let mut v = b as f64 / 1000.0;
    let mut u = 0;
    while v >= 1000.0 && u < U.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if v >= 100.0 {
        format!("{v:.0}{}", U[u])
    } else {
        format!("{v:.1}{}", U[u])
    }
}

/// A history step as a Dockerfile instruction: `/bin/sh -c #(nop) X` is `X`, `/bin/sh -c X`
/// is `RUN X`, anything else (BuildKit `RUN |2 …`, `… # buildkit`) is kept.
pub fn instruction(created_by: &str) -> String {
    if let Some(rest) = created_by.strip_prefix("/bin/sh -c #(nop) ") {
        rest.trim_start().to_owned()
    } else if let Some(rest) = created_by.strip_prefix("/bin/sh -c ") {
        format!("RUN {rest}")
    } else {
        created_by.to_owned()
    }
}

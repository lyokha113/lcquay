use std::fmt;

use super::RegistryError;

macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub String);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_newtype!(RegistryId);
string_newtype!(RepoPath);

/// `<alg>:<hex>`; `alg` is lowercase `[a-z0-9]+`, hex is lowercase (64 chars for sha256).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest(String);

impl Digest {
    pub fn parse(s: &str) -> Result<Digest, RegistryError> {
        let bad = || RegistryError::Internal(format!("invalid digest: {s}"));
        let (alg, hex) = s.split_once(':').ok_or_else(bad)?;
        let alg_ok = !alg.is_empty()
            && alg
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        let hex_ok = !hex.is_empty()
            && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            && (alg != "sha256" || hex.len() == 64);
        if alg_ok && hex_ok {
            Ok(Digest(s.to_owned()))
        } else {
            Err(bad())
        }
    }

    pub fn algorithm(&self) -> &str {
        self.0.split_once(':').map_or("", |(a, _)| a)
    }

    pub fn hex(&self) -> &str {
        self.0.split_once(':').map_or("", |(_, h)| h)
    }

    /// First 12 hex chars.
    pub fn short(&self) -> String {
        self.hex().chars().take(12).collect()
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reference {
    Tag(String),
    Digest(Digest),
}

pub fn image_ref(ref_host: &str, repo: &RepoPath, r: &Reference) -> String {
    match r {
        Reference::Tag(t) => format!("{ref_host}/{repo}:{t}"),
        Reference::Digest(d) => format!("{ref_host}/{repo}@{d}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_parse_and_display() {
        let d = Digest::parse(
            "sha256:e4498f74aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        assert_eq!(d.algorithm(), "sha256");
        assert_eq!(d.short(), "e4498f74aaaa");
        assert!(Digest::parse("sha256:XYZ").is_err());
        assert!(Digest::parse("nocolon").is_err());
        assert!(Digest::parse("sha256:abc").is_err()); // wrong length
    }

    #[test]
    fn image_ref_forms() {
        let repo = RepoPath("api-svc".into());
        assert_eq!(
            image_ref("h.example", &repo, &Reference::Tag("0.1.0-dev".into())),
            "h.example/api-svc:0.1.0-dev"
        );
        let d = Digest::parse(&format!("sha256:{}", "a".repeat(64))).unwrap();
        assert_eq!(
            image_ref("h.example", &repo, &Reference::Digest(d)),
            format!("h.example/api-svc@sha256:{}", "a".repeat(64))
        );
    }
}

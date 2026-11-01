//! Pagination helpers (spec §8 pagination guard).

use std::collections::HashSet;

use reqwest::header::{HeaderMap, LINK};
use url::Url;

use crate::domain::RegistryError;

/// The `rel="next"` target of the `Link` header(s), resolved against `base`
/// (the request URL).
pub fn next_link(headers: &HeaderMap, base: &Url) -> Option<Url> {
    for value in headers.get_all(LINK) {
        let Ok(mut rest) = value.to_str() else {
            continue;
        };
        // Each link is `<target>; param; param`, links separated by commas.
        // Targets are delimited by `<>` so commas inside them are harmless.
        while let Some(start) = rest.find('<') {
            let Some(len) = rest[start + 1..].find('>') else {
                break;
            };
            let target = &rest[start + 1..start + 1 + len];
            rest = &rest[start + 2 + len..];
            let params_end = rest.find('<').unwrap_or(rest.len());
            let is_next = rest[..params_end].split([';', ',']).any(|p| {
                p.trim().split_once('=').is_some_and(|(k, v)| {
                    k.trim().eq_ignore_ascii_case("rel")
                        && v.trim()
                            .trim_matches('"')
                            .split_whitespace()
                            .any(|r| r.eq_ignore_ascii_case("next"))
                })
            });
            if is_next {
                return base.join(target).ok();
            }
        }
    }
    None
}

/// Ends a listing whose cursor repeats or whose page adds nothing while
/// promising more.
#[derive(Default)]
pub struct CursorGuard {
    seen: HashSet<String>,
}

impl CursorGuard {
    pub fn check(
        &mut self,
        cursor: &str,
        new_items: usize,
        promises_more: bool,
    ) -> Result<(), RegistryError> {
        if !self.seen.insert(cursor.to_owned()) || (new_items == 0 && promises_more) {
            // Link cursors are URLs: never put their query string in a message.
            let shown = cursor.split('?').next().unwrap_or_default();
            return Err(RegistryError::RepeatedCursor(shown.to_owned()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RegistryError;
    use reqwest::header::{HeaderMap, HeaderValue, LINK};
    use url::Url;

    fn link(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(LINK, HeaderValue::from_str(v).unwrap());
        h
    }

    #[test]
    fn next_link_resolves_relative() {
        let base = Url::parse("https://r.example/v2/_catalog?n=100").unwrap();
        let h = link(r#"</v2/_catalog?last=b&n=100>; rel="next""#);
        assert_eq!(
            next_link(&h, &base).unwrap().as_str(),
            "https://r.example/v2/_catalog?last=b&n=100"
        );
        let h = link(r#"<https://other.example/x>; rel=next"#);
        assert_eq!(
            next_link(&h, &base).unwrap().as_str(),
            "https://other.example/x"
        );
    }

    #[test]
    fn next_link_picks_next_among_many() {
        let base = Url::parse("https://r.example/v2/").unwrap();
        let h = link(r#"</p?a=1,2>; rel="prev", </n>; title="x"; rel="next""#);
        assert_eq!(
            next_link(&h, &base).unwrap().as_str(),
            "https://r.example/n"
        );
        assert_eq!(next_link(&link(r#"</p>; rel="prev""#), &base), None);
        assert_eq!(next_link(&HeaderMap::new(), &base), None);
    }

    #[test]
    fn cursor_guard() {
        let mut g = CursorGuard::default();
        assert_eq!(g.check("a", 5, true), Ok(()));
        assert_eq!(g.check("b", 5, true), Ok(()));
        assert!(matches!(
            g.check("a", 5, true),
            Err(RegistryError::RepeatedCursor(_))
        ));
        let mut g = CursorGuard::default();
        assert!(matches!(
            g.check("c", 0, true),
            Err(RegistryError::RepeatedCursor(_))
        ));
        assert_eq!(g.check("d", 0, false), Ok(()));
    }

    #[test]
    fn cursor_error_has_no_query_string() {
        let mut g = CursorGuard::default();
        let c = "https://r.example/v2/_catalog?last=secretrepo&n=100";
        g.check(c, 5, true).unwrap();
        let msg = g.check(c, 5, true).unwrap_err().to_string();
        assert!(!msg.contains('?') && !msg.contains("secretrepo"), "{msg}");
        assert!(msg.contains("/v2/_catalog"), "{msg}");
        let mut g = CursorGuard::default();
        let msg = g
            .check("/v2/_catalog?last=x", 0, true)
            .unwrap_err()
            .to_string();
        assert!(!msg.contains('?') && !msg.contains("last="), "{msg}");
    }
}

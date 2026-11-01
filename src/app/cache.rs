//! One map of fetch results keyed by `CacheKey` (spec §8): one fetch per key in flight,
//! request-id guard, `refetch` flag for refreshes that arrive while loading.

use std::collections::HashMap;

use time::OffsetDateTime;

use super::state::{CacheKey, CacheValue, Effect, RequestId};
use crate::domain::RegistryError;

// CacheValue is large (manifest/config inline); one value per fetch, so size is irrelevant.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Loading {
        req: RequestId,
        refetch: bool,
        pages: usize,
        /// The result applied while `refetch` was set, shown until this fetch completes.
        stale: Option<Box<Entry>>,
    },
    Ready {
        req: RequestId,
        fetched_at: OffsetDateTime,
        value: CacheValue,
    },
    Failed {
        req: RequestId,
        error: RegistryError,
    },
}

/// Owns the monotonic request counter: every issued fetch gets a fresh, never reused id.
#[derive(Debug, Default)]
pub struct Cache {
    entries: HashMap<CacheKey, Entry>,
    next_req: u64,
}

impl Cache {
    pub fn get(&self, key: &CacheKey) -> Option<&Entry> {
        self.entries.get(key)
    }

    /// The `Ready` value, or the stale `Ready` value of a refetch in flight.
    pub fn value(&self, key: &CacheKey) -> Option<&CacheValue> {
        match self.get(key)? {
            Entry::Ready { value, .. } => Some(value),
            Entry::Loading { stale: Some(e), .. } => match &**e {
                Entry::Ready { value, .. } => Some(value),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = &CacheKey> {
        self.entries.keys()
    }

    /// Issues a fetch only when the key is absent.
    pub fn ensure(&mut self, key: CacheKey) -> Option<Effect> {
        (!self.entries.contains_key(&key)).then(|| self.issue(key, None))
    }

    /// `r`: `Ready`/`Failed`/absent → (re)issue; `Loading` → set `refetch`. A `Ready` value
    /// stays visible (as `stale`) until the new result arrives.
    pub fn refresh(&mut self, key: CacheKey) -> Option<Effect> {
        self.reload(key, true)
    }

    /// After an own push/delete: as `refresh`, but a `Ready` value is dropped (it is known to be
    /// out of date, e.g. still lists deleted images).
    pub fn invalidate(&mut self, key: CacheKey) -> Option<Effect> {
        self.reload(key, false)
    }

    fn reload(&mut self, key: CacheKey, keep: bool) -> Option<Effect> {
        let stale = match self.entries.get_mut(&key) {
            Some(Entry::Loading { refetch, .. }) => {
                *refetch = true;
                return None;
            }
            Some(e @ Entry::Ready { .. }) if keep => Some(Box::new(e.clone())),
            _ => None,
        };
        Some(self.issue(key, stale))
    }

    /// True when `key` is `Loading` with this `req` (the only state a result is applied in).
    pub fn is_pending(&self, key: &CacheKey, req: RequestId) -> bool {
        matches!(self.get(key), Some(Entry::Loading { req: r, .. }) if *r == req)
    }

    pub fn set_pages(&mut self, key: &CacheKey, n: usize) {
        if let Some(Entry::Loading { pages, .. }) = self.entries.get_mut(key) {
            *pages = n;
        }
    }

    /// Ignored unless `Loading` with the same `req`. With `refetch` set, the result is stored
    /// and a new fetch is issued.
    pub fn apply(
        &mut self,
        key: CacheKey,
        req: RequestId,
        result: Result<CacheValue, RegistryError>,
        now: OffsetDateTime,
    ) -> Option<Effect> {
        let Some(Entry::Loading {
            req: r, refetch, ..
        }) = self.entries.get(&key)
        else {
            return None;
        };
        if *r != req {
            return None;
        }
        let refetch = *refetch;
        let done = match result {
            Ok(value) => Entry::Ready {
                req,
                fetched_at: now,
                value,
            },
            Err(error) => Entry::Failed { req, error },
        };
        if refetch {
            return Some(self.issue(key, Some(Box::new(done))));
        }
        self.entries.insert(key, done);
        None
    }

    /// Replaces any entry with a fresh `Loading` (the "remove + issue" of a refresh).
    fn issue(&mut self, key: CacheKey, stale: Option<Box<Entry>>) -> Effect {
        self.next_req += 1;
        let req = RequestId(self.next_req);
        let loading = Entry::Loading {
            req,
            refetch: false,
            pages: 0,
            stale,
        };
        self.entries.insert(key.clone(), loading);
        Effect::Fetch { key, req }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RegistryId;

    fn key() -> CacheKey {
        CacheKey::Repos(RegistryId("r".into()))
    }

    fn ok() -> Result<CacheValue, RegistryError> {
        Ok(CacheValue::Repos(vec![]))
    }

    fn fetch_req(e: Option<Effect>) -> RequestId {
        match e {
            Some(Effect::Fetch { req, .. }) => req,
            other => panic!("expected fetch, got {other:?}"),
        }
    }

    const NOW: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

    #[test]
    fn cache_ensure_only_when_absent() {
        let mut c = Cache::default();
        let r1 = fetch_req(c.ensure(key()));
        assert_eq!(c.ensure(key()), None, "loading: no second fetch");
        assert_eq!(c.apply(key(), r1, ok(), NOW), None);
        assert_eq!(c.ensure(key()), None, "ready: no fetch");
        assert_eq!(c.value(&key()), Some(&CacheValue::Repos(vec![])));
    }

    #[test]
    fn cache_refresh_keeps_ready_value_until_result() {
        let mut c = Cache::default();
        let r1 = fetch_req(c.ensure(key()));
        c.apply(key(), r1, ok(), NOW);
        let r2 = fetch_req(c.refresh(key()));
        assert_eq!(
            c.value(&key()),
            Some(&CacheValue::Repos(vec![])),
            "stale while loading"
        );
        c.apply(key(), r2, Err(RegistryError::NotFound), NOW);
        assert_eq!(c.value(&key()), None, "the new result replaces it");
    }

    #[test]
    fn cache_invalidate_drops_ready_value() {
        let mut c = Cache::default();
        let r1 = fetch_req(c.ensure(key()));
        c.apply(key(), r1, ok(), NOW);
        fetch_req(c.invalidate(key()));
        assert_eq!(c.value(&key()), None);
    }

    #[test]
    fn cache_refresh_on_loading_sets_refetch_and_issues_after_result() {
        let mut c = Cache::default();
        let r1 = fetch_req(c.ensure(key()));
        assert_eq!(c.refresh(key()), None);
        assert!(matches!(
            c.get(&key()),
            Some(Entry::Loading { refetch: true, .. })
        ));
        let r2 = fetch_req(c.apply(key(), r1, ok(), NOW));
        assert_ne!(r1, r2);
        assert_eq!(
            c.value(&key()),
            Some(&CacheValue::Repos(vec![])),
            "value stored"
        );
        assert!(c.is_pending(&key(), r2));
        assert_eq!(c.apply(key(), r2, ok(), NOW), None, "no further refetch");
        assert!(matches!(c.get(&key()), Some(Entry::Ready { req, .. }) if *req == r2));
    }

    #[test]
    fn cache_ignores_wrong_req() {
        let mut c = Cache::default();
        let r1 = fetch_req(c.ensure(key()));
        let wrong = RequestId(r1.0 + 100);
        assert_eq!(c.apply(key(), wrong, ok(), NOW), None);
        assert!(c.is_pending(&key(), r1));
        c.apply(key(), r1, ok(), NOW);
        assert_eq!(c.apply(key(), r1, Err(RegistryError::NotFound), NOW), None);
        assert!(
            matches!(c.get(&key()), Some(Entry::Ready { .. })),
            "duplicate ignored"
        );
    }

    #[test]
    fn cache_refresh_ready_and_failed_reissues() {
        let mut c = Cache::default();
        let r1 = fetch_req(c.ensure(key()));
        c.apply(key(), r1, Err(RegistryError::NotFound), NOW);
        let r2 = fetch_req(c.refresh(key()));
        assert!(r2 > r1);
        c.set_pages(&key(), 3);
        assert!(matches!(
            c.get(&key()),
            Some(Entry::Loading { pages: 3, .. })
        ));
    }
}

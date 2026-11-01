//! Progress of a multi-item transfer (pull, copy, export): per-item state, byte totals and a
//! sliding-window speed.

use std::collections::VecDeque;

use time::OffsetDateTime;

use crate::domain::{Digest, ItemStep, RegistryError, TransferStep, fmt_size};

/// The speed is measured over this much recent progress.
pub const SPEED_WINDOW: time::Duration = time::Duration::seconds(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferKind {
    Pull,
    Copy,
    Export,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: String,
    /// The planned size, else the first total a step reported.
    pub size: Option<u64>,
    pub state: ItemStep,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Transfer {
    pub kind: TransferKind,
    pub items: Vec<Item>,
    pub started: OffsetDateTime,
    pub finished: Option<OffsetDateTime>,
    /// `(time, moved bytes)`, oldest first, trimmed to the speed window.
    samples: VecDeque<(OffsetDateTime, u64)>,
}

impl Transfer {
    /// `planned`: `(id, size)` of the items known up front, in display order.
    pub fn new(kind: TransferKind, planned: Vec<(String, u64)>, now: OffsetDateTime) -> Self {
        let items = planned
            .into_iter()
            .map(|(id, size)| Item {
                id,
                size: Some(size),
                state: ItemStep::Waiting { total: Some(size) },
            })
            .collect();
        Transfer {
            kind,
            items,
            started: now,
            finished: None,
            samples: VecDeque::new(),
        }
    }

    /// Updates (or appends) the item of `s`; a planned size is never replaced.
    pub fn apply(&mut self, s: TransferStep, now: OffsetDateTime) {
        let total = match s.step {
            ItemStep::Waiting { total } => total,
            ItemStep::Bytes { total, .. } => Some(total),
            _ => None,
        };
        let i = match self.items.iter().position(|i| i.id == s.id) {
            Some(i) => i,
            None => {
                self.items.push(Item {
                    id: s.id,
                    size: None,
                    state: s.step.clone(),
                });
                self.items.len() - 1
            }
        };
        let item = &mut self.items[i];
        item.size = item.size.or(total);
        item.state = s.step;
        self.samples.push_back((now, self.moved()));
        let bound = now - SPEED_WINDOW;
        while self.samples.get(1).is_some_and(|(t, _)| *t <= bound) {
            self.samples.pop_front();
        }
    }

    /// A successful transfer ends: items still `Waiting` had nothing to move, so they existed.
    pub fn settle(&mut self) {
        for i in &mut self.items {
            if matches!(i.state, ItemStep::Waiting { .. }) {
                i.state = ItemStep::Exists;
            }
        }
    }

    fn done(i: &Item) -> u64 {
        match i.state {
            ItemStep::Waiting { .. } => 0,
            ItemStep::Bytes { current, .. } => current,
            _ => i.size.unwrap_or(0),
        }
    }

    /// Done of total, `None` while the total is unknown (0).
    pub fn percent(&self) -> Option<u64> {
        let total = self.total_bytes();
        (total > 0).then(|| (self.done_bytes() as u128 * 100 / total as u128).min(100) as u64)
    }

    pub fn done_bytes(&self) -> u64 {
        self.items.iter().map(Self::done).sum()
    }

    pub fn total_bytes(&self) -> u64 {
        self.items.iter().filter_map(|i| i.size).sum()
    }

    /// Bytes actually transferred: skipped (`Exists`/`Mounted`) items do not count.
    pub fn moved(&self) -> u64 {
        self.items
            .iter()
            .filter(|i| !matches!(i.state, ItemStep::Exists | ItemStep::Mounted))
            .map(Self::done)
            .sum()
    }

    /// Bytes/s over the last `SPEED_WINDOW` (plus the one sample before it); 0 when stalled.
    pub fn speed(&self, now: OffsetDateTime) -> f64 {
        let bound = now - SPEED_WINDOW;
        let Some(&(last_t, last)) = self.samples.back() else {
            return 0.0;
        };
        if last_t < bound {
            return 0.0;
        }
        let inside = self.samples.partition_point(|(t, _)| *t < bound);
        let (first_t, first) = self.samples[inside.saturating_sub(1)];
        let span = (last_t - first_t).as_seconds_f64();
        if span <= 0.0 {
            return 0.0;
        }
        last.saturating_sub(first) as f64 / span
    }

    /// `None` while the speed is 0.
    pub fn eta(&self, now: OffsetDateTime) -> Option<time::Duration> {
        let speed = self.speed(now);
        (speed > 0.0).then(|| {
            let left = self.total_bytes().saturating_sub(self.done_bytes());
            time::Duration::seconds_f64(left as f64 / speed)
        })
    }

    pub fn phase(&self) -> &'static str {
        if self.finished.is_some() {
            return "Done";
        }
        match self.kind {
            TransferKind::Copy => "Uploading",
            TransferKind::Export => "Downloading",
            // Docker extracts layers in order, so extraction is the phase once all are fetched.
            TransferKind::Pull
                if !self.items.is_empty()
                    && self.items.iter().all(|i| {
                        !matches!(i.state, ItemStep::Waiting { .. } | ItemStep::Bytes { .. })
                    }) =>
            {
                "Extracting"
            }
            TransferKind::Pull => "Downloading",
        }
    }

    /// At most `max` items, unfinished first, then finished; plus how many more there are.
    pub fn rows(&self, max: usize) -> (Vec<&Item>, usize) {
        let finished = |i: &&Item| {
            matches!(
                i.state,
                ItemStep::Done | ItemStep::Exists | ItemStep::Mounted
            )
        };
        let (done, open): (Vec<&Item>, Vec<&Item>) = self.items.iter().partition(finished);
        let mut shown: Vec<&Item> = open.into_iter().chain(done).collect();
        let more = shown.len().saturating_sub(max);
        shown.truncate(max);
        (shown, more)
    }
}

impl Transfer {
    /// ` · 12.9MB in 4.2s (avg 3.0 MB/s)` once finished, else empty. The size is left out while
    /// unknown; the average counts moved bytes only and is left out when nothing
    /// moved (every blob existed or was mounted).
    pub fn summary(&self) -> String {
        let Some(end) = self.finished else {
            return String::new();
        };
        let secs = (end - self.started).as_seconds_f64();
        let mut s = String::new();
        if self.total_bytes() > 0 {
            s += &format!(" · {}", fmt_size(self.total_bytes()));
        }
        s += &format!(" in {secs:.1}s");
        if secs > 0.0 && self.moved() > 0 {
            s += &format!(" (avg {})", fmt_speed(self.moved() as f64 / secs));
        }
        s
    }
}

/// `3.1 MB/s`.
pub fn fmt_speed(bytes_per_s: f64) -> String {
    format!("{:.1} MB/s", bytes_per_s / 1e6)
}

#[derive(Clone, Debug, PartialEq)]
pub enum TransferOutcome {
    Done,
    Cancelled,
    Failed(RegistryError),
    Uncertain(String),
    DigestDiffers { got: Digest, want: Digest },
    Exists,
}

#[cfg(test)]
mod tests {
    use time::Duration;
    use time::macros::datetime;

    use super::*;

    const T0: OffsetDateTime = datetime!(2026-10-31 12:00 UTC);

    fn step(id: &str, step: ItemStep) -> TransferStep {
        TransferStep {
            id: id.into(),
            step,
        }
    }

    fn bytes(current: u64, total: u64) -> ItemStep {
        ItemStep::Bytes { current, total }
    }

    fn pull(planned: &[(&str, u64)]) -> Transfer {
        let planned = planned.iter().map(|(i, s)| (i.to_string(), *s)).collect();
        Transfer::new(TransferKind::Pull, planned, T0)
    }

    #[test]
    fn planned_sizes_win() {
        let mut t = pull(&[("aaaaaaaaaaaa", 100)]);
        t.apply(step("aaaaaaaaaaaa", bytes(50, 999)), T0);
        assert_eq!(t.total_bytes(), 100);
        assert_eq!(t.done_bytes(), 50);
    }

    #[test]
    fn unknown_id_appended() {
        let mut t = pull(&[("aaaaaaaaaaaa", 100)]);
        t.apply(step("bbbbbbbbbbbb", ItemStep::Waiting { total: None }), T0);
        assert_eq!(t.items[1].size, None);
        t.apply(step("bbbbbbbbbbbb", bytes(10, 40)), T0);
        t.apply(step("cccccccccccc", bytes(5, 70)), T0);
        let ids: Vec<_> = t.items.iter().map(|i| (i.id.as_str(), i.size)).collect();
        assert_eq!(
            ids,
            [
                ("aaaaaaaaaaaa", Some(100)),
                ("bbbbbbbbbbbb", Some(40)),
                ("cccccccccccc", Some(70)),
            ]
        );
        assert_eq!(t.total_bytes(), 210);
        assert_eq!(t.done_bytes(), 15);
    }

    #[test]
    fn exists_counts_for_percent_not_speed() {
        let mut t = pull(&[("a", 100), ("m", 50)]);
        t.apply(step("a", ItemStep::Exists), T0);
        t.apply(step("m", ItemStep::Mounted), T0 + Duration::seconds(1));
        assert_eq!(t.done_bytes(), 150);
        assert_eq!(t.speed(T0 + Duration::seconds(1)), 0.0);
    }

    #[test]
    fn speed_over_two_seconds() {
        let mut t = pull(&[("a", 3_000_000)]);
        t.apply(step("a", bytes(0, 3_000_000)), T0);
        t.apply(
            step("a", bytes(1_000_000, 3_000_000)),
            T0 + Duration::seconds(1),
        );
        t.apply(
            step("a", bytes(3_000_000, 3_000_000)),
            T0 + Duration::seconds(2),
        );
        assert_eq!(t.speed(T0 + Duration::seconds(2)), 1_500_000.0);
        assert_eq!(t.eta(T0 + Duration::seconds(2)), Some(Duration::ZERO));
        assert_eq!(t.speed(T0 + Duration::seconds(5)), 0.0);
    }

    #[test]
    fn speed_keeps_one_sample_before_the_window() {
        let mut t = pull(&[("a", 10_000)]);
        t.apply(step("a", bytes(0, 10_000)), T0);
        t.apply(step("a", bytes(1_000, 10_000)), T0 + Duration::seconds(1));
        t.apply(step("a", bytes(4_000, 10_000)), T0 + Duration::seconds(4));
        // Window [2 s, 4 s] plus the 1 s sample before it: 3000 B over 3 s.
        assert_eq!(t.speed(T0 + Duration::seconds(4)), 1_000.0);
        assert_eq!(t.eta(T0 + Duration::seconds(4)), Some(Duration::seconds(6)));
    }

    #[test]
    fn eta_hidden_at_zero_speed() {
        let mut t = pull(&[("a", 100)]);
        assert_eq!(t.eta(T0), None);
        t.apply(step("a", bytes(10, 100)), T0);
        assert_eq!(t.eta(T0), None, "one sample spans no time");
    }

    #[test]
    fn rows_unfinished_first() {
        let ids: Vec<String> = (0..8).map(|i| format!("{i}")).collect();
        let mut t = Transfer::new(
            TransferKind::Pull,
            ids.iter().map(|i| (i.clone(), 10)).collect(),
            T0,
        );
        for i in ["1", "4", "6"] {
            t.apply(step(i, ItemStep::Done), T0);
        }
        let (shown, more) = t.rows(6);
        let shown: Vec<_> = shown.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(shown, ["0", "2", "3", "5", "7", "1"]);
        assert_eq!(more, 2);
        let (all, more) = t.rows(20);
        assert_eq!((all.len(), more), (8, 0));
    }

    #[test]
    fn summary_once_finished() {
        let mut t = pull(&[("a", 3_000_000)]);
        assert_eq!(t.summary(), "");
        t.apply(step("a", ItemStep::Done), T0);
        t.finished = Some(T0 + Duration::seconds(2));
        assert_eq!(t.summary(), " · 3.0MB in 2.0s (avg 1.5 MB/s)");
        // Nothing moved: no average, whatever the kind.
        let mut p = pull(&[("a", 100)]);
        p.apply(step("a", ItemStep::Exists), T0);
        p.finished = Some(T0 + Duration::seconds(1));
        assert_eq!(p.summary(), " · 100B in 1.0s");
        let mut c = Transfer::new(TransferKind::Copy, vec![("a".into(), 100)], T0);
        c.apply(step("a", ItemStep::Mounted), T0);
        c.finished = Some(T0 + Duration::seconds(1));
        assert_eq!(c.summary(), " · 100B in 1.0s");
    }

    #[test]
    fn phase_by_kind() {
        let mut t = pull(&[("a", 10), ("b", 10)]);
        assert_eq!(t.phase(), "Downloading");
        t.apply(step("a", ItemStep::Downloaded), T0);
        assert_eq!(t.phase(), "Downloading");
        t.apply(step("b", ItemStep::Extracting), T0);
        assert_eq!(t.phase(), "Extracting");
        t.finished = Some(T0);
        assert_eq!(t.phase(), "Done");
        let copy = Transfer::new(TransferKind::Copy, vec![], T0);
        assert_eq!(copy.phase(), "Uploading");
        let export = Transfer::new(TransferKind::Export, vec![], T0);
        assert_eq!(export.phase(), "Downloading");
    }
}

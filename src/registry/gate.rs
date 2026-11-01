//! Per-registry request permits with a focused lane (spec §8).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::oneshot;

type Probe = Arc<dyn Fn() -> bool + Send + Sync>;

tokio::task_local! {
    /// "Is the item this task fetches focused right now?" Unset = background.
    pub static FOCUSED: Probe;
}

struct Waiter {
    id: u64,
    probe: Option<Probe>,
    tx: oneshot::Sender<()>,
}

impl Waiter {
    fn is_focused(&self) -> bool {
        self.probe.as_ref().is_some_and(|p| p())
    }
}

#[derive(Default)]
struct State {
    available: usize,
    next_id: u64,
    focused: VecDeque<Waiter>,
    background: VecDeque<Waiter>,
}

impl State {
    /// First waiter whose probe says focused now (focused lane first, so
    /// background waiters are promoted), else the oldest by lane.
    // ponytail: O(waiters) probe calls per release under the lock; fine for a
    // per-registry queue, index by key if queues ever get long.
    fn pick(&mut self) -> Option<Waiter> {
        for lane in [&mut self.focused, &mut self.background] {
            if let Some(i) = lane.iter().position(Waiter::is_focused) {
                return lane.remove(i);
            }
        }
        self.focused
            .pop_front()
            .or_else(|| self.background.pop_front())
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Hands one permit to the next waiter, or returns it to the pool.
fn release(state: &Mutex<State>) {
    let mut st = lock(state);
    while let Some(w) = st.pick() {
        if w.tx.send(()).is_ok() {
            return;
        }
    }
    st.available += 1;
}

pub struct Gate {
    state: Arc<Mutex<State>>,
}

impl Gate {
    pub fn new(permits: usize) -> Self {
        Gate {
            state: Arc::new(Mutex::new(State {
                available: permits,
                ..State::default()
            })),
        }
    }

    /// Waits for a permit. `FOCUSED` (if set) picks the lane now and is asked
    /// again on every release. Cancel-safe: dropping the future never loses a
    /// permit.
    pub async fn acquire(&self) -> GatePermit {
        let probe = FOCUSED.try_with(Arc::clone).ok();
        let focused = probe.as_ref().is_some_and(|p| p());
        let mut waiting = {
            let mut st = lock(&self.state);
            if st.available > 0 {
                st.available -= 1;
                return self.permit();
            }
            let (tx, rx) = oneshot::channel();
            let id = st.next_id;
            st.next_id += 1;
            let w = Waiter { id, probe, tx };
            if focused {
                st.focused.push_back(w);
            } else {
                st.background.push_back(w);
            }
            Waiting {
                state: &self.state,
                id,
                rx,
                granted: false,
            }
        };
        (&mut waiting.rx)
            .await
            .expect("waiter senders live in the gate state, which outlives this future");
        waiting.granted = true;
        self.permit()
    }

    fn permit(&self) -> GatePermit {
        GatePermit {
            state: self.state.clone(),
        }
    }
}

/// A queued `acquire`; on drop before grant, leaves the queue and passes on a
/// permit that was already sent to it.
struct Waiting<'a> {
    state: &'a Mutex<State>,
    id: u64,
    rx: oneshot::Receiver<()>,
    granted: bool,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if self.granted {
            return;
        }
        {
            // Senders are only used under this lock, so after it either we
            // were still queued (removed here) or the permit is in `rx`.
            let mut st = lock(self.state);
            st.focused.retain(|w| w.id != self.id);
            st.background.retain(|w| w.id != self.id);
        }
        if self.rx.try_recv().is_ok() {
            release(self.state);
        }
    }
}

/// One request permit; released (to the next waiter) on drop.
pub struct GatePermit {
    state: Arc<Mutex<State>>,
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        release(&self.state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::time::{sleep, timeout};

    /// Lets every spawned task run until it blocks (time is paused).
    async fn settle() {
        sleep(Duration::from_millis(1)).await;
    }

    fn spawn_waiter(
        gate: &Arc<Gate>,
        tx: &mpsc::UnboundedSender<&'static str>,
        name: &'static str,
        probe: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) {
        let (gate, tx) = (gate.clone(), tx.clone());
        tokio::spawn(async move {
            let fut = async {
                let _p = gate.acquire().await;
                tx.send(name).unwrap();
            };
            match probe {
                Some(p) => FOCUSED.scope(p, fut).await,
                None => fut.await,
            }
        });
    }

    #[tokio::test(start_paused = true)]
    async fn gate_prefers_focused() {
        let gate = Arc::new(Gate::new(1));
        let held = gate.acquire().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        spawn_waiter(&gate, &tx, "B", None);
        settle().await;
        spawn_waiter(&gate, &tx, "F", Some(Arc::new(|| true)));
        settle().await;
        drop(held);
        assert_eq!(rx.recv().await, Some("F"));
        assert_eq!(rx.recv().await, Some("B"));
    }

    #[tokio::test(start_paused = true)]
    async fn background_waiter_promoted_when_probe_turns_true() {
        let gate = Arc::new(Gate::new(1));
        let held = gate.acquire().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let flag = Arc::new(AtomicBool::new(false));
        spawn_waiter(&gate, &tx, "B1", None);
        settle().await;
        let f = flag.clone();
        spawn_waiter(
            &gate,
            &tx,
            "B2",
            Some(Arc::new(move || f.load(Ordering::SeqCst))),
        );
        settle().await;
        flag.store(true, Ordering::SeqCst);
        drop(held);
        assert_eq!(rx.recv().await, Some("B2"));
        assert_eq!(rx.recv().await, Some("B1"));
    }

    #[tokio::test(start_paused = true)]
    async fn fifo_without_focus_and_permits_counted() {
        let gate = Arc::new(Gate::new(2));
        let a = gate.acquire().await;
        let _b = gate.acquire().await;
        assert!(
            timeout(Duration::from_secs(1), gate.acquire())
                .await
                .is_err()
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        spawn_waiter(&gate, &tx, "1", None);
        settle().await;
        spawn_waiter(&gate, &tx, "2", None);
        settle().await;
        drop(a);
        assert_eq!(rx.recv().await, Some("1"));
        assert_eq!(rx.recv().await, Some("2"));
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_queued_waiter_does_not_leak() {
        let gate = Arc::new(Gate::new(1));
        let held = gate.acquire().await;
        let g = gate.clone();
        let task = tokio::spawn(async move {
            let _p = g.acquire().await;
        });
        settle().await;
        task.abort();
        settle().await;
        drop(held);
        let p = timeout(Duration::from_secs(1), gate.acquire())
            .await
            .expect("permit leaked");
        // still exactly one permit
        assert!(
            timeout(Duration::from_secs(1), gate.acquire())
                .await
                .is_err()
        );
        drop(p);
    }

    #[tokio::test(start_paused = true)]
    async fn permit_handed_to_dropped_waiter_is_returned() {
        let gate = Gate::new(1);
        let held = gate.acquire().await;
        let mut fut = Box::pin(gate.acquire());
        assert!(futures::poll!(&mut fut).is_pending());
        drop(held); // permit is sent to `fut`'s channel...
        drop(fut); // ...which is dropped before it ever sees it
        let p = timeout(Duration::from_secs(1), gate.acquire())
            .await
            .expect("permit leaked");
        assert!(
            timeout(Duration::from_secs(1), gate.acquire())
                .await
                .is_err()
        );
        drop(p);
    }

    #[test]
    fn permit_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<GatePermit>();
    }
}

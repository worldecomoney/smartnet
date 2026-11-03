//! Node-level boot readiness — "mesh synced" as a gate for the rest of startup.
//!
//! The boot sequence is **engine → mesh → sync → rumor → RPC**: the node waits on this
//! gate until every outbound link (the peers-file targets) has completed one bootstrap
//! catch-up exchange, the local tip has reached the highest peer tip any exchange
//! reported, and no ordered pull is still armed. Only then does the RPC server bind, so
//! the node's first external answer is a synced one.
//!
//! Accounting is best-effort and monotone: tips merge with `max`, reported/pulling sets
//! are keyed per link, and readiness is sticky (once true it never reverts). Each target
//! gets **one handshake try per boot** — a failed dial/handshake reports the link
//! immediately (`peer_tip` 0) so an absent or dead peer never blocks boot; the timeout
//! remains only as a bound for a hung connect. [`wait_ready`] returns `false` when the
//! timeout fires anyway so boot proceeds with a warning (fail-open: a dead peer must not
//! brick the node).

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;

/// How long boot waits on the sync gate before proceeding anyway (with a warning) —
/// bounded so a dead or unreachable peer can never brick the node's RPC.
pub const BOOT_READY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
struct GateState {
    /// False until boot seeds the expected link set — the gate can never evaluate
    /// ready (much less vacuously) before it knows which peers must report.
    finalized: bool,
    /// Our peer-file targets: each must finish one bootstrap exchange.
    expected: HashSet<String>,
    /// Links that completed a bootstrap exchange (caught up or skipped — they had
    /// their chance).
    reported: HashSet<String>,
    /// Highest peer tip any status/announce reported.
    max_peer_tip: u64,
    /// Highest local tip any note observed (bootstrap result or a fresh head read).
    local_tip: u64,
    /// Connections with an ordered pull still in flight (catch-up not finished).
    pulling: HashSet<String>,
    /// Sticky: once true, never reverts — the wait is over.
    ready: bool,
}

/// The sync-readiness gate: shared by the mesh links/sessions (which post notes) and
/// the boot orchestrator (which waits on [`Self::subscribe`]).
#[derive(Debug)]
pub struct SyncGate {
    state: Mutex<GateState>,
    tx: watch::Sender<bool>,
}

impl SyncGate {
    /// Creates a gate with no expected set yet — not ready until [`Self::set_expected`]
    /// finalizes it (notes arriving meanwhile just accumulate).
    pub fn new() -> Arc<Self> {
        let (tx, _no_receivers_yet) = watch::channel(false);
        Arc::new(Self { state: Mutex::new(GateState::default()), tx })
    }

    /// Seeds the expected link ids (one peer-file snapshot) and finalizes the gate.
    /// Idempotent-ish by convention: boot calls this exactly once, before dialing.
    pub fn set_expected(&self, expected: impl IntoIterator<Item = String>) {
        {
            let mut state = self.state.lock().expect("sync gate poisoned");
            state.expected = expected.into_iter().collect();
            state.finalized = true;
        }
        self.evaluate();
    }

    /// Receiver for readiness — the value only ever flips once, `false → true`.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.tx.subscribe()
    }

    /// Whether the gate has already fired.
    pub fn is_ready(&self) -> bool {
        self.state.lock().expect("sync gate poisoned").ready
    }

    /// One outbound link finished its bootstrap exchange: it had its chance (now
    /// `reported`), its status tip enters the max, and the local tip it left behind is
    /// a floor.
    pub fn note_bootstrap(&self, link: &str, peer_tip: u64, local_tip: u64) {
        {
            let mut state = self.state.lock().expect("sync gate poisoned");
            state.reported.insert(link.to_string());
            state.max_peer_tip = state.max_peer_tip.max(peer_tip);
            state.local_tip = state.local_tip.max(local_tip);
        }
        self.evaluate();
    }

    /// A status exchange or block announce reported this peer's tip — evidence a peer
    /// is ahead, even if our own bootstrap to it never completed.
    pub fn note_peer_tip(&self, tip: u64) {
        {
            let mut state = self.state.lock().expect("sync gate poisoned");
            state.max_peer_tip = state.max_peer_tip.max(tip);
        }
        self.evaluate();
    }

    /// The local tip advanced (a fresh head read after inject/bootstrap).
    pub fn note_local_tip(&self, tip: u64) {
        {
            let mut state = self.state.lock().expect("sync gate poisoned");
            state.local_tip = state.local_tip.max(tip);
        }
        self.evaluate();
    }

    /// Whether `link` currently has an ordered pull in flight — idempotent per call,
    /// posted at every session-loop turn and cleared by the connection's drop guard.
    pub fn note_pull(&self, link: &str, armed: bool) {
        {
            let mut state = self.state.lock().expect("sync gate poisoned");
            if armed {
                state.pulling.insert(link.to_string());
            } else {
                state.pulling.remove(link);
            }
        }
        self.evaluate();
    }

    /// Evaluates readiness: expected links all reported, local tip ≥ the highest
    /// reported peer tip, no pull armed — sticky `false → true` only.
    fn evaluate(&self) {
        let mut state = self.state.lock().expect("sync gate poisoned");
        if state.ready || !state.finalized {
            return;
        }
        if state.expected.is_subset(&state.reported)
            && state.local_tip >= state.max_peer_tip
            && state.pulling.is_empty()
        {
            state.ready = true;
            drop(state);
            // `send_replace`, not `send`: `send` refuses to store the value when no
            // receiver exists yet (boot subscribes only after this may have fired) and
            // that lost `true` would strand every later subscriber at `false`.
            self.tx.send_replace(true);
        }
    }
}

/// Waits for a gate receiver to turn ready; `false` when `timeout` fires first or the
/// gate is dropped — the caller proceeds with a warning (fail-open: a dead peer must
/// not brick the node).
pub async fn wait_ready(mut ready: watch::Receiver<bool>, timeout: Duration) -> bool {
    let wait = async move {
        loop {
            if *ready.borrow_and_update() {
                return true;
            }
            if ready.changed().await.is_err() {
                return false; // gate dropped — treat as "not ready", caller warns
            }
        }
    };
    tokio::time::timeout(timeout, wait).await.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> String {
        format!("10.0.0.{n}:30811")
    }

    #[test]
    fn not_ready_before_expected_is_set_even_if_all_notes_arrive() {
        let gate = SyncGate::new();
        gate.note_bootstrap(&id(1), 5, 5);
        assert!(!gate.is_ready(), "unfinalized gate must never fire");
    }

    #[test]
    fn ready_when_every_target_reported_and_tips_met() {
        let gate = SyncGate::new();
        gate.set_expected([id(1), id(2)]);
        gate.note_bootstrap(&id(1), 7, 7);
        assert!(!gate.is_ready(), "one of two links still silent");
        gate.note_bootstrap(&id(2), 7, 7);
        assert!(gate.is_ready());
    }

    #[test]
    fn holds_while_local_tip_lags_a_reported_peer_tip() {
        let gate = SyncGate::new();
        gate.set_expected([id(1)]);
        gate.note_bootstrap(&id(1), 21, 15); // skipped mid-pull: peer ahead of us
        assert!(!gate.is_ready(), "tip 15 < reported peer tip 21");
        gate.note_local_tip(21);
        assert!(gate.is_ready());
    }

    #[test]
    fn holds_while_a_pull_is_armed_and_clears() {
        let gate = SyncGate::new();
        gate.set_expected([id(1)]);
        // A pull arms while the last link is still bootstrapping — readiness must wait
        // for the catch-up to finish (armed before the report that would fire it).
        gate.note_pull("10.0.0.9:40000", true);
        gate.note_bootstrap(&id(1), 9, 9);
        assert!(!gate.is_ready(), "catch-up still in flight");
        gate.note_pull("10.0.0.9:40000", false);
        assert!(gate.is_ready());
    }

    #[test]
    fn empty_expected_set_is_ready_immediately() {
        let gate = SyncGate::new();
        gate.set_expected(Vec::<String>::new());
        assert!(gate.is_ready());
    }

    #[test]
    fn readiness_is_sticky() {
        let gate = SyncGate::new();
        gate.set_expected(Vec::<String>::new());
        assert!(gate.is_ready());
        gate.note_pull("late", true);
        gate.note_peer_tip(u64::MAX);
        assert!(gate.is_ready(), "once fired, the gate never reverts");
    }

    #[tokio::test]
    async fn wait_ready_times_out_on_a_silent_gate_and_passes_on_a_ready_one() {
        let silent = SyncGate::new();
        silent.set_expected([id(1)]);
        let clock = Duration::from_millis(20);
        assert!(!wait_ready(silent.subscribe(), clock).await, "expected timeout → false");

        let quick = SyncGate::new();
        quick.set_expected(Vec::<String>::new());
        assert!(wait_ready(quick.subscribe(), clock).await);
    }

    #[tokio::test]
    async fn wait_ready_returns_when_the_gate_fires_later() {
        let gate = SyncGate::new();
        gate.set_expected([id(1)]);
        let waiter = {
            let receiver = gate.subscribe();
            tokio::spawn(async move { wait_ready(receiver, Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        gate.note_bootstrap(&id(1), 3, 3);
        assert!(waiter.await.expect("wait task"));
    }
}

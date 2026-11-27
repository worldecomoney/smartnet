//! Node-owned transaction-pool lifecycle policy.
//!
//! Every node — validator or RPC — owns and maintains its own txpool. The policy values live here,
//! in the node crate, and are pushed down into each txpool when the node builds it. The txpool
//! crate stays a mechanism: it holds no policy globals and makes no policy decisions.

use std::{sync::OnceLock, time::Duration};

/// Queued-tx TTL default and ceiling (operator ruling 2026-11-22): 10 minutes.
pub const DEFAULT_QUEUED_TTL: Duration = Duration::from_secs(10 * 60);
/// How often the node sweeps stale txs, even when the chain emits no canonical event.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// Default number of txs the validator picks up from its pool snapshot in one seal pass
/// (`TXPOOL_VAL_MAX_TXS`). The node owns the key but does not pre-cap the pool it serves.
pub const DEFAULT_VAL_MAX_TXS: usize = 512;

/// Node-level txpool lifecycle policy, handed to every txpool this node builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxpoolPolicy {
    /// Non-executable (future-nonce) external txs older than this are evicted.
    pub queued_ttl: Duration,
    /// Executable external txs older than this are evicted; `None` = no cap.
    pub pending_ttl: Option<Duration>,
    /// TTL sweep period, independent of canonical events so a stalled chain still self-heals.
    pub sweep_interval: Duration,
    /// A zero-value native no-op may replace a plain native transfer at the same fixed fee.
    pub native_same_fee_cancel: bool,
    /// Validator's own pick count: at most N txs from the snapshot it fetches enter one seal
    /// pass (oldest first). Node config/parsing only — the node serves the full pool, uncapped.
    pub val_max_txs: usize,
}

impl Default for TxpoolPolicy {
    fn default() -> Self {
        Self {
            queued_ttl: DEFAULT_QUEUED_TTL,
            pending_ttl: None,
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
            native_same_fee_cancel: true,
            val_max_txs: DEFAULT_VAL_MAX_TXS,
        }
    }
}

static POLICY: OnceLock<TxpoolPolicy> = OnceLock::new();

/// Pins the node's txpool policy once, at bootstrap. Later calls are no-ops (`false`).
pub fn set_txpool_policy(policy: TxpoolPolicy) -> bool {
    POLICY.set(policy).is_ok()
}

/// Returns the node's txpool policy (defaults until bootstrap pins it).
pub fn txpool_policy() -> TxpoolPolicy {
    *POLICY.get_or_init(TxpoolPolicy::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_operator_ruling() {
        let p = TxpoolPolicy::default();
        assert_eq!(p.queued_ttl, Duration::from_secs(600));
        assert_eq!(p.pending_ttl, None);
        assert_eq!(p.sweep_interval, Duration::from_secs(60));
        assert_eq!(p.val_max_txs, 512);
        assert!(p.native_same_fee_cancel);
    }
}

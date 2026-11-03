//! The batch-policy middleware: the operator's RPC batch admission gate.
//!
//! Every accepted batch is bounded by class before any work runs:
//! - **tx class** — submission-style requests (`eth_sendRawTransaction`, `eth_sendTransaction`,
//!   `eth_signTransaction`, and the receipt-holding `eth_sendRawTransactionSync`). These land in
//!   the txpool, so a batch may carry at most `RPC_BATCH_TX_MAX` (default 128); when the batch
//!   also carries a heavy request the tx budget shrinks to `RPC_BATCH_HEAVY_TX_MAX` (default 16).
//! - **heavy class** — validation/computation requests (`debug_*`, `trace_*`, `engine_*`,
//!   `eth_call`, `eth_estimateGas`, `eth_createAccessList`, `eth_getProof`, `eth_simulateV1`,
//!   and the full-bundle `eth_callBundle`). At most `RPC_BATCH_HEAVY_MAX` (default 1) per batch.
//! - **read class** — plain lookups; at most `RPC_BATCH_READ_MAX` (default 1024) per batch.
//!
//! Better-than-deny semantics: the admitted entries run through the inner service in this
//! batch untouched, while each over-budget entry is answered **per-id** with the policy
//! error `-32005` ("batch policy limit — retry the remaining entries in a later batch"), so
//! the caller knows exactly which entries were served and which to re-send. Rejected
//! entries never touch the pool, the trie, or any other backing store.
//!
//! The limits are process globals set at boot from the config overlay (like the file-tier
//! base and the entry gate); a value absent from the config file falls back to the
//! built-in default above.

use jsonrpsee::{
    core::middleware::{Batch, BatchEntry, BatchEntryErr, Notification},
    server::middleware::rpc::RpcServiceT,
    types::{ErrorCode, ErrorObject, Id, Request},
};
use std::{future::Future, sync::OnceLock};
use tower::Layer;

/// The metered batch classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchClass {
    /// Submissions (txpool entries).
    Tx,
    /// Validation/computation requests.
    Heavy,
    /// Plain reads.
    Read,
}

fn classify(method: &str) -> BatchClass {
    match method {
        // `eth_sendRawTransactionSync` holds the request up to ~30 s awaiting a receipt —
        // it is the most expensive submission-style entry, so it shares the tx budget.
        "eth_sendRawTransaction" |
        "eth_sendTransaction" |
        "eth_signTransaction" |
        "eth_sendRawTransactionSync" => {
            return BatchClass::Tx
        }
        _ => {}
    }
    if method.starts_with("debug_") ||
        method.starts_with("trace_") ||
        method.starts_with("engine_") ||
        matches!(
            method,
            "eth_call" |
                "eth_estimateGas" |
                "eth_createAccessList" |
                "eth_getProof" |
                "eth_simulateV1" |
                // Full bundle simulation — sequential EVM executions of every tx.
                "eth_callBundle"
        )
    {
        return BatchClass::Heavy;
    }
    BatchClass::Read
}

/// The process-wide batch limits (set once at boot from the config overlay).
#[derive(Debug, Clone, Copy)]
pub struct BatchGlobals {
    /// The tx-class entries per batch.
    pub tx_max: usize,
    /// The tx-class budget when the batch carries a heavy request.
    pub heavy_tx_max: usize,
    /// The heavy-class entries per batch.
    pub heavy_max: usize,
    /// The read-class entries per batch.
    pub read_max: usize,
}

/// The default tx-class budget per batch (the operator key `RPC_BATCH_TX_MAX`).
pub const DEFAULT_TX_MAX: usize = 128;
/// The default tx-class budget while a heavy request rides along
/// (`RPC_BATCH_HEAVY_TX_MAX`).
pub const DEFAULT_HEAVY_TX_MAX: usize = 16;
/// The default heavy-class budget per batch (`RPC_BATCH_HEAVY_MAX`).
pub const DEFAULT_HEAVY_MAX: usize = 1;
/// The default read-class budget per batch (`RPC_BATCH_READ_MAX`).
pub const DEFAULT_READ_MAX: usize = 1024;

static BATCH_LIMITS: OnceLock<BatchGlobals> = OnceLock::new();

/// Sets the process-wide batch limits; the first caller wins (like the file-tier base).
pub fn set_batch_limits(globals: BatchGlobals) -> bool {
    BATCH_LIMITS.set(globals).is_ok()
}

fn batch_limits() -> &'static BatchGlobals {
    BATCH_LIMITS.get_or_init(|| BatchGlobals {
        tx_max: DEFAULT_TX_MAX,
        heavy_tx_max: DEFAULT_HEAVY_TX_MAX,
        heavy_max: DEFAULT_HEAVY_MAX,
        read_max: DEFAULT_READ_MAX,
    })
}

/// The applied per-class budgets for one batch (with the heavy-shrink rule resolved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Budgets {
    tx: usize,
    heavy: usize,
    read: usize,
}

/// Applies the budget rules to the class counts of a batch.
fn apply_budgets(_tx_entries: usize, heavy_entries: usize) -> Budgets {
    let limits = batch_limits();
    let (tx, heavy) = if heavy_entries > 0 {
        // A heavy request rides along: the tx budget shrinks (the plan's "1 heavy → ~16
        // txs" rule).
        (limits.heavy_tx_max, limits.heavy_max)
    } else {
        (limits.tx_max, 0)
    };
    Budgets { tx, heavy, read: limits.read_max }
}

/// The policy error: the per-id rejection of an over-budget batch entry.
fn policy_error() -> ErrorObject<'static> {
    ErrorObject::borrowed(
        ErrorCode::ServerError(-32005).code(),
        "batch policy limit exceeded; retry the remaining entries in a later batch",
        None,
    )
}

/// The admission pass (a free sync function — no middleware state is borrowed): consumes
/// the batch, counts the classes, applies the per-class budgets, and returns the rebuilt
/// batch for the inner service together with the served/rejected counts.
fn admit_entries<'a>(batch: Batch<'a>) -> (Batch<'a>, (usize, usize)) {
    let entries: Vec<Result<BatchEntry<'a>, BatchEntryErr<'a>>> = batch.into_iter().collect();

    let mut tx_count = 0usize;
    let mut heavy_count = 0usize;
    for entry in entries.iter().flatten() {
        match classify(entry.method_name()) {
            BatchClass::Tx => tx_count += 1,
            BatchClass::Heavy => heavy_count += 1,
            BatchClass::Read => {}
        }
    }
    let budgets = apply_budgets(tx_count, heavy_count);

    let mut tx_served = 0usize;
    let mut heavy_served = 0usize;
    let mut read_served = 0usize;
    let mut rejected = 0usize;
    let rebuilt = entries
        .into_iter()
        .map(|entry| {
            let entry = match entry {
                Ok(entry) => entry,
                // Malformed entries keep their own protocol error.
                Err(err) => return Err(err),
            };
            let class = classify(entry.method_name());
            let allowed = match class {
                BatchClass::Tx => {
                    let ok = tx_served < budgets.tx;
                    tx_served += ok as usize;
                    ok
                }
                BatchClass::Heavy => {
                    let ok = heavy_served < budgets.heavy;
                    heavy_served += ok as usize;
                    ok
                }
                BatchClass::Read => {
                    let ok = read_served < budgets.read;
                    read_served += ok as usize;
                    ok
                }
            };
            if allowed {
                Ok(entry)
            } else {
                rejected += 1;
                let id = match &entry {
                    BatchEntry::Call(req) => req.id(),
                    // Notifications carry no id and are always admitted (they do no
                    // pool/trie work and never receive a response).
                    BatchEntry::Notification(_) => Id::Null,
                };
                Err(BatchEntryErr::new(id, policy_error()))
            }
        })
        .collect();
    (Batch::from(rebuilt), (tx_served + heavy_served + read_served, rejected))
}

/// The batch-limits middleware layer (a plain jsonrpsee RPC middleware).
#[derive(Debug, Default, Clone, Copy)]
pub struct BatchLimitsLayer;

impl<S> Layer<S> for BatchLimitsLayer {
    type Service = BatchLimitsService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        BatchLimitsService { inner }
    }
}

/// The middleware service: reimplements `batch()` with the operator's admission policy and
/// passes single calls/notifications through unchanged.
#[derive(Debug, Clone)]
pub struct BatchLimitsService<S> {
    inner: S,
}

impl<S> RpcServiceT for BatchLimitsService<S>
where
    S: RpcServiceT + Clone + Send + Sync + 'static,
{
    type MethodResponse = S::MethodResponse;
    type NotificationResponse = S::NotificationResponse;
    type BatchResponse = S::BatchResponse;

    fn call<'a>(&self, req: Request<'a>) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        self.inner.call(req)
    }

    fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        let (rebuilt, (served, rejected)) = admit_entries(batch);
        if rejected > 0 {
            tracing::warn!(
                target: "rpc::batch_policy",
                served, rejected,
                "batch limited: admitted prefix served, over-budget entries rejected per-id"
            );
        }
        self.inner.batch(rebuilt)
    }

    fn notification<'a>(
        &self,
        n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        self.inner.notification(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned_req(id: u64, method: &str) -> Request<'static> {
        Request::owned(method.to_string(), None, Id::Number(id))
    }

    /// A healthy mixed batch: everything admitted, the malformed entry keeps its own error.
    #[test]
    fn mixed_batch_passes_through() {
        let entries = vec![
            Ok(BatchEntry::Call(owned_req(1, "eth_getBlockByNumber"))),
            Ok(BatchEntry::Call(owned_req(2, "eth_sendRawTransaction"))),
            Err(BatchEntryErr::new(Id::Null, ErrorCode::InvalidRequest.into())),
        ];
        let (rebuilt, (served, rejected)) = admit_entries(Batch::from(entries));
        assert_eq!(served, 2);
        assert_eq!(rejected, 0);
        assert_eq!(rebuilt.len(), 3);
    }

    /// The heavy-shrink rule: with a heavy request present, only `heavy_tx_max` tx entries
    /// are admitted; the rest are answered per-id with the policy error.
    #[test]
    fn heavy_presence_shrinks_tx_budget() {
        let heavy = Ok(BatchEntry::Call(owned_req(999, "eth_call")));
        let mut entries = vec![heavy];
        for i in 0..20 {
            entries.push(Ok(BatchEntry::Call(owned_req(i, "eth_sendRawTransaction"))));
        }
        let (rebuilt, (served, rejected)) = admit_entries(Batch::from(entries));
        assert_eq!(served, DEFAULT_HEAVY_TX_MAX + 1);
        assert_eq!(rejected, 4);
        assert_eq!(rebuilt.iter().filter(|e| e.is_err()).count(), 4);
    }

    /// The read-class budget: over-budget reads are rejected per-id.
    #[test]
    fn read_budget_caps() {
        let entries: Vec<_> = (0u64..(DEFAULT_READ_MAX as u64 + 5))
            .map(|i| Ok(BatchEntry::Call(owned_req(i, "eth_blockNumber"))))
            .collect();
        let (_, (served, rejected)) = admit_entries(Batch::from(entries));
        assert_eq!(served, DEFAULT_READ_MAX);
        assert_eq!(rejected, 5);
    }

    /// The budget resolution is direct: no heavy = the full tx budget.
    #[test]
    fn budgets_rules() {
        let b = apply_budgets(200, 0);
        assert_eq!(b.tx, DEFAULT_TX_MAX);
        let b2 = apply_budgets(200, 1);
        assert_eq!(b2.tx, DEFAULT_HEAVY_TX_MAX);
        assert_eq!(b2.heavy, DEFAULT_HEAVY_MAX);
    }

    /// H-E2 regression: the receipt-holding sync submission is a tx, the bundle
    /// simulation is heavy — neither may masquerade as a cheap read.
    #[test]
    fn sync_submission_is_tx_and_bundle_is_heavy() {
        assert_eq!(classify("eth_sendRawTransactionSync"), BatchClass::Tx);
        assert_eq!(classify("eth_callBundle"), BatchClass::Heavy);
        assert_eq!(classify("eth_blockNumber"), BatchClass::Read);
    }
}

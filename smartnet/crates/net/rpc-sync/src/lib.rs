//! RPC-based block sync with a DNS-gated fixed peer list.
//!
//! Implements the `SmartNet` sync design (`doc/PROJECT.md` — SYNC DESIGN): new nodes join by
//! fetching blocks over JSON-RPC from a fixed, DNS-gated peer list — never via devp2p and
//! never through `smartnet_newPayload` (seal-submission-only API).
//!
//! Layers:
//! - [`peers`]: fixed peer list + DNS gate (allow-list, NOT data integrity — block seals
//!   authenticate content).
//! - [`fetch`]: [`BlockSource`](fetch::BlockSource) abstraction over `eth_blockNumber` /
//!   `eth_getBlockByNumber` with batched range fetches.
//! - [`ancestor`]: binary-search the common ancestor (O(log n) round trips — not from-0 re-walks of
//!   the already-trusted prefix).
//! - [`verify`]: parent-hash chain verification of every fetched batch.
//! - [`driver`]: orchestration loop — height check, ancestor match, batched fetch → verify → import
//!   through a [`BlockSink`](driver::BlockSink) (the node's native import pipeline).

pub mod ancestor;
pub mod driver;
pub mod fetch;
pub mod peers;
pub mod verify;

/// Default number of blocks fetched per batch round trip group.
pub const DEFAULT_FETCH_BATCH: u64 = 4;

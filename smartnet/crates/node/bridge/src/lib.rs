//! THE bridge for our private EVM network (see `BRIDGE-DOC.md`) — built from scratch
//! on base node functionality, never touching the EVM/execution layer. A bridge node
//! is **bridge only** — never also a validator or any other node purpose.
//!
//! What lives here, growing step-by-step along the IN/OUT procedures (`TOD.md`):
//! configuration (`bridges.json`, shared schema in `smartnet-primitives-traits`),
//! the record store ([`record`]), the scan + accept pipeline, the flows, the payouts
//! and the key handling.

/// Pure integer money math (§3.5/§10): fixed WEM fees, decimal scaling, net payout
/// and the `minAmount` gate. No I/O, no EVM, no prices.
pub mod money;
/// THE record store (§4): the per-network JSONL buffers, the RAM working set, stages
/// `0..4`, and the 48 h hash-reply memory.
pub mod record;

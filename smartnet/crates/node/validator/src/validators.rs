//! The validator registry and chair — `user-in.md` #3: **exactly three functions.**
//!
//! 1. [`load_validators`] — bootstrap: read `validators.jsonl` into the RAM array once;
//! 2. [`get_val_all`] — that RAM array (validators + statuses), kept for the whole run;
//! 3. [`get_val_chair`] — the chair for `(block_hash, ts, purpose)`, per `doc/quorum-doc.md`.
//!
//! The canonical question is `get_val_chair(B256::ZERO, 0, "")`. With exactly **one** enabled
//! validator the detour returns that validator outright — no entropy, no rotation, not
//! even a chain read — which is what makes a single-validator bootstrap trivial. The
//! genesis `extraData` signer list is legacy: it is never consulted (user-in #3, "skip
//! genesis validator entirely"); `validators.jsonl` is the only authority.

use alloy_primitives::Address;

/// Function 1 of the three — reads `validators.jsonl` **and fills the RAM array** on the
/// first successful load. Any path, one record per line:
/// `{"address":"0x…","status":true|false|null}`.
pub use calibri_lite_consensus::registry::load_validators;

/// Function 2 of the three — the RAM array of validators and statuses.
pub use calibri_lite_consensus::registry::get_val_all;

/// The registry file path (`$HOME/.smartnet/validators.jsonl`) — bootstrap reads it
/// once; nothing else opens it.
pub use calibri_lite_consensus::registry::default_validators_file;

/// The enabled (`status: true`) addresses of the RAM array — **the** enabled set.
///
/// This is the exact list [`get_val_chair`] builds its candidate pool from, extracted
/// so the import/sync validator gate asks the same question from the same place: there
/// is no second `true`/`false` match anywhere, and a `status: false` address can reach
/// neither the chair nor an accepted block.
pub fn enabled_set() -> Vec<Address> {
    get_val_all().iter().filter(|record| record.is_enabled()).map(|record| record.address).collect()
}

/// Re-exported so callers never reach past this module for a validator question.
pub use calibri_lite_consensus::registry::ValidatorRecord;

/// Function 3 of three — the chair for `(block_hash, ts, purpose)` (`doc/quorum-doc.md`).
///
/// **Pure math: no async, no I/O per call** (operator, 2026-11-19 — "it is just math").
/// Implemented in `calibri-lite` next to SELECT and the registry: it loads
/// `validators.jsonl` **with statuses** once per process, takes only `status: true`
/// records into the calculation, has the 1-validator math detour, and otherwise decides
/// with keccak entropy over the real `(block_hash, ts)`. A `status: false` record can
/// therefore never come back as chair — fail closed: no enabled validator ⇒ `None`,
/// which the caller must treat as "cannot act", never as "everyone may act".
pub use calibri_lite_consensus::get_val_chair;

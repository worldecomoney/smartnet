//! Stage 6 file tier: plain-JSONL active shards, gzip sealed shards (AMEND1).
//!
//! Block payload lives in files under `blocks/{S}/{Y:02}/` (beside the engine dir, at the chain
//! datadir level). Paths are derived from the block height by pure math (no per-height index),
//! per `fjall4a.md`. Each sub-shard holds four files — `b` (block headers), `txs` (transactions,
//! with the receipt scalar folded in), `logs`, and `4626` (ERC-4626 events). The **active**
//! sub-shard is written as plain JSONL (`{name}.jsonl`, one record per line, uncompressed — hot
//! reads never decompress); at the boundary the seal compresses each `.jsonl` into `.gz` and
//! deletes the source. Sealed shards are gzip and read-only. Empty blocks are first-class — `b`
//! carries the header with `tc: 0`, and the other files hold no line until their first record.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use alloy_consensus::BlockBody;
use alloy_eips::eip4895::Withdrawals;

/// Error types for file-tier operations.
pub mod error;
pub use error::{FileTierError, FileTierResult};

/// Shard math, path derivation, and the per-sub-shard file names.
pub mod shard;
pub use shard::{
    get_shard, get_shard_range, is_in_current_shard, is_in_last_two_shards, is_in_recent_shards,
    is_last_block_of_shard, shard_dir, ShardId,
};

/// The record types (`b`, `txs`, `logs`, `4626`), transcribed from the spec's field tables
/// plus the operator-authorized 6B extensions (`logs` is a new file).
pub mod records;
pub use records::{
    AccessListRecord, AuthorizationRecord, BlockLogsRecord, BtRecord, Erc4626Event, Erc4626Record,
    LogRecord, TxRecord, TxStubRecord, WithdrawalRecord,
};

/// Sealing a sub-shard: compress the active `.jsonl` files into `.gz` (AMEND1 + A3, run on a
/// background thread by the persistence service).
pub mod seal;
pub use seal::{active_file_path, seal_shard, seal_tmp_path, sealed_file_path, SealedShard};

/// Seal-time helpers; the seal trigger is pure shard math, never a record count.
pub mod verify;
pub use verify::verify_bt_records;

/// Crash recovery (RECOVERY.md + AMEND1): find the active sub-shard, remove its last 2 lines to
/// get `BX`, finish pending seals, and flush the files for a clean shutdown. Exposes the
/// line-based tools [`rm_last_line`] / [`rm_above_block`].
pub mod recovery;
pub use recovery::{flush, reconcile_seals, remove_last_lines, rm_above_block, rm_last_line};

/// The append-side writer: one plain-JSONL appender per active sub-shard file. Passive — never
/// compresses or rotates (the engine's seal step does).
pub mod writer;
pub use writer::{
    append_records_and_seal, write_genesis_block, AppendOutcome, FileTierWriter, FileWriteTimes,
    SubShardFiles,
};

/// Extraction: alloy consensus types → the record types. Generic over the consensus traits so
/// this crate needs only `alloy-consensus` + `alloy-primitives`.
pub mod extract;
pub use extract::{extract_block, extract_empty_block, BlockRecords, SignatureAccess};

/// Read path: locate a block's records (plain `.jsonl` or gzip `.gz`) and rebuild the consensus
/// types from them (spec §6).
pub mod reader;
pub use reader::{
    base, bootstrap_network_settings, bootstrap_network_settings_with_fallback, build_withdrawals,
    get_chain_id, get_validators, network_settings_initialised, read_block, read_block_at,
    read_blocks_range, read_blocks_range_at, set_base, verify_network_settings, BlockBundle,
    ChainIdentityError, ReceiptBuild,
};

/// Runtime bytecode in content-addressed files (6C): raw binary, write-once, keyed by
/// `keccak256(code)` — `bytecodes/{h[0..2]}/{h[2..4]}/{hash}`.
pub mod bytecode;
pub use bytecode::{bytecode_path, read_bytecode, write_bytecode, BYTECODES_DIR};

/// The four files that make up a sub-shard (`logs` joins the spec's `b`/`txs`/`4626`).
pub const FILE_NAMES: [&str; 4] = ["b", "txs", "logs", "4626"];

/// Rebuild a consensus block body from file-tier records (spec §6).
///
/// The provider's aged-block paths hold records, not a stored body: the `Blocks` row of
/// an handed-off block is gone, so the body's `{transactions, ommers, withdrawals}`
/// fields must be assembled from the bundle instead. Ommers are always empty — the node
/// is engine-driven, so uncles never exist — and withdrawals come from `b.wd` (`None`
/// pre-Shanghai).
///
/// The trait is not generic over the transaction type: taking [`TxRecord`]s directly is
/// what lets a provider bound it as a nested associated bound
/// (`NodePrimitives<BlockBody: BuildBody>`) without referencing the sibling `SignedTx`
/// projection, which rustc will not elaborate through a trait's own where-clause.
pub trait BuildBody {
    /// Assemble the body from the block's transaction records and withdrawals.
    fn build_from_records(txs: Vec<TxRecord>, withdrawals: Option<Withdrawals>) -> Self;
}

impl<T, H> BuildBody for BlockBody<T, H>
where
    T: for<'a> From<&'a TxRecord>,
{
    fn build_from_records(txs: Vec<TxRecord>, withdrawals: Option<Withdrawals>) -> Self {
        Self { transactions: txs.iter().map(T::from).collect(), ommers: Vec::new(), withdrawals }
    }
}

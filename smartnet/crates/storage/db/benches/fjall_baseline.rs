#![allow(missing_docs, unreachable_pub)]
//! F0 baseline benchmarks for the Fjall backend (no behavior change — numbers only).
//!
//! These record the *current* engine at library defaults so every F1 tuning knob has a
//! before/after comparison. They deliberately cover the three suspects named in
//! FJALL-REPORT.md:
//!
//! * R7 — `PersistMode::SyncData` on every commit (the commit benches include the fsync);
//! * R6 — the 15-row `StageCheckpoints` rewrite on every commit;
//! * R4 — cursor/iterator re-seek cost (the range-scan bench).
//!
//! Notes on methodology (FJALL.md §4): these are warm-process, warm-OS-cache numbers.
//! True cold-cache runs need a page-cache drop and are an operator-run step, not
//! something a portable bench can do. Commit benches *overwrite* a fixed key range so
//! the database size stays constant across iterations — a stable baseline beats a
//! realistic-but-shifting one when the job is comparing F1 against F0.
//!
//! # F0 baseline (2026-10-10, this machine, fjall 3.1.10 at library defaults)
//!
//! | Benchmark                                   | Median     |
//! |---------------------------------------------|------------|
//! | `point_read/single_get_new_tx`              | ~986 ns    |
//! | `point_read/100_gets_one_tx`                | ~809 ns/op |
//! | `range_scan/walk_1000`                      | ~1.40 µs/r |
//! | `commit/15_checkpoints_plus_watermark`      | ~26 µs     |
//! | `commit/block_100_accounts_plus_checkpoints`| ~256 µs    |
//!
//! Reading: a fresh RO tx (pinned snapshot) costs ~177 ns over a warm long-lived tx
//! (986 − 809). Walking rows costs ~1.7× a point get per row — the cursor re-seek
//! pattern of the adapter (FJALL-REPORT R4). A tiny 16-row commit still pays ~26 µs,
//! which was *hypothesized* to be the per-commit `SyncData` floor (R7); F1 proved that
//! wrong on this hardware — see below. A 100-account block-shaped commit is ~256 µs
//! (~2.5 µs/account including checkpoints + watermark + fsync).
//!
//! # F1 result (2026-10-10, after durability classification + keyspace options)
//!
//! | Benchmark                                   | F0      | F1      | Change |
//! |---------------------------------------------|---------|---------|--------|
//! | `point_read/single_get_new_tx`              | ~986 ns | ~960 ns | −4.8%  |
//! | `point_read/100_gets_one_tx`                | ~809 ns | ~805 ns | ~flat  |
//! | `range_scan/walk_1000`                      | ~1.40µs | ~1.40µs | ~flat  |
//! | `commit/15_checkpoints_plus_watermark`      | ~26 µs  | ~27 µs  | flat*  |
//! | `commit/block_100_accounts_plus_checkpoints`| ~256 µs | ~244 µs | −3.8%* |
//!
//! (*`p > 0.05`, not statistically significant.) The checkpoint commit's fsync *was*
//! removed by the regenerable-table classification, yet the median did not move:
//! the ~26 µs is CPU in the commit path (journal record encoding + memtable inserts,
//! ~1.6–2.4 µs/row), not the fsync. On NVMe-class hardware per-commit `fdatasync` is
//! cheap; the classification's real value is for slow-disk deployments and for not
//! paying fsync on the pipeline's many tiny control commits. The honest ceiling of
//! engine-level tuning without schema change is therefore ~small single-digit percent;
//! the large levers are F2 (fewer rows per commit) and F4 (memtable/compaction, measured).

use alloy_primitives::{Address, U256};
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use smartnet_primitives_traits::Account;
use smartnet_stages_types::{StageCheckpoint, StageId};
use smartnet_db::{
    cursor::DbCursorRO,
    init_db,
    models::PersistenceWatermark,
    transaction::{DbTx, DbTxMut},
    Database, DatabaseArguments, DatabaseEnv, PersistenceProgress, PersistenceProgressKey,
    PlainAccountState, StageCheckpoints,
};
use std::{hint::black_box, path::Path};

/// Accounts seeded for the read benches (working set).
const SEED_ACCOUNTS: u64 = 50_000;
/// Gets per iteration in the batched point-read bench.
const POINT_READS: usize = 100;
/// Rows walked per iteration in the range-scan bench.
const SCAN_ROWS: usize = 1_000;
/// Account updates written per iteration in the block-shaped commit bench.
const BLOCK_ACCOUNT_UPDATES: u64 = 100;
/// Chunks of account puts used while seeding.
const SEED_CHUNK: u64 = 10_000;

/// Big-endian fixed-width address so lexicographic order == numeric order (FJALL.md §3.C).
fn bench_addr(i: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[12..].copy_from_slice(&i.to_be_bytes());
    Address::from(bytes)
}

fn account(i: u64) -> Account {
    Account { nonce: i, balance: U256::from(i) * U256::from(1_000_000_000u64), bytecode_hash: None }
}

fn open_db(dir: &Path) -> DatabaseEnv {
    init_db(dir, DatabaseArguments::test()).expect("bench: open fjall db")
}

/// Seed `n` accounts in chunks — one commit per chunk, like a short sync run.
fn seed_accounts(db: &DatabaseEnv, n: u64) {
    let mut start = 0u64;
    while start < n {
        let end = (start + SEED_CHUNK).min(n);
        let tx = db.tx_mut().expect("bench: seed tx");
        for i in start..end {
            tx.put::<PlainAccountState>(bench_addr(i), account(i)).expect("bench: seed put");
        }
        tx.commit().expect("bench: seed commit");
        start = end;
    }
}

/// One production-shaped persistence commit: account updates + all 15 stage checkpoints +
/// the persistence watermark in a single transaction (`engine/tree` `save_blocks` shape).
fn block_commit(db: &DatabaseEnv, block: u64, first_idx: u64, updates: u64) {
    let tx = db.tx_mut().expect("bench: block tx");
    for j in 0..updates {
        let idx = first_idx + j;
        tx.put::<PlainAccountState>(bench_addr(idx), account(idx)).expect("bench: block put");
    }
    for stage in StageId::ALL {
        tx.put::<StageCheckpoints>(stage.to_string(), StageCheckpoint::new(block))
            .expect("bench: checkpoint put");
    }
    tx.put::<PersistenceProgress>(
        PersistenceProgressKey::Singleton,
        PersistenceWatermark {
            commit_sequence: block,
            last_persisted_block: block,
            last_persisted_tx: block.saturating_mul(100),
        },
    )
    .expect("bench: watermark put");
    tx.commit().expect("bench: block commit");
}

/// Point reads: RPC-shaped (fresh RO tx = fresh pinned snapshot per call) and
/// bulk-shaped (one long-lived tx, many gets) — the two shapes providers actually use.
fn point_reads(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("bench: tempdir");
    let db = open_db(dir.path());
    seed_accounts(&db, SEED_ACCOUNTS);

    let mut group = c.benchmark_group("fjall/point_read");
    group.throughput(Throughput::Elements(1));

    let mut i = 0u64;
    group.bench_function("single_get_new_tx", |b| {
        b.iter(|| {
            let tx = db.tx().expect("bench: ro tx");
            let key = bench_addr(black_box(i % SEED_ACCOUNTS));
            i = i.wrapping_add(1);
            black_box(tx.get::<PlainAccountState>(key).expect("bench: get"))
        })
    });

    group.throughput(Throughput::Elements(POINT_READS as u64));
    let mut i = 0u64;
    group.bench_function("100_gets_one_tx", |b| {
        b.iter(|| {
            let tx = db.tx().expect("bench: ro tx");
            let mut hits = 0usize;
            for _ in 0..POINT_READS {
                let key = bench_addr(black_box(i % SEED_ACCOUNTS));
                i = i.wrapping_add(1);
                if tx.get::<PlainAccountState>(key).expect("bench: get").is_some() {
                    hits += 1;
                }
            }
            black_box(hits)
        })
    });
    group.finish();
}

/// Sequential scan over the account keyspace (R4: every cursor move re-seeks in the
/// Fjall adapter, so walk cost is the number to watch).
fn range_scan(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("bench: tempdir");
    let db = open_db(dir.path());
    seed_accounts(&db, SEED_ACCOUNTS);

    let mut group = c.benchmark_group("fjall/range_scan");
    group.throughput(Throughput::Elements(SCAN_ROWS as u64));
    group.bench_function("walk_1000", |b| {
        b.iter(|| {
            let tx = db.tx().expect("bench: ro tx");
            let mut cursor = tx.cursor_read::<PlainAccountState>().expect("bench: cursor");
            let n = cursor.walk(None).expect("bench: walk").take(SCAN_ROWS).count();
            black_box(n)
        })
    });
    group.finish();
}

/// Commit cost. The checkpoint-only bench is the R6/R7 smoking gun in isolation:
/// 15 tiny rows plus the watermark, paid for by a full `SyncData` fsync.
/// The block-shaped bench is the real production unit of work.
fn commits(c: &mut Criterion) {
    let mut group = c.benchmark_group("fjall/commit");

    {
        let dir = tempfile::tempdir().expect("bench: tempdir");
        let db = open_db(dir.path());
        let mut block = 0u64;
        group.bench_function("15_checkpoints_plus_watermark", |b| {
            b.iter(|| {
                block += 1;
                block_commit(&db, block, 0, 0);
                black_box(block)
            })
        });
    }

    {
        let dir = tempfile::tempdir().expect("bench: tempdir");
        let db = open_db(dir.path());
        // Overwrite a fixed 100-account range every iteration so the keyspace size stays
        // constant; block number still advances so checkpoint/watermark values change.
        let mut block = 0u64;
        group.bench_function("block_100_accounts_plus_checkpoints", |b| {
            b.iter(|| {
                block += 1;
                block_commit(&db, block, SEED_ACCOUNTS, BLOCK_ACCOUNT_UPDATES);
                black_box(block)
            })
        });
    }

    group.finish();
}

criterion_group!(benches, point_reads, range_scan, commits);
criterion_main!(benches);

#![allow(missing_docs, unreachable_pub)]
//! File-tier cost benchmarks on the import critical path (`IMPORT_WALL`).
//!
//! Reproduces the exact per-batch persistence work the engine does for an empty-block
//! chain ([`extract_block`] → `append_bt` × N → `flush_active`), at the driver's
//! 4-block fetch batch and the 19-block chain shape observed in ISSUE-3.md.
//!
//! Two extra benches split the cost:
//! * `extract/empty_block` — the record extraction per empty block;
//! * `line/serialize_only` — `serde_json::to_string` of a production-shape `b` record, isolating
//!   formatting from the FS appends.
//!
//! Methodology: warm-process, warm-OS-cache, page-cache writes only (the writer never
//! fsyncs — DB-commit durability is separate). The tempdir is created once per group
//! and the shard grows slowly across iterations (~few MB), matching a young shard 0.
//! Run with `cargo bench --offline -p smartnet-file-tier`.

use alloy_consensus::Header;
use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{address, b256, Bloom, Bytes, B256, B64, U256};
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use smartnet_file_tier::{extract_block, read_block_at, BlockRecords, FileTierWriter};
use std::hint::black_box;

/// A production-shape post-merge empty-block header: full roots, bloom, mix hash,
/// nonce, base fee, withdrawals root, parent beacon root — every `Option`-shaped 6B
/// extension field populated the way `extract_block` sees on a live Shanghai+ chain.
fn empty_header(b: u64) -> Header {
    Header {
        parent_hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
        ommers_hash: b256!("1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0e142543b9fe5a9"),
        beneficiary: address!("1111111111111111111111111111111111111111"),
        state_root: b256!("2222222222222222222222222222222222222222222222222222222222222222"),
        transactions_root: b256!(
            "3333333333333333333333333333333333333333333333333333333333333333"
        ),
        receipts_root: b256!("4444444444444444444444444444444444444444444444444444444444444444"),
        logs_bloom: Bloom::default(),
        difficulty: U256::ZERO,
        number: b,
        gas_limit: 30_000_000,
        gas_used: 0,
        timestamp: 1_700_000_000 + b,
        extra_data: Bytes::from(vec![0x5a; 32]),
        mix_hash: b256!("6666666666666666666666666666666666666666666666666666666666666666"),
        nonce: B64::ZERO,
        base_fee_per_gas: Some(30),
        withdrawals_root: Some(b256!(
            "7777777777777777777777777777777777777777777777777777777777777777"
        )),
        blob_gas_used: Some(0),
        excess_blob_gas: Some(1_000_000),
        parent_beacon_block_root: Some(b256!(
            "8888888888888888888888888888888888888888888888888888888888888888"
        )),
        requests_hash: Some(b256!(
            "9999999999999999999999999999999999999999999999999999999999999999"
        )),
        ..Default::default()
    }
}

/// A synthetic-but-length-realistic block hash (only the hex length reaches the JSON).
fn synthetic_hash(b: u64) -> B256 {
    let mut raw = [b as u8; 32];
    raw[0] = 0xab;
    B256::from(raw)
}

/// Extracts the file-tier records for one empty Shanghai block, in the exact shape the
/// persistence layer does it (no txs, no receipts, empty withdrawals body).
fn extract_empty(b: u64) -> BlockRecords {
    let header = empty_header(b);
    let hash = synthetic_hash(b);
    let no_txs: &[alloy_consensus::Signed<alloy_consensus::TxEip1559>] = &[];
    let no_receipts: &[alloy_consensus::ReceiptEnvelope<alloy_primitives::Log>] = &[];
    let no_withdrawals: &[Withdrawal] = &[];
    extract_block(&header, hash, no_txs, &[], no_receipts, Some(no_withdrawals))
}

/// A populated block records-set: `tx_count` EIP-1559 txs with ~100-byte calldata each
/// (contract-call shape, receipts in tx order with 21k gas deltas, no logs).
fn extract_populated(b: u64, tx_count: usize) -> BlockRecords {
    use alloy_consensus::{ReceiptEnvelope, ReceiptWithBloom, Signed, TxEip1559};
    use alloy_primitives::Signature;

    let mut header = empty_header(b);
    header.gas_used = 21_000 * tx_count as u64;
    let hash = synthetic_hash(b);

    let signed: Vec<Signed<TxEip1559>> = (0..tx_count)
        .map(|i| {
            let tx = TxEip1559 {
                chain_id: 1,
                nonce: i as u64,
                gas_limit: 21_000,
                max_fee_per_gas: 30,
                max_priority_fee_per_gas: 1,
                input: Bytes::from(vec![0xa9; 100]),
                ..Default::default()
            };
            let signature = Signature::new(U256::from(123_456 + i), U256::from(654_321 + i), false);
            Signed::new_unchecked(tx, signature, synthetic_hash((b % 1000) * 7919 + i as u64))
        })
        .collect();
    let senders: Vec<alloy_primitives::Address> =
        (0..tx_count).map(|i| alloy_primitives::Address::from([(i % 256) as u8; 20])).collect();
    let receipts: Vec<ReceiptEnvelope<alloy_primitives::Log>> = (0..tx_count)
        .map(|i| {
            ReceiptEnvelope::Eip1559(ReceiptWithBloom::new(
                alloy_consensus::Receipt {
                    status: alloy_consensus::Eip658Value::Eip658(true),
                    cumulative_gas_used: 21_000 * (i as u64 + 1),
                    logs: vec![],
                },
                Bloom::default(),
            ))
        })
        .collect();

    extract_block(&header, hash, &signed, &senders, &receipts, None)
}

fn bench_extract(c: &mut Criterion) {
    c.bench_function("extract/empty_block", |b| b.iter(|| black_box(extract_empty(black_box(1)))));
    for tx_count in [50usize, 200, 1000] {
        c.bench_function(&format!("extract/populated_block_{tx_count}"), |b| {
            b.iter(|| black_box(extract_populated(black_box(1), tx_count)))
        });
    }
}

fn bench_line_serialize(c: &mut Criterion) {
    let record = extract_empty(1).bt;
    c.bench_function("line/serialize_only", |b| {
        b.iter(|| black_box(serde_json::to_string(&record)))
    });
}

/// One engine save-batch for an empty-block chain: extract each block, append its `b`
/// record, then one flush per batch — the exact sequence of `append_file_tier_and_seal`
/// (persistence.rs) minus the DB commit and the seal threads.
fn bench_append_batch(c: &mut Criterion, name: &str, batch_size: u64) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut writer = FileTierWriter::new(dir.path());
    let records: Vec<BlockRecords> = (0..batch_size).map(extract_empty).collect();

    let mut group = c.benchmark_group(format!("append/{name}"));
    group.throughput(Throughput::Elements(batch_size));
    group.bench_function(format!("{batch_size}_blocks"), |b| {
        b.iter(|| {
            for rec in &records {
                writer.files_for_block(rec.bt.b).unwrap().append_bt(&rec.bt).unwrap();
            }
            writer.flush_active().unwrap();
        })
    });
    group.finish();
}

fn bench_appends(c: &mut Criterion) {
    bench_append_batch(c, "empty_batch_4", 4);
    bench_append_batch(c, "empty_batch_19", 19);
}

/// Reads back one populated block through the public read path — the exact
/// `eth_getTransaction*` record-fetch hop. Via [`read_block_at`], every read streams the
/// shard from offset 0 with an early stop at the first record past the target
/// (reader.rs `read_matching`), so a read of the *last* block of a full shard is the
/// worst case: the whole shard is walked (plain JSONL) or decompressed (sealed `.gz`).
///
/// All blocks stay inside shard 0 (`SHARD_Z_SIZE = 10_000`); the fixture writes 50-tx
/// blocks so the shard is at its production shape (b + txs records).
fn bench_point_read_shard(c: &mut Criterion, fill_blocks: u64) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut writer = FileTierWriter::new(dir.path());
    let records: Vec<BlockRecords> = (0..fill_blocks).map(|b| extract_populated(b, 50)).collect();
    for rec in &records {
        writer.files_for_block(rec.bt.b).unwrap().append_bt(&rec.bt).unwrap();
        for tx in &rec.txs {
            writer.files_for_block(rec.bt.b).unwrap().append_tx(tx).unwrap();
        }
    }
    writer.flush_active().unwrap();

    let mut group = c.benchmark_group(format!("read/plain_shard_{fill_blocks}"));
    group.bench_function("first_block", |b| {
        b.iter(|| black_box(read_block_at(dir.path(), 0).unwrap().is_some()))
    });
    group.bench_function("last_block", |b| {
        b.iter(|| black_box(read_block_at(dir.path(), fill_blocks - 1).unwrap().is_some()))
    });
    group.finish();

    // Seal so the same two reads go through the gzip path.
    smartnet_file_tier::seal_shard(dir.path(), smartnet_file_tier::ShardId { s: 0, y: 0 }).unwrap();
    let mut group = c.benchmark_group(format!("read/sealed_shard_{fill_blocks}"));
    group.bench_function("first_block", |b| {
        b.iter(|| black_box(read_block_at(dir.path(), 0).unwrap().is_some()))
    });
    group.bench_function("last_block", |b| {
        b.iter(|| black_box(read_block_at(dir.path(), fill_blocks - 1).unwrap().is_some()))
    });
    group.finish();
    drop(dir);
}

/// Point-read cost vs how far the target block is into the shard: first block (scan stops
/// after one record) vs last block (full shard walk/decompress per read).
fn bench_point_reads(c: &mut Criterion) {
    for fill_blocks in [500u64, 2000, 9000] {
        bench_point_read_shard(c, fill_blocks);
    }
}

criterion_group!(benches, bench_extract, bench_line_serialize, bench_appends, bench_point_reads);
criterion_main!(benches);

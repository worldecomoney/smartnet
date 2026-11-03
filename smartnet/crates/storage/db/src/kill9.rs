//! `kill -9` crash-consistency gate (SQLITE.md crash gate).
//!
//! Each test spawns **this same test binary** as a child that opens the database at a
//! path the parent provides, performs its work, and then dies by `SIGKILL` (either
//! self-inflicted via `raise(9)` or killed by the parent). The parent then reopens the
//! database and asserts that:
//!
//! * every commit whose `commit` returned before the kill is durable (I1/I2);
//! * an uncommitted transaction leaves no trace (rollback);
//! * the persistence watermark stays relationally consistent with the `Execution` checkpoint and
//!   the block body indices written in the same commit (I3/I4);
//! * **(F3)** a commit is atomic across the State/History keyspace split: the History rows
//!   (canonical + block) and the State rows (checkpoint + metadata) of a batch appear together or
//!   not at all, at every durability point — including an unwind, which lowers the checkpoint and
//!   deletes History rows in one commit.
//!
//! The child never selects an engine itself: it inherits `SMARTNET_DB_BACKEND` from the
//! parent process, so the gate runs unchanged — the same `DatabaseBackend::resolve`
//! rule the node uses.

use crate::{
    init_db,
    models::PersistenceWatermark,
    open_db_read_only,
    tables::{self, PersistenceProgressKey, EXECUTION_STAGE_KEY},
    DatabaseArguments,
};
use alloy_primitives::{B256, U256};
use smartnet_stages_types::StageCheckpoint;
use smartnet_db_api::{
    cursor::DbCursorRO,
    database::Database,
    models::StoredTransactionLocation,
    transaction::{DbTx, DbTxMut},
};
use std::{
    fs::OpenOptions,
    io::Write,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    time::Duration,
};

/// Mode of the child process (`batch`, `uncommitted`, `sweep`).
const CRASH_MODE: &str = "SMARTNET_DB_CRASH_MODE";
/// Database root path the child opens and the parent reopens.
const CRASH_PATH: &str = "SMARTNET_DB_CRASH_PATH";
/// File the child appends one line to after each commit returns.
const CRASH_MARKERS: &str = "SMARTNET_DB_CRASH_MARKERS";
/// Number of commits for `batch` mode.
const CRASH_BATCHES: &str = "SMARTNET_DB_CRASH_BATCHES";
/// Exact libtest name of the child entry point.
const CHILD_FILTER: &str = "kill9::crash_child";

unsafe extern "C" {
    fn raise(sig: std::os::raw::c_int) -> std::os::raw::c_int;
}

/// Terminates this process with `SIGKILL` without unwinding, closing or flushing
/// anything — the state a `kill -9` leaves behind.
fn sigkill_self() -> ! {
    // SAFETY: `raise` takes no pointers and always succeeds for SIGKILL.
    unsafe { raise(9) };
    // Unreachable: SIGKILL cannot be caught or ignored.
    std::process::exit(127)
}

/// Child entry point: runs when spawned by a parent test, no-ops otherwise.
#[test]
fn crash_child() {
    if std::env::var_os(CRASH_MODE).is_some() {
        child_main()
    }
}

/// Commits one batch that mirrors a persisted block: Execution checkpoint, the
/// `TxLocations` index and an unrelated metadata row, all in the same transaction (so the
/// watermark row that the commit hook writes into the same transaction derives block and
/// tx from them).
fn commit_batch(db: &crate::DatabaseEnv, block: u64) {
    let tx = db.tx_mut().expect("child tx_mut");
    tx.put::<tables::StageCheckpoints>(
        EXECUTION_STAGE_KEY.to_string(),
        StageCheckpoint::new(block),
    )
    .expect("put checkpoint");
    // Fixture hash mirrors the persistence tests: deterministic per block.
    let hash = B256::from(U256::from(block + 1));
    tx.put::<tables::CanonicalChain>(block, hash).expect("put canonical");
    for idx in 0..3 {
        tx.put::<tables::TxLocations>(block * 10 + idx, StoredTransactionLocation::new(block, idx))
            .expect("put tx location");
    }
    tx.put::<tables::Metadata>(format!("batch-{block}"), vec![0xa5; 64]).expect("put metadata");
    tx.commit().expect("child commit");
}

/// Appends one marker line. Called only after `commit` returned, so `markers <= seq`
/// must hold after any kill; at most the one in-flight commit can add `+1`.
fn write_marker(markers: &Path, block: u64) {
    let mut file =
        OpenOptions::new().create(true).append(true).open(markers).expect("open markers file");
    file.write_all(format!("{block}\n").as_bytes()).expect("write marker");
}

/// Counts complete marker lines (a torn trailing line is not counted; its commit may or
/// may not be durable, which the `markers <= seq <= markers + 1` bound covers).
fn count_markers(markers: &Path) -> u64 {
    let Ok(content) = std::fs::read_to_string(markers) else { return 0 };
    content.lines().filter(|line| !line.is_empty()).count() as u64
}

/// The child's work, selected by `SMARTNET_DB_CRASH_MODE`; never returns.
fn child_main() -> ! {
    let mode = std::env::var(CRASH_MODE).expect("child mode");
    let path = PathBuf::from(std::env::var_os(CRASH_PATH).expect("child path"));
    let markers = PathBuf::from(std::env::var_os(CRASH_MARKERS).expect("child markers"));

    let db = match init_db(&path, DatabaseArguments::test()) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("crash child: init_db failed: {err}");
            std::process::exit(3)
        }
    };

    match mode.as_str() {
        // Commit `n` batches, then die immediately — the durable head must be exactly
        // the last committed batch once the parent reopens.
        "batch" => {
            let n: u64 = std::env::var(CRASH_BATCHES)
                .expect("child batches")
                .parse()
                .expect("batches is a number");
            for block in 1..=n {
                commit_batch(&db, block);
                write_marker(&markers, block);
            }
            sigkill_self()
        }
        // One durable commit, then an open transaction that writes and never commits
        // before dying: nothing from it may survive — across both keyspace groups.
        "uncommitted" => {
            commit_batch(&db, 1);
            write_marker(&markers, 1);
            let tx = db.tx_mut().expect("child uncommitted tx");
            // State side of the doomed transaction.
            tx.put::<tables::Metadata>("uncommitted".to_string(), b"must-not-survive".to_vec())
                .expect("put uncommitted row");
            // History side of the same doomed transaction: rollback must span the split,
            // so a History write leaves no more trace than a State one (F3).
            tx.put::<tables::TxLocations>(999, StoredTransactionLocation::new(1, 0))
                .expect("put uncommitted tx location");
            // Hold the transaction (and its writes) alive; SIGKILL means it is never
            // committed and never rolled back cleanly — the crash case itself.
            sigkill_self()
        }
        // Commit forever; the parent kills this process at a random instant.
        "sweep" => {
            let mut block = 1u64;
            loop {
                commit_batch(&db, block);
                write_marker(&markers, block);
                block = block.saturating_add(1);
            }
        }
        // Persist two blocks, then unwind above block 1 in one transaction — lowering
        // the Execution checkpoint (State) and deleting the canonical/block rows
        // (History) together — and die. The checkpoint and the History rows must move
        // as one unit or not at all (F3).
        "unwind" => {
            commit_batch(&db, 1);
            write_marker(&markers, 1);
            commit_batch(&db, 2);
            write_marker(&markers, 2);
            let tx = db.tx_mut().expect("child unwind tx");
            tx.put::<tables::StageCheckpoints>(
                EXECUTION_STAGE_KEY.to_string(),
                StageCheckpoint::new(1),
            )
            .expect("put lowered checkpoint");
            tx.delete::<tables::CanonicalChain>(2, None).expect("delete canonical 2");
            for tx_num in 20..23 {
                tx.delete::<tables::TxLocations>(tx_num, None).expect("delete tx location");
            }
            tx.commit().expect("child unwind commit");
            // The unwind commit returned, so it is durable; die without a clean close.
            sigkill_self()
        }
        other => {
            eprintln!("crash child: unknown mode {other:?}");
            std::process::exit(3)
        }
    }
}

/// Spawns this test binary as the crash child for one mode.
fn spawn_child(mode: &str, db_path: &Path, markers: &Path, batches: Option<u64>) -> Child {
    let exe = std::env::current_exe().expect("test binary path");
    let mut cmd = Command::new(exe);
    cmd.args([CHILD_FILTER, "--exact", "--nocapture"])
        .env(CRASH_MODE, mode)
        .env(CRASH_PATH, db_path)
        .env(CRASH_MARKERS, markers);
    if let Some(n) = batches {
        cmd.env(CRASH_BATCHES, n.to_string());
    }
    // `SMARTNET_DB_BACKEND` (if the tier set it) is inherited — the child opens exactly
    // the engine the parent will reopen.
    cmd.spawn().expect("spawn crash child")
}

/// Waits for a child that dies by `SIGKILL` and asserts that is how it died.
fn wait_for_sigkill(mut child: Child) -> ExitStatus {
    let status = child.wait().expect("wait for crash child");
    assert_eq!(status.signal(), Some(9), "crash child must die by SIGKILL, got {status:?}");
    status
}

/// Reopens the killed child's database read-only — the crash-recovery entry point.
fn reopen(path: &Path) -> crate::DatabaseEnv {
    open_db_read_only(path, DatabaseArguments::test()).expect("reopen after SIGKILL")
}

fn read_progress(db: &crate::DatabaseEnv) -> Option<PersistenceWatermark> {
    let tx = db.tx().expect("progress tx");
    tx.get::<tables::PersistenceProgress>(PersistenceProgressKey::Singleton).expect("progress get")
}

/// I3/I4: the watermark must equal the `Execution` checkpoint and the
/// indices-derived exclusive next transaction number of that same commit.
fn assert_watermark_relations(db: &crate::DatabaseEnv) {
    let progress = match read_progress(db) {
        Some(progress) => progress,
        None => return,
    };
    assert_eq!(progress.commit_sequence, db.last_txnid().unwrap_or(0));
    let tx = db.tx().expect("relation tx");
    let checkpoint = tx
        .get::<tables::StageCheckpoints>(EXECUTION_STAGE_KEY.to_string())
        .expect("checkpoint get")
        .expect("committed batch always writes a checkpoint");
    assert_eq!(
        progress.last_persisted_block, checkpoint.block_number,
        "watermark block must equal the Execution checkpoint"
    );
    tx.get::<tables::CanonicalChain>(progress.last_persisted_block)
        .expect("canonical get")
        .expect("committed batch always writes a canonical entry");
    let last_tx = tx
        .cursor_read::<tables::TxLocations>()
        .expect("tx locations cursor")
        .last()
        .expect("tx locations last")
        .map(|(tx_num, _)| tx_num);
    assert_eq!(
        progress.last_persisted_tx,
        last_tx.map_or(0, |tx_num| tx_num + 1),
        "watermark tx must equal the TxLocations-derived next transaction number"
    );
}

/// F3 cross-group atomicity: for every batch up to `durable`, the History rows
/// (canonical entry + block row) and the State rows (checkpoint + metadata marker)
/// written by that batch's single commit are all present or all absent. A `kill -9`
/// must never tear a commit down the State/History split.
fn assert_cross_group_atomicity(db: &crate::DatabaseEnv, durable: u64) {
    let tx = db.tx().expect("atomicity tx");
    for block in 1..=durable {
        let hash = B256::from(U256::from(block + 1));
        // History side of the commit.
        let canonical = tx
            .get::<tables::CanonicalChain>(block)
            .expect("canonical get")
            .unwrap_or_else(|| panic!("batch {block}: canonical row missing (History torn)"));
        assert_eq!(canonical, hash, "batch {block}: canonical hash mismatch");
        for idx in 0..3 {
            tx.get::<tables::TxLocations>(block * 10 + idx)
                .expect("tx location get")
                .unwrap_or_else(|| {
                    panic!("batch {block}: tx location {idx} missing (History torn)")
                });
        }
        // State side of the same commit.
        tx.get::<tables::Metadata>(format!("batch-{block}"))
            .expect("metadata get")
            .unwrap_or_else(|| panic!("batch {block}: metadata row missing (State torn)"));
    }
}

/// Is `path` a database root with content (vs. missing or a freshly created empty
/// directory from a kill during setup)?
fn db_root_has_content(path: &Path) -> bool {
    path.is_dir() &&
        std::fs::read_dir(path).map(|mut entries| entries.next().is_some()).unwrap_or(false)
}

/// kill -9 immediately after `n` committed batches: every commit that returned is
/// durable, the watermark is exactly `n`, and reopening succeeds.
#[test]
fn kill9_after_committed_batches_keeps_every_commit() {
    const BATCHES: u64 = 5;
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("db");
    let markers = tmp.path().join("markers");

    let child = spawn_child("batch", &db_path, &markers, Some(BATCHES));
    wait_for_sigkill(child);

    assert_eq!(count_markers(&markers), BATCHES, "all child commits must have returned");
    let db = reopen(&db_path);
    assert_eq!(db.last_txnid(), Some(BATCHES), "every returned commit must be durable");
    let progress = read_progress(&db).expect("watermark row");
    assert_eq!(
        (progress.commit_sequence, progress.last_persisted_block, progress.last_persisted_tx),
        (BATCHES, BATCHES, BATCHES * 10 + 3)
    );
    assert_watermark_relations(&db);
    // Every durable batch is complete across the State/History split, not just the last.
    assert_cross_group_atomicity(&db, BATCHES);
}

/// kill -9 while a transaction is open and written but not committed: the setup commit
/// survives, the uncommitted row leaves no trace.
#[test]
fn kill9_with_uncommitted_transaction_leaves_no_trace() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("db");
    let markers = tmp.path().join("markers");

    let child = spawn_child("uncommitted", &db_path, &markers, None);
    wait_for_sigkill(child);

    assert_eq!(count_markers(&markers), 1, "only the setup commit returned");
    let db = reopen(&db_path);
    assert_eq!(db.last_txnid(), Some(1), "only the committed setup batch counts");
    let tx = db.tx().expect("verify tx");
    assert!(
        tx.get::<tables::Metadata>("uncommitted".to_string()).expect("get").is_none(),
        "an uncommitted State write must leave no trace after SIGKILL"
    );
    assert!(
        tx.get::<tables::TxLocations>(999).expect("get").is_none(),
        "an uncommitted History write must leave no trace after SIGKILL (rollback spans the split)"
    );
    let progress = read_progress(&db).expect("watermark row");
    assert_eq!(
        (progress.commit_sequence, progress.last_persisted_block, progress.last_persisted_tx),
        (1, 1, 13)
    );
    assert_watermark_relations(&db);
    assert_cross_group_atomicity(&db, 1);
}

/// kill -9 at a set of instants during a commit loop: whatever instant the process dies
/// at, reopening succeeds and the relational invariants hold — with the number of
/// durable commits bounded by the markers the child wrote after each `commit` returned.
#[test]
fn kill9_at_any_instant_preserves_relational_invariants() {
    for delay_ms in [0u64, 1, 3, 7, 13, 23] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("db");
        let markers = tmp.path().join("markers");

        let mut child = spawn_child("sweep", &db_path, &markers, None);
        std::thread::sleep(Duration::from_millis(delay_ms));
        child.kill().expect("SIGKILL sweep child");
        wait_for_sigkill(child);

        let committed = count_markers(&markers);
        if !db_root_has_content(&db_path) {
            // Died before or during setup: no commit could have returned.
            assert_eq!(committed, 0, "delay {delay_ms}ms: markers without a database");
            continue;
        }
        let db = match open_db_read_only(&db_path, DatabaseArguments::test()) {
            Ok(db) => db,
            Err(err) => {
                // A re-open failure is only acceptable when nothing was ever committed;
                // once work is durable, the database must come back.
                assert_eq!(committed, 0, "delay {delay_ms}ms: reopen failed: {err}");
                continue;
            }
        };
        let seq = db.last_txnid().unwrap_or(0);
        assert!(
            (committed..=committed + 1).contains(&seq),
            "delay {delay_ms}ms: durable commits {seq} outside markers {committed}..={}",
            committed + 1
        );
        if seq == 0 {
            continue;
        }
        assert_watermark_relations(&db);
        // Cross-group atomicity at this durability point: every batch up to the
        // durable head is complete across State and History.
        assert_cross_group_atomicity(&db, seq);
    }
}

/// F3: an unwind lowers the Execution checkpoint (State) and deletes the canonical +
/// block rows (History) in one commit. After `kill -9` the checkpoint and the History
/// rows must have moved together — block 1 intact, block 2's rows gone — and the
/// watermark rewinds with them.
#[test]
fn kill9_unwind_moves_checkpoint_and_history_together() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("db");
    let markers = tmp.path().join("markers");

    let child = spawn_child("unwind", &db_path, &markers, None);
    wait_for_sigkill(child);

    // Both setup commits returned; the unwind commit returned too (it is durable).
    assert_eq!(count_markers(&markers), 2, "both setup commits returned");
    let db = reopen(&db_path);
    assert_eq!(db.last_txnid(), Some(3), "setup + unwind commits are all durable");

    let tx = db.tx().expect("verify tx");
    // The unwind's State side landed: the checkpoint is back to block 1.
    let checkpoint = tx
        .get::<tables::StageCheckpoints>(EXECUTION_STAGE_KEY.to_string())
        .expect("checkpoint get")
        .expect("checkpoint row");
    assert_eq!(checkpoint.block_number, 1, "checkpoint must be lowered by the unwind");
    // The unwind's History side landed too: block 2's canonical + tx location rows are gone,
    // so the State and History halves of the unwind commit moved as one unit.
    assert!(
        tx.get::<tables::CanonicalChain>(2).expect("canonical get").is_none(),
        "unwind must delete block 2's canonical row (History half of the commit)"
    );
    assert!(
        tx.get::<tables::TxLocations>(20).expect("tx location get").is_none(),
        "unwind must delete block 2's tx locations (History half of the commit)"
    );
    // Block 1 is still complete across both groups.
    assert_cross_group_atomicity(&db, 1);
    drop(tx);

    // The watermark rewinds with the data: block 1, tx 13, sequence 3.
    let progress = read_progress(&db).expect("watermark row");
    assert_eq!(
        (progress.commit_sequence, progress.last_persisted_block, progress.last_persisted_tx),
        (3, 1, 13)
    );
    assert_watermark_relations(&db);
}

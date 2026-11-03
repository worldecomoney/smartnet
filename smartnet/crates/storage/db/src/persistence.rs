//! The persistence watermark, written by every backend's commit.
//!
//! Stage 2 of `SQLITE.md`: each transaction that changed data carries one extra row —
//! [`tables::PersistenceProgress`] with the incremented `commit_sequence` and the
//! block/transaction watermarks derived from the data in the same transaction. Because
//! the row commits atomically with that data, it is always exactly the durable head,
//! on every backend, which is what makes `last_txnid` and crash recovery sound.

use crate::{
    models::PersistenceWatermark,
    tables::{self, PersistenceProgressKey, EXECUTION_STAGE_KEY},
    DatabaseError,
};
use smartnet_db_api::{
    cursor::DbCursorRO,
    transaction::{DbTx, DbTxMut},
};

/// Writes the persistence watermark row into `tx`, to be committed atomically with it.
///
/// Called by each backend's `DbTx::commit` for every transaction that changed data and
/// wrote at least one non-schema-control row (`bumps_watermark`). A transaction that
/// wrote only schema-control rows (schema info, migrations) passes `false` and leaves
/// the watermark absent, so a fresh database's version-init commit is not mistaken for
/// the first data commit.
///
/// * `commit_sequence` — the previous value plus one; this is what `last_txnid` reports. `SQLite`'s
///   connection change counter (and Fjall's buffered-write flag) is consulted by the caller only to
///   decide *whether* the transaction changed anything; it is never the reported id (SQLITE.md
///   §4.6).
/// * `last_persisted_block` — the `Execution` stage checkpoint observed inside `tx` (`save_blocks`,
///   `remove_block_and_execution_above` and stage commits write it in the same transaction, and
///   unwinds lower it there too), falling back to the previous row's value only when the checkpoint
///   is absent entirely (a database that predates checkpoints).
/// * `last_persisted_tx` — the exclusive next transaction number, derived from the last
///   [`tables::TxLocations`] key (the DB numbering authority; payload lives in the file tier), i.e.
///   the "`last(tx_number)` + 1 computed inside the write transaction" rule; it rewinds with
///   unwinds exactly like every other positional `TxNumber`.
pub(crate) fn bump_persistence_progress<Tx>(
    tx: &Tx,
    bumps_watermark: bool,
) -> Result<(), DatabaseError>
where
    Tx: DbTx + DbTxMut + ?Sized,
{
    // A transaction that wrote only schema-control rows must leave the watermark
    // untouched: a fresh database still reports no watermark at all (`last_txnid`
    // stays "unknown") until the first real commit.
    if !bumps_watermark {
        return Ok(());
    }

    let previous = tx
        .get::<tables::PersistenceProgress>(PersistenceProgressKey::Singleton)?
        .unwrap_or_default();

    let last_persisted_block =
        match tx.get::<tables::StageCheckpoints>(EXECUTION_STAGE_KEY.to_string())? {
            Some(checkpoint) => checkpoint.block_number,
            None => previous.last_persisted_block,
        };

    // The payload lives in the file tier; `TxLocations` is the DB index and the numbering
    // authority, so its last key + 1 is the next transaction number at this checkpoint.
    let last_persisted_tx = tx
        .cursor_read::<tables::TxLocations>()?
        .last()?
        .map(|(tx_num, _)| tx_num.saturating_add(1))
        .unwrap_or(previous.last_persisted_tx);

    tx.put::<tables::PersistenceProgress>(
        PersistenceProgressKey::Singleton,
        PersistenceWatermark {
            commit_sequence: previous.commit_sequence.saturating_add(1),
            last_persisted_block,
            last_persisted_tx,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_utils::{create_test_rw_db, TempDatabase},
        DatabaseEnv,
    };
    use alloy_primitives::{B256, U256};
    use smartnet_stages_types::StageCheckpoint;
    use smartnet_db_api::{
        database::Database, models::StoredTransactionLocation, transaction::DbTxMut,
    };

    fn read_progress(db: &impl Database) -> Option<PersistenceWatermark> {
        let tx = db.tx().unwrap();
        tx.get::<tables::PersistenceProgress>(PersistenceProgressKey::Singleton).unwrap()
    }

    /// Deterministic fixture hash for a block row.
    fn fixture_hash(block: u64) -> B256 {
        B256::from(U256::from(block + 1))
    }

    /// Commits a batch that sets the Execution checkpoint to `block` and the
    /// `TxLocations` index rows, like a persisted block would (payload is file-tier only).
    fn persist_block(db: &impl Database, block: u64, first_tx_num: u64, tx_count: u64) {
        let tx = db.tx_mut().unwrap();
        tx.put::<tables::StageCheckpoints>(
            EXECUTION_STAGE_KEY.to_string(),
            StageCheckpoint::new(block),
        )
        .unwrap();
        tx.put::<tables::CanonicalChain>(block, fixture_hash(block)).unwrap();
        for idx in 0..tx_count {
            tx.put::<tables::TxLocations>(
                first_tx_num + idx,
                StoredTransactionLocation::new(block, idx),
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    fn watermark_semantics(db: &TempDatabase<DatabaseEnv>) {
        // A fresh database has no watermark row: `last_txnid` is "unknown" (= 0).
        assert!(read_progress(db).is_none());
        assert_eq!(db.last_txnid(), None);

        // The first data commit bumps to 1, defaulting block/tx while nothing has
        // written a checkpoint yet.
        {
            let tx = db.tx_mut().unwrap();
            tx.put::<tables::Metadata>("k".to_string(), b"v".to_vec()).unwrap();
            tx.commit().unwrap();
        }
        let progress = read_progress(db).unwrap();
        assert_eq!(
            (progress.commit_sequence, progress.last_persisted_block, progress.last_persisted_tx),
            (1, 0, 0)
        );
        assert_eq!(db.last_txnid(), Some(1));

        // An empty commit changes nothing: no bump (this is the guard both backends
        // share with their empty-commit fast paths).
        db.tx_mut().unwrap().commit().unwrap();
        assert_eq!(db.last_txnid(), Some(1));

        // A rolled-back transaction leaves no trace: no bump.
        {
            let tx = db.tx_mut().unwrap();
            tx.put::<tables::Metadata>("rolled".to_string(), b"back".to_vec()).unwrap();
            drop(tx);
        }
        assert_eq!(db.last_txnid(), Some(1));

        // A batch that persists a block derives the watermark from the Execution
        // checkpoint and the `TxLocations` index written in the same transaction:
        // block 7, exclusive next tx = 10 + 3.
        persist_block(db, 7, 10, 3);
        let progress = read_progress(db).unwrap();
        assert_eq!(
            (progress.commit_sequence, progress.last_persisted_block, progress.last_persisted_tx),
            (2, 7, 13)
        );
        assert_eq!(db.last_txnid(), Some(2));

        // An unwind rewinds the watermark exactly like the data: the checkpoint moves
        // back and the block rows above it are gone.
        {
            let tx = db.tx_mut().unwrap();
            tx.put::<tables::StageCheckpoints>(
                EXECUTION_STAGE_KEY.to_string(),
                StageCheckpoint::new(5),
            )
            .unwrap();
            tx.put::<tables::CanonicalChain>(5, fixture_hash(5)).unwrap();
            for idx in 0..2 {
                tx.put::<tables::TxLocations>(6 + idx, StoredTransactionLocation::new(5, idx))
                    .unwrap();
            }
            tx.delete::<tables::CanonicalChain>(7, None).unwrap();
            tx.delete::<tables::TxLocations>(8, None).unwrap();
            tx.delete::<tables::TxLocations>(9, None).unwrap();
            tx.delete::<tables::TxLocations>(10, None).unwrap();
            tx.delete::<tables::TxLocations>(11, None).unwrap();
            tx.delete::<tables::TxLocations>(12, None).unwrap();
            tx.commit().unwrap();
        }
        let progress = read_progress(db).unwrap();
        assert_eq!(
            (progress.commit_sequence, progress.last_persisted_block, progress.last_persisted_tx),
            (3, 5, 8)
        );

        // A commit that changes other data (chain state, metadata) but not the
        // checkpoint bumps only the sequence: block and tx stay put.
        {
            let tx = db.tx_mut().unwrap();
            tx.put::<tables::Metadata>("after-unwind".to_string(), b"v".to_vec()).unwrap();
            tx.commit().unwrap();
        }
        let progress = read_progress(db).unwrap();
        assert_eq!(
            (progress.commit_sequence, progress.last_persisted_block, progress.last_persisted_tx),
            (4, 5, 8)
        );
        assert_eq!(db.last_txnid(), Some(4));
    }

    #[test]
    fn watermark_semantics_fjall() {
        watermark_semantics(&create_test_rw_db());
    }
}

//! Backend-agnostic conformance suite for the [`Database`] abstraction.
//!
//! These functions define the behavioral contract the backend must satisfy. Each
//! function is generic over [`Database`] and receives a fresh, empty database. The
//! suite is written against the **normalized schema** (SQLITE.md §11, §13.5 3B.1–3B.3):
//! it exercises the schema's key shapes — fixed-width hash keys (`HeaderNumbers`), the
//! numeric `TxLocations` index with its single-range-delete unwind invariant, and the
//! `SchemaInfo` singleton control key — alongside the numeric and dupsort shapes the
//! cursor contract was originally pinned on. Instantiate the suite for a backend with
//! [`conformance_tests!`](crate::conformance_tests).

use crate::{models::AccountBeforeTx, tables, DatabaseError};
use alloy_primitives::{Address, BlockHash, B256};
use smartnet_db_api::{
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW},
    database::Database,
    models::{StoredSchemaInfo, StoredTransactionLocation},
    tables::SchemaInfoKey,
    transaction::{DbTx, DbTxMut},
};

type Canonical = tables::CanonicalChain;
type Changes = tables::AccountChangeSets;

/// Seeds `CanonicalChain` with the given `(block, value byte)` pairs.
fn seed(db: &impl Database, entries: &[(u64, u8)]) {
    let tx = db.tx_mut().unwrap();
    for (block, byte) in entries {
        tx.put::<Canonical>(*block, B256::with_last_byte(*byte)).unwrap();
    }
    tx.commit().unwrap();
}

/// A change-set row for `address` with no account state.
const fn cs(address: Address) -> AccountBeforeTx {
    AccountBeforeTx { address, info: None }
}

const fn value(byte: u8) -> B256 {
    B256::with_last_byte(byte)
}

/// First/last/next/prev/current over a plain table, including the position kept
/// when stepping past either end.
pub fn cursor_navigation_plain_table<D: Database>(db: D) {
    seed(&db, &[(10, 1), (20, 2), (30, 3)]);

    let tx = db.tx().unwrap();
    let mut cursor = tx.cursor_read::<Canonical>().unwrap();

    // A fresh cursor is positioned before the first entry.
    assert_eq!(cursor.current().unwrap(), None);

    assert_eq!(cursor.first().unwrap(), Some((10, value(1))));
    assert_eq!(cursor.next().unwrap(), Some((20, value(2))));
    assert_eq!(cursor.next().unwrap(), Some((30, value(3))));
    assert_eq!(cursor.next().unwrap(), None);
    // No successor: the position is kept.
    assert_eq!(cursor.current().unwrap(), Some((30, value(3))));

    assert_eq!(cursor.last().unwrap(), Some((30, value(3))));
    assert_eq!(cursor.prev().unwrap(), Some((20, value(2))));
    assert_eq!(cursor.prev().unwrap(), Some((10, value(1))));
    assert_eq!(cursor.prev().unwrap(), None);
    assert_eq!(cursor.first().unwrap(), Some((10, value(1))));
}

/// Every read on an empty table reports `None`.
pub fn cursor_empty_table_reads_none<D: Database>(db: D) {
    let tx = db.tx().unwrap();
    let mut cursor = tx.cursor_read::<Canonical>().unwrap();

    assert_eq!(cursor.first().unwrap(), None);
    assert_eq!(cursor.last().unwrap(), None);
    assert_eq!(cursor.next().unwrap(), None);
    assert_eq!(cursor.prev().unwrap(), None);
    assert_eq!(cursor.current().unwrap(), None);
    assert_eq!(cursor.seek_exact(1).unwrap(), None);
    assert_eq!(cursor.seek(1).unwrap(), None);

    let rows = cursor.walk(None).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert!(rows.is_empty());
}

/// `seek` lands on the first entry at or after the key; a missed `seek_exact`
/// clears the position.
pub fn cursor_seek_positions_at_or_after_key<D: Database>(db: D) {
    seed(&db, &[(10, 1), (20, 2), (30, 3)]);

    let tx = db.tx().unwrap();
    let mut cursor = tx.cursor_read::<Canonical>().unwrap();

    assert_eq!(cursor.seek_exact(20).unwrap(), Some((20, value(2))));
    assert_eq!(cursor.seek(15).unwrap(), Some((20, value(2))));
    assert_eq!(cursor.seek(10).unwrap(), Some((10, value(1))));
    assert_eq!(cursor.seek(31).unwrap(), None);

    // A missed exact seek clears the position.
    assert_eq!(cursor.seek_exact(15).unwrap(), None);
    assert_eq!(cursor.current().unwrap(), None);

    // A seek past the last key misses, and the cleared position must not restart the
    // cursor at the first entry: `next` yields nothing.
    assert_eq!(cursor.seek(31).unwrap(), None);
    assert_eq!(cursor.next().unwrap(), None);
}
/// `insert` rejects an existing key, `upsert` overwrites it, `append` adds.
pub fn cursor_insert_rejects_duplicates_upsert_overwrites<D: Database>(db: D) {
    seed(&db, &[(20, 2)]);

    let tx = db.tx_mut().unwrap();
    let mut cursor = tx.cursor_write::<Canonical>().unwrap();

    cursor.insert(10, &value(1)).unwrap();
    assert!(cursor.insert(10, &value(9)).is_err());
    // The failed insert left the position on the existing entry.
    assert_eq!(cursor.current().unwrap(), Some((10, value(1))));

    cursor.upsert(20, &value(9)).unwrap();
    assert_eq!(tx.get::<Canonical>(20).unwrap(), Some(value(9)));

    cursor.upsert(30, &value(3)).unwrap();
    cursor.append(40, &value(4)).unwrap();

    tx.commit().unwrap();
    let tx = db.tx().unwrap();
    assert_eq!(tx.get::<Canonical>(10).unwrap(), Some(value(1)));
    assert_eq!(tx.get::<Canonical>(20).unwrap(), Some(value(9)));
    assert_eq!(tx.get::<Canonical>(30).unwrap(), Some(value(3)));
    assert_eq!(tx.get::<Canonical>(40).unwrap(), Some(value(4)));
}

/// Dropping a transaction without committing restores every entry the cursor
/// upserted, deleted or appended, plus the transaction's own writes.
pub fn cursor_writes_roll_back_when_transaction_is_dropped<D: Database>(db: D) {
    seed(&db, &[(20, 2), (30, 3)]);

    {
        let tx = db.tx_mut().unwrap();
        let mut cursor = tx.cursor_write::<Canonical>().unwrap();

        cursor.upsert(10, &value(1)).unwrap(); // new entry
        cursor.upsert(20, &value(9)).unwrap(); // overwrite
        cursor.seek_exact(30).unwrap();
        cursor.delete_current().unwrap();
        tx.put::<Canonical>(40, value(4)).unwrap();
        // dropped without commit
    }

    let tx = db.tx().unwrap();
    assert_eq!(tx.get::<Canonical>(10).unwrap(), None);
    assert_eq!(tx.get::<Canonical>(20).unwrap(), Some(value(2)));
    assert_eq!(tx.get::<Canonical>(30).unwrap(), Some(value(3)));
    assert_eq!(tx.get::<Canonical>(40).unwrap(), None);
}

/// Cursor writes and transaction writes interleave within one transaction, are
/// visible to each other immediately (read-your-writes), and persist on commit.
pub fn cursor_and_transaction_writes_interleave_in_one_transaction<D: Database>(db: D) {
    seed(&db, &[(20, 2)]);

    let tx = db.tx_mut().unwrap();
    let mut cursor = tx.cursor_write::<Canonical>().unwrap();

    cursor.upsert(10, &value(1)).unwrap();
    tx.put::<Canonical>(30, value(3)).unwrap();
    cursor.upsert(40, &value(4)).unwrap();
    tx.delete::<Canonical>(20, None).unwrap();
    cursor.seek_exact(40).unwrap();
    cursor.delete_current().unwrap();

    assert_eq!(tx.get::<Canonical>(10).unwrap(), Some(value(1)));
    assert_eq!(tx.get::<Canonical>(20).unwrap(), None);
    assert_eq!(tx.get::<Canonical>(30).unwrap(), Some(value(3)));
    assert_eq!(tx.get::<Canonical>(40).unwrap(), None);

    // The cursor reads the transaction's own writes back too, deletions included.
    assert_eq!(cursor.seek_exact(20).unwrap(), None);
    assert_eq!(cursor.first().unwrap(), Some((10, value(1))));

    tx.commit().unwrap();
    let tx = db.tx().unwrap();
    assert_eq!(tx.get::<Canonical>(10).unwrap(), Some(value(1)));
    assert_eq!(tx.get::<Canonical>(20).unwrap(), None);
    assert_eq!(tx.get::<Canonical>(30).unwrap(), Some(value(3)));
    assert_eq!(tx.get::<Canonical>(40).unwrap(), None);
}

/// A cursor navigates the transaction's buffered writes over the stored entries: new
/// entries, overwrites and deletions all show up in order without having been written.
pub fn cursor_navigates_buffered_writes_over_stored_entries<D: Database>(db: D) {
    seed(&db, &[(20, 2), (40, 4), (60, 6)]);

    let tx = db.tx_mut().unwrap();
    let mut cursor = tx.cursor_write::<Canonical>().unwrap();

    cursor.upsert(10, &value(1)).unwrap(); // before every stored entry
    cursor.upsert(30, &value(3)).unwrap(); // between stored entries
    cursor.upsert(70, &value(7)).unwrap(); // after every stored entry
    cursor.seek_exact(40).unwrap();
    cursor.delete_current().unwrap(); // removes a stored entry
    cursor.seek_exact(60).unwrap();
    cursor.delete_current().unwrap();
    cursor.upsert(60, &value(66)).unwrap(); // replaces the deletion

    assert_eq!(cursor.first().unwrap(), Some((10, value(1))));
    assert_eq!(cursor.next().unwrap(), Some((20, value(2))));
    assert_eq!(cursor.next().unwrap(), Some((30, value(3))));
    assert_eq!(cursor.next().unwrap(), Some((60, value(66))));
    assert_eq!(cursor.next().unwrap(), Some((70, value(7))));
    assert_eq!(cursor.next().unwrap(), None);

    assert_eq!(cursor.last().unwrap(), Some((70, value(7))));
    assert_eq!(cursor.prev().unwrap(), Some((60, value(66))));
    assert_eq!(cursor.prev().unwrap(), Some((30, value(3))));
    assert_eq!(cursor.prev().unwrap(), Some((20, value(2))));
    assert_eq!(cursor.prev().unwrap(), Some((10, value(1))));
    assert_eq!(cursor.prev().unwrap(), None);

    assert_eq!(cursor.seek(35).unwrap(), Some((60, value(66))));
    assert_eq!(cursor.seek_exact(40).unwrap(), None);
    assert_eq!(tx.entries::<Canonical>().unwrap(), 5);
}

/// Dup cursor navigation: next/prev within a key's duplicates, `last_dup`,
/// `next_no_dup` across keys, exact seek and subkey seek.
pub fn dup_cursor_navigates_duplicates_of_each_key<D: Database>(db: D) {
    let a1 = Address::repeat_byte(0x11);
    let a2 = Address::repeat_byte(0x22);
    let a3 = Address::repeat_byte(0x33);
    let b1 = Address::repeat_byte(0xaa);

    let tx = db.tx_mut().unwrap();
    tx.put::<Changes>(1, cs(a1)).unwrap();
    tx.put::<Changes>(1, cs(a2)).unwrap();
    tx.put::<Changes>(1, cs(a3)).unwrap();
    tx.put::<Changes>(2, cs(b1)).unwrap();
    tx.commit().unwrap();

    let tx = db.tx().unwrap();
    let mut cursor = tx.cursor_dup_read::<Changes>().unwrap();

    // Duplicates of a key sort by their address bytes.
    assert_eq!(cursor.first().unwrap(), Some((1, cs(a1))));
    assert_eq!(cursor.next_dup().unwrap(), Some((1, cs(a2))));
    assert_eq!(cursor.next_dup().unwrap(), Some((1, cs(a3))));
    // The next physical entry belongs to block 2, so the dup walk stops.
    assert_eq!(cursor.next_dup().unwrap(), None);

    assert_eq!(cursor.prev_dup().unwrap(), Some((1, cs(a2))));
    assert_eq!(cursor.last_dup().unwrap(), Some(cs(a3)));
    assert_eq!(cursor.seek_exact(1).unwrap(), Some((1, cs(a1))));
    assert_eq!(cursor.prev_dup().unwrap(), None);

    assert_eq!(cursor.seek_exact(1).unwrap(), Some((1, cs(a1))));
    assert_eq!(cursor.next_no_dup().unwrap(), Some((2, cs(b1))));
    assert_eq!(cursor.next_no_dup().unwrap(), None);

    assert_eq!(cursor.seek_exact(2).unwrap(), Some((2, cs(b1))));
    assert_eq!(cursor.seek_exact(3).unwrap(), None);
}

/// Subkey seeks land on the first duplicate at or after the subkey, and
/// `walk_dup` walks the duplicates of its starting key.
/// single-entry `delete_current`.
pub fn dup_cursor_writes_insert_append_and_delete<D: Database>(db: D) {
    let a1 = Address::repeat_byte(0x11);
    let a2 = Address::repeat_byte(0x22);
    let a3 = Address::repeat_byte(0x33);
    let a4 = Address::repeat_byte(0x44);
    let b1 = Address::repeat_byte(0xaa);

    {
        let tx = db.tx_mut().unwrap();
        let mut cursor = tx.cursor_dup_write::<Changes>().unwrap();

        tx.put::<Changes>(1, cs(a1)).unwrap();
        tx.put::<Changes>(1, cs(a2)).unwrap();
        tx.put::<Changes>(1, cs(a3)).unwrap();
        tx.put::<Changes>(2, cs(b1)).unwrap();

        // A duplicate value may not be inserted twice, a new one may.
        assert!(cursor.insert(1, &cs(a1)).is_err());
        cursor.insert(1, &cs(a4)).unwrap();
        cursor.append_dup(3, cs(a1)).unwrap();
        assert_eq!(cursor.current().unwrap(), Some((3, cs(a1))));

        // Remove every duplicate of block 1; the cursor parks on the emptied key.
        cursor.seek_exact(1).unwrap();
        cursor.delete_current_duplicates().unwrap();
        assert_eq!(cursor.current().unwrap(), None);
        assert_eq!(cursor.next().unwrap(), Some((2, cs(b1))));

        // Single-entry delete keeps the position for next/prev.
        cursor.delete_current().unwrap();
        assert_eq!(cursor.current().unwrap(), None);
        // Blocks 1 and 2 are gone; the entry appended to block 3 is still there.
        assert_eq!(cursor.next().unwrap(), Some((3, cs(a1))));
        assert_eq!(cursor.next().unwrap(), None);
        assert_eq!(cursor.prev().unwrap(), None);

        tx.commit().unwrap();
    }

    let tx = db.tx().unwrap();
    assert_eq!(tx.get::<Changes>(1).unwrap(), None);
    assert_eq!(tx.get::<Changes>(2).unwrap(), None);
    assert_eq!(tx.get::<Changes>(3).unwrap(), Some(cs(a1)));
}

/// Every write on a read-only cursor fails. The error itself is
/// backend-specific (Fjall returns `Other("...read-only...")`); only the
/// failure is contractual. Backends must therefore expose write-capable
/// cursor types even on read transactions, rejecting writes at runtime.
pub fn read_only_cursor_rejects_writes<D>(db: D)
where
    D: Database,
    <<D as Database>::TX as DbTx>::Cursor<Canonical>: DbCursorRW<Canonical>,
    <<D as Database>::TX as DbTx>::DupCursor<Changes>: DbDupCursorRW<Changes>,
{
    seed(&db, &[(10, 1)]);

    let tx = db.tx().unwrap();
    let mut cursor = tx.cursor_read::<Canonical>().unwrap();

    assert!(cursor.upsert(20, &value(2)).is_err());
    assert!(cursor.insert(20, &value(2)).is_err());
    assert!(cursor.append(20, &value(2)).is_err());
    cursor.seek_exact(10).unwrap();
    assert!(cursor.delete_current().is_err());

    let mut dup_cursor = tx.cursor_dup_read::<Changes>().unwrap();
    assert!(dup_cursor.append_dup(1, cs(Address::repeat_byte(0x11))).is_err());
    dup_cursor.first().unwrap();
    assert!(dup_cursor.delete_current_duplicates().is_err());
}

/// Deleting keys/duplicates that were never written must succeed as a no-op and report
/// `false`, matching MDBX semantics (backends would otherwise error on missing keys).
pub fn delete_missing_entries_are_noops<D: Database>(db: D) {
    let tx = db.tx_mut().unwrap();

    // Plain table: absent key -> no-op; present key -> deleted once, then no-op.
    assert!(!tx.delete::<tables::CanonicalChain>(1, None).unwrap());
    tx.put::<tables::CanonicalChain>(1, B256::ZERO).unwrap();
    assert!(tx.delete::<tables::CanonicalChain>(1, None).unwrap());
    assert!(!tx.delete::<tables::CanonicalChain>(1, None).unwrap());

    // Dupsort table: absent key, absent value, repeated value and final key wipe are
    // all no-op-safe.
    let a1 = Address::repeat_byte(0x11);
    let a2 = Address::repeat_byte(0x22);
    let cs = |address| AccountBeforeTx { address, info: None };

    assert!(!tx.delete::<tables::AccountChangeSets>(1, None).unwrap());

    tx.put::<tables::AccountChangeSets>(1, cs(a1)).unwrap();
    tx.put::<tables::AccountChangeSets>(1, cs(a2)).unwrap();

    assert!(!tx
        .delete::<tables::AccountChangeSets>(1, Some(cs(Address::repeat_byte(0x33))))
        .unwrap());
    assert!(tx.delete::<tables::AccountChangeSets>(1, Some(cs(a1))).unwrap());
    assert!(!tx.delete::<tables::AccountChangeSets>(1, Some(cs(a1))).unwrap());
    assert!(tx.delete::<tables::AccountChangeSets>(1, None).unwrap());
    assert!(!tx.delete::<tables::AccountChangeSets>(1, None).unwrap());
}

/// Cursor contract over a **fixed-width hash-keyed** table: `HeaderNumbers` is keyed
/// by `BlockHash` (32 bytes), whose byte order is the ordering the cursor must expose —
/// not the numeric order the rest of the suite pins.
pub fn cursor_navigation_hash_keyed_table<D: Database>(db: D) {
    // Big-endian keys, so `with_last_byte(n)` orders as `n`.
    let key = |byte: u8| BlockHash::with_last_byte(byte);

    {
        let tx = db.tx_mut().unwrap();
        for byte in [10u8, 20, 30] {
            tx.put::<tables::HeaderNumbers>(key(byte), byte as u64).unwrap();
        }
        tx.commit().unwrap();
    }

    let tx = db.tx().unwrap();
    let mut cursor = tx.cursor_read::<tables::HeaderNumbers>().unwrap();

    assert_eq!(cursor.first().unwrap().map(|(k, _)| k), Some(key(10)));
    assert_eq!(cursor.last().unwrap().map(|(k, _)| k), Some(key(30)));

    // Forward walk visits every hash key in byte order.
    let forward = cursor
        .walk(None)
        .unwrap()
        .map(|row| row.map(|(k, _)| k))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(forward, vec![key(10), key(20), key(30)]);

    // Seek lands at or after a hash key; a missed exact seek clears the position and
    // `next` yields nothing (the past-end rule the suite pins on numeric keys too).
    assert_eq!(cursor.seek(key(20)).unwrap().map(|(k, _)| k), Some(key(20)));
    assert_eq!(cursor.seek_exact(key(99)).unwrap(), None);
    assert_eq!(cursor.next().unwrap(), None);

    // Seek between two keys rounds up to the next one; past the last key misses.
    assert_eq!(cursor.seek(key(15)).unwrap().map(|(k, _)| k), Some(key(20)));
    assert_eq!(cursor.seek(key(31)).unwrap(), None);
}

/// The `TxLocations` unwind invariant: the numeric index is one contiguous, gap-free
/// key block, so a cursor range delete removes exactly the unwound transaction numbers
/// and leaves lower numbers untouched. This is the primitive the provider's
/// unwind/prune is built on.
pub fn logs_composite_key_range_delete<D: Database>(db: D) {
    let loc = |block: u64, index: u64| StoredTransactionLocation::new(block, index);

    // tx 0..5, one location row each.
    {
        let tx = db.tx_mut().unwrap();
        for tx_number in 0..6u64 {
            tx.put::<tables::TxLocations>(tx_number, loc(0, tx_number)).unwrap();
        }
        tx.commit().unwrap();
    }

    // Keys stay in numeric order, gap-free.
    {
        let tx = db.tx().unwrap();
        let keys = tx
            .cursor_read::<tables::TxLocations>()
            .unwrap()
            .walk(None)
            .unwrap()
            .map(|row| row.map(|(k, _)| k))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(keys, vec![0, 1, 2, 3, 4, 5]);
    }

    // Range-delete everything from tx 3 upward: one seek + one cursor walk, the same
    // shape the provider's unwind/prune uses.
    {
        let tx = db.tx_mut().unwrap();
        {
            let mut cursor = tx.cursor_write::<tables::TxLocations>().unwrap();
            let mut deleted = 0;
            let mut item = cursor.seek(3).unwrap();
            while let Some((key, _)) = item {
                if key >= 6 {
                    break;
                }
                cursor.delete_current().unwrap();
                deleted += 1;
                item = cursor.next().unwrap();
            }
            assert_eq!(deleted, 3, "tx 3, 4 and 5");
        }
        tx.commit().unwrap();
    }

    // Exactly tx 0..2 remain.
    let tx = db.tx().unwrap();
    let keys = tx
        .cursor_read::<tables::TxLocations>()
        .unwrap()
        .walk(None)
        .unwrap()
        .map(|row| row.map(|(k, _)| k))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(keys, vec![0, 1, 2]);
    assert_eq!(tx.get::<tables::TxLocations>(3).unwrap(), None);
}

/// The `SchemaInfo` singleton (§11 A): a one-row table whose key is a fixed 1-byte
/// encoding, written once on fresh init and read on every open. The cursor contract
/// holds on it like any other plain table, and deleting the row leaves the table
/// empty rather than resurrecting it.
pub fn schema_info_singleton_cursor<D: Database>(db: D) {
    let row = StoredSchemaInfo { schema_version: 7 };

    {
        let tx = db.tx_mut().unwrap();
        tx.put::<tables::SchemaInfo>(SchemaInfoKey::Singleton, row.clone()).unwrap();
        tx.commit().unwrap();
    }

    {
        let tx = db.tx().unwrap();
        assert_eq!(
            tx.get::<tables::SchemaInfo>(SchemaInfoKey::Singleton).unwrap(),
            Some(row.clone())
        );

        let mut cursor = tx.cursor_read::<tables::SchemaInfo>().unwrap();
        assert_eq!(cursor.first().unwrap().map(|(k, _)| k), Some(SchemaInfoKey::Singleton));
        assert_eq!(cursor.last().unwrap().map(|(k, _)| k), Some(SchemaInfoKey::Singleton));
        // One entry: stepping off either end yields nothing.
        assert_eq!(cursor.next().unwrap(), None);
        assert_eq!(cursor.prev().unwrap(), None);
        assert_eq!(tx.entries::<tables::SchemaInfo>().unwrap(), 1);
    }

    // Deleting the singleton empties the table; re-putting restores it.
    {
        let tx = db.tx_mut().unwrap();
        assert!(tx.delete::<tables::SchemaInfo>(SchemaInfoKey::Singleton, None).unwrap());
        assert_eq!(tx.entries::<tables::SchemaInfo>().unwrap(), 0);
        assert_eq!(tx.cursor_write::<tables::SchemaInfo>().unwrap().first().unwrap(), None);

        tx.put::<tables::SchemaInfo>(SchemaInfoKey::Singleton, row.clone()).unwrap();
        tx.commit().unwrap();
    }

    assert_eq!(
        db.tx().unwrap().get::<tables::SchemaInfo>(SchemaInfoKey::Singleton).unwrap(),
        Some(row)
    );
}

/// A writable transaction holds the environment's single-writer lock for its whole
/// lifetime: a second `tx_mut` on the same thread fails with `InitTx` instead of
/// deadlocking, and dropping the first hands the lock out again.
pub fn nested_write_transaction_is_rejected<D: Database>(db: D) {
    let tx = db.tx_mut().unwrap();
    let err = db.tx_mut().unwrap_err();
    assert!(matches!(&err, DatabaseError::InitTx(_)), "expected InitTx error, got {err:?}");
    drop(tx);

    db.tx_mut().unwrap().commit().unwrap();
}

/// Dropping a transaction without committing rolls it back first and only then
/// releases the single-writer lock, so the next writer on the same thread proceeds.
pub fn write_lock_released_after_rollback<D: Database>(db: D) {
    let tx = db.tx_mut().unwrap();
    tx.put::<tables::CanonicalChain>(1, B256::ZERO).unwrap();
    drop(tx);

    db.tx_mut().unwrap().commit().unwrap();
}

/// A writer on another thread blocks until the live transaction is dropped, then
/// acquires the lock (MDBX single-writer semantics).
pub fn write_transactions_serialize_across_threads<D: Database + 'static>(db: D) {
    use std::time::Duration;

    let held = db.tx_mut().unwrap();

    let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let tx = db.tx_mut().expect("second writer must acquire after the first drops");
        acquired_tx.send(()).expect("main thread gone");
        drop(tx);
    });

    // While `held` is alive the second writer must not get the lock.
    assert!(acquired_rx.recv_timeout(Duration::from_millis(250)).is_err());
    drop(held);
    acquired_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("second writer should acquire the lock once the first drops");
    writer.join().unwrap();
}

/// Writes stay inside the transaction until it commits: another transaction still sees
/// the committed state, the writer sees its own writes through the overlay, and dropping
/// the writer leaves nothing behind — not even a count change.
pub fn writes_stay_buffered_until_commit_and_vanish_on_drop<D: Database>(db: D) {
    let hash = |byte| B256::repeat_byte(byte);

    let seed = db.tx_mut().unwrap();
    seed.put::<tables::CanonicalChain>(1, hash(1)).unwrap();
    seed.put::<tables::CanonicalChain>(2, hash(2)).unwrap();
    seed.commit().unwrap();

    let writer = db.tx_mut().unwrap();
    writer.put::<tables::CanonicalChain>(2, hash(9)).unwrap();
    writer.put::<tables::CanonicalChain>(3, hash(3)).unwrap();
    writer.delete::<tables::CanonicalChain>(1, None).unwrap();

    // Nothing has reached the keyspace, so a concurrent transaction still reads the
    // committed state ...
    {
        let reader = db.tx().unwrap();
        assert_eq!(reader.get::<tables::CanonicalChain>(1).unwrap(), Some(hash(1)));
        assert_eq!(reader.get::<tables::CanonicalChain>(2).unwrap(), Some(hash(2)));
        assert_eq!(reader.get::<tables::CanonicalChain>(3).unwrap(), None);
        assert_eq!(reader.entries::<tables::CanonicalChain>().unwrap(), 2);
    }

    // ... while the writer reads its own buffered writes on top of it.
    assert_eq!(writer.get::<tables::CanonicalChain>(1).unwrap(), None);
    assert_eq!(writer.get::<tables::CanonicalChain>(2).unwrap(), Some(hash(9)));
    assert_eq!(writer.get::<tables::CanonicalChain>(3).unwrap(), Some(hash(3)));
    assert_eq!(writer.entries::<tables::CanonicalChain>().unwrap(), 2);

    // Dropping the writer without committing leaves the table exactly as it was.
    drop(writer);

    let reader = db.tx().unwrap();
    assert_eq!(reader.get::<tables::CanonicalChain>(1).unwrap(), Some(hash(1)));
    assert_eq!(reader.get::<tables::CanonicalChain>(2).unwrap(), Some(hash(2)));
    assert_eq!(reader.get::<tables::CanonicalChain>(3).unwrap(), None);
    assert_eq!(reader.entries::<tables::CanonicalChain>().unwrap(), 2);
}

/// Committing makes every buffered write visible at once, across tables, while a
/// read transaction opened earlier keeps observing the snapshot it started with.
pub fn commit_is_atomic_and_read_transactions_keep_their_snapshot<D: Database>(db: D) {
    let hash = |byte| B256::repeat_byte(byte);

    let seed = db.tx_mut().unwrap();
    seed.put::<tables::CanonicalChain>(1, hash(1)).unwrap();
    seed.commit().unwrap();

    let reader = db.tx().unwrap();

    let writer = db.tx_mut().unwrap();
    writer.put::<tables::CanonicalChain>(1, hash(9)).unwrap();
    writer.put::<tables::CanonicalChain>(2, hash(2)).unwrap();
    writer
        .put::<tables::AccountChangeSets>(
            1,
            AccountBeforeTx { address: Address::repeat_byte(0x11), info: None },
        )
        .unwrap();
    writer.commit().unwrap();

    // The reader pinned a snapshot before the commit, so it still sees the old state
    // through both its point reads and its cursor.
    assert_eq!(reader.get::<tables::CanonicalChain>(1).unwrap(), Some(hash(1)));
    assert_eq!(reader.get::<tables::CanonicalChain>(2).unwrap(), None);
    assert_eq!(reader.entries::<tables::CanonicalChain>().unwrap(), 1);
    let mut cursor = reader.cursor_read::<tables::CanonicalChain>().unwrap();
    assert_eq!(cursor.first().unwrap(), Some((1, hash(1))));

    // A transaction opened afterwards sees the whole commit, both tables at once.
    let fresh = db.tx().unwrap();
    assert_eq!(fresh.get::<tables::CanonicalChain>(1).unwrap(), Some(hash(9)));
    assert_eq!(fresh.get::<tables::CanonicalChain>(2).unwrap(), Some(hash(2)));
    assert_eq!(
        fresh.get::<tables::AccountChangeSets>(1).unwrap(),
        Some(AccountBeforeTx { address: Address::repeat_byte(0x11), info: None })
    );
    assert_eq!(fresh.entries::<tables::CanonicalChain>().unwrap(), 2);
}

/// Instantiates the whole conformance suite as `#[test]` functions for one backend.
///
/// `$new_db` must be an expression evaluating to a factory function returning a fresh,
/// empty database (e.g. `crate::test_utils::create_test_rw_db`).
#[macro_export]
macro_rules! conformance_tests {
    ($new_db:expr) => {
        $crate::__conformance_test_fns!(
            $new_db,
            cursor_navigation_plain_table,
            cursor_navigation_hash_keyed_table,
            cursor_empty_table_reads_none,
            cursor_seek_positions_at_or_after_key,
            cursor_insert_rejects_duplicates_upsert_overwrites,
            cursor_writes_roll_back_when_transaction_is_dropped,
            cursor_and_transaction_writes_interleave_in_one_transaction,
            cursor_navigates_buffered_writes_over_stored_entries,
            dup_cursor_navigates_duplicates_of_each_key,
            dup_cursor_writes_insert_append_and_delete,
            logs_composite_key_range_delete,
            schema_info_singleton_cursor,
            read_only_cursor_rejects_writes,
            delete_missing_entries_are_noops,
            nested_write_transaction_is_rejected,
            write_lock_released_after_rollback,
            write_transactions_serialize_across_threads,
            writes_stay_buffered_until_commit_and_vanish_on_drop,
            commit_is_atomic_and_read_transactions_keep_their_snapshot,
        );
    };
}

/// Hidden helper for [`conformance_tests!`](crate::conformance_tests); one `#[test]`
/// per suite function, each running against a fresh database from the factory.
#[doc(hidden)]
#[macro_export]
macro_rules! __conformance_test_fns {
    ($new_db:expr, $($name:ident),+ $(,)?) => {
        $(
            #[test]
            fn $name() {
                $crate::test_utils::conformance::$name(($new_db)());
            }
        )+
    };
}

/// The suite instantiated for the Fjall backend.
#[cfg(test)]
mod fjall {
    crate::conformance_tests!(crate::test_utils::create_test_rw_db);
}

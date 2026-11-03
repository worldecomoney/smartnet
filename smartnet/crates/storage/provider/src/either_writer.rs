//! Generic reader and writer abstractions for interacting with either database tables or static
//! files.

use std::{collections::BTreeSet, marker::PhantomData, ops::RangeInclusive};

use crate::providers::{aux_db::AuxDbBatch, history_info, HistoryInfo};
use alloy_primitives::{Address, BlockNumber, TxHash, TxNumber, B256};
use rayon::slice::ParallelSliceMut;
use smartnet_db::{
    cursor::{DbCursorRO, DbDupCursorRW},
    models::{AccountBeforeTx, StorageBeforeTx},
    transaction::{CursorMutTy, CursorTy, DbTx, DbTxMut, DupCursorMutTy, DupCursorTy},
};
use smartnet_db_api::{
    cursor::DbCursorRW,
    models::{storage_sharded_key::StorageShardedKey, BlockNumberAddress, ShardedKey},
    tables,
    tables::BlockNumberList,
};
use smartnet_errors::ProviderError;
use smartnet_node_types::NodePrimitives;
use smartnet_primitives_traits::StorageEntry;
use smartnet_storage_api::{DBProvider, NodePrimitivesProvider, StorageSettingsCache};
use smartnet_storage_errors::provider::ProviderResult;
use strum::Display;

/// Type alias for [`EitherReader`] constructors.
type EitherReaderTy<'a, P, T> =
    EitherReader<'a, CursorTy<<P as DBProvider>::Tx, T>, <P as NodePrimitivesProvider>::Primitives>;

/// Type alias for [`EitherReader`] constructors.
type DupEitherReaderTy<'a, P, T> = EitherReader<
    'a,
    DupCursorTy<<P as DBProvider>::Tx, T>,
    <P as NodePrimitivesProvider>::Primitives,
>;

/// Type alias for dup [`EitherWriter`] constructors.
type DupEitherWriterTy<'a, P, T> = EitherWriter<'a, DupCursorMutTy<<P as DBProvider>::Tx, T>>;

/// Type alias for [`EitherWriter`] constructors.
type EitherWriterTy<'a, P, T> = EitherWriter<'a, CursorMutTy<<P as DBProvider>::Tx, T>>;

/// Helper type for `aux DB` batch argument in writer constructors.
pub type AuxDbBatchArg<'a> = crate::providers::aux_db::AuxDbBatch<'a>;

/// The raw `aux DB` batch type returned by [`EitherWriter::into_raw_aux_db_batch`].
pub type RawAuxDbBatch = crate::providers::aux_db::RawAuxDbBatch;

/// Helper type for `aux DB` snapshot argument in reader constructors.
///
/// The `Option` allows callers to skip `aux DB` access when it isn't needed
/// (e.g., on nodes without an `aux DB`).
pub type AuxDbRefArg<'a> = Option<crate::providers::aux_db::AuxDbReadSnapshot<'a>>;

/// Represents a destination for writing data, either to database or `aux DB`.
#[derive(Debug, Display)]
pub enum EitherWriter<'a, CURSOR> {
    /// Write to database table via cursor
    Database(CURSOR),
    /// Write to `aux DB` using a write-only batch (historical tables).
    AuxDb(AuxDbBatch<'a>),
}

impl<'a> EitherWriter<'a, ()> {
    /// Creates a new [`EitherWriter`] for account changesets. Static file destinations were
    /// retired in Stage 3 (3A.3), so changesets are always written to the primary database.
    pub fn new_account_changesets<P>(
        provider: &'a P,
        _block_number: BlockNumber,
    ) -> ProviderResult<DupEitherWriterTy<'a, P, tables::AccountChangeSets>>
    where
        P: DBProvider + NodePrimitivesProvider + StorageSettingsCache,
        P::Tx: DbTxMut,
    {
        Ok(EitherWriter::Database(
            provider.tx_ref().cursor_dup_write::<tables::AccountChangeSets>()?,
        ))
    }

    /// Creates a new [`EitherWriter`] for storage changesets. Static file destinations were
    /// retired in Stage 3 (3A.3), so changesets are always written to the primary database.
    pub fn new_storage_changesets<P>(
        provider: &'a P,
        _block_number: BlockNumber,
    ) -> ProviderResult<DupEitherWriterTy<'a, P, tables::StorageChangeSets>>
    where
        P: DBProvider + NodePrimitivesProvider + StorageSettingsCache,
        P::Tx: DbTxMut,
    {
        Ok(EitherWriter::Database(
            provider.tx_ref().cursor_dup_write::<tables::StorageChangeSets>()?,
        ))
    }

    /// Creates a new [`EitherWriter`] for storages history based on storage settings.
    pub fn new_storages_history<P>(
        provider: &P,
        _aux_db_batch: AuxDbBatchArg<'a>,
    ) -> ProviderResult<EitherWriterTy<'a, P, tables::StoragesHistory>>
    where
        P: DBProvider + NodePrimitivesProvider + StorageSettingsCache,
        P::Tx: DbTxMut,
    {
        Ok(EitherWriter::Database(provider.tx_ref().cursor_write::<tables::StoragesHistory>()?))
    }

    /// Creates a new [`EitherWriter`] for transaction hash numbers based on storage settings.
    pub fn new_transaction_hash_numbers<P>(
        provider: &P,
        _aux_db_batch: AuxDbBatchArg<'a>,
    ) -> ProviderResult<EitherWriterTy<'a, P, tables::TransactionHashNumbers>>
    where
        P: DBProvider + NodePrimitivesProvider + StorageSettingsCache,
        P::Tx: DbTxMut,
    {
        Ok(EitherWriter::Database(
            provider.tx_ref().cursor_write::<tables::TransactionHashNumbers>()?,
        ))
    }

    /// Creates a new [`EitherWriter`] for account history based on storage settings.
    pub fn new_accounts_history<P>(
        provider: &P,
        _aux_db_batch: AuxDbBatchArg<'a>,
    ) -> ProviderResult<EitherWriterTy<'a, P, tables::AccountsHistory>>
    where
        P: DBProvider + NodePrimitivesProvider + StorageSettingsCache,
        P::Tx: DbTxMut,
    {
        Ok(EitherWriter::Database(provider.tx_ref().cursor_write::<tables::AccountsHistory>()?))
    }
}

impl<'a, CURSOR> EitherWriter<'a, CURSOR> {
    /// Extracts the raw `aux DB` write batch from this writer, if it contains one.
    ///
    /// Returns `Some(WriteBatchWithTransaction)` for [`Self::AuxDb`] variant,
    /// `None` for other variants.
    ///
    /// This is used to defer `aux DB` commits to the provider level, ensuring all
    /// storage commits (database, `aux DB`) happen atomically in a single place.
    pub fn into_raw_aux_db_batch(self) -> Option<RawAuxDbBatch> {
        match self {
            Self::Database(_) => None,
            Self::AuxDb(batch) => {
                batch.into_inner();
                Some(())
            }
        }
    }

    /// Increment the block number.
    ///
    /// No-op for [`Self::Database`]; block positioning only ever applied to the retired
    /// static file writers.
    pub const fn increment_block(
        &mut self,
        _expected_block_number: BlockNumber,
    ) -> ProviderResult<()> {
        match self {
            Self::Database(_) => Ok(()),
            Self::AuxDb(_) => Err(ProviderError::UnsupportedProvider),
        }
    }

    /// Ensures that the writer is positioned at the specified block number.
    ///
    /// No-op for [`Self::Database`]; block positioning only ever applied to the retired
    /// static file writers.
    pub const fn ensure_at_block(&mut self, _block_number: BlockNumber) -> ProviderResult<()> {
        match self {
            Self::Database(_) => Ok(()),
            Self::AuxDb(_) => Err(ProviderError::UnsupportedProvider),
        }
    }
}

impl<'a, CURSOR> EitherWriter<'a, CURSOR>
where
    CURSOR: DbCursorRW<tables::TransactionHashNumbers> + DbCursorRO<tables::TransactionHashNumbers>,
{
    /// Puts a transaction hash number mapping.
    ///
    /// When `append_only` is true, uses `cursor.append()` which is significantly faster
    /// but requires entries to be inserted in order and the table to be empty.
    /// When false, uses `cursor.upsert()` which handles arbitrary insertion order and duplicates.
    pub fn put_transaction_hash_number(
        &mut self,
        hash: TxHash,
        tx_num: TxNumber,
        append_only: bool,
    ) -> ProviderResult<()> {
        match self {
            Self::Database(cursor) => {
                if append_only {
                    Ok(cursor.append(hash, &tx_num)?)
                } else {
                    Ok(cursor.upsert(hash, &tx_num)?)
                }
            }
            Self::AuxDb(batch) => batch.put::<tables::TransactionHashNumbers>(hash, &tx_num),
        }
    }

    /// Puts multiple transaction hash number mappings in a batch.
    ///
    /// Accepts a vector of `(TxHash, TxNumber)` tuples and writes them all using the same cursor.
    /// This is more efficient than calling `put_transaction_hash_number` repeatedly.
    ///
    /// When `append_only` is true, uses `cursor.append()` which requires entries to be
    /// pre-sorted and the table to be empty or have only lower keys.
    /// When false, uses `cursor.upsert()` which handles arbitrary insertion order.
    pub fn put_transaction_hash_numbers_batch(
        &mut self,
        entries: Vec<(TxHash, TxNumber)>,
        append_only: bool,
    ) -> ProviderResult<()> {
        match self {
            Self::Database(cursor) => {
                for (hash, tx_num) in entries {
                    if append_only {
                        cursor.append(hash, &tx_num)?;
                    } else {
                        cursor.upsert(hash, &tx_num)?;
                    }
                }
            }
            Self::AuxDb(batch) => {
                for (hash, tx_num) in entries {
                    batch.put::<tables::TransactionHashNumbers>(hash, &tx_num)?;
                }
            }
        }
        Ok(())
    }

    /// Deletes a transaction hash number mapping.
    pub fn delete_transaction_hash_number(&mut self, hash: TxHash) -> ProviderResult<()> {
        match self {
            Self::Database(cursor) => {
                if cursor.seek_exact(hash)?.is_some() {
                    cursor.delete_current()?;
                }
                Ok(())
            }
            Self::AuxDb(batch) => batch.delete::<tables::TransactionHashNumbers>(hash),
        }
    }
}

impl<'a, CURSOR> EitherWriter<'a, CURSOR>
where
    CURSOR: DbCursorRW<tables::StoragesHistory> + DbCursorRO<tables::StoragesHistory>,
{
    /// Puts a storage history entry.
    pub fn put_storage_history(
        &mut self,
        key: StorageShardedKey,
        value: &BlockNumberList,
    ) -> ProviderResult<()> {
        match self {
            Self::Database(cursor) => Ok(cursor.upsert(key, value)?),
            Self::AuxDb(batch) => batch.put::<tables::StoragesHistory>(key, value),
        }
    }

    /// Deletes a storage history entry.
    pub fn delete_storage_history(&mut self, key: StorageShardedKey) -> ProviderResult<()> {
        match self {
            Self::Database(cursor) => {
                if cursor.seek_exact(key)?.is_some() {
                    cursor.delete_current()?;
                }
                Ok(())
            }
            Self::AuxDb(batch) => batch.delete::<tables::StoragesHistory>(key),
        }
    }

    /// Appends a storage history entry (for first sync - more efficient).
    pub fn append_storage_history(
        &mut self,
        key: StorageShardedKey,
        value: &BlockNumberList,
    ) -> ProviderResult<()> {
        match self {
            Self::Database(cursor) => Ok(cursor.append(key, value)?),
            Self::AuxDb(batch) => batch.put::<tables::StoragesHistory>(key, value),
        }
    }

    /// Upserts a storage history entry (for incremental sync).
    pub fn upsert_storage_history(
        &mut self,
        key: StorageShardedKey,
        value: &BlockNumberList,
    ) -> ProviderResult<()> {
        match self {
            Self::Database(cursor) => Ok(cursor.upsert(key, value)?),
            Self::AuxDb(batch) => batch.put::<tables::StoragesHistory>(key, value),
        }
    }

    /// Gets the last shard for an address and storage key (keyed with `u64::MAX`).
    pub fn get_last_storage_history_shard(
        &mut self,
        address: Address,
        storage_key: B256,
    ) -> ProviderResult<Option<BlockNumberList>> {
        let key = StorageShardedKey::last(address, storage_key);
        match self {
            Self::Database(cursor) => Ok(cursor.seek_exact(key)?.map(|(_, v)| v)),
            Self::AuxDb(batch) => batch.get::<tables::StoragesHistory>(key),
        }
    }
}

impl<'a, CURSOR> EitherWriter<'a, CURSOR>
where
    CURSOR: DbCursorRW<tables::AccountsHistory> + DbCursorRO<tables::AccountsHistory>,
{
    /// Appends an account history entry (for first sync - more efficient).
    pub fn append_account_history(
        &mut self,
        key: ShardedKey<Address>,
        value: &BlockNumberList,
    ) -> ProviderResult<()> {
        match self {
            Self::Database(cursor) => Ok(cursor.append(key, value)?),
            Self::AuxDb(batch) => batch.put::<tables::AccountsHistory>(key, value),
        }
    }

    /// Upserts an account history entry (for incremental sync).
    pub fn upsert_account_history(
        &mut self,
        key: ShardedKey<Address>,
        value: &BlockNumberList,
    ) -> ProviderResult<()> {
        match self {
            Self::Database(cursor) => Ok(cursor.upsert(key, value)?),
            Self::AuxDb(batch) => batch.put::<tables::AccountsHistory>(key, value),
        }
    }

    /// Gets the last shard for an address (keyed with `u64::MAX`).
    pub fn get_last_account_history_shard(
        &mut self,
        address: Address,
    ) -> ProviderResult<Option<BlockNumberList>> {
        match self {
            Self::Database(cursor) => {
                Ok(cursor.seek_exact(ShardedKey::last(address))?.map(|(_, v)| v))
            }
            Self::AuxDb(batch) => batch.get::<tables::AccountsHistory>(ShardedKey::last(address)),
        }
    }

    /// Deletes an account history entry.
    pub fn delete_account_history(&mut self, key: ShardedKey<Address>) -> ProviderResult<()> {
        match self {
            Self::Database(cursor) => {
                if cursor.seek_exact(key)?.is_some() {
                    cursor.delete_current()?;
                }
                Ok(())
            }
            Self::AuxDb(batch) => batch.delete::<tables::AccountsHistory>(key),
        }
    }
}

impl<'a, CURSOR> EitherWriter<'a, CURSOR>
where
    CURSOR: DbDupCursorRW<tables::AccountChangeSets>,
{
    /// Append account changeset for a block.
    ///
    /// NOTE: This _sorts_ the changesets by address before appending
    pub fn put_account_changeset(
        &mut self,
        block_number: BlockNumber,
        mut changeset: Vec<AccountBeforeTx>,
    ) -> ProviderResult<()> {
        // First sort the changesets
        changeset.par_sort_by_key(|a| a.address);
        match self {
            Self::Database(cursor) => {
                for change in changeset {
                    cursor.append_dup(block_number, change)?;
                }
            }
            Self::AuxDb(_) => return Err(ProviderError::UnsupportedProvider),
        }

        Ok(())
    }
}

impl<'a, CURSOR> EitherWriter<'a, CURSOR>
where
    CURSOR: DbDupCursorRW<tables::StorageChangeSets>,
{
    /// Append storage changeset for a block.
    ///
    /// NOTE: This _sorts_ the changesets by address and storage key before appending.
    pub fn put_storage_changeset(
        &mut self,
        block_number: BlockNumber,
        mut changeset: Vec<StorageBeforeTx>,
    ) -> ProviderResult<()> {
        changeset.par_sort_by_key(|change| (change.address, change.key));

        match self {
            Self::Database(cursor) => {
                for change in changeset {
                    let storage_id = BlockNumberAddress((block_number, change.address));
                    cursor.append_dup(
                        storage_id,
                        StorageEntry { key: change.key, value: change.value },
                    )?;
                }
            }
            Self::AuxDb(_) => return Err(ProviderError::UnsupportedProvider),
        }

        Ok(())
    }
}

/// Represents a source for reading data, either from database or `aux DB`.
#[derive(Debug, Display)]
pub enum EitherReader<'a, CURSOR, N> {
    /// Read from database table via cursor. `N` is carried as phantom data only; static
    /// file reads were retired in Stage 3 (3A.3).
    Database(CURSOR, PhantomData<(&'a (), N)>),
    /// Read from `aux DB` snapshot (works in both read-only and read-write modes)
    AuxDb(crate::providers::aux_db::AuxDbReadSnapshot<'a>),
}

impl<'a> EitherReader<'a, (), ()> {
    /// Creates a new [`EitherReader`] for storages history based on storage settings.
    pub fn new_storages_history<P>(
        provider: &P,
        _aux_db: AuxDbRefArg<'a>,
    ) -> ProviderResult<EitherReaderTy<'a, P, tables::StoragesHistory>>
    where
        P: DBProvider + NodePrimitivesProvider + StorageSettingsCache,
        P::Tx: DbTx,
    {
        Ok(EitherReader::Database(
            provider.tx_ref().cursor_read::<tables::StoragesHistory>()?,
            PhantomData,
        ))
    }

    /// Creates a new [`EitherReader`] for transaction hash numbers based on storage settings.
    pub fn new_transaction_hash_numbers<P>(
        provider: &P,
        _aux_db: AuxDbRefArg<'a>,
    ) -> ProviderResult<EitherReaderTy<'a, P, tables::TransactionHashNumbers>>
    where
        P: DBProvider + NodePrimitivesProvider + StorageSettingsCache,
        P::Tx: DbTx,
    {
        Ok(EitherReader::Database(
            provider.tx_ref().cursor_read::<tables::TransactionHashNumbers>()?,
            PhantomData,
        ))
    }

    /// Creates a new [`EitherReader`] for account history based on storage settings.
    pub fn new_accounts_history<P>(
        provider: &P,
        _aux_db: AuxDbRefArg<'a>,
    ) -> ProviderResult<EitherReaderTy<'a, P, tables::AccountsHistory>>
    where
        P: DBProvider + NodePrimitivesProvider + StorageSettingsCache,
        P::Tx: DbTx,
    {
        Ok(EitherReader::Database(
            provider.tx_ref().cursor_read::<tables::AccountsHistory>()?,
            PhantomData,
        ))
    }

    /// Creates a new [`EitherReader`] for account changesets. Static file destinations were
    /// retired in Stage 3 (3A.3), so changesets are always read from the primary database.
    pub fn new_account_changesets<P>(
        provider: &P,
    ) -> ProviderResult<DupEitherReaderTy<'a, P, tables::AccountChangeSets>>
    where
        P: DBProvider + NodePrimitivesProvider + StorageSettingsCache,
        P::Tx: DbTx,
    {
        Ok(EitherReader::Database(
            provider.tx_ref().cursor_dup_read::<tables::AccountChangeSets>()?,
            PhantomData,
        ))
    }
}

impl<CURSOR, N: NodePrimitives> EitherReader<'_, CURSOR, N>
where
    CURSOR: DbCursorRO<tables::TransactionHashNumbers>,
{
    /// Gets a transaction number by its hash.
    pub fn get_transaction_hash_number(
        &mut self,
        hash: TxHash,
    ) -> ProviderResult<Option<TxNumber>> {
        match self {
            Self::Database(cursor, _) => Ok(cursor.seek_exact(hash)?.map(|(_, v)| v)),
            Self::AuxDb(snapshot) => snapshot.get::<tables::TransactionHashNumbers>(hash),
        }
    }
}

impl<CURSOR, N: NodePrimitives> EitherReader<'_, CURSOR, N>
where
    CURSOR: DbCursorRO<tables::StoragesHistory>,
{
    /// Gets a storage history shard entry for the given [`StorageShardedKey`], if present.
    pub fn get_storage_history(
        &mut self,
        key: StorageShardedKey,
    ) -> ProviderResult<Option<BlockNumberList>> {
        match self {
            Self::Database(cursor, _) => Ok(cursor.seek_exact(key)?.map(|(_, v)| v)),
            Self::AuxDb(snapshot) => snapshot.get::<tables::StoragesHistory>(key),
        }
    }

    /// Lookup storage history and return [`HistoryInfo`].
    pub fn storage_history_info(
        &mut self,
        address: Address,
        storage_key: alloy_primitives::B256,
        block_number: BlockNumber,
        lowest_available_block_number: Option<BlockNumber>,
        visible_tip: BlockNumber,
    ) -> ProviderResult<HistoryInfo> {
        match self {
            Self::Database(cursor, _) => {
                let key = StorageShardedKey::new(address, storage_key, block_number);
                history_info::<tables::StoragesHistory, _, _>(
                    cursor,
                    key,
                    block_number,
                    |k| k.address == address && k.sharded_key.key == storage_key,
                    lowest_available_block_number,
                )
            }
            Self::AuxDb(snapshot) => snapshot.storage_history_info(
                address,
                storage_key,
                block_number,
                lowest_available_block_number,
                visible_tip,
            ),
        }
    }
}

impl<CURSOR, N: NodePrimitives> EitherReader<'_, CURSOR, N>
where
    CURSOR: DbCursorRO<tables::AccountsHistory>,
{
    /// Gets an account history shard entry for the given [`ShardedKey`], if present.
    pub fn get_account_history(
        &mut self,
        key: ShardedKey<Address>,
    ) -> ProviderResult<Option<BlockNumberList>> {
        match self {
            Self::Database(cursor, _) => Ok(cursor.seek_exact(key)?.map(|(_, v)| v)),
            Self::AuxDb(snapshot) => snapshot.get::<tables::AccountsHistory>(key),
        }
    }

    /// Lookup account history and return [`HistoryInfo`].
    pub fn account_history_info(
        &mut self,
        address: Address,
        block_number: BlockNumber,
        lowest_available_block_number: Option<BlockNumber>,
        visible_tip: BlockNumber,
    ) -> ProviderResult<HistoryInfo> {
        match self {
            Self::Database(cursor, _) => {
                let key = ShardedKey::new(address, block_number);
                history_info::<tables::AccountsHistory, _, _>(
                    cursor,
                    key,
                    block_number,
                    |k| k.key == address,
                    lowest_available_block_number,
                )
            }
            Self::AuxDb(snapshot) => snapshot.account_history_info(
                address,
                block_number,
                lowest_available_block_number,
                visible_tip,
            ),
        }
    }
}

impl<CURSOR, N: NodePrimitives> EitherReader<'_, CURSOR, N>
where
    CURSOR: DbCursorRO<tables::AccountChangeSets>,
{
    /// Iterate over account changesets and return all account address that were changed.
    pub fn changed_accounts_with_range(
        &mut self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<BTreeSet<Address>> {
        match self {
            Self::Database(provider, _) => provider
                .walk_range(range)?
                .map(|entry| {
                    entry.map(|(_, account_before)| account_before.address).map_err(Into::into)
                })
                .collect(),
            Self::AuxDb(_) => Err(ProviderError::UnsupportedProvider),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::create_test_provider_factory;

    use super::*;
    use alloy_primitives::Address;
    use smartnet_db::models::AccountBeforeTx;
    use smartnet_storage_api::DatabaseProviderFactory;

    /// Verifies that `changed_accounts_with_range` reads account changesets back from the
    /// primary database and is not confused by rows outside the requested range.
    #[test]
    fn test_changed_accounts_with_range_from_database() {
        let factory = create_test_provider_factory();
        let highest_block = 5u64;

        let addresses: Vec<Address> = (0..=highest_block)
            .map(|i| {
                let mut addr = Address::ZERO;
                addr.0[0] = i as u8;
                addr
            })
            .collect();

        {
            let provider = factory.database_provider_rw().unwrap();
            let mut cursor = provider.tx_ref().cursor_write::<tables::AccountChangeSets>().unwrap();
            for block_num in 0..=highest_block {
                let changeset =
                    vec![AccountBeforeTx { address: addresses[block_num as usize], info: None }];
                for change in changeset {
                    cursor.append_dup(block_num, change).unwrap();
                }
            }
            provider.commit().unwrap();
        }

        let provider = factory.database_provider_ro().unwrap();

        let mut reader = EitherReader::new_account_changesets(&provider).unwrap();
        assert!(matches!(reader, EitherReader::Database(_, _)));

        // Query range 0..=10 when only blocks 0-5 exist - should return only accounts from
        // blocks 0-5.
        let result = reader.changed_accounts_with_range(0..=10).unwrap();

        let expected: BTreeSet<Address> = addresses.into_iter().collect();
        assert_eq!(result, expected);
    }
}

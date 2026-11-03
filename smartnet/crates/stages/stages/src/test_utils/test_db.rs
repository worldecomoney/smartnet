use alloy_primitives::{keccak256, Address, BlockNumber, TxHash, TxNumber, B256};
use smartnet_ethereum_primitives::{Block, Receipt};
use smartnet_primitives_traits::{Account, SealedBlock, SealedHeader, StorageEntry};
use smartnet_testing_utils::generators::ChangeSet;
use smartnet_chainspec::MAINNET;
use smartnet_db::{
    test_utils::{create_test_rw_db, create_test_rw_db_with_path},
    DatabaseEnv,
};
use smartnet_db_api::{
    common::KeyValue,
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO},
    database::Database,
    models::{AccountBeforeTx, StorageSettings},
    table::Table,
    tables,
    transaction::{DbTx, DbTxMut},
    DatabaseError as DbError,
};
use smartnet_provider::{
    test_utils::MockNodeTypesWithDB, DatabaseProviderFactory, HistoryWriter, ProviderError,
    ProviderFactory, StatsReader,
};
use smartnet_storage_api::metadata::StorageSettingsCache;
use smartnet_storage_errors::provider::ProviderResult;
use std::{collections::BTreeMap, fmt::Debug, path::Path};

/// Test database that is used for testing stage implementations.
#[derive(Debug)]
pub struct TestStageDB {
    pub factory: ProviderFactory<MockNodeTypesWithDB>,
}

impl Default for TestStageDB {
    /// Create a new instance of [`TestStageDB`]
    fn default() -> Self {
        let factory = ProviderFactory::new(
            create_test_rw_db(),
            MAINNET.clone(),
            None,
            smartnet_tasks::Runtime::test(),
        )
        .expect("failed to create test provider factory");
        // Stage tests target upstream archive semantics (history indexing on) unless a
        // test explicitly overrides the settings with `set_storage_settings_cache`.
        factory.set_storage_settings_cache(StorageSettings::archive());
        Self { factory }
    }
}

impl TestStageDB {
    pub fn new(path: &Path) -> Self {
        let factory = ProviderFactory::new(
            create_test_rw_db_with_path(path),
            MAINNET.clone(),
            None,
            smartnet_tasks::Runtime::test(),
        )
        .expect("failed to create test provider factory");
        factory.set_storage_settings_cache(StorageSettings::archive());
        Self { factory }
    }

    /// Invoke a callback with transaction committing it afterwards
    pub fn commit<F>(&self, f: F) -> ProviderResult<()>
    where
        F: FnOnce(&<DatabaseEnv as Database>::TXMut) -> ProviderResult<()>,
    {
        let tx = self.factory.provider_rw()?;
        f(tx.tx_ref())?;
        tx.commit().expect("failed to commit");
        Ok(())
    }

    /// Invoke a callback with a read transaction
    pub fn query<F, Ok>(&self, f: F) -> ProviderResult<Ok>
    where
        F: FnOnce(&<DatabaseEnv as Database>::TX) -> ProviderResult<Ok>,
    {
        f(self.factory.provider()?.tx_ref())
    }

    /// Invoke a callback with a provider that can be used to create transactions or fetch from
    /// static files.
    pub fn query_with_provider<F, Ok>(&self, f: F) -> ProviderResult<Ok>
    where
        F: FnOnce(
            <ProviderFactory<MockNodeTypesWithDB> as DatabaseProviderFactory>::Provider,
        ) -> ProviderResult<Ok>,
    {
        f(self.factory.provider()?)
    }

    /// Check if the table is empty
    pub fn table_is_empty<T: Table>(&self) -> ProviderResult<bool> {
        self.query(|tx| {
            let last = tx.cursor_read::<T>()?.last()?;
            Ok(last.is_none())
        })
    }

    /// Return full table as Vec
    pub fn table<T: Table>(&self) -> ProviderResult<Vec<KeyValue<T>>>
    where
        T::Key: Default + Ord,
    {
        self.query(|tx| {
            Ok(tx
                .cursor_read::<T>()?
                .walk(Some(T::Key::default()))?
                .collect::<Result<Vec<_>, DbError>>()?)
        })
    }

    /// Return the number of entries in the table or static file segment
    pub fn count_entries<T: Table>(&self) -> ProviderResult<usize> {
        self.factory.provider()?.count_entries::<T>()
    }

    /// Check that there is no table entry above a given
    /// number by [`Table::Key`]
    pub fn ensure_no_entry_above<T, F>(&self, num: u64, mut selector: F) -> ProviderResult<()>
    where
        T: Table,
        F: FnMut(T::Key) -> BlockNumber,
    {
        self.query(|tx| {
            let mut cursor = tx.cursor_read::<T>()?;
            if let Some((key, _)) = cursor.last()? {
                assert!(selector(key) <= num);
            }
            Ok(())
        })
    }

    /// Check that there is no table entry above a given
    /// number by [`Table::Value`]
    pub fn ensure_no_entry_above_by_value<T, F>(
        &self,
        num: u64,
        mut selector: F,
    ) -> ProviderResult<()>
    where
        T: Table,
        F: FnMut(T::Value) -> BlockNumber,
    {
        self.query(|tx| {
            let mut cursor = tx.cursor_read::<T>()?;
            let mut rev_walker = cursor.walk_back(None)?;
            while let Some((_, value)) = rev_walker.next().transpose()? {
                assert!(selector(value) <= num);
            }
            Ok(())
        })
    }

    /// Insert the canonical index rows for a header.
    pub fn insert_header<TX: DbTx + DbTxMut>(tx: &TX, header: &SealedHeader) -> ProviderResult<()> {
        tx.put::<tables::CanonicalChain>(header.number, header.hash())?;
        tx.put::<tables::HeaderNumbers>(header.hash(), header.number)?;
        Ok(())
    }

    fn insert_headers_inner<'a, I>(&self, headers: I) -> ProviderResult<()>
    where
        I: IntoIterator<Item = &'a SealedHeader>,
    {
        let tx = self.factory.provider_rw()?.into_tx();

        for header in headers {
            Self::insert_header(&tx, header)?;
        }

        tx.commit()?;

        Ok(())
    }

    /// Insert ordered collection of [`SealedHeader`] into the corresponding static file and tables
    /// that are supposed to be populated by the headers stage.
    pub fn insert_headers<'a, I>(&self, headers: I) -> ProviderResult<()>
    where
        I: IntoIterator<Item = &'a SealedHeader>,
    {
        self.insert_headers_inner::<I>(headers)
    }

    /// Insert ordered collection of [`SealedBlock`] into corresponding tables.
    /// Superset functionality of [`TestStageDB::insert_headers`].
    ///
    /// If `tx_offset` is set to `None`, then transactions will be stored on static files, otherwise
    /// database.
    ///
    /// Assumes that there's a single transition for each transaction (i.e. no block rewards).
    pub fn insert_blocks<'a, I>(&self, blocks: I, storage_kind: StorageKind) -> ProviderResult<()>
    where
        I: IntoIterator<Item = &'a SealedBlock<Block>>,
    {
        let tx = self.factory.provider_rw().unwrap().into_tx();
        let mut next_tx_num = storage_kind.tx_offset();

        let blocks = blocks.into_iter().collect::<Vec<_>>();

        blocks.iter().try_for_each(|block| Self::insert_header(&tx, block.sealed_header()))?;

        blocks.into_iter().try_for_each(|block| {
            // The payload lives in the file tier; the fixture writes the tx index rows that
            // production's `save_block_state_and_index` also writes.
            block.body().transactions.iter().enumerate().try_for_each(|(index, body_tx)| {
                let tx_num = next_tx_num;
                tx.put::<tables::TxLocations>(
                    tx_num,
                    smartnet_db_api::models::StoredTransactionLocation::new(
                        block.number,
                        index as u64,
                    ),
                )?;
                tx.put::<tables::TransactionHashNumbers>(*body_tx.tx_hash(), tx_num)?;
                next_tx_num += 1;
                Ok::<(), ProviderError>(())
            })
        })?;

        tx.commit()?;

        Ok(())
    }

    pub fn insert_tx_hash_numbers<I>(&self, tx_hash_numbers: I) -> ProviderResult<()>
    where
        I: IntoIterator<Item = (TxHash, TxNumber)>,
    {
        self.commit(|tx| {
            let mut cursor = tx.cursor_write::<tables::TransactionHashNumbers>()?;
            for (tx_hash, tx_num) in tx_hash_numbers {
                cursor.upsert(tx_hash, &tx_num)?;
            }
            Ok(())
        })
    }

    /// Insert collection of ([`TxNumber`], [Receipt]) into the corresponding table.
    pub fn insert_receipts<I>(&self, receipts: I) -> ProviderResult<()>
    where
        I: IntoIterator<Item = (TxNumber, Receipt)>,
    {
        let _ = receipts;
        // Receipts are part of the immutable file-tier payload.
        Ok(())
    }

    /// Insert collection of ([`TxNumber`], [Receipt]) organized by respective block numbers into
    /// the corresponding table. `storage_kind` only selects the transaction numbering offset —
    /// since Stage 3 (3A.3) receipts live in the primary database in both cases.
    pub fn insert_receipts_by_block<I, J>(
        &self,
        receipts: I,
        storage_kind: StorageKind,
    ) -> ProviderResult<()>
    where
        I: IntoIterator<Item = (BlockNumber, J)>,
        J: IntoIterator<Item = (TxNumber, Receipt)>,
    {
        let _ = (receipts, storage_kind);
        // Receipts are part of the immutable file-tier payload.
        Ok(())
    }

    pub fn insert_transaction_senders<I>(&self, transaction_senders: I) -> ProviderResult<()>
    where
        I: IntoIterator<Item = (TxNumber, Address)>,
    {
        let _ = transaction_senders;
        // Senders are part of the immutable file-tier payload.
        Ok(())
    }

    /// Insert collection of ([Address], [Account]) into corresponding tables.
    pub fn insert_accounts_and_storages<I, S>(&self, accounts: I) -> ProviderResult<()>
    where
        I: IntoIterator<Item = (Address, (Account, S))>,
        S: IntoIterator<Item = StorageEntry>,
    {
        self.commit(|tx| {
            accounts.into_iter().try_for_each(|(address, (account, storage))| {
                let hashed_address = keccak256(address);

                // Insert into account tables.
                tx.put::<tables::PlainAccountState>(address, account)?;
                tx.put::<tables::HashedAccounts>(hashed_address, account)?;

                // Insert into storage tables.
                storage.into_iter().filter(|e| !e.value.is_zero()).try_for_each(|entry| {
                    let hashed_entry = StorageEntry { key: keccak256(entry.key), ..entry };

                    let mut cursor = tx.cursor_dup_write::<tables::PlainStorageState>()?;
                    if cursor
                        .seek_by_key_subkey(address, entry.key)?
                        .is_some_and(|e| e.key == entry.key)
                    {
                        cursor.delete_current()?;
                    }
                    cursor.upsert(address, &entry)?;

                    let mut cursor = tx.cursor_dup_write::<tables::HashedStorages>()?;
                    if cursor
                        .seek_by_key_subkey(hashed_address, hashed_entry.key)?
                        .is_some_and(|e| e.key == hashed_entry.key)
                    {
                        cursor.delete_current()?;
                    }
                    cursor.upsert(hashed_address, &hashed_entry)?;

                    Ok(())
                })
            })
        })
    }

    /// Insert collection of [`ChangeSet`] into corresponding tables.
    pub fn insert_changesets<I>(
        &self,
        changesets: I,
        block_offset: Option<u64>,
    ) -> ProviderResult<()>
    where
        I: IntoIterator<Item = ChangeSet>,
    {
        let offset = block_offset.unwrap_or_default();
        self.commit(|tx| {
            changesets.into_iter().enumerate().try_for_each(|(block, changeset)| {
                changeset.into_iter().try_for_each(|(address, old_account, old_storage)| {
                    let block = offset + block as u64;
                    // Insert into account changeset.
                    tx.put::<tables::AccountChangeSets>(
                        block,
                        AccountBeforeTx { address, info: Some(old_account) },
                    )?;

                    let block_address = (block, address).into();

                    // Insert into storage changeset.
                    old_storage.into_iter().try_for_each(|entry| {
                        Ok(tx.put::<tables::StorageChangeSets>(block_address, entry)?)
                    })
                })
            })
        })
    }

    pub fn insert_history<I>(&self, changesets: I, _block_offset: Option<u64>) -> ProviderResult<()>
    where
        I: IntoIterator<Item = ChangeSet>,
    {
        let mut accounts = BTreeMap::<Address, Vec<u64>>::new();
        let mut storages = BTreeMap::<(Address, B256), Vec<u64>>::new();

        for (block, changeset) in changesets.into_iter().enumerate() {
            for (address, _, storage_entries) in changeset {
                accounts.entry(address).or_default().push(block as u64);
                for storage_entry in storage_entries {
                    storages.entry((address, storage_entry.key)).or_default().push(block as u64);
                }
            }
        }

        let provider_rw = self.factory.provider_rw()?;
        provider_rw.insert_account_history_index(accounts)?;
        provider_rw.insert_storage_history_index(storages)?;
        provider_rw.commit()?;

        Ok(())
    }
}

/// Used to identify where to store data when setting up a test.
#[derive(Debug)]
pub enum StorageKind {
    Database(Option<u64>),
    Static,
}

impl StorageKind {
    #[expect(dead_code)]
    const fn is_database(&self) -> bool {
        matches!(self, Self::Database(_))
    }

    fn tx_offset(&self) -> u64 {
        if let Self::Database(offset) = self {
            return offset.unwrap_or_default();
        }
        0
    }
}

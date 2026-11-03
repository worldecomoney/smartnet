use alloc::collections::{BTreeMap, BTreeSet};
use alloy_primitives::{map::B256Map, Address, BlockNumber, B256};
use auto_impl::auto_impl;
use smartnet_primitives_traits::{Account, StorageEntry};
use smartnet_db_api::models::BlockNumberAddress;
use smartnet_db_models::AccountBeforeTx;
use smartnet_storage_errors::provider::ProviderResult;

/// Hashing Writer
#[auto_impl(&, Arc, Box)]
pub trait HashingWriter: Send {
    /// Unwind and clear account hashing.
    ///
    /// # Returns
    ///
    /// Set of hashed keys of updated accounts.
    fn unwind_account_hashing<'a>(
        &self,
        changesets: impl Iterator<Item = &'a (BlockNumber, AccountBeforeTx)>,
    ) -> ProviderResult<BTreeMap<B256, Option<Account>>>;

    /// Inserts all accounts into [`AccountsHistory`][smartnet_db_api::tables::AccountsHistory]
    /// table.
    ///
    /// # Returns
    ///
    /// Set of hashed keys of updated accounts.
    fn insert_account_for_hashing(
        &self,
        accounts: impl IntoIterator<Item = (Address, Option<Account>)>,
    ) -> ProviderResult<BTreeMap<B256, Option<Account>>>;

    /// Unwind and clear storage hashing.
    ///
    /// # Returns
    ///
    /// Mapping of hashed keys of updated accounts to their respective updated hashed slots.
    fn unwind_storage_hashing(
        &self,
        changesets: impl Iterator<Item = (BlockNumberAddress, StorageEntry)>,
    ) -> ProviderResult<B256Map<BTreeSet<B256>>>;

    /// Iterates over storages and inserts them to hashing table.
    ///
    /// # Returns
    ///
    /// Mapping of hashed keys of updated accounts to their respective updated hashed slots.
    fn insert_storage_for_hashing(
        &self,
        storages: impl IntoIterator<Item = (Address, impl IntoIterator<Item = StorageEntry>)>,
    ) -> ProviderResult<B256Map<BTreeSet<B256>>>;
}

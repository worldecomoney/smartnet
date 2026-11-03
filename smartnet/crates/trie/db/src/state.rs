use crate::{DatabaseHashedCursorFactory, DatabaseTrieCursorFactory};
use alloy_primitives::{keccak256, map::B256Map, BlockNumber, B256};
use smartnet_trie_common::StateRootError;
use smartnet_trie::{
    hashed_cursor::HashedPostStateCursorFactory, trie_cursor::InMemoryTrieCursorFactory,
    updates::TrieUpdates, HashedPostStateSorted, HashedStorageSorted, StateRoot, StateRootProgress,
    TrieInputSorted,
};
use smartnet_db_api::{
    models::{AccountBeforeTx, BlockNumberAddress},
    transaction::DbTx,
};
use smartnet_storage_api::{
    BlockNumReader, ChangeSetReader, DBProvider, StorageChangeSetReader, StorageSettingsCache,
};
use smartnet_storage_errors::provider::ProviderError;
use std::{
    collections::HashSet,
    ops::{Bound, RangeBounds, RangeInclusive},
};
use tracing::{debug, instrument};

/// Extends [`StateRoot`] with operations specific for working with a database transaction.
pub trait DatabaseStateRoot<'a, TX>: Sized {
    /// Create a new [`StateRoot`] instance.
    fn from_tx(tx: &'a TX) -> Self;

    /// Given a block number range, identifies all the accounts and storage keys that
    /// have changed.
    ///
    /// # Returns
    ///
    /// An instance of state root calculator with account and storage prefixes loaded.
    fn incremental_root_calculator(
        provider: &'a (impl ChangeSetReader
                 + StorageChangeSetReader
                 + StorageSettingsCache
                 + DBProvider<Tx = TX>),
        range: RangeInclusive<BlockNumber>,
    ) -> Result<Self, StateRootError>;

    /// Computes the state root of the trie with the changed account and storage prefixes and
    /// existing trie nodes.
    ///
    /// # Returns
    ///
    /// The updated state root.
    fn incremental_root(
        provider: &'a (impl ChangeSetReader
                 + StorageChangeSetReader
                 + StorageSettingsCache
                 + DBProvider<Tx = TX>),
        range: RangeInclusive<BlockNumber>,
    ) -> Result<B256, StateRootError>;

    /// Computes the state root of the trie with the changed account and storage prefixes and
    /// existing trie nodes collecting updates in the process.
    ///
    /// Ignores the threshold.
    ///
    /// # Returns
    ///
    /// The updated state root and the trie updates.
    fn incremental_root_with_updates(
        provider: &'a (impl ChangeSetReader
                 + StorageChangeSetReader
                 + StorageSettingsCache
                 + DBProvider<Tx = TX>),
        range: RangeInclusive<BlockNumber>,
    ) -> Result<(B256, TrieUpdates), StateRootError>;

    /// Computes the state root of the trie with the changed account and storage prefixes and
    /// existing trie nodes collecting updates in the process.
    ///
    /// # Returns
    ///
    /// The intermediate progress of state root computation.
    fn incremental_root_with_progress(
        provider: &'a (impl ChangeSetReader
                 + StorageChangeSetReader
                 + StorageSettingsCache
                 + DBProvider<Tx = TX>),
        range: RangeInclusive<BlockNumber>,
    ) -> Result<StateRootProgress, StateRootError>;

    /// Calculate the state root for this [`HashedPostStateSorted`].
    /// Internally, this method retrieves prefixsets and uses them
    /// to calculate incremental state root.
    ///
    /// # Example
    ///
    /// ```
    /// use alloy_primitives::U256;
    /// use smartnet_primitives_traits::Account;
    /// use smartnet_trie::{updates::TrieUpdates, HashedPostState, StateRoot};
    /// use smartnet_trie_db::{DatabaseStateRoot, LegacyKeyAdapter};
    /// use smartnet_db::test_utils::create_test_rw_db;
    /// use smartnet_db_api::database::Database;
    ///
    /// // Initialize the database
    /// let db = create_test_rw_db();
    ///
    /// // Initialize hashed post state
    /// let mut hashed_state = HashedPostState::default();
    /// hashed_state.accounts.insert(
    ///     [0x11; 32].into(),
    ///     Some(Account { nonce: 1, balance: U256::from(10), bytecode_hash: None }),
    /// );
    ///
    /// // Calculate the state root
    /// let tx = db.tx().expect("failed to create transaction");
    /// let state_root = <StateRoot<
    ///     smartnet_trie_db::DatabaseTrieCursorFactory<_, LegacyKeyAdapter>,
    ///     smartnet_trie_db::DatabaseHashedCursorFactory<_>,
    /// > as DatabaseStateRoot<_>>::overlay_root(&tx, &hashed_state.into_sorted());
    /// ```
    ///
    /// # Returns
    ///
    /// The state root for this [`HashedPostStateSorted`].
    fn overlay_root(tx: &'a TX, post_state: &HashedPostStateSorted)
        -> Result<B256, StateRootError>;

    /// Calculates the state root for this [`HashedPostStateSorted`] and returns it alongside trie
    /// updates. See [`Self::overlay_root`] for more info.
    fn overlay_root_with_updates(
        tx: &'a TX,
        post_state: &HashedPostStateSorted,
    ) -> Result<(B256, TrieUpdates), StateRootError>;

    /// Calculates the state root for provided [`HashedPostStateSorted`] using cached intermediate
    /// nodes.
    fn overlay_root_from_nodes(tx: &'a TX, input: TrieInputSorted) -> Result<B256, StateRootError>;

    /// Calculates the state root and trie updates for provided [`HashedPostStateSorted`] using
    /// cached intermediate nodes.
    fn overlay_root_from_nodes_with_updates(
        tx: &'a TX,
        input: TrieInputSorted,
    ) -> Result<(B256, TrieUpdates), StateRootError>;
}

/// Extends [`HashedPostStateSorted`] with operations specific for working with a database
/// transaction.
pub trait DatabaseHashedPostState: Sized {
    /// Initializes [`HashedPostStateSorted`] from reverts. Iterates over state reverts in the
    /// specified range and aggregates them into sorted hashed state.
    fn from_reverts(
        provider: &(impl ChangeSetReader + StorageChangeSetReader + BlockNumReader + DBProvider),
        range: impl RangeBounds<BlockNumber>,
    ) -> Result<HashedPostStateSorted, ProviderError>;
}

impl<'a, TX: DbTx, A: crate::TrieTableAdapter> DatabaseStateRoot<'a, TX>
    for StateRoot<DatabaseTrieCursorFactory<&'a TX, A>, DatabaseHashedCursorFactory<&'a TX>>
{
    fn from_tx(tx: &'a TX) -> Self {
        Self::new(DatabaseTrieCursorFactory::new(tx), DatabaseHashedCursorFactory::new(tx))
    }

    fn incremental_root_calculator(
        provider: &'a (impl ChangeSetReader
                 + StorageChangeSetReader
                 + StorageSettingsCache
                 + DBProvider<Tx = TX>),
        range: RangeInclusive<BlockNumber>,
    ) -> Result<Self, StateRootError> {
        let loaded_prefix_sets =
            crate::prefix_set::load_prefix_sets_with_provider(provider, range)?;
        Ok(Self::from_tx(provider.tx_ref()).with_prefix_sets(loaded_prefix_sets))
    }

    fn incremental_root(
        provider: &'a (impl ChangeSetReader
                 + StorageChangeSetReader
                 + StorageSettingsCache
                 + DBProvider<Tx = TX>),
        range: RangeInclusive<BlockNumber>,
    ) -> Result<B256, StateRootError> {
        debug!(target: "trie::loader", ?range, "incremental state root");
        Self::incremental_root_calculator(provider, range)?.root()
    }

    fn incremental_root_with_updates(
        provider: &'a (impl ChangeSetReader
                 + StorageChangeSetReader
                 + StorageSettingsCache
                 + DBProvider<Tx = TX>),
        range: RangeInclusive<BlockNumber>,
    ) -> Result<(B256, TrieUpdates), StateRootError> {
        debug!(target: "trie::loader", ?range, "incremental state root");
        Self::incremental_root_calculator(provider, range)?.root_with_updates()
    }

    fn incremental_root_with_progress(
        provider: &'a (impl ChangeSetReader
                 + StorageChangeSetReader
                 + StorageSettingsCache
                 + DBProvider<Tx = TX>),
        range: RangeInclusive<BlockNumber>,
    ) -> Result<StateRootProgress, StateRootError> {
        debug!(target: "trie::loader", ?range, "incremental state root with progress");
        Self::incremental_root_calculator(provider, range)?.root_with_progress()
    }

    fn overlay_root(
        tx: &'a TX,
        post_state: &HashedPostStateSorted,
    ) -> Result<B256, StateRootError> {
        let prefix_sets = post_state.construct_prefix_sets().freeze();
        StateRoot::new(
            DatabaseTrieCursorFactory::<_, A>::new(tx),
            HashedPostStateCursorFactory::new(DatabaseHashedCursorFactory::new(tx), post_state),
        )
        .with_prefix_sets(prefix_sets)
        .root()
    }

    fn overlay_root_with_updates(
        tx: &'a TX,
        post_state: &HashedPostStateSorted,
    ) -> Result<(B256, TrieUpdates), StateRootError> {
        let prefix_sets = post_state.construct_prefix_sets().freeze();
        StateRoot::new(
            DatabaseTrieCursorFactory::<_, A>::new(tx),
            HashedPostStateCursorFactory::new(DatabaseHashedCursorFactory::new(tx), post_state),
        )
        .with_prefix_sets(prefix_sets)
        .root_with_updates()
    }

    fn overlay_root_from_nodes(tx: &'a TX, input: TrieInputSorted) -> Result<B256, StateRootError> {
        StateRoot::new(
            InMemoryTrieCursorFactory::new(
                DatabaseTrieCursorFactory::<_, A>::new(tx),
                input.nodes.as_ref(),
            ),
            HashedPostStateCursorFactory::new(
                DatabaseHashedCursorFactory::new(tx),
                input.state.as_ref(),
            ),
        )
        .with_prefix_sets(input.prefix_sets.freeze())
        .root()
    }

    fn overlay_root_from_nodes_with_updates(
        tx: &'a TX,
        input: TrieInputSorted,
    ) -> Result<(B256, TrieUpdates), StateRootError> {
        StateRoot::new(
            InMemoryTrieCursorFactory::new(
                DatabaseTrieCursorFactory::<_, A>::new(tx),
                input.nodes.as_ref(),
            ),
            HashedPostStateCursorFactory::new(
                DatabaseHashedCursorFactory::new(tx),
                input.state.as_ref(),
            ),
        )
        .with_prefix_sets(input.prefix_sets.freeze())
        .root_with_updates()
    }
}

/// Calls [`HashedPostStateSorted::from_reverts`].
pub fn from_reverts_auto(
    provider: &(impl ChangeSetReader
          + StorageChangeSetReader
          + BlockNumReader
          + DBProvider
          + StorageSettingsCache),
    range: impl RangeBounds<BlockNumber>,
) -> Result<HashedPostStateSorted, ProviderError> {
    HashedPostStateSorted::from_reverts(provider, range)
}

impl DatabaseHashedPostState for HashedPostStateSorted {
    /// Builds a sorted hashed post-state from reverts.
    ///
    /// Reads database data directly into Vecs, using `HashSet`s only to track seen keys.
    /// This avoids intermediate `HashMap` allocations since the data is already sorted.
    ///
    /// - Reads the first occurrence of each changed account/storage slot in the range.
    /// - Addresses are always keccak256-hashed.
    /// - Storage keys are always plain and are hashed via `keccak256`.
    /// - Returns keys already ordered for trie iteration.
    #[instrument(target = "trie::db", skip(provider), fields(range))]
    fn from_reverts(
        provider: &(impl ChangeSetReader + StorageChangeSetReader + BlockNumReader + DBProvider),
        range: impl RangeBounds<BlockNumber>,
    ) -> Result<Self, ProviderError> {
        // Extract concrete start/end values to use for both account and storage changesets.
        let start = match range.start_bound() {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n + 1,
            Bound::Unbounded => 0,
        };

        let end = match range.end_bound() {
            Bound::Included(&n) => n + 1,
            Bound::Excluded(&n) => n,
            Bound::Unbounded => BlockNumber::MAX,
        };

        // Iterate over account changesets and record value before first occurring account change
        let mut accounts = Vec::new();
        let mut seen_accounts = HashSet::new();
        for entry in provider.account_changesets_range(start..end)? {
            let (_, AccountBeforeTx { address, info }) = entry;
            if seen_accounts.insert(address) {
                accounts.push((keccak256(address), info));
            }
        }
        accounts.sort_unstable_by_key(|(hash, _)| *hash);

        // Read storages into B256Map<Vec<_>> with HashSet to track seen keys.
        // Only keep the first (oldest) occurrence of each (address, slot) pair.
        let mut storages = B256Map::<Vec<_>>::default();
        let mut seen_storage_keys = HashSet::new();

        if start < end {
            let end_inclusive = end.saturating_sub(1);
            for (BlockNumberAddress((_, address)), storage) in
                provider.storage_changesets_range(start..=end_inclusive)?
            {
                if seen_storage_keys.insert((address, storage.key)) {
                    let hashed_address = keccak256(address);
                    storages
                        .entry(hashed_address)
                        .or_default()
                        .push((keccak256(storage.key), storage.value));
                }
            }
        }

        // Sort storage slots and convert to HashedStorageSorted
        let hashed_storages = storages
            .into_iter()
            .map(|(address, mut slots)| {
                slots.sort_unstable_by_key(|(slot, _)| *slot);
                (address, HashedStorageSorted { storage_slots: slots, wiped: false })
            })
            .collect();

        Ok(Self::new(accounts, hashed_storages))
    }
}


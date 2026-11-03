use crate::{
    AccountReader, BlockHashReader, HashedPostStateProvider, StateProvider, StateRootProvider,
};
use alloy_primitives::{Address, BlockNumber, Bytes, StorageKey, StorageValue, B256};
use smartnet_db_api::{cursor::DbDupCursorRO, tables, transaction::DbTx};
use smartnet_primitives_traits::{Account, Bytecode};
use smartnet_storage_api::{
    BytecodeReader, DBProvider, StateProofProvider, StorageRootProvider, StorageSettingsCache,
};
use smartnet_storage_errors::provider::{ProviderError, ProviderResult};
use smartnet_trie::{
    hashed_cursor::HashedPostStateCursorFactory,
    proof::{Proof, StorageProof},
    trie_cursor::InMemoryTrieCursorFactory,
    updates::TrieUpdates,
    witness::TrieWitness,
    AccountProof, ExecutionWitnessMode, HashedPostState, HashedStorage, KeccakKeyHasher,
    MultiProof, MultiProofTargets, StateRoot, StorageMultiProof, StorageRoot, TrieInput,
    TrieInputSorted,
};
use smartnet_trie_db::{
    DatabaseProof, DatabaseStateRoot, DatabaseStorageProof, DatabaseStorageRoot,
};

type DbStateRoot<'a, TX, A> = StateRoot<
    smartnet_trie_db::DatabaseTrieCursorFactory<&'a TX, A>,
    smartnet_trie_db::DatabaseHashedCursorFactory<&'a TX>,
>;
type DbStorageRoot<'a, TX, A> = StorageRoot<
    smartnet_trie_db::DatabaseTrieCursorFactory<&'a TX, A>,
    smartnet_trie_db::DatabaseHashedCursorFactory<&'a TX>,
>;
type DbStorageProof<'a, TX, A> = StorageProof<
    'static,
    smartnet_trie_db::DatabaseTrieCursorFactory<&'a TX, A>,
    smartnet_trie_db::DatabaseHashedCursorFactory<&'a TX>,
>;
type DbProof<'a, TX, A> = Proof<
    smartnet_trie_db::DatabaseTrieCursorFactory<&'a TX, A>,
    smartnet_trie_db::DatabaseHashedCursorFactory<&'a TX>,
>;
/// State provider over latest state that takes tx reference.
///
/// Wraps a [`DBProvider`] to get access to database.
#[derive(Debug)]
pub struct LatestStateProviderRef<'b, Provider>(&'b Provider);

impl<'b, Provider: DBProvider> LatestStateProviderRef<'b, Provider> {
    /// Create new state provider
    pub const fn new(provider: &'b Provider) -> Self {
        Self(provider)
    }

    fn tx(&self) -> &Provider::Tx {
        self.0.tx_ref()
    }
}

impl<Provider: DBProvider + StorageSettingsCache> AccountReader
    for LatestStateProviderRef<'_, Provider>
{
    /// Get basic account information.
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        self.tx().get_by_encoded_key::<tables::PlainAccountState>(address).map_err(Into::into)
    }
}

impl<Provider: BlockHashReader> BlockHashReader for LatestStateProviderRef<'_, Provider> {
    /// Get block hash by number.
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        self.0.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.0.canonical_hashes_range(start, end)
    }
}

impl<Provider: DBProvider + StorageSettingsCache> StateRootProvider
    for LatestStateProviderRef<'_, Provider>
{
    fn state_root(&self, hashed_state: HashedPostState) -> ProviderResult<B256> {
        smartnet_trie_db::with_adapter!(self.0, |A| {
            let sorted = hashed_state.into_sorted();
            Ok(<DbStateRoot<'_, _, A> as DatabaseStateRoot<_>>::overlay_root(self.tx(), &sorted)?)
        })
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        smartnet_trie_db::with_adapter!(self.0, |A| {
            Ok(<DbStateRoot<'_, _, A> as DatabaseStateRoot<_>>::overlay_root_from_nodes(
                self.tx(),
                TrieInputSorted::from_unsorted(input),
            )?)
        })
    }

    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        smartnet_trie_db::with_adapter!(self.0, |A| {
            let sorted = hashed_state.into_sorted();
            Ok(<DbStateRoot<'_, _, A> as DatabaseStateRoot<_>>::overlay_root_with_updates(
                self.tx(),
                &sorted,
            )?)
        })
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        smartnet_trie_db::with_adapter!(self.0, |A| {
            Ok(
                <DbStateRoot<'_, _, A> as DatabaseStateRoot<_>>::overlay_root_from_nodes_with_updates(
                    self.tx(),
                    TrieInputSorted::from_unsorted(input),
                )?,
            )
        })
    }
}

impl<Provider: DBProvider + StorageSettingsCache> StorageRootProvider
    for LatestStateProviderRef<'_, Provider>
{
    fn storage_root(
        &self,
        address: Address,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        smartnet_trie_db::with_adapter!(self.0, |A| {
            <DbStorageRoot<'_, _, A>>::overlay_root(self.tx(), address, hashed_storage)
                .map_err(|err| ProviderError::Database(err.into()))
        })
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<smartnet_trie::StorageProof> {
        smartnet_trie_db::with_adapter!(self.0, |A| {
            <DbStorageProof<'_, _, A>>::overlay_storage_proof(
                self.tx(),
                address,
                slot,
                hashed_storage,
            )
            .map_err(ProviderError::from)
        })
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        smartnet_trie_db::with_adapter!(self.0, |A| {
            <DbStorageProof<'_, _, A>>::overlay_storage_multiproof(
                self.tx(),
                address,
                slots,
                hashed_storage,
            )
            .map_err(ProviderError::from)
        })
    }
}

impl<Provider: DBProvider + StorageSettingsCache> StateProofProvider
    for LatestStateProviderRef<'_, Provider>
{
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        smartnet_trie_db::with_adapter!(self.0, |A| {
            let proof = <DbProof<'_, _, A> as DatabaseProof>::from_tx(self.tx());
            proof.overlay_account_proof(input, address, slots).map_err(ProviderError::from)
        })
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        smartnet_trie_db::with_adapter!(self.0, |A| {
            let proof = <DbProof<'_, _, A> as DatabaseProof>::from_tx(self.tx());
            proof.overlay_multiproof(input, targets).map_err(ProviderError::from)
        })
    }

    fn witness(
        &self,
        input: TrieInput,
        target: HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        smartnet_trie_db::with_adapter!(self.0, |A| {
            let nodes_sorted = input.nodes.into_sorted();
            let state_sorted = input.state.into_sorted();
            let witness = TrieWitness::new(
                InMemoryTrieCursorFactory::new(
                    smartnet_trie_db::DatabaseTrieCursorFactory::<_, A>::new(self.tx()),
                    &nodes_sorted,
                ),
                HashedPostStateCursorFactory::new(
                    smartnet_trie_db::DatabaseHashedCursorFactory::new(self.tx()),
                    &state_sorted,
                ),
            )
            .with_prefix_sets_mut(input.prefix_sets)
            .with_execution_witness_mode(mode);
            let witness =
                if mode.is_canonical() { witness } else { witness.always_include_root_node() };
            let mut values: Vec<_> = witness.compute(target)?.into_values().collect();
            if mode.is_canonical() {
                values.sort_unstable();
            }
            Ok(values)
        })
    }
}

impl<Provider: DBProvider> HashedPostStateProvider for LatestStateProviderRef<'_, Provider> {
    fn hashed_post_state(&self, bundle_state: &revm_database::BundleState) -> HashedPostState {
        HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state())
    }
}

impl<Provider: DBProvider + BlockHashReader + StorageSettingsCache> StateProvider
    for LatestStateProviderRef<'_, Provider>
{
    /// Get storage by plain (unhashed) storage key slot.
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        let mut cursor = self.tx().cursor_dup_read::<tables::PlainStorageState>()?;
        if let Some(entry) = cursor.seek_by_key_subkey(account, storage_key)? &&
            entry.key == storage_key
        {
            return Ok(Some(entry.value));
        }
        Ok(None)
    }
}

impl<Provider: DBProvider + BlockHashReader> BytecodeReader
    for LatestStateProviderRef<'_, Provider>
{
    /// Get account code by its hash.
    ///
    /// Contract bytecode is **file-tier only** when the file tier is enabled — the
    /// `Bytecodes` table is never consulted as a fallback (operator rule: bytecode has no
    /// DB fallback). `Ok(None)` means the code is not in the content-addressed files.
    /// The table read exists solely for the file-tier-disabled test mode, where the table
    /// is the only place bytecode can live.
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        if smartnet_file_tier::base().is_some() {
            return super::bytecode_from_files(code_hash);
        }
        self.tx().get_by_encoded_key::<tables::Bytecodes>(code_hash).map_err(Into::into)
    }
}

/// State provider for the latest state.
#[derive(Debug)]
pub struct LatestStateProvider<Provider>(Provider);

impl<Provider: DBProvider> LatestStateProvider<Provider> {
    /// Create new state provider
    pub const fn new(db: Provider) -> Self {
        Self(db)
    }

    /// Returns a new provider that takes the `TX` as reference
    #[inline(always)]
    const fn as_ref(&self) -> LatestStateProviderRef<'_, Provider> {
        LatestStateProviderRef::new(&self.0)
    }
}

// Delegates all provider impls to [LatestStateProviderRef]
smartnet_storage_api::macros::delegate_provider_impls!(LatestStateProvider<Provider> where [Provider: DBProvider + BlockHashReader + StorageSettingsCache]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_test_provider_factory;
    use alloy_primitives::{address, b256, keccak256, U256};
    use smartnet_db_api::{
        tables,
        transaction::{DbTx, DbTxMut},
    };
    use smartnet_primitives_traits::StorageEntry;
    use smartnet_storage_api::StorageSettingsCache;

    const fn assert_state_provider<T: StateProvider>() {}
    #[expect(dead_code)]
    const fn assert_latest_state_provider<
        T: DBProvider + BlockHashReader + StorageSettingsCache,
    >() {
        assert_state_provider::<LatestStateProvider<T>>();
    }

    #[test]
    fn test_latest_storage_legacy() {
        let factory = create_test_provider_factory();

        let address = address!("0x0000000000000000000000000000000000000001");
        let slot = b256!("0x0000000000000000000000000000000000000000000000000000000000000005");

        let tx = factory.provider_rw().unwrap().into_tx();
        tx.put::<tables::PlainStorageState>(
            address,
            StorageEntry { key: slot, value: U256::from(42) },
        )
        .unwrap();
        tx.commit().unwrap();

        let db = factory.provider().unwrap();
        let provider_ref = LatestStateProviderRef::new(&db);

        assert_eq!(provider_ref.storage(address, slot).unwrap(), Some(U256::from(42)));

        let other_slot =
            b256!("0x0000000000000000000000000000000000000000000000000000000000000099");
        assert_eq!(provider_ref.storage(address, other_slot).unwrap(), None);
    }

    #[test]
    fn test_latest_storage_legacy_does_not_read_hashed() {
        let factory = create_test_provider_factory();

        let address = address!("0x0000000000000000000000000000000000000001");
        let slot = b256!("0x0000000000000000000000000000000000000000000000000000000000000005");
        let hashed_address = keccak256(address);
        let hashed_slot = keccak256(slot);

        let tx = factory.provider_rw().unwrap().into_tx();
        tx.put::<tables::HashedStorages>(
            hashed_address,
            StorageEntry { key: hashed_slot, value: U256::from(42) },
        )
        .unwrap();
        tx.commit().unwrap();

        let db = factory.provider().unwrap();
        let provider_ref = LatestStateProviderRef::new(&db);
        assert_eq!(provider_ref.storage(address, slot).unwrap(), None);
    }
}

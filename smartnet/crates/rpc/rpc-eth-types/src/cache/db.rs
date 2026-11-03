//! Helper types to workaround 'higher-ranked lifetime error'
//! <https://github.com/rust-lang/rust/issues/100013> in default implementation of
//! `smartnet_rpc_eth_api::helpers::Call`.

use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use smartnet_revm::database::StateProviderDatabase;
use smartnet_trie::{HashedStorage, MultiProofTargets};
use revm::database::{BundleState, State};
use smartnet_errors::ProviderResult;
use smartnet_storage_api::{
    BytecodeReader, HashedPostStateProvider, StateProvider, StateProviderBox,
};

/// Helper alias type for the state's [`State`]
pub type StateCacheDb = State<StateProviderDatabase<StateProviderTraitObjWrapper>>;

/// Underlying historical state used by this cache DB (for state-root / proof calculation).
pub fn state_cache_db_provider(db: &StateCacheDb) -> &dyn StateProvider {
    db.database.0.as_state_provider()
}

/// Hack to get around 'higher-ranked lifetime error', see
/// <https://github.com/rust-lang/rust/issues/100013>
///
/// Apparently, when dealing with our RPC code, compiler is struggling to prove lifetimes around
/// [`StateProvider`] trait objects. This type is a workaround which should help the compiler to
/// understand that there are no lifetimes involved.
#[derive(Clone)]
#[expect(missing_debug_implementations)]
pub struct StateProviderTraitObjWrapper(pub Arc<dyn StateProvider + Send>);

impl StateProviderTraitObjWrapper {
    /// Wraps a boxed state provider for use in [`StateCacheDb`].
    pub fn new(state: StateProviderBox) -> Self {
        Self(Arc::from(state))
    }

    /// Returns the underlying historical state provider.
    pub fn as_state_provider(&self) -> &dyn StateProvider {
        self.0.as_ref()
    }
}

impl smartnet_storage_api::StateRootProvider for StateProviderTraitObjWrapper {
    fn state_root(
        &self,
        hashed_state: smartnet_trie::HashedPostState,
    ) -> smartnet_errors::ProviderResult<B256> {
        self.0.state_root(hashed_state)
    }

    fn state_root_from_nodes(
        &self,
        input: smartnet_trie::TrieInput,
    ) -> smartnet_errors::ProviderResult<B256> {
        self.0.state_root_from_nodes(input)
    }

    fn state_root_with_updates(
        &self,
        hashed_state: smartnet_trie::HashedPostState,
    ) -> smartnet_errors::ProviderResult<(B256, smartnet_trie::updates::TrieUpdates)> {
        self.0.state_root_with_updates(hashed_state)
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: smartnet_trie::TrieInput,
    ) -> smartnet_errors::ProviderResult<(B256, smartnet_trie::updates::TrieUpdates)> {
        self.0.state_root_from_nodes_with_updates(input)
    }
}

impl smartnet_storage_api::StorageRootProvider for StateProviderTraitObjWrapper {
    fn storage_root(
        &self,
        address: Address,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        self.0.storage_root(address, hashed_storage)
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<smartnet_trie::StorageProof> {
        self.0.storage_proof(address, slot, hashed_storage)
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        hashed_storage: HashedStorage,
    ) -> ProviderResult<smartnet_trie::StorageMultiProof> {
        self.0.storage_multiproof(address, slots, hashed_storage)
    }
}

impl smartnet_storage_api::StateProofProvider for StateProviderTraitObjWrapper {
    fn proof(
        &self,
        input: smartnet_trie::TrieInput,
        address: Address,
        slots: &[B256],
    ) -> smartnet_errors::ProviderResult<smartnet_trie::AccountProof> {
        self.0.proof(input, address, slots)
    }

    fn multiproof(
        &self,
        input: smartnet_trie::TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<smartnet_trie::MultiProof> {
        self.0.multiproof(input, targets)
    }

    fn witness(
        &self,
        input: smartnet_trie::TrieInput,
        target: smartnet_trie::HashedPostState,
        mode: smartnet_trie::ExecutionWitnessMode,
    ) -> smartnet_errors::ProviderResult<Vec<alloy_primitives::Bytes>> {
        self.0.witness(input, target, mode)
    }
}

impl smartnet_storage_api::AccountReader for StateProviderTraitObjWrapper {
    fn basic_account(
        &self,
        address: &Address,
    ) -> smartnet_errors::ProviderResult<Option<smartnet_primitives_traits::Account>> {
        self.0.basic_account(address)
    }
}

impl smartnet_storage_api::BlockHashReader for StateProviderTraitObjWrapper {
    fn block_hash(
        &self,
        block_number: alloy_primitives::BlockNumber,
    ) -> smartnet_errors::ProviderResult<Option<B256>> {
        self.0.block_hash(block_number)
    }

    fn convert_block_hash(
        &self,
        hash_or_number: alloy_rpc_types_eth::BlockHashOrNumber,
    ) -> smartnet_errors::ProviderResult<Option<B256>> {
        self.0.convert_block_hash(hash_or_number)
    }

    fn canonical_hashes_range(
        &self,
        start: alloy_primitives::BlockNumber,
        end: alloy_primitives::BlockNumber,
    ) -> smartnet_errors::ProviderResult<Vec<B256>> {
        self.0.canonical_hashes_range(start, end)
    }
}

impl HashedPostStateProvider for StateProviderTraitObjWrapper {
    fn hashed_post_state(&self, bundle_state: &BundleState) -> smartnet_trie::HashedPostState {
        self.0.hashed_post_state(bundle_state)
    }
}

impl StateProvider for StateProviderTraitObjWrapper {
    fn storage(
        &self,
        account: Address,
        storage_key: alloy_primitives::StorageKey,
    ) -> smartnet_errors::ProviderResult<Option<alloy_primitives::StorageValue>> {
        self.0.storage(account, storage_key)
    }

    fn account_code(
        &self,
        addr: &Address,
    ) -> smartnet_errors::ProviderResult<Option<smartnet_primitives_traits::Bytecode>> {
        self.0.account_code(addr)
    }

    fn account_balance(&self, addr: &Address) -> smartnet_errors::ProviderResult<Option<U256>> {
        self.0.account_balance(addr)
    }

    fn account_nonce(&self, addr: &Address) -> smartnet_errors::ProviderResult<Option<u64>> {
        self.0.account_nonce(addr)
    }
}

impl BytecodeReader for StateProviderTraitObjWrapper {
    fn bytecode_by_hash(
        &self,
        code_hash: &B256,
    ) -> smartnet_errors::ProviderResult<Option<smartnet_primitives_traits::Bytecode>> {
        self.0.bytecode_by_hash(code_hash)
    }
}

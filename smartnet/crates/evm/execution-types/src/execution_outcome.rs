use crate::{BlockExecutionOutput, BlockExecutionResult};
use alloc::{vec, vec::Vec};
use alloy_eips::eip7685::Requests;
use alloy_primitives::{
    logs_bloom,
    map::{AddressMap, B256Map, HashMap},
    Address, BlockNumber, Bloom, Log, B256, U256,
};
use smartnet_primitives_traits::{Account, Bytecode, Receipt, StorageEntry};
use smartnet_trie_common::{HashedPostState, KeyHasher};
use revm::{
    database::{states::BundleState, BundleAccount},
    state::AccountInfo,
};

/// Type used to initialize revms bundle state.
pub type BundleStateInit = AddressMap<(Option<Account>, Option<Account>, B256Map<(U256, U256)>)>;

/// Types used inside `RevertsInit` to initialize revms reverts.
pub type AccountRevertInit = (Option<Option<Account>>, Vec<StorageEntry>);

/// Type used to initialize revms reverts.
pub type RevertsInit = HashMap<BlockNumber, AddressMap<AccountRevertInit>>;

/// Represents a changed account
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChangedAccount {
    /// The address of the account.
    pub address: Address,
    /// Account nonce.
    pub nonce: u64,
    /// Account balance.
    pub balance: U256,
}

impl ChangedAccount {
    /// Creates a new [`ChangedAccount`] with the given address and 0 balance and nonce.
    pub const fn empty(address: Address) -> Self {
        Self { address, nonce: 0, balance: U256::ZERO }
    }
}

/// Represents the outcome of block execution, including post-execution changes and reverts.
///
/// The `ExecutionOutcome` structure aggregates the state changes over an arbitrary number of
/// blocks, capturing the resulting state, receipts, and requests following the execution.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ExecutionOutcome<T = smartnet_ethereum_primitives::Receipt> {
    /// Bundle state with reverts.
    pub bundle: BundleState,
    /// The collection of receipts.
    /// Outer vector stores receipts for each block sequentially.
    /// The inner vector stores receipts ordered by transaction number.
    pub receipts: Vec<Vec<T>>,
    /// First block of bundle state.
    pub first_block: BlockNumber,
    /// The collection of EIP-7685 requests.
    /// Outer vector stores requests for each block sequentially.
    /// The inner vector stores requests ordered by transaction number.
    ///
    /// A transaction may have zero or more requests, so the length of the inner vector is not
    /// guaranteed to be the same as the number of transactions.
    pub requests: Vec<Requests>,
}

impl<T> Default for ExecutionOutcome<T> {
    fn default() -> Self {
        Self {
            bundle: Default::default(),
            receipts: Default::default(),
            first_block: Default::default(),
            requests: Default::default(),
        }
    }
}

impl<T> ExecutionOutcome<T> {
    /// Creates a new `ExecutionOutcome`.
    ///
    /// This constructor initializes a new `ExecutionOutcome` instance with the provided
    /// bundle state, receipts, first block number, and EIP-7685 requests.
    pub const fn new(
        bundle: BundleState,
        receipts: Vec<Vec<T>>,
        first_block: BlockNumber,
        requests: Vec<Requests>,
    ) -> Self {
        Self { bundle, receipts, first_block, requests }
    }

    /// Creates a new `ExecutionOutcome` from initialization parameters.
    ///
    /// This constructor initializes a new `ExecutionOutcome` instance using detailed
    /// initialization parameters.
    pub fn new_init(
        state_init: BundleStateInit,
        revert_init: RevertsInit,
        contracts_init: impl IntoIterator<Item = (B256, Bytecode)>,
        receipts: Vec<Vec<T>>,
        first_block: BlockNumber,
        requests: Vec<Requests>,
    ) -> Self {
        // sort reverts by block number
        let mut reverts = revert_init.into_iter().collect::<Vec<_>>();
        reverts.sort_unstable_by_key(|a| a.0);

        // initialize revm bundle
        let bundle = BundleState::new(
            state_init.into_iter().map(|(address, (original, present, storage))| {
                (
                    address,
                    original.map(Into::into),
                    present.map(Into::into),
                    storage.into_iter().map(|(k, s)| (k.into(), s)).collect(),
                )
            }),
            reverts.into_iter().map(|(_, reverts)| {
                // does not need to be sorted, it is done when taking reverts.
                reverts.into_iter().map(|(address, (original, storage))| {
                    (
                        address,
                        original.map(|i| i.map(Into::into)),
                        storage.into_iter().map(|entry| (entry.key.into(), entry.value)),
                    )
                })
            }),
            contracts_init.into_iter().map(|(code_hash, bytecode)| (code_hash, bytecode.0)),
        );

        Self { bundle, receipts, first_block, requests }
    }

    /// Creates a new `ExecutionOutcome` from a single block execution result.
    pub fn single(block_number: u64, output: BlockExecutionOutput<T>) -> Self {
        Self {
            bundle: output.state,
            receipts: vec![output.result.receipts],
            first_block: block_number,
            requests: vec![output.result.requests],
        }
    }

    /// Creates a new `ExecutionOutcome` from multiple [`BlockExecutionResult`]s.
    pub fn from_blocks(
        first_block: u64,
        bundle: BundleState,
        results: Vec<BlockExecutionResult<T>>,
    ) -> Self {
        let mut value = Self {
            bundle,
            first_block,
            receipts: Vec::with_capacity(results.len()),
            requests: Vec::with_capacity(results.len()),
        };
        for result in results {
            value.receipts.push(result.receipts);
            value.requests.push(result.requests);
        }
        value
    }

    /// Return revm bundle state.
    pub const fn state(&self) -> &BundleState {
        &self.bundle
    }

    /// Returns mutable revm bundle state.
    pub const fn state_mut(&mut self) -> &mut BundleState {
        &mut self.bundle
    }

    /// Set first block.
    pub const fn set_first_block(&mut self, first_block: BlockNumber) {
        self.first_block = first_block;
    }

    /// Return iterator over all accounts
    pub fn accounts_iter(&self) -> impl Iterator<Item = (Address, Option<&AccountInfo>)> {
        self.bundle.state().iter().map(|(a, acc)| (*a, acc.info.as_ref()))
    }

    /// Return iterator over all [`BundleAccount`]s in the bundle
    pub fn bundle_accounts_iter(&self) -> impl Iterator<Item = (Address, &BundleAccount)> {
        self.bundle.state().iter().map(|(a, acc)| (*a, acc))
    }

    /// Get account if account is known.
    pub fn account(&self, address: &Address) -> Option<Option<Account>> {
        self.bundle.account(address).map(|a| a.info.as_ref().map(Into::into))
    }

    /// Returns the state [`BundleAccount`] for the given account.
    pub fn account_state(&self, address: &Address) -> Option<&BundleAccount> {
        self.bundle.account(address)
    }

    /// Get storage if value is known.
    ///
    /// This means that depending on status we can potentially return `U256::ZERO`.
    pub fn storage(&self, address: &Address, storage_key: U256) -> Option<U256> {
        self.bundle.account(address).and_then(|a| a.storage_slot(storage_key))
    }

    /// Return bytecode if known.
    pub fn bytecode(&self, code_hash: &B256) -> Option<Bytecode> {
        self.bundle.bytecode(code_hash).map(Bytecode)
    }

    /// Returns [`HashedPostState`] for this execution outcome.
    /// See [`HashedPostState::from_bundle_state`] for more info.
    pub fn hash_state_slow<KH: KeyHasher>(&self) -> HashedPostState {
        HashedPostState::from_bundle_state::<KH>(&self.bundle.state)
    }

    /// Transform block number to the index of block.
    pub const fn block_number_to_index(&self, block_number: BlockNumber) -> Option<usize> {
        if self.first_block > block_number {
            return None;
        }
        let index = block_number - self.first_block;
        if index >= self.receipts.len() as u64 {
            return None;
        }
        Some(index as usize)
    }

    /// Returns the receipt root for all recorded receipts.
    /// Note: this function calculated Bloom filters for every receipt and created merkle trees
    /// of receipt. This is an expensive operation.
    pub fn generic_receipts_root_slow(
        &self,
        block_number: BlockNumber,
        f: impl FnOnce(&[T]) -> B256,
    ) -> Option<B256> {
        Some(f(self.receipts.get(self.block_number_to_index(block_number)?)?))
    }

    /// Returns reference to receipts.
    pub const fn receipts(&self) -> &Vec<Vec<T>> {
        &self.receipts
    }

    /// Returns mutable reference to receipts.
    pub const fn receipts_mut(&mut self) -> &mut Vec<Vec<T>> {
        &mut self.receipts
    }

    /// Return all block receipts
    pub fn receipts_by_block(&self, block_number: BlockNumber) -> &[T] {
        let Some(index) = self.block_number_to_index(block_number) else { return &[] };
        &self.receipts[index]
    }

    /// Returns an iterator over receipt slices, one per block.
    ///
    /// This is a more ergonomic alternative to `receipts()` that yields slices
    /// instead of requiring indexing into a nested `Vec<Vec<T>>`.
    pub fn receipts_iter(&self) -> impl Iterator<Item = &[T]> + '_ {
        self.receipts.iter().map(|v| v.as_slice())
    }

    /// Is execution outcome empty.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of blocks in the execution outcome.
    pub const fn len(&self) -> usize {
        self.receipts.len()
    }

    /// Return first block of the execution outcome
    pub const fn first_block(&self) -> BlockNumber {
        self.first_block
    }

    /// Return last block of the execution outcome
    pub const fn last_block(&self) -> BlockNumber {
        (self.first_block + self.len() as u64).saturating_sub(1)
    }

    /// Revert the state to the given block number.
    ///
    /// Returns false if the block number is not in the bundle state.
    ///
    /// # Note
    ///
    /// The provided block number will stay inside the bundle state.
    pub fn revert_to(&mut self, block_number: BlockNumber) -> bool {
        let Some(index) = self.block_number_to_index(block_number) else { return false };

        // +1 is for number of blocks that we have as index is included.
        let new_len = index + 1;
        let rm_trx: usize = self.len() - new_len;

        // remove receipts
        self.receipts.truncate(new_len);
        // remove requests
        self.requests.truncate(new_len);
        // Revert last n reverts.
        self.bundle.revert(rm_trx);

        true
    }

    /// Splits the block range state at a given block number.
    /// Returns two split states ([..at], [at..]).
    /// The plain state of the 2nd bundle state will contain extra changes
    /// that were made in state transitions belonging to the lower state.
    ///
    /// # Panics
    ///
    /// If the target block number is not included in the state block range.
    pub fn split_at(self, at: BlockNumber) -> (Option<Self>, Self)
    where
        T: Clone,
    {
        if at == self.first_block {
            return (None, self);
        }

        let (mut lower_state, mut higher_state) = (self.clone(), self);

        // Revert lower state to [..at].
        lower_state.revert_to(at.checked_sub(1).unwrap());

        // Truncate higher state to [at..].
        let at_idx = higher_state.block_number_to_index(at).unwrap();
        higher_state.receipts = higher_state.receipts.split_off(at_idx);
        // Ensure that there are enough requests to truncate.
        // Sometimes we just have receipts and no requests.
        if at_idx < higher_state.requests.len() {
            higher_state.requests = higher_state.requests.split_off(at_idx);
        }
        higher_state.bundle.take_n_reverts(at_idx);
        higher_state.first_block = at;

        (Some(lower_state), higher_state)
    }

    /// Extend one state from another
    ///
    /// For state this is very sensitive operation and should be used only when
    /// we know that other state was build on top of this one.
    /// In most cases this would be true.
    pub fn extend(&mut self, other: Self) {
        self.bundle.extend(other.bundle);
        self.receipts.extend(other.receipts);
        self.requests.extend(other.requests);
    }

    /// Prepends present the state with the given `BundleState`.
    /// It adds changes from the given state but does not override any existing changes.
    ///
    /// Reverts and receipts are not updated.
    pub fn prepend_state(&mut self, mut other: BundleState) {
        let other_len = other.reverts.len();
        // take this bundle
        let this_bundle = core::mem::take(&mut self.bundle);
        // extend other bundle with this
        other.extend(this_bundle);
        // discard other reverts
        other.take_n_reverts(other_len);
        // swap bundles
        core::mem::swap(&mut self.bundle, &mut other)
    }

    /// Create a new instance with updated receipts.
    pub fn with_receipts(mut self, receipts: Vec<Vec<T>>) -> Self {
        self.receipts = receipts;
        self
    }

    /// Create a new instance with updated requests.
    pub fn with_requests(mut self, requests: Vec<Requests>) -> Self {
        self.requests = requests;
        self
    }

    /// Returns an iterator over all changed accounts from the `ExecutionOutcome`.
    ///
    /// This method filters the accounts to return only those that have undergone changes
    /// and maps them into `ChangedAccount` instances, which include the address, nonce, and
    /// balance.
    pub fn changed_accounts(&self) -> impl Iterator<Item = ChangedAccount> + '_ {
        self.accounts_iter().filter_map(|(addr, acc)| acc.map(|acc| (addr, acc))).map(
            |(address, acc)| ChangedAccount { address, nonce: acc.nonce, balance: acc.balance },
        )
    }
}

impl<T: Receipt<Log = Log>> ExecutionOutcome<T> {
    /// Returns an iterator over all block logs.
    pub fn logs(&self, block_number: BlockNumber) -> Option<impl Iterator<Item = &Log>> {
        let index = self.block_number_to_index(block_number)?;
        Some(self.receipts[index].iter().flat_map(|r| r.logs()))
    }

    /// Return blocks logs bloom
    pub fn block_logs_bloom(&self, block_number: BlockNumber) -> Option<Bloom> {
        Some(logs_bloom(self.logs(block_number)?))
    }
}

impl ExecutionOutcome {
    /// Returns the ethereum receipt root for all recorded receipts.
    ///
    /// Note: this function calculated Bloom filters for every receipt and created merkle trees
    /// of receipt. This is an expensive operation.
    pub fn ethereum_receipts_root(&self, block_number: BlockNumber) -> Option<B256> {
        self.generic_receipts_root_slow(
            block_number,
            smartnet_ethereum_primitives::calculate_receipt_root_no_memo,
        )
    }
}

impl<T> From<(BlockExecutionOutput<T>, BlockNumber)> for ExecutionOutcome<T> {
    fn from((output, block_number): (BlockExecutionOutput<T>, BlockNumber)) -> Self {
        Self::single(block_number, output)
    }
}

#[cfg(feature = "serde-bincode-compat")]
pub(super) mod serde_bincode_compat {
    use alloc::{borrow::Cow, vec::Vec};
    use alloy_eips::eip7685::Requests;
    use alloy_primitives::{BlockNumber, Bytes};
    use smartnet_primitives_traits::Receipt;
    use revm::database::BundleState;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_with::{DeserializeAs, SerializeAs};

    /// Bincode-compatible [`super::ExecutionOutcome`] serde implementation.
    ///
    /// Intended to use with the [`serde_with::serde_as`] macro in the following way:
    /// ```rust
    /// use smartnet_execution_types::{serde_bincode_compat, ExecutionOutcome};
    /// use serde::{Deserialize, Serialize};
    /// use serde_with::serde_as;
    ///
    /// #[serde_as]
    /// #[derive(Serialize, Deserialize)]
    /// struct Data {
    ///     #[serde_as(as = "serde_bincode_compat::ExecutionOutcome<'_>")]
    ///     chain: ExecutionOutcome,
    /// }
    /// ```
    #[derive(Debug, Serialize, Deserialize)]
    pub struct ExecutionOutcome<'a> {
        bundle: Cow<'a, BundleState>,
        receipts: Vec<Vec<Bytes>>,
        first_block: BlockNumber,
        #[expect(clippy::owned_cow)]
        requests: Cow<'a, Vec<Requests>>,
    }

    impl<'a, T> From<&'a super::ExecutionOutcome<T>> for ExecutionOutcome<'a>
    where
        T: Receipt,
    {
        fn from(value: &'a super::ExecutionOutcome<T>) -> Self {
            ExecutionOutcome {
                bundle: Cow::Borrowed(&value.bundle),
                receipts: value
                    .receipts
                    .iter()
                    .map(|vec| {
                        vec.iter().map(|receipt| Bytes::from(alloy_rlp::encode(receipt))).collect()
                    })
                    .collect(),
                first_block: value.first_block,
                requests: Cow::Borrowed(&value.requests),
            }
        }
    }

    impl<T> From<ExecutionOutcome<'_>> for super::ExecutionOutcome<T>
    where
        T: Receipt,
    {
        fn from(value: ExecutionOutcome<'_>) -> Self {
            Self {
                bundle: value.bundle.into_owned(),
                receipts: value
                    .receipts
                    .into_iter()
                    .map(|vec| {
                        vec.into_iter()
                            .map(|rlp| {
                                T::decode(&mut rlp.as_ref())
                                    .expect("invalid RLP for receipt in serde_bincode_compat")
                            })
                            .collect()
                    })
                    .collect(),
                first_block: value.first_block,
                requests: value.requests.into_owned(),
            }
        }
    }

    impl<T> SerializeAs<super::ExecutionOutcome<T>> for ExecutionOutcome<'_>
    where
        T: Receipt,
    {
        fn serialize_as<S>(
            source: &super::ExecutionOutcome<T>,
            serializer: S,
        ) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            ExecutionOutcome::from(source).serialize(serializer)
        }
    }

    impl<'de, T> DeserializeAs<'de, super::ExecutionOutcome<T>> for ExecutionOutcome<'de>
    where
        T: Receipt,
    {
        fn deserialize_as<D>(deserializer: D) -> Result<super::ExecutionOutcome<T>, D::Error>
        where
            D: Deserializer<'de>,
        {
            ExecutionOutcome::deserialize(deserializer).map(Into::into)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::TxType;
    use alloy_primitives::{bytes, Address};

    #[test]
    fn test_initialization() {
        // Create a new BundleState object with initial data
        let bundle = BundleState::new(
            vec![(Address::new([2; 20]), None, Some(AccountInfo::default()), HashMap::default())],
            vec![vec![(Address::new([2; 20]), None, vec![])]],
            vec![],
        );

        // Create a Receipts object with a vector of receipt vectors
        let receipts = vec![vec![Some(smartnet_ethereum_primitives::Receipt {
            tx_type: TxType::Legacy,
            cumulative_gas_used: 46913,
            logs: vec![],
            success: true,
        })]];

        // Create a Requests object with a vector of requests
        let requests = vec![Requests::new(vec![bytes!("dead"), bytes!("beef"), bytes!("beebee")])];

        // Define the first block number
        let first_block = 123;

        // Create a ExecutionOutcome object with the created bundle, receipts, requests, and
        // first_block
        let exec_res = ExecutionOutcome {
            bundle: bundle.clone(),
            receipts: receipts.clone(),
            requests: requests.clone(),
            first_block,
        };

        // Assert that creating a new ExecutionOutcome using the constructor matches exec_res
        assert_eq!(
            ExecutionOutcome::new(bundle, receipts.clone(), first_block, requests.clone()),
            exec_res
        );

        // Create a BundleStateInit object and insert initial data
        let mut state_init: BundleStateInit = AddressMap::default();
        state_init
            .insert(Address::new([2; 20]), (None, Some(Account::default()), B256Map::default()));

        // Create an AddressMap for account reverts and insert initial data
        let mut revert_inner: AddressMap<AccountRevertInit> = AddressMap::default();
        revert_inner.insert(Address::new([2; 20]), (None, vec![]));

        // Create a RevertsInit object and insert the revert_inner data
        let mut revert_init: RevertsInit = HashMap::default();
        revert_init.insert(123, revert_inner);

        // Assert that creating a new ExecutionOutcome using the new_init method matches
        // exec_res
        assert_eq!(
            ExecutionOutcome::new_init(
                state_init,
                revert_init,
                vec![],
                receipts,
                first_block,
                requests,
            ),
            exec_res
        );
    }
}

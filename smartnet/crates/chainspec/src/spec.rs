pub use alloy_eips::eip1559::BaseFeeParams;
use alloy_evm::eth::spec::EthExecutorSpec;

use crate::{
    constants::{MAINNET_DEPOSIT_CONTRACT, MAINNET_PRUNE_DELETE_LIMIT},
    EthChainSpec,
};
use alloc::{boxed::Box, format, sync::Arc, vec::Vec};
use alloy_chains::Chain;
use alloy_consensus::{Header, SignableTransaction, TxLegacy};
use alloy_eips::{eip1559::INITIAL_BASE_FEE, eip7892::BlobScheduleBlobParams};
use alloy_genesis::Genesis;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, BlockNumber, B256, Bytes, TxKind, U256};
use alloy_primitives::Signature;
use alloy_trie::root::state_root_ref_unhashed;
use core::fmt::Debug;
use derive_more::From;
use smartnet_ethereum_forks::{
    ChainHardforks, DisplayHardforks, EthereumHardfork, EthereumHardforks, ForkCondition,
    ForkFilter, ForkFilterKey, ForkHash, ForkId, Hardfork, Hardforks, Head, DEV_HARDFORKS,
};
use smartnet_ethereum_primitives::TransactionSigned;
use smartnet_primitives_traits::{
    proofs::ordered_trie_root_with_encoder, sync::LazyLock, BlockHeader, SealedHeader,
};

/// Helper method building a [`Header`] given [`Genesis`] and [`ChainHardforks`].
pub fn make_genesis_header(genesis: &Genesis, hardforks: &ChainHardforks) -> Header {
    // If London is activated at genesis, we set the initial base fee as per EIP-1559.
    let base_fee_per_gas = hardforks
        .fork(EthereumHardfork::London)
        .active_at_block(0)
        .then(|| genesis.base_fee_per_gas.map(|fee| fee as u64).unwrap_or(INITIAL_BASE_FEE));

    // Calibri* pre-fund records (GENESIS.md): the alloc entries as REAL block-0
    // transactions — `from = 0x0` (zero signature: the records are listings of the
    // genesis allocation, never executed, never metered — the alloc is the single
    // source of truth for opening balances, no doubles), nonce = index, deterministic
    // by the alloc's address sort. Clique genesis: NO records (byte-perfect legacy).
    let records = genesis_pre_fund_records(
        genesis,
        genesis.config.chain_id,
        genesis.config.clique.is_none(),
    );

    // WEM: no Shanghai/Cancun/Prague/Amsterdam, so the header never carries a withdrawals
    // root, blob gas fields, parent beacon block root, requests hash, or access list hash
    // (all left as `None` via the struct default).
    Header {
        number: genesis.number.unwrap_or_default(),
        parent_hash: genesis.parent_hash.unwrap_or_default(),
        gas_limit: genesis.gas_limit,
        difficulty: genesis.difficulty,
        nonce: genesis.nonce.into(),
        extra_data: genesis.extra_data.clone(),
        state_root: state_root_ref_unhashed(&genesis.alloc),
        timestamp: genesis.timestamp,
        mix_hash: genesis.mix_hash,
        beneficiary: genesis.coinbase,
        base_fee_per_gas,
        transactions_root: ordered_trie_root_with_encoder(&records, |tx, buf| {
            tx.encode_2718(buf)
        }),
        ..Default::default()
    }
}

/// The calibri\* genesis pre-fund records (GENESIS.md): one legacy tx per non-zero
/// alloc entry — `from = 0x0` with the zero signature (EIP-155 encoding, `v` derived
/// from the chain id, `r = s = 0`), `nonce = index`, `to/value` = the alloc entry.
/// Deterministic (the alloc is address-sorted) so every node derives the identical
/// block 0 and genesis hash. Empty for legacy clique genesis (byte-perfect legacy).
fn genesis_pre_fund_records(
    genesis: &Genesis,
    chain_id: u64,
    is_calibri: bool,
) -> Vec<TransactionSigned> {
    if !is_calibri {
        return Vec::new();
    }
    genesis
        .alloc
        .iter()
        .filter(|(_, account)| !account.balance.is_zero())
        .enumerate()
        .map(|(idx, (address, account))| {
            let inner = TxLegacy {
                chain_id: Some(chain_id),
                nonce: idx as u64,
                gas_price: 0,
                gas_limit: 0,
                to: TxKind::Call(*address),
                value: account.balance,
                input: Bytes::new(),
            };
            // The zero signature: the record is a genesis listing, never a pool tx —
            // reconstruction trusts the stored hash and signature bytes (the file-tier
            // path never recovers senders).
            inner.into_signed(Signature::new(U256::ZERO, U256::ZERO, false))
        })
        .map(TransactionSigned::from)
        .collect()
}

/// Default chain spec for this fork (not Ethereum L1 mainnet).
///
/// Genesis state comes from `fork_default.json` (same shape as the dev template, but not
/// `dev.json`); the chain id comes from that genesis file's `config.chainId` — the genesis is the
/// single source of truth (no hard-coded chain id in code). `dev.json` is reserved for
/// [`dev_chain_spec`] (`--dev` only).
pub static MAINNET: LazyLock<Arc<ChainSpec>> = LazyLock::new(|| {
    let genesis: Genesis = serde_json::from_str(include_str!("../res/genesis/fork_default.json"))
        .expect("Can't deserialize fork default genesis (fork_default.json)");
    // Exactly what the genesis file declares (chain 315, its own fork set) — no
    // Ethereum-mainnet schedule, merge point, deposit contract, blob schedule, or DAO fork.
    Arc::new(ChainSpec::from(genesis))
});

/// Dev testnet specification (loads `dev.json` on first use).
///
/// Includes 20 prefunded accounts with `10_000` ETH each derived from mnemonic "test test test test
/// test test test test test test test junk".
///
/// Call this from tests or programmatic chain construction. The CLI rejects `--chain dev`
/// at startup; use a genesis JSON path with `--chain <path>` and an external validator.
pub fn dev_chain_spec() -> Arc<ChainSpec> {
    static DEV: LazyLock<Arc<ChainSpec>> = LazyLock::new(|| {
        let genesis = serde_json::from_str(include_str!("../res/genesis/dev.json"))
            .expect("Can't deserialize Dev testnet genesis json");
        let hardforks = DEV_HARDFORKS.clone();
        ChainSpec {
            chain: Chain::dev(),
            genesis_header: SealedHeader::seal_slow(make_genesis_header(&genesis, &hardforks)),
            genesis,
            paris_block_and_final_difficulty: Some((0, U256::from(0))),
            hardforks,
            base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::ethereum()),
            deposit_contract: None, // TODO: do we even have?
            ..Default::default()
        }
        .into()
    });
    DEV.clone()
}

/// A wrapper around [`BaseFeeParams`] that allows for specifying constant or dynamic EIP-1559
/// parameters based on the active [Hardfork].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaseFeeParamsKind {
    /// Constant [`BaseFeeParams`]; used for chains that don't have dynamic EIP-1559 parameters
    Constant(BaseFeeParams),
    /// Variable [`BaseFeeParams`]; used for chains that have dynamic EIP-1559 parameters like
    /// Optimism
    Variable(ForkBaseFeeParams),
}

impl Default for BaseFeeParamsKind {
    fn default() -> Self {
        BaseFeeParams::ethereum().into()
    }
}

impl From<BaseFeeParams> for BaseFeeParamsKind {
    fn from(params: BaseFeeParams) -> Self {
        Self::Constant(params)
    }
}

impl From<ForkBaseFeeParams> for BaseFeeParamsKind {
    fn from(params: ForkBaseFeeParams) -> Self {
        Self::Variable(params)
    }
}

/// A type alias to a vector of tuples of [Hardfork] and [`BaseFeeParams`], sorted by [Hardfork]
/// activation order. This is used to specify dynamic EIP-1559 parameters for chains like Optimism.
#[derive(Clone, Debug, PartialEq, Eq, From)]
pub struct ForkBaseFeeParams(Vec<(Box<dyn Hardfork>, BaseFeeParams)>);

impl<H: BlockHeader> core::ops::Deref for ChainSpec<H> {
    type Target = ChainHardforks;

    fn deref(&self) -> &Self::Target {
        &self.hardforks
    }
}

/// An Ethereum chain specification.
///
/// A chain specification describes:
///
/// - Meta-information about the chain (the chain ID)
/// - The genesis block of the chain ([`Genesis`])
/// - What hardforks are activated, and under which conditions
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainSpec<H: BlockHeader = Header> {
    /// The chain ID
    pub chain: Chain,

    /// The genesis block.
    pub genesis: Genesis,

    /// The header corresponding to the genesis block.
    pub genesis_header: SealedHeader<H>,

    /// The block at which [`EthereumHardfork::Paris`] was activated and the final difficulty at
    /// this block.
    pub paris_block_and_final_difficulty: Option<(u64, U256)>,

    /// The active hard forks and their activation conditions
    pub hardforks: ChainHardforks,

    /// The deposit contract deployed for `PoS`
    pub deposit_contract: Option<DepositContract>,

    /// The parameters that configure how a block's base fee is computed
    pub base_fee_params: BaseFeeParamsKind,

    /// The delete limit for pruner, per run.
    pub prune_delete_limit: usize,

    /// The settings passed for blob configurations for specific hardforks.
    pub blob_params: BlobScheduleBlobParams,
}

impl<H: BlockHeader> Default for ChainSpec<H> {
    fn default() -> Self {
        Self {
            chain: Default::default(),
            genesis: Default::default(),
            genesis_header: Default::default(),
            paris_block_and_final_difficulty: Default::default(),
            hardforks: Default::default(),
            deposit_contract: Default::default(),
            base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::ethereum()),
            prune_delete_limit: MAINNET_PRUNE_DELETE_LIMIT,
            blob_params: Default::default(),
        }
    }
}

impl ChainSpec {
    /// Converts the given [`Genesis`] into a [`ChainSpec`].
    pub fn from_genesis(genesis: Genesis) -> Self {
        genesis.into()
    }

    /// Build a chainspec using [`ChainSpecBuilder`]
    pub fn builder() -> ChainSpecBuilder {
        ChainSpecBuilder::default()
    }
}

impl<H: BlockHeader> ChainSpec<H> {
    /// Get information about the chain itself
    pub const fn chain(&self) -> Chain {
        self.chain
    }

    /// Returns `true` if this chain contains Ethereum configuration.
    #[inline]
    pub const fn is_ethereum(&self) -> bool {
        self.chain.is_ethereum()
    }

    /// Returns `true` if this chain is Optimism mainnet.
    #[inline]
    pub fn is_optimism_mainnet(&self) -> bool {
        self.chain == Chain::optimism_mainnet()
    }

    /// Returns the known paris block, if it exists.
    #[inline]
    pub fn paris_block(&self) -> Option<u64> {
        self.paris_block_and_final_difficulty.map(|(block, _)| block)
    }

    /// Get the genesis block specification.
    ///
    /// To get the header for the genesis block, use [`Self::genesis_header`] instead.
    pub const fn genesis(&self) -> &Genesis {
        &self.genesis
    }

    /// Get the header for the genesis block.
    pub fn genesis_header(&self) -> &H {
        &self.genesis_header
    }

    /// Get the sealed header for the genesis block.
    pub fn sealed_genesis_header(&self) -> SealedHeader<H> {
        SealedHeader::new(self.genesis_header().clone(), self.genesis_hash())
    }

    /// Get the initial base fee of the genesis block.
    pub fn initial_base_fee(&self) -> Option<u64> {
        // If the base fee is set in the genesis block, we use that instead of the default.
        let genesis_base_fee =
            self.genesis.base_fee_per_gas.map(|fee| fee as u64).unwrap_or(INITIAL_BASE_FEE);

        // If London is activated at genesis, we set the initial base fee as per EIP-1559.
        self.hardforks.fork(EthereumHardfork::London).active_at_block(0).then_some(genesis_base_fee)
    }

    /// Get the [`BaseFeeParams`] for the chain at the given timestamp.
    pub fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams {
        match self.base_fee_params {
            BaseFeeParamsKind::Constant(bf_params) => bf_params,
            BaseFeeParamsKind::Variable(ForkBaseFeeParams(ref bf_params)) => {
                // Walk through the base fee params configuration in reverse order, and return the
                // first one that corresponds to a hardfork that is active at the
                // given timestamp.
                for (fork, params) in bf_params.iter().rev() {
                    if self.hardforks.is_fork_active_at_timestamp(fork.clone(), timestamp) {
                        return *params;
                    }
                }

                bf_params.first().map(|(_, params)| *params).unwrap_or_else(BaseFeeParams::ethereum)
            }
        }
    }

    /// Get the hash of the genesis block.
    pub fn genesis_hash(&self) -> B256 {
        self.genesis_header.hash()
    }

    /// The calibri\* genesis pre-fund records (GENESIS.md): the alloc entries as real
    /// block-0 tx listings (`from = 0x0`, zero signature, never executed, never
    /// metered — the alloc is the single source of truth for opening balances).
    /// Empty for legacy clique genesis.
    pub fn genesis_tx_records(&self) -> Vec<TransactionSigned> {
        genesis_pre_fund_records(
            &self.genesis,
            self.chain_id(),
            self.genesis.config.clique.is_none(),
        )
    }

    /// Get the timestamp of the genesis block.
    pub const fn genesis_timestamp(&self) -> u64 {
        self.genesis.timestamp
    }

    /// Returns the final total difficulty if the Paris hardfork is known.
    pub fn get_final_paris_total_difficulty(&self) -> Option<U256> {
        self.paris_block_and_final_difficulty.map(|(_, final_difficulty)| final_difficulty)
    }

    /// Get the fork filter for the given hardfork
    pub fn hardfork_fork_filter<HF: Hardfork + Clone>(&self, fork: HF) -> Option<ForkFilter> {
        match self.hardforks.fork(fork.clone()) {
            ForkCondition::Never => None,
            _ => Some(self.fork_filter(self.satisfy(self.hardforks.fork(fork)))),
        }
    }

    /// Returns the hardfork display helper.
    pub fn display_hardforks(&self) -> DisplayHardforks {
        // Create an iterator with hardfork, condition, and optional blob metadata
        let hardforks_with_meta = self.hardforks.forks_iter().map(|(fork, condition)| {
            // Generate blob metadata for timestamp-based hardforks that have blob params
            let metadata = match condition {
                ForkCondition::Timestamp(timestamp) => {
                    // Try to get blob params for this timestamp
                    // This automatically handles all hardforks with blob support
                    EthChainSpec::blob_params_at_timestamp(self, timestamp).map(|params| {
                        format!(
                            "blob: (target: {}, max: {}, fraction: {})",
                            params.target_blob_count, params.max_blob_count, params.update_fraction
                        )
                    })
                }
                _ => None,
            };
            (fork, condition, metadata)
        });

        DisplayHardforks::with_meta(hardforks_with_meta)
    }

    /// Get the fork id for the given hardfork.
    #[inline]
    pub fn hardfork_fork_id<HF: Hardfork + Clone>(&self, fork: HF) -> Option<ForkId> {
        let condition = self.hardforks.fork(fork);
        match condition {
            ForkCondition::Never => None,
            _ => Some(self.fork_id(&self.satisfy(condition))),
        }
    }

    /// Convenience method to get the latest fork id from the chainspec. Panics if chainspec has no
    /// hardforks.
    #[inline]
    pub fn latest_fork_id(&self) -> ForkId {
        self.hardfork_fork_id(self.hardforks.last().unwrap().0).unwrap()
    }

    /// Creates a [`ForkFilter`] for the block described by [Head].
    pub fn fork_filter(&self, head: Head) -> ForkFilter {
        let forks = self.hardforks.forks_iter().filter_map(|(_, condition)| {
            // We filter out TTD-based forks w/o a pre-known block since those do not show up in
            // the fork filter.
            Some(match condition {
                ForkCondition::Block(block) |
                ForkCondition::TTD { fork_block: Some(block), .. } => ForkFilterKey::Block(block),
                ForkCondition::Timestamp(time) => ForkFilterKey::Time(time),
                _ => return None,
            })
        });

        ForkFilter::new(head, self.genesis_hash(), self.genesis_timestamp(), forks)
    }

    /// Compute the [`ForkId`] for the given [`Head`] following eip-6122 spec.
    ///
    /// The fork hash is computed by starting from the genesis hash and iteratively adding
    /// block numbers (for block-based forks) or timestamps (for timestamp-based forks) of
    /// active forks. The `next` field indicates the next fork activation point, or `0` if
    /// all forks are active.
    ///
    /// Block-based forks are processed first, then timestamp-based forks. Multiple hardforks
    /// activated at the same block or timestamp: only the first one is applied.
    ///
    /// See: <https://eips.ethereum.org/EIPS/eip-6122>
    pub fn fork_id(&self, head: &Head) -> ForkId {
        let mut forkhash = ForkHash::from(self.genesis_hash());

        // this tracks the last applied block or timestamp fork. This is necessary for optimism,
        // because for the optimism hardforks both the optimism and the corresponding ethereum
        // hardfork can be configured in `ChainHardforks` if it enables ethereum equivalent
        // functionality (e.g. additional header,body fields) This is set to 0 so that all
        // block based hardforks are skipped in the following loop
        let mut current_applied = 0;

        // handle all block forks before handling timestamp based forks. see: https://eips.ethereum.org/EIPS/eip-6122
        for (_, cond) in self.hardforks.forks_iter() {
            // handle block based forks, including TTD forks that also pin an activation block
            if let ForkCondition::Block(block) |
            ForkCondition::TTD { fork_block: Some(block), .. } = cond
            {
                if head.number >= block {
                    // skip duplicated hardforks: hardforks enabled at genesis block
                    if block != current_applied {
                        forkhash += block;
                        current_applied = block;
                    }
                } else {
                    // we can return here because this block fork is not active, so we set the
                    // `next` value
                    return ForkId { hash: forkhash, next: block };
                }
            }
        }

        // timestamp are ALWAYS applied after the merge.
        //
        // this filter ensures that no block-based forks are returned
        for timestamp in self.hardforks.forks_iter().filter_map(|(_, cond)| {
            // ensure we only get timestamp forks activated __after__ the genesis block
            cond.as_timestamp().filter(|time| time > &self.genesis.timestamp)
        }) {
            if head.timestamp >= timestamp {
                // skip duplicated hardfork activated at the same timestamp
                if timestamp != current_applied {
                    forkhash += timestamp;
                    current_applied = timestamp;
                }
            } else {
                // can safely return here because we have already handled all block forks and
                // have handled all active timestamp forks, and set the next value to the
                // timestamp that is known but not active yet
                return ForkId { hash: forkhash, next: timestamp };
            }
        }

        ForkId { hash: forkhash, next: 0 }
    }

    /// An internal helper function that returns a head block that satisfies a given Fork condition.
    ///
    /// Creates a [`Head`] representation for a fork activation point, used by [`Self::fork_id`] to
    /// compute fork IDs. For timestamp-based forks, includes the last block-based fork number
    /// before the merge (if any).
    pub(crate) fn satisfy(&self, cond: ForkCondition) -> Head {
        match cond {
            ForkCondition::Block(number) => Head { number, ..Default::default() },
            ForkCondition::Timestamp(timestamp) => {
                // to satisfy every timestamp ForkCondition, we find the last ForkCondition::Block
                // if one exists, and include its block_num in the returned Head
                Head {
                    timestamp,
                    number: self.last_block_fork_before_merge_or_timestamp().unwrap_or_default(),
                    ..Default::default()
                }
            }
            ForkCondition::TTD { total_difficulty, fork_block, .. } => Head {
                total_difficulty,
                number: fork_block.unwrap_or_default(),
                ..Default::default()
            },
            ForkCondition::Never => unreachable!(),
        }
    }

    /// This internal helper function retrieves the block number of the last block-based fork
    /// that occurs before:
    /// - Any existing Total Terminal Difficulty (TTD) or
    /// - Timestamp-based forks in the current [`ChainSpec`].
    ///
    /// The function operates by examining the configured hard forks in the chain. It iterates
    /// through the fork conditions and identifies the most recent block-based fork that
    /// precedes any TTD or timestamp-based conditions.
    ///
    /// If there are no block-based forks found before these conditions, or if the [`ChainSpec`]
    /// is not configured with a TTD or timestamp fork, this function will return `None`.
    pub(crate) fn last_block_fork_before_merge_or_timestamp(&self) -> Option<u64> {
        let mut hardforks_iter = self.hardforks.forks_iter().peekable();
        while let Some((_, curr_cond)) = hardforks_iter.next() {
            if let Some((_, next_cond)) = hardforks_iter.peek() {
                // Match against the `next_cond` to see if it represents:
                // - A TTD (merge)
                // - A timestamp-based fork
                match next_cond {
                    // If the next fork is TTD and specifies a specific block, return that block
                    // number
                    ForkCondition::TTD { fork_block: Some(block), .. } => return Some(*block),

                    // If the next fork is TTD without a specific block or is timestamp-based,
                    // return the block number of the current condition if it is block-based.
                    ForkCondition::TTD { .. } | ForkCondition::Timestamp(_) => {
                        // Check if `curr_cond` is a block-based fork and return its block number if
                        // true.
                        if let ForkCondition::Block(block_num) = curr_cond {
                            return Some(block_num);
                        }
                    }
                    ForkCondition::Block(_) | ForkCondition::Never => {}
                }
            }
        }
        None
    }

    /// Convert header to another type.
    pub fn map_header<NewH: BlockHeader>(self, f: impl FnOnce(H) -> NewH) -> ChainSpec<NewH> {
        let Self {
            chain,
            genesis,
            genesis_header,
            paris_block_and_final_difficulty,
            hardforks,
            deposit_contract,
            base_fee_params,
            prune_delete_limit,
            blob_params,
        } = self;
        ChainSpec {
            chain,
            genesis,
            genesis_header: SealedHeader::new_unhashed(f(genesis_header.into_header())),
            paris_block_and_final_difficulty,
            hardforks,
            deposit_contract,
            base_fee_params,
            prune_delete_limit,
            blob_params,
        }
    }
}

impl From<Genesis> for ChainSpec {
    fn from(genesis: Genesis) -> Self {
        // MARS: **one schedule, uniform at every height, never read from a genesis**. The
        // supported pre-London fork ladder is active from block 0 (uniform EVM rules for
        // native/ERC-20/type-0x2/deFi execution); London-family block forks, Paris/TTD, and
        // every time-based fork stay off ("validate history exactly as produced" — headers
        // carry no `baseFeePerGas`; verified on chain). The genesis file's legacy
        // `*_Block`/`clique` fields are inert (GENESIS-LEGACY-CONFIG.md §4). The fixed-fee
        // era is activated ONLY through an explicit coordinated chainspec update (never from
        // a genesis file) — see doc/rlp/istanbul-london-header-rlp.md.
        let hardforks = ChainHardforks::new(mars_schedule_entries());

        // Extract blob parameters directly from blob_schedule
        let blob_params = genesis.config.blob_schedule_blob_params();

        // NOTE: in full node, we prune all receipts except the deposit contract's. We do not
        // have the deployment block in the genesis file, so we use block zero. We use the same
        // deposit topic as the mainnet contract if we have the deposit contract address in the
        // genesis json.
        let deposit_contract = genesis.config.deposit_contract_address.map(|address| {
            DepositContract { address, block: 0, topic: MAINNET_DEPOSIT_CONTRACT.topic }
        });

        Self {
            chain: genesis.config.chain_id.into(),
            genesis_header: SealedHeader::new_unhashed(make_genesis_header(&genesis, &hardforks)),
            genesis,
            hardforks,
            // Mars: no merge/Paris transition exists on this network.
            paris_block_and_final_difficulty: None,
            deposit_contract,
            blob_params,
            ..Default::default()
        }
    }
}

/// The ONE Mars schedule: every supported pre-London fork at block 0, uniform for all
/// heights. Not genesis-derived.
pub(crate) fn mars_schedule_entries() -> alloc::vec::Vec<(alloc::boxed::Box<dyn Hardfork>, ForkCondition)>
{
    alloc::vec![
        (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)),
    ]
}

/// Signer-sealed chains (`SmartNet` / WEM 315) pay **no block reward** — only transaction
/// fees. Returning `true` here makes the block executor skip
/// the beneficiary credit entirely, so an empty block leaves the state unchanged and the
/// state root matches. See `alloy_evm::eth::spec::DisableBlockRewards` and
/// `doc/istanbul-london-header-rlp.md`.
impl alloy_evm::eth::spec::DisableBlockRewards for ChainSpec {
    fn disable_block_rewards(&self) -> bool {
        self.genesis().config.clique.is_some()
    }
}

impl<H: BlockHeader> Hardforks for ChainSpec<H> {
    fn fork<HF: Hardfork>(&self, fork: HF) -> ForkCondition {
        self.hardforks.fork(fork)
    }

    fn forks_iter(&self) -> impl Iterator<Item = (&dyn Hardfork, ForkCondition)> {
        self.hardforks.forks_iter()
    }

    fn fork_id(&self, head: &Head) -> ForkId {
        self.fork_id(head)
    }

    fn latest_fork_id(&self) -> ForkId {
        self.latest_fork_id()
    }

    fn fork_filter(&self, head: Head) -> ForkFilter {
        self.fork_filter(head)
    }
}

impl<H: BlockHeader> EthereumHardforks for ChainSpec<H> {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.fork(fork)
    }
}

/// A trait for reading the current chainspec.
#[auto_impl::auto_impl(&, Arc)]
pub trait ChainSpecProvider: Debug + Send {
    /// The chain spec type.
    type ChainSpec: EthChainSpec + 'static;

    /// Get an [`Arc`] to the chainspec.
    fn chain_spec(&self) -> Arc<Self::ChainSpec>;
}

/// A helper to build custom chain specs
#[derive(Debug, Default, Clone)]
pub struct ChainSpecBuilder {
    chain: Option<Chain>,
    genesis: Option<Genesis>,
    hardforks: ChainHardforks,
}

impl ChainSpecBuilder {
    /// Construct a new builder from the mainnet chain spec.
    pub fn mainnet() -> Self {
        Self {
            chain: Some(MAINNET.chain),
            genesis: Some(MAINNET.genesis.clone()),
            hardforks: MAINNET.hardforks.clone(),
        }
    }
}

impl ChainSpecBuilder {
    /// Set the chain ID
    pub const fn chain(mut self, chain: Chain) -> Self {
        self.chain = Some(chain);
        self
    }

    /// Resets any existing hardforks from the builder.
    pub fn reset(mut self) -> Self {
        self.hardforks = ChainHardforks::default();
        self
    }

    /// Set the genesis block.
    pub fn genesis(mut self, genesis: Genesis) -> Self {
        self.genesis = Some(genesis);
        self
    }

    /// Add the given fork with the given activation condition to the spec.
    pub fn with_fork<H: Hardfork>(mut self, fork: H, condition: ForkCondition) -> Self {
        self.hardforks.insert(fork, condition);
        self
    }

    /// Add the given chain hardforks to the spec.
    pub fn with_forks(mut self, forks: ChainHardforks) -> Self {
        self.hardforks = forks;
        self
    }

    /// Remove the given fork from the spec.
    pub fn without_fork<H: Hardfork>(mut self, fork: H) -> Self {
        self.hardforks.remove(&fork);
        self
    }

    /// Enable the Paris hardfork at the given TTD.
    ///
    /// Does not set the merge netsplit block.
    pub fn paris_at_ttd(self, ttd: U256, activation_block_number: BlockNumber) -> Self {
        self.with_fork(
            EthereumHardfork::Paris,
            ForkCondition::TTD { activation_block_number, total_difficulty: ttd, fork_block: None },
        )
    }

    /// Enable Frontier at genesis.
    pub fn frontier_activated(mut self) -> Self {
        self.hardforks.insert(EthereumHardfork::Frontier, ForkCondition::Block(0));
        self
    }

    /// Enable Dao at genesis.
    pub fn dao_activated(mut self) -> Self {
        self = self.frontier_activated();
        self.hardforks.insert(EthereumHardfork::Dao, ForkCondition::Block(0));
        self
    }

    /// Enable Homestead at genesis.
    pub fn homestead_activated(mut self) -> Self {
        self = self.dao_activated();
        self.hardforks.insert(EthereumHardfork::Homestead, ForkCondition::Block(0));
        self
    }

    /// Enable Tangerine at genesis.
    pub fn tangerine_whistle_activated(mut self) -> Self {
        self = self.homestead_activated();
        self.hardforks.insert(EthereumHardfork::Tangerine, ForkCondition::Block(0));
        self
    }

    /// Enable Spurious Dragon at genesis.
    pub fn spurious_dragon_activated(mut self) -> Self {
        self = self.tangerine_whistle_activated();
        self.hardforks.insert(EthereumHardfork::SpuriousDragon, ForkCondition::Block(0));
        self
    }

    /// Enable Byzantium at genesis.
    pub fn byzantium_activated(mut self) -> Self {
        self = self.spurious_dragon_activated();
        self.hardforks.insert(EthereumHardfork::Byzantium, ForkCondition::Block(0));
        self
    }

    /// Enable Constantinople at genesis.
    pub fn constantinople_activated(mut self) -> Self {
        self = self.byzantium_activated();
        self.hardforks.insert(EthereumHardfork::Constantinople, ForkCondition::Block(0));
        self
    }

    /// Enable Petersburg at genesis.
    pub fn petersburg_activated(mut self) -> Self {
        self = self.constantinople_activated();
        self.hardforks.insert(EthereumHardfork::Petersburg, ForkCondition::Block(0));
        self
    }

    /// Enable Istanbul at genesis.
    pub fn istanbul_activated(mut self) -> Self {
        self = self.petersburg_activated();
        self.hardforks.insert(EthereumHardfork::Istanbul, ForkCondition::Block(0));
        self
    }

    /// Enable Muir Glacier at genesis.
    pub fn muirglacier_activated(mut self) -> Self {
        self = self.istanbul_activated();
        self.hardforks.insert(EthereumHardfork::MuirGlacier, ForkCondition::Block(0));
        self
    }

    /// Enable Berlin at genesis.
    pub fn berlin_activated(mut self) -> Self {
        self = self.muirglacier_activated();
        self.hardforks.insert(EthereumHardfork::Berlin, ForkCondition::Block(0));
        self
    }

    /// Enable London at genesis.
    pub fn london_activated(mut self) -> Self {
        self = self.berlin_activated();
        self.hardforks.insert(EthereumHardfork::London, ForkCondition::Block(0));
        self
    }

    /// Enable Arrow Glacier at genesis.
    pub fn arrowglacier_activated(mut self) -> Self {
        self = self.london_activated();
        self.hardforks.insert(EthereumHardfork::ArrowGlacier, ForkCondition::Block(0));
        self
    }

    /// Enable Gray Glacier at genesis.
    pub fn grayglacier_activated(mut self) -> Self {
        self = self.arrowglacier_activated();
        self.hardforks.insert(EthereumHardfork::GrayGlacier, ForkCondition::Block(0));
        self
    }

    /// Enable Paris at genesis.
    pub fn paris_activated(mut self) -> Self {
        self = self.grayglacier_activated();
        self.hardforks.insert(
            EthereumHardfork::Paris,
            ForkCondition::TTD {
                activation_block_number: 0,
                total_difficulty: U256::ZERO,
                fork_block: None,
            },
        );
        self
    }

    /// Build the resulting [`ChainSpec`].
    ///
    /// # Panics
    ///
    /// This function panics if the chain ID and genesis is not set ([`Self::chain`] and
    /// [`Self::genesis`])
    pub fn build(self) -> ChainSpec {
        let paris_block_and_final_difficulty = {
            self.hardforks.get(EthereumHardfork::Paris).and_then(|cond| {
                if let ForkCondition::TTD { total_difficulty, activation_block_number, .. } = cond {
                    Some((activation_block_number, total_difficulty))
                } else {
                    None
                }
            })
        };
        let genesis = self.genesis.expect("The genesis is required");
        ChainSpec {
            chain: self.chain.expect("The chain is required"),
            genesis_header: SealedHeader::new_unhashed(make_genesis_header(
                &genesis,
                &self.hardforks,
            )),
            genesis,
            hardforks: self.hardforks,
            paris_block_and_final_difficulty,
            deposit_contract: None,
            ..Default::default()
        }
    }
}

impl From<&Arc<ChainSpec>> for ChainSpecBuilder {
    fn from(value: &Arc<ChainSpec>) -> Self {
        Self {
            chain: Some(value.chain),
            genesis: Some(value.genesis.clone()),
            hardforks: value.hardforks.clone(),
        }
    }
}

impl<H: BlockHeader> EthExecutorSpec for ChainSpec<H> {
    fn deposit_contract_address(&self) -> Option<Address> {
        self.deposit_contract.map(|deposit_contract| deposit_contract.address)
    }
}

/// `PoS` deposit contract details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepositContract {
    /// Deposit Contract Address
    pub address: Address,
    /// Deployment Block
    pub block: BlockNumber,
    /// `DepositEvent` event signature
    pub topic: B256,
}

impl DepositContract {
    /// Creates a new [`DepositContract`].
    pub const fn new(address: Address, block: BlockNumber, topic: B256) -> Self {
        Self { address, block, topic }
    }
}

/// Verifies [`ChainSpec`] configuration against expected data in given cases.
#[cfg(any(test, feature = "test-utils"))]
pub fn test_fork_ids(spec: &ChainSpec, cases: &[(Head, ForkId)]) {
    for (block, expected_id) in cases {
        let computed_id = spec.fork_id(block);
        assert_eq!(
            expected_id, &computed_id,
            "Expected fork ID {:?}, computed fork ID {:?} at block {}",
            expected_id, computed_id, block.number
        );
    }
}

#[cfg(test)]
mod spec_tests;

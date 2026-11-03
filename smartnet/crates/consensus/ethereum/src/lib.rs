//! Beacon consensus implementation.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::{fmt::Debug, sync::Arc};
use alloy_consensus::{constants::MAXIMUM_EXTRA_DATA_SIZE, BlockHeader as _};
use alloy_eips::eip7840::BlobParams;
use smartnet_execution_types::BlockExecutionResult;
use smartnet_primitives_traits::{Block, NodePrimitives, RecoveredBlock, SealedBlock, SealedHeader};
use smartnet_chainspec::{EthChainSpec, EthereumHardforks};
use smartnet_consensus::{
    Consensus, ConsensusError, FullConsensus, HeaderConsensusError, HeaderValidator,
    ReceiptRootBloom, TransactionRoot,
};
use crate::validation::{
    validate_4844_header_standalone, validate_against_parent_4844,
    validate_against_parent_eip1559_base_fee, validate_against_parent_gas_limit,
    validate_against_parent_hash_number, validate_against_parent_timestamp,
    validate_block_pre_execution, validate_block_pre_execution_with_tx_root,
    validate_body_against_header, validate_header_base_fee, validate_header_extra_data,
    validate_header_gas,
};
pub mod validation;
pub use validation::validate_block_post_execution;

/// Ethereum beacon consensus
///
/// This consensus engine does basic checks as outlined in the execution specs.
///
/// # `SmartNet` Skip Flags
///
/// `SmartNet` intentionally disables two Ethereum consensus checks because its fee model
/// differs from Ethereum's dynamic market model. These are not bugs or TODOs — they are
/// deliberate design decisions documented in `doc/FEES-CURR.md`.
///
/// ## `skip_gas_limit_ramp_check`
///
/// **Intentionally dead.** `SmartNet` does not use Ethereum's gas-limit adjustment model.
/// Ethereum adjusts gas limit ±1/1024 per block based on parent gas limit; `SmartNet` copies
/// the parent gas limit unchanged (`child_header_from_parent` → `gas_limit: parent.gas_limit`).
/// The ramp check would reject blocks whose gas limit deviates from the parent by more than
/// `GAS_LIMIT_BOUND_DIVISOR`; since `SmartNet` never changes gas limit, this check is unnecessary
/// and would add friction to block validation without improving correctness.
///
/// ## `skip_eip1559_transition_checks`
///
/// **Intentionally dead.** `SmartNet` uses fixed fees rather than EIP-1559 dynamic base fees.
/// Ethereum's `validate_against_parent_eip1559_base_fee` computes the expected base fee from
/// the parent using EIP-1559 elasticity and denominator parameters; `SmartNet`'s
/// `child_base_fee_per_gas` returns a protocol constant (`SMARTNET_FIXED_GAS_PRICE_WEI ≈ 238.1
/// gwei`), identical on every block. The parent-derived EIP-1559 formula would reject every
/// `SmartNet` block whose base fee is the fixed constant instead of the EIP-1559-derived value.
///
/// ## Future Replacement
///
/// The `SmartNet` replacement for the skipped Ethereum base-fee check now exists
/// (`doc/FEES-CURR.md`): `beacon_pre_consensus` enforces that a present `baseFeePerGas`
/// equals `SMARTNET_FIXED_GAS_PRICE_WEI` (absent stays legal for pre SmartNet-era headers).
/// A maximum timestamp-gap rule is **deferred** — see `24H.md`. Neither re-enables an
/// Ethereum EIP-1559 check.
#[derive(Debug, Clone)]
pub struct EthBeaconConsensus<ChainSpec> {
    /// Configuration
    chain_spec: Arc<ChainSpec>,
    /// Maximum allowed extra data size in bytes
    max_extra_data_size: usize,
    /// When true, skips the gas limit change validation between parent and child blocks.
    skip_gas_limit_ramp_check: bool,
    /// When true, skips the blob gas used check in header validation.
    skip_blob_gas_used_check: bool,
    /// When true, skips the requests hash check in post-execution validation.
    skip_requests_hash_check: bool,
    /// When true, skips Ethereum London transition base-fee rules
    /// (`validate_against_parent_eip1559_base_fee`).
    skip_eip1559_transition_checks: bool,
    /// SmartNet-native runtime profile: skips inherited fork-transition consensus; uses proposal
    /// semantics.
    smartnet_runtime: Option<calibri_lite_consensus::SmartNetRuntimeProfile>,
}

impl<ChainSpec: EthChainSpec + EthereumHardforks> EthBeaconConsensus<ChainSpec> {
    /// Create a new instance of [`EthBeaconConsensus`]
    pub const fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self {
            chain_spec,
            max_extra_data_size: MAXIMUM_EXTRA_DATA_SIZE,
            skip_gas_limit_ramp_check: false,
            skip_blob_gas_used_check: false,
            skip_requests_hash_check: false,
            skip_eip1559_transition_checks: false,
            smartnet_runtime: None,
        }
    }

    /// Returns the maximum allowed extra data size.
    pub const fn max_extra_data_size(&self) -> usize {
        self.max_extra_data_size
    }

    /// Sets the maximum allowed extra data size and returns the updated instance.
    pub const fn with_max_extra_data_size(mut self, size: usize) -> Self {
        self.max_extra_data_size = size;
        self
    }

    /// Disables the gas limit change validation between parent and child blocks.
    pub const fn with_skip_gas_limit_ramp_check(mut self, skip: bool) -> Self {
        self.skip_gas_limit_ramp_check = skip;
        self
    }

    /// Disables the blob gas used check in header validation.
    pub const fn with_skip_blob_gas_used_check(mut self, skip: bool) -> Self {
        self.skip_blob_gas_used_check = skip;
        self
    }

    /// Disables the requests hash check in post-execution validation.
    pub const fn with_skip_requests_hash_check(mut self, skip: bool) -> Self {
        self.skip_requests_hash_check = skip;
        self
    }

    /// Disables Ethereum London fork-transition base-fee validation (SmartNet-native consensus).
    pub const fn with_skip_eip1559_transition_checks(mut self, skip: bool) -> Self {
        self.skip_eip1559_transition_checks = skip;
        self
    }

    /// Enables SmartNet-native consensus (skips seal turn + London transition rules on import).
    pub fn with_smartnet_runtime_profile(
        mut self,
        profile: calibri_lite_consensus::SmartNetRuntimeProfile,
    ) -> Self {
        profile.log_active();
        self.smartnet_runtime = Some(profile);
        self.skip_gas_limit_ramp_check = true;
        self.skip_eip1559_transition_checks = true;
        self
    }

    /// Returns the chain spec associated with this consensus engine.
    pub const fn chain_spec(&self) -> &Arc<ChainSpec> {
        &self.chain_spec
    }
}

impl<ChainSpec, N> FullConsensus<N> for EthBeaconConsensus<ChainSpec>
where
    ChainSpec:
        Send + Sync + EthChainSpec<Header = alloy_consensus::Header> + EthereumHardforks + Debug,
    N: NodePrimitives<BlockHeader = alloy_consensus::Header>,
{
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<N::Block>,
        result: &BlockExecutionResult<N::Receipt>,
        receipt_root_bloom: Option<ReceiptRootBloom>,
    ) -> Result<(), ConsensusError> {
        let res =
            validate_block_post_execution(block, &self.chain_spec, result, receipt_root_bloom);

        if self.skip_requests_hash_check &&
            let Err(ConsensusError::BodyRequestsHashDiff(_)) = &res
        {
            return Ok(());
        }

        res
    }
}

impl<B, ChainSpec> Consensus<B> for EthBeaconConsensus<ChainSpec>
where
    B: Block<Header = alloy_consensus::Header>,
    ChainSpec:
        EthChainSpec<Header = alloy_consensus::Header> + EthereumHardforks + Debug + Send + Sync,
{
    fn validate_body_against_header(
        &self,
        body: &B::Body,
        header: &SealedHeader<B::Header>,
    ) -> Result<(), ConsensusError> {
        validate_body_against_header(body, header.header())
    }

    fn validate_block_pre_execution(&self, block: &SealedBlock<B>) -> Result<(), ConsensusError> {
        validate_block_pre_execution(block, &self.chain_spec)
    }

    fn validate_block_pre_execution_with_tx_root(
        &self,
        block: &SealedBlock<B>,
        transaction_root: Option<TransactionRoot>,
    ) -> Result<(), ConsensusError> {
        validate_block_pre_execution_with_tx_root(block, &self.chain_spec, transaction_root)
    }
}

impl<ChainSpec> HeaderValidator<alloy_consensus::Header> for EthBeaconConsensus<ChainSpec>
where
    ChainSpec:
        EthChainSpec<Header = alloy_consensus::Header> + EthereumHardforks + Debug + Send + Sync,
{
    fn validate_header(
        &self,
        header: &SealedHeader<alloy_consensus::Header>,
    ) -> Result<(), ConsensusError> {
        self.validate_header_inner(header)
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<alloy_consensus::Header>,
        parent: &SealedHeader<alloy_consensus::Header>,
    ) -> Result<(), ConsensusError> {
        validate_against_parent_hash_number(header.header(), parent)?;

        validate_against_parent_timestamp(header.header(), parent.header())?;

        self.validate_authored_header_against_parent(header.header(), parent.header())?;

        if !self.skip_gas_limit_ramp_check {
            validate_against_parent_gas_limit(header, parent, &self.chain_spec)?;
        }
        // SmartNet profile (skip_gas_limit_ramp_check): NO gas-limit check at all —
        // operator decision (XCURR D-1 stripped). Native fee is fixed by construction;
        // ERC-20 keeps its own economy; neither needs an import-side continuity check.

        if !self.skip_eip1559_transition_checks {
            validate_against_parent_eip1559_base_fee(
                header.header(),
                parent.header(),
                &self.chain_spec,
            )?;
        }

        // ensure that the blob gas fields for this block
        if let Some(blob_params) = self.chain_spec.blob_params_at_timestamp(header.timestamp()) {
            validate_against_parent_4844(header.header(), parent.header(), blob_params)?;
        }

        Ok(())
    }

    fn validate_header_range(
        &self,
        headers: &[SealedHeader<alloy_consensus::Header>],
    ) -> Result<(), HeaderConsensusError<alloy_consensus::Header>> {
        if let Some((initial_header, remaining_headers)) = headers.split_first() {
            self.validate_header_inner(initial_header)
                .map_err(|e| HeaderConsensusError::new(e, initial_header.clone()))?;
            let mut parent = initial_header;
            for child in remaining_headers {
                self.validate_header_inner(child)
                    .map_err(|e| HeaderConsensusError::new(e, child.clone()))?;
                self.validate_header_against_parent(child, parent)
                    .map_err(|e| HeaderConsensusError::new(e, child.clone()))?;
                parent = child;
            }
        }

        Ok(())
    }
}

impl<ChainSpec> EthBeaconConsensus<ChainSpec>
where
    ChainSpec:
        EthChainSpec<Header = alloy_consensus::Header> + EthereumHardforks + Debug + Send + Sync,
{
    fn validate_header_inner(
        &self,
        header: &SealedHeader<alloy_consensus::Header>,
    ) -> Result<(), ConsensusError> {
        let header = header.header();
        // ONE straight path (Mars Phase B): the merge identity checks are REMOVED, not
        // unconditional — this wire carries `difficulty` as the post-vote rec-id meta
        // (calibri pre-consensus; see `rlp_decode.rs`) and `nonce`/`ommers_hash` are fixed
        // by the header layout, so Ethereum's post-merge zero-checks are meaningless here.
        // The seal/signature/validator rules live in the pre-consensus path instead.
        //
        // Wall-clock freshness (std only): a header far in the future is rejected
        // unconditionally (previously fork-conditional). Tier-0 parent-ts monotonicity runs
        // elsewhere; this is belt-and-braces.
        #[cfg(feature = "std")]
        {
            let present_timestamp = std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            if header.timestamp() >
                present_timestamp + alloy_eips::merge::ALLOWED_FUTURE_BLOCK_TIME_SECONDS
            {
                return Err(ConsensusError::TimestampIsInFuture {
                    timestamp: header.timestamp(),
                    present_timestamp,
                });
            }
        }
        validate_header_extra_data(header, self.max_extra_data_size)?;
        validate_header_gas(header)?;
        validate_header_base_fee(header, &self.chain_spec)?;

        // EIP-4895: Beacon chain push withdrawals as operations
        if self.chain_spec.is_shanghai_active_at_timestamp(header.timestamp()) &&
            header.withdrawals_root().is_none()
        {
            return Err(ConsensusError::WithdrawalsRootMissing);
        } else if !self.chain_spec.is_shanghai_active_at_timestamp(header.timestamp()) &&
            header.withdrawals_root().is_some()
        {
            return Err(ConsensusError::WithdrawalsRootUnexpected);
        }

        // Ensures that EIP-4844 fields are valid once cancun is active.
        if self.chain_spec.is_cancun_active_at_timestamp(header.timestamp()) {
            if !self.skip_blob_gas_used_check {
                validate_4844_header_standalone(
                    header,
                    self.chain_spec
                        .blob_params_at_timestamp(header.timestamp())
                        .unwrap_or_else(BlobParams::cancun),
                )?;
            }
        } else if header.blob_gas_used().is_some() {
            return Err(ConsensusError::BlobGasUsedUnexpected);
        } else if header.excess_blob_gas().is_some() {
            return Err(ConsensusError::ExcessBlobGasUnexpected);
        } else if header.parent_beacon_block_root().is_some() {
            return Err(ConsensusError::ParentBeaconBlockRootUnexpected);
        }

        if self.chain_spec.is_prague_active_at_timestamp(header.timestamp()) {
            if header.requests_hash().is_none() {
                return Err(ConsensusError::RequestsHashMissing);
            }
        } else if header.requests_hash().is_some() {
            return Err(ConsensusError::RequestsHashUnexpected);
        }

        self.validate_authored_header(header, false)?;

        Ok(())
    }
}

mod beacon_pre_consensus;

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_primitives::B256;
    use smartnet_primitives_traits::constants::{GAS_LIMIT_BOUND_DIVISOR, MINIMUM_GAS_LIMIT};
    use smartnet_chainspec::ChainSpec;
    use crate::validation::validate_against_parent_gas_limit;

    fn header_with_gas_limit(gas_limit: u64) -> SealedHeader {
        let header = smartnet_primitives_traits::Header { gas_limit, ..Default::default() };
        SealedHeader::new(header, B256::ZERO)
    }

    #[test]
    fn test_valid_gas_limit_increase() {
        let parent = header_with_gas_limit(GAS_LIMIT_BOUND_DIVISOR * 10);
        let child = header_with_gas_limit(parent.gas_limit + 5);

        assert!(validate_against_parent_gas_limit(
            &child,
            &parent,
            &ChainSpec::<Header>::default()
        )
        .is_ok());
    }

    #[test]
    fn test_gas_limit_below_minimum() {
        let parent = header_with_gas_limit(MINIMUM_GAS_LIMIT);
        let child = header_with_gas_limit(MINIMUM_GAS_LIMIT - 1);

        assert!(matches!(
            validate_against_parent_gas_limit(&child, &parent, &ChainSpec::<Header>::default()).unwrap_err(),
            ConsensusError::GasLimitInvalidMinimum { child_gas_limit }
                if child_gas_limit == child.gas_limit
        ));
    }

    #[test]
    fn test_invalid_gas_limit_increase_exceeding_limit() {
        let parent = header_with_gas_limit(GAS_LIMIT_BOUND_DIVISOR * 10);
        let child = header_with_gas_limit(
            parent.gas_limit + parent.gas_limit / GAS_LIMIT_BOUND_DIVISOR + 1,
        );

        assert!(matches!(
            validate_against_parent_gas_limit(&child, &parent, &ChainSpec::<Header>::default()).unwrap_err(),
            ConsensusError::GasLimitInvalidIncrease { parent_gas_limit, child_gas_limit }
                if parent_gas_limit == parent.gas_limit && child_gas_limit == child.gas_limit
        ));
    }

    #[test]
    fn test_valid_gas_limit_decrease_within_limit() {
        let parent = header_with_gas_limit(GAS_LIMIT_BOUND_DIVISOR * 10);
        let child = header_with_gas_limit(parent.gas_limit - 5);

        assert!(validate_against_parent_gas_limit(
            &child,
            &parent,
            &ChainSpec::<Header>::default()
        )
        .is_ok());
    }

    #[test]
    fn test_invalid_gas_limit_decrease_exceeding_limit() {
        let parent = header_with_gas_limit(GAS_LIMIT_BOUND_DIVISOR * 10);
        let child = header_with_gas_limit(
            parent.gas_limit - parent.gas_limit / GAS_LIMIT_BOUND_DIVISOR - 1,
        );

        assert!(matches!(
            validate_against_parent_gas_limit(&child, &parent, &ChainSpec::<Header>::default()).unwrap_err(),
            ConsensusError::GasLimitInvalidDecrease { parent_gas_limit, child_gas_limit }
                if parent_gas_limit == parent.gas_limit && child_gas_limit == child.gas_limit
        ));
    }

    // (D-1 gas-limit continuity check removed by operator decision — SmartNet profile
    // performs no gas-limit validation.)
}

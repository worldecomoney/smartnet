use crate::{ChainSpec, DepositContract};
use alloc::boxed::Box;
use alloc::vec::Vec;
use alloy_chains::Chain;
use alloy_eips::{calc_next_block_base_fee, eip1559::BaseFeeParams, eip7840::BlobParams};
use alloy_genesis::Genesis;
use alloy_primitives::{B256, U256};
use core::fmt::{Debug, Display};
use smartnet_ethereum_forks::EthereumHardforks;
use smartnet_ethereum_primitives::TransactionSigned;
use smartnet_primitives_traits::{AlloyBlockHeader, BlockHeader};

/// Trait representing type configuring a chain spec.
#[auto_impl::auto_impl(&, Arc)]
pub trait EthChainSpec: Send + Sync + Unpin + Debug {
    /// The header type of the network.
    type Header: BlockHeader;

    /// Returns the [`Chain`] object this spec targets.
    fn chain(&self) -> Chain;

    /// Returns the chain id number
    fn chain_id(&self) -> u64 {
        self.chain().id()
    }

    /// Get the [`BaseFeeParams`] for the chain at the given timestamp.
    fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams;

    /// Get the [`BlobParams`] for the given timestamp
    fn blob_params_at_timestamp(&self, timestamp: u64) -> Option<BlobParams>;

    /// Returns the deposit contract data for the chain, if it's present
    fn deposit_contract(&self) -> Option<&DepositContract>;

    /// The genesis hash.
    fn genesis_hash(&self) -> B256;

    /// The calibri\* genesis pre-fund records (GENESIS.md): the alloc entries as real
    /// block-0 transactions — `from = 0x0` (zero signature), never executed, never
    /// metered; the alloc is the single source of truth for opening balances. Empty
    /// for legacy clique genesis (byte-perfect legacy).
    fn genesis_tx_records(&self) -> Vec<TransactionSigned> {
        Vec::new()
    }

    /// The delete limit for pruner, per run.
    fn prune_delete_limit(&self) -> usize;

    /// Returns a string representation of the hardforks.
    fn display_hardforks(&self) -> Box<dyn Display>;

    /// The genesis header.
    fn genesis_header(&self) -> &Self::Header;

    /// The genesis block specification.
    fn genesis(&self) -> &Genesis;

    /// Returns `true` if this chain contains Optimism configuration.
    fn is_optimism(&self) -> bool {
        self.chain().is_optimism()
    }

    /// Returns `true` if this chain contains Ethereum configuration.
    fn is_ethereum(&self) -> bool {
        self.chain().is_ethereum()
    }

    /// Returns the final total difficulty if the Paris hardfork is known.
    fn final_paris_total_difficulty(&self) -> Option<U256>;

    /// See [`calc_next_block_base_fee`].
    fn next_block_base_fee(&self, parent: &Self::Header, target_timestamp: u64) -> Option<u64> {
        Some(calc_next_block_base_fee(
            parent.gas_used(),
            parent.gas_limit(),
            parent.base_fee_per_gas()?,
            self.base_fee_params_at_timestamp(target_timestamp),
        ))
    }
}

impl<H: BlockHeader> EthChainSpec for ChainSpec<H> {
    type Header = H;

    fn chain(&self) -> Chain {
        self.chain
    }

    /// `SmartNet` fixed-fee policy: chains whose genesis carries a seal config pin the protocol
    /// gas price into every header (`doc/FEES-CURR.md`), so the "next" base fee is the
    /// constant — never the EIP-1559 elasticity formula. This keeps every consumer
    /// (fee-history, tx-pool, pending-block env, wallet quotes) on the same value the
    /// validator will seal. Standard Ethereum specs keep the default calculation.
    fn next_block_base_fee(&self, parent: &Self::Header, target_timestamp: u64) -> Option<u64> {
        if self.genesis.config.clique.is_some() {
            return Some(crate::SMARTNET_FIXED_GAS_PRICE_WEI);
        }
        Some(calc_next_block_base_fee(
            parent.gas_used(),
            parent.gas_limit(),
            parent.base_fee_per_gas()?,
            self.base_fee_params_at_timestamp(target_timestamp),
        ))
    }

    fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams {
        self.base_fee_params_at_timestamp(timestamp)
    }

    fn blob_params_at_timestamp(&self, timestamp: u64) -> Option<BlobParams> {
        if let Some(blob_param) = self.blob_params.active_scheduled_params_at_timestamp(timestamp) {
            Some(*blob_param)
        } else if self.is_osaka_active_at_timestamp(timestamp) {
            Some(self.blob_params.osaka)
        } else if self.is_prague_active_at_timestamp(timestamp) {
            Some(self.blob_params.prague)
        } else if self.is_cancun_active_at_timestamp(timestamp) {
            Some(self.blob_params.cancun)
        } else {
            None
        }
    }

    fn deposit_contract(&self) -> Option<&DepositContract> {
        self.deposit_contract.as_ref()
    }

    fn genesis_hash(&self) -> B256 {
        self.genesis_hash()
    }

    fn genesis_tx_records(&self) -> Vec<TransactionSigned> {
        Self::genesis_tx_records(self)
    }

    fn prune_delete_limit(&self) -> usize {
        self.prune_delete_limit
    }

    fn display_hardforks(&self) -> Box<dyn Display> {
        Box::new(Self::display_hardforks(self))
    }

    fn genesis_header(&self) -> &Self::Header {
        self.genesis_header()
    }

    fn genesis(&self) -> &Genesis {
        self.genesis()
    }

    fn is_optimism(&self) -> bool {
        false
    }

    fn final_paris_total_difficulty(&self) -> Option<U256> {
        self.get_final_paris_total_difficulty()
    }
}

//! Explicit SmartNet runtime identity (consensus vs execution vs wallet compatibility).

use alloy_consensus::Header;
use alloy_genesis::Genesis;
use alloy_primitives::U256;
use tracing::debug;

/// Protocol version encoded in [`super::proposal_meta::SmartNetProposalMeta`].
pub const CALIBRI_LITE_RUNTIME_PROTOCOL_VERSION: u8 = 1;

/// SmartNet fixed-fee policy constants — **native coins only**; ERC-20/DeFi keep standard
/// `gasUsed × gasPrice` EVM metering (no contract-visible special-casing). The single
/// source lives in the chain-rulebook crate (`smartnet-chainspec::fixed_fee`, with the
/// full derivation and compatibility-boundary docs); re-exported here so the consensus
/// layer, node CLI, and sync verifier all read one definition. See `doc/FEES-CURR.md`.
pub use smartnet_chainspec::{SMARTNET_FIXED_GAS_PRICE_WEI, SMARTNET_FIXED_TRANSFER_FEE_WEI};

/// How execution behaves (EVM, receipts, gas accounting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionCompatProfile {
    /// Standard REVM / Ethereum opcode and state semantics.
    EthereumEvm,
}

/// How consensus admits proposals (not Ethereum fork-choice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsensusProfile {
    /// Authorized signer + canonical parent + proposal metadata; no seal turn / London transition rules.
    SmartNetNative,
}

/// Wallet / RPC transaction surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxCompatProfile {
    /// Legacy (type-0) and EIP-1559 type-2 txs; `baseFeePerGas` / `feeHistory` fields as needed.
    LegacyAndType2,
}

/// Active Calibri Lite runtime modes — logged at node startup (`CALIBRI_LITE_RUNTIME_PROFILE_ACTIVE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmartNetRuntimeProfile {
    /// Version in [`super::proposal_meta::SmartNetProposalMeta`].
    pub protocol_version: u8,
    /// EIP-155 chain id.
    pub chain_id: u64,
    /// EVM execution semantics.
    pub execution_profile: ExecutionCompatProfile,
    /// SmartNet-native proposal admission rules.
    pub consensus_profile: ConsensusProfile,
    /// Wallet-visible transaction types.
    pub tx_compat_profile: TxCompatProfile,
}

impl SmartNetRuntimeProfile {
    /// Detect SmartNet-native profile from genesis: a chain with validators (the genesis
    /// `extraData` signer section) is SmartNet-native. The legacy `config.clique` object is
    /// not read.
    pub fn detect(chain_id: u64, genesis: &Genesis) -> Option<Self> {
        let validators =
            smartnet_chainspec::parse_extra_data_validators(genesis.extra_data.as_ref())
                .unwrap_or_default();
        if validators.is_empty() {
            return None;
        }
        Some(Self::new(chain_id))
    }

    /// Same profile when the caller already proved the genesis seal config (e.g. validator).
    pub fn for_sealed_chain(chain_id: u64) -> Self {
        Self::new(chain_id)
    }

    const fn new(chain_id: u64) -> Self {
        Self {
            protocol_version: CALIBRI_LITE_RUNTIME_PROTOCOL_VERSION,
            chain_id,
            execution_profile: ExecutionCompatProfile::EthereumEvm,
            consensus_profile: ConsensusProfile::SmartNetNative,
            tx_compat_profile: TxCompatProfile::LegacyAndType2,
        }
    }

    /// Parent-derived header fields for the next sealed block (same rule for all heights).
    pub fn child_header_from_parent(&self, parent: &Header) -> ChildHeaderFields {
        ChildHeaderFields {
            gas_limit: parent.gas_limit,
            base_fee_per_gas: self
                .child_base_fee_per_gas(parent.base_fee_per_gas)
                .expect("SmartNet profile always sets wallet-visible base fee"),
            // Chain convention: single authorized signer = always in-turn ⇒ seal
            // difficulty 2, uniform with the existing chain history (see rlp_decode.rs).
            difficulty: U256::from(2),
        }
    }

    /// Child `baseFeePerGas`: the SmartNet fixed gas price, identical on every block
    /// (no EIP-1559 fork-transition math — the fee is policy, not a market).
    pub fn child_base_fee_per_gas(&self, parent_base_fee: Option<u64>) -> Option<u64> {
        let _ = self;
        let _ = parent_base_fee;
        Some(SMARTNET_FIXED_GAS_PRICE_WEI)
    }

    /// Emit structured startup log.
    pub fn log_active(&self) {
        debug!(
            target: "calibri_lite_consensus",
            event = "CALIBRI_LITE_RUNTIME_PROFILE_ACTIVE",
            protocol_version = self.protocol_version,
            chain_id = self.chain_id,
            execution_profile = ?self.execution_profile,
            consensus_profile = ?self.consensus_profile,
            tx_compat_profile = ?self.tx_compat_profile,
            "SmartNet-native consensus/runtime identity (Ethereum execution + wallet compat only)"
        );
    }
}

/// Header fields derived from parent (consensus + wallet policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildHeaderFields {
    /// Child block gas limit (copied from parent in current profile).
    pub gas_limit: u64,
    /// Child EIP-1559 base fee for wallets.
    pub base_fee_per_gas: u64,
    /// Wire difficulty (zero on SmartNet).
    pub difficulty: U256,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_header_from_genesis_parent() {
        let profile = SmartNetRuntimeProfile::for_sealed_chain(315);
        let parent = Header {
            gas_limit: 4_700_000,
            ..Default::default()
        };
        let child = profile.child_header_from_parent(&parent);
        assert_eq!(child.gas_limit, 4_700_000);
        assert_eq!(child.base_fee_per_gas, SMARTNET_FIXED_GAS_PRICE_WEI);
        assert_eq!(child.difficulty, U256::from(2u8));
    }

    #[test]
    fn base_fee_is_fixed_regardless_of_parent() {
        let profile = SmartNetRuntimeProfile::for_sealed_chain(315);
        // Genesis default (1 gwei), a mismatched parent, and None all map to the fixed price.
        for parent_fee in [None, Some(1_000_000_000u64), Some(u64::MAX)] {
            assert_eq!(
                profile.child_base_fee_per_gas(parent_fee),
                Some(SMARTNET_FIXED_GAS_PRICE_WEI)
            );
        }
    }

    #[test]
    fn transfer_fee_is_fixed_fee_policy() {
        assert_eq!(SMARTNET_FIXED_GAS_PRICE_WEI, 238_095_238_095);
        assert_eq!(SMARTNET_FIXED_TRANSFER_FEE_WEI, 4_999_999_999_995_000);
        // 0.005 coins ± rounding of integer division (5,000,000 wei at 18 decimals).
        assert!(5_000_000_000_000_000u128 - SMARTNET_FIXED_TRANSFER_FEE_WEI <= 5_000_000);
    }
}

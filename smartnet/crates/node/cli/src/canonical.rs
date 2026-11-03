//! SmartNet canonical chain rulebook — TEST/DEV-ONLY data, runtime-unused.
//!
//! The runtime chain rulebook derives entirely from the database: the exact `Genesis` the
//! genesis file declared at `smartnet init` (stored at init, rebuilt at every boot via
//! `From<Genesis>`). There is no canonical rulebook in a running node — this module is compiled
//! only for tests and dev tooling (the `test-utils` feature), keeping chain values out of
//! production code entirely.
//!
//! A genesis file is **not** part of this codebase: its location is known only to the user,
//! who supplies it by path for exactly one purpose — `smartnet init {full_path}`. That is the
//! sole place code ever parses genesis data.

use alloy_genesis::{ChainConfig, CliqueConfig, Genesis, GenesisAccount};
use alloy_primitives::{Address, B256, U256};
use smartnet_chainspec::ChainSpec;
use std::{collections::HashMap, sync::Arc};

/// `SmartNet` seal period (seconds) — canonical value.
const SEAL_PERIOD_SECS: u64 = 5;
/// `SmartNet` seal epoch (blocks) — canonical value.
const SEAL_EPOCH_BLOCKS: u64 = 30_000;
/// Genesis `extraData` vanity prefix (32 bytes) + signer (20 bytes) + seal (65 bytes).
const GENESIS_VANITY_BYTES: usize = 32;
const GENESIS_SEAL_BYTES: usize = 65;
/// Genesis block timestamp — canonical value.
const GENESIS_TIMESTAMP: u64 = 0x675f_d019;
/// Genesis block gas limit — canonical value.
const GENESIS_GAS_LIMIT: u64 = 0x0047_b760;
/// Pre-funded burnable addresses `0x00..=0xff`, each with 1 wei — canonical value.
const PREMINE_SMALL_BALANCE: u64 = 1;
/// Genesis signer premine (WEM) — canonical value.
const SIGNER_PREMINE_WEI: &str = "200000000000000000000000000000000000000";

/// Builds the canonical `SmartNet` chain rulebook in code — tests and dev tooling only.
///
/// `chain_id` and the `validators` list are the network values the database carries
/// (`CHAIN_ID` / `VALIDATORS`, written at genesis init — the database is the single source
/// of truth, with no config fallback); neither is ever a literal in the code.
pub fn smartnet_chain_spec(chain_id: u64, validators: Vec<Address>) -> Arc<ChainSpec> {
    Arc::new(smartnet_genesis(chain_id, validators).into())
}

/// The canonical `SmartNet` genesis state, expressed as Rust data.
///
/// `validators` is the resolved signer list: each address is placed in the genesis
/// `extraData` signer list and receives the signer premine. A single-signer chain is a
/// one-element list.
fn smartnet_genesis(chain_id: u64, validators: Vec<Address>) -> Genesis {
    let mut alloc = HashMap::with_capacity(256 + validators.len());

    // Pre-funded 1-wei addresses `0x0000..00` … `0x0000..ff`.
    for byte in 0u8..=255u8 {
        let mut address_bytes = [0u8; 20];
        address_bytes[19] = byte;
        alloc.insert(
            Address::from(address_bytes),
            GenesisAccount::default().with_balance(U256::from(PREMINE_SMALL_BALANCE)),
        );
    }

    // Consensus-critical genesis signers and their premine (must match the signer set parsed from
    // `extraData` by the validator). The addresses themselves are network data — the
    // `VALIDATORS` list read from the database (the single source, no config fallback) —
    // never literals in this file.
    let premine = U256::from_str_radix(SIGNER_PREMINE_WEI, 16).expect("static premine");
    for signer in &validators {
        alloc.insert(*signer, GenesisAccount::default().with_balance(premine));
    }

    // Genesis `extraData`: vanity (32) + signer(s) (20 each) + seal (65).
    let mut extra_data =
        Vec::with_capacity(GENESIS_VANITY_BYTES + validators.len() * 20 + GENESIS_SEAL_BYTES);
    extra_data.extend_from_slice(&[0u8; GENESIS_VANITY_BYTES]);
    for signer in &validators {
        extra_data.extend_from_slice(signer.as_slice());
    }
    extra_data.extend_from_slice(&[0u8; GENESIS_SEAL_BYTES]);

    Genesis {
        config: ChainConfig {
            chain_id,
            homestead_block: Some(0),
            eip150_block: Some(0),
            eip155_block: Some(0),
            eip158_block: Some(0),
            byzantium_block: Some(0),
            constantinople_block: Some(0),
            petersburg_block: Some(0),
            istanbul_block: Some(0),
            clique: Some(CliqueConfig {
                period: Some(SEAL_PERIOD_SECS),
                epoch: Some(SEAL_EPOCH_BLOCKS),
            }),
            ..Default::default()
        },
        nonce: 0,
        timestamp: GENESIS_TIMESTAMP,
        extra_data: extra_data.into(),
        gas_limit: GENESIS_GAS_LIMIT,
        difficulty: U256::from(1),
        mix_hash: B256::ZERO,
        coinbase: Address::ZERO,
        parent_hash: Some(B256::ZERO),
        ..Default::default()
    }
    .extend_accounts(alloc)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture signer for tests only — synthetic, not an operator identity.
    const TEST_SIGNER: Address = Address::repeat_byte(0x11);

    #[test]
    fn rulebook_uses_passed_chain_id_not_a_constant() {
        let a = smartnet_chain_spec(315, vec![TEST_SIGNER]);
        let b = smartnet_chain_spec(7, vec![TEST_SIGNER]);
        assert_eq!(a.genesis.config.chain_id, 315);
        assert_eq!(b.genesis.config.chain_id, 7);
        assert_eq!(a.chain.id(), 315);
        assert_eq!(b.chain.id(), 7);
        assert_ne!(a.chain.id(), b.chain.id());
    }

    #[test]
    fn rulebook_carries_canonical_network_parameters() {
        let spec = smartnet_chain_spec(1, vec![TEST_SIGNER]);
        let genesis = &spec.genesis;
        assert_eq!(genesis.timestamp, GENESIS_TIMESTAMP);
        assert_eq!(genesis.gas_limit, GENESIS_GAS_LIMIT);
        assert_eq!(genesis.difficulty, U256::from(1));
        assert_eq!(genesis.config.homestead_block, Some(0));
        assert_eq!(genesis.config.eip150_block, Some(0));
        assert_eq!(genesis.config.eip155_block, Some(0));
        assert_eq!(genesis.config.eip158_block, Some(0));
        assert_eq!(genesis.config.byzantium_block, Some(0));
        assert_eq!(genesis.config.constantinople_block, Some(0));
        assert_eq!(genesis.config.petersburg_block, Some(0));
        assert_eq!(genesis.config.istanbul_block, Some(0));
        assert_eq!(
            genesis.config.clique,
            Some(CliqueConfig { period: Some(SEAL_PERIOD_SECS), epoch: Some(SEAL_EPOCH_BLOCKS) })
        );
        assert_eq!(genesis.config.berlin_block, None);
        assert_eq!(genesis.config.london_block, None);
        assert_eq!(genesis.config.terminal_total_difficulty, None);
        assert_eq!(genesis.alloc.len(), 257);
    }
}

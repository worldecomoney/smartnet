//! WEM chain-spec tests.
//!
//! WEM only: chain id comes from the genesis file (`fork_default.json` = 315), signer-sealed
//! consensus, EVM spec forced to LONDON at the env layer. Ethereum-network coverage
//! (sepolia/holesky/hoodi/eth-mainnet fork IDs, hive geth JSONs, Cancun/Prague genesis
//! parsing, `PoW` block rewards) was removed together with those networks.

use super::*;
use crate::{parse_extra_data_validators, ChainSpecBuilder};
use alloy_eips::eip1559::INITIAL_BASE_FEE;
use alloy_evm::eth::spec::DisableBlockRewards;
use alloy_genesis::{ChainConfig, CliqueConfig};
use alloy_primitives::U256;
use smartnet_ethereum_forks::{EthereumHardfork, ForkCondition, ForkHash, ForkId, Head};

/// A minimal WEM-shaped genesis: pre-London block forks at 0, no timestamp forks.
fn wem_shape_genesis(chain_id: u64) -> ChainConfig {
    ChainConfig {
        chain_id,
        homestead_block: Some(0),
        eip150_block: Some(0),
        eip155_block: Some(0),
        eip158_block: Some(0),
        byzantium_block: Some(0),
        constantinople_block: Some(0),
        petersburg_block: Some(0),
        istanbul_block: Some(0),
        ..Default::default()
    }
}

fn london_test_spec() -> ChainSpec {
    ChainSpecBuilder::default()
        .chain(Chain::from(99_999))
        .genesis(Genesis::default())
        .london_activated()
        .build()
}

#[test]
fn wem_mainnet_is_chain_315_from_genesis() {
    // The genesis file is the single source of truth for the chain id.
    assert_eq!(MAINNET.genesis.config.chain_id, 315);
    assert_eq!(MAINNET.chain.id(), MAINNET.genesis.config.chain_id);
}

#[test]
fn wem_mainnet_genesis_header_is_sealed() {
    assert_eq!(MAINNET.genesis_hash(), MAINNET.genesis_header.hash());
    assert_eq!(MAINNET.genesis_header().number, 0);
}

#[test]
fn from_genesis_keeps_wem_block_forks_and_does_not_inject_london() {
    // No fee-era fork is injected from a hard-coded block number: historical sealed-block
    // headers carry no base fee and replay under Istanbul (per-header import spec), while live
    // blocks use the uniform Mars/EVM profile. See crates/evm/ethereum/src/lib.rs.
    let genesis = Genesis { config: wem_shape_genesis(4_242_424), ..Default::default() };
    let spec = ChainSpec::from_genesis(genesis);

    assert_eq!(spec.chain.id(), 4_242_424);
    assert!(spec.hardforks.fork(EthereumHardfork::Istanbul).active_at_block(0));
    assert_eq!(spec.hardforks.fork(EthereumHardfork::London), ForkCondition::Never);
    assert!(spec.paris_block().is_none(), "WEM has no merge");
    assert!(spec.deposit_contract.is_none(), "WEM has no deposit contract");
}

#[test]
fn london_activated_at_genesis_activates_base_fee() {
    let spec = london_test_spec();
    assert!(spec.hardforks.fork(EthereumHardfork::London).active_at_block(0));
    assert_eq!(spec.initial_base_fee(), Some(INITIAL_BASE_FEE));
    assert_eq!(spec.genesis_header().base_fee_per_gas, Some(INITIAL_BASE_FEE));
}

#[test]
fn pre_london_chain_has_no_base_fee() {
    let spec = ChainSpecBuilder::default()
        .chain(Chain::from(99_999))
        .genesis(Genesis::default())
        .istanbul_activated()
        .build();
    assert!(spec.hardforks.fork(EthereumHardfork::London) == ForkCondition::Never);
    assert_eq!(spec.initial_base_fee(), None);
    assert_eq!(spec.genesis_header().base_fee_per_gas, None);
}

#[test]
fn london_builder_does_not_activate_later_forks() {
    let spec = london_test_spec();
    for later in [
        EthereumHardfork::Paris,
        EthereumHardfork::Shanghai,
        EthereumHardfork::Cancun,
        EthereumHardfork::Prague,
    ] {
        assert!(
            !spec.hardforks.fork(later).active_at_block(0) &&
                !spec.hardforks.fork(later).active_at_timestamp(0),
            "{later:?} must not be active on a London-only spec"
        );
    }
}

#[test]
fn fork_id_of_london_only_spec_is_genesis_only() {
    // London sits at block 0 (genesis) so it adds no fork-hash entries.
    let spec = london_test_spec();
    let expected = ForkId { hash: ForkHash::from(spec.genesis_hash()), next: 0 };
    assert_eq!(spec.fork_id(&Head::default()), expected);
    assert_eq!(spec.latest_fork_id(), expected);
}

#[test]
fn sealed_genesis_disables_block_rewards() {
    // WEM (signer-sealed) pays no block reward — empty blocks leave state untouched.
    let mut genesis = Genesis::default();
    genesis.config.clique = Some(CliqueConfig { period: Some(5), epoch: Some(30_000) });
    let spec = ChainSpec::from_genesis(genesis);
    assert!(spec.disable_block_rewards());
}

#[test]
fn unsealed_genesis_keeps_block_rewards() {
    let spec =
        ChainSpec::from_genesis(Genesis { config: wem_shape_genesis(555), ..Default::default() });
    assert!(!spec.disable_block_rewards());
}

#[test]
fn dev_chain_spec_loads() {
    let dev = dev_chain_spec();
    assert_eq!(dev.chain, Chain::dev());
}

/// The dev chain is the one programmatic launches (`NodeConfig::test()`) boot, so its genesis must
/// carry a signer: the node refuses to start without a chain id and a genesis signer list.
#[test]
fn dev_genesis_carries_a_signer() {
    let dev = dev_chain_spec();
    let signers = parse_extra_data_validators(&dev.genesis.extra_data)
        .expect("dev extraData must parse as a signer list");
    assert!(!signers.is_empty(), "dev genesis must authorise at least one signer");
    assert!(signers.iter().all(|signer| !signer.is_zero()));
}

#[test]
fn base_fee_uses_ethereum_params_for_wem() {
    // WEM's fixed-fee era still computes EIP-1559 base fee with the Ethereum constants.
    let spec = london_test_spec();
    let params = spec.base_fee_params_at_timestamp(0);
    assert_eq!(params, alloy_eips::eip1559::BaseFeeParams::ethereum());
}

#[test]
fn genesis_difficulty_untouched_by_spec_defaults() {
    let spec = ChainSpec::from_genesis(Genesis {
        config: wem_shape_genesis(7),
        difficulty: U256::from(1),
        ..Default::default()
    });
    assert_eq!(spec.genesis().difficulty, U256::from(1));
}

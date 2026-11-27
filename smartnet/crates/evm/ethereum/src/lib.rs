//! EVM config for vanilla ethereum.
//!
//! # Revm features
//!
//! This crate does __not__ enforce specific revm features such as `blst` or `c-kzg`, which are
//! critical for revm's evm internals, it is the responsibility of the implementer to ensure the
//! proper features are selected.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::{borrow::Cow, sync::Arc};
use alloy_consensus::Header;
use alloy_evm::{
    eth::{EthBlockExecutionCtx, EthBlockExecutorFactory},
    EthEvmFactory, FromRecoveredTx, FromTxWithEncoded,
};
use core::{convert::Infallible, fmt::Debug};
use smartnet_ethereum_primitives::{Block, EthPrimitives, TransactionSigned};
use smartnet_evm::{
    eth::NextEvmEnvAttributes, precompiles::PrecompilesMap, ConfigureEvm, EvmEnv, EvmFactory,
    NextBlockEnvAttributes, TransactionEnvMut,
};
use smartnet_primitives_traits::{SealedBlock, SealedHeader};
use revm::{context::BlockEnv, primitives::hardfork::SpecId};
use smartnet_chainspec::{extra_data_validators, ChainSpec, EthChainSpec, MAINNET};

#[cfg(feature = "std")]
use smartnet_evm::{ConfigureEngineEvm, ExecutableTxIterator};
#[allow(unused_imports)]
use {
    alloy_eips::Decodable2718,
    alloy_primitives::{Bytes, U256},
    alloy_rpc_types_engine::ExecutionData,
    smartnet_evm::{EvmEnvFor, ExecutionCtxFor},
    smartnet_primitives_traits::{constants::MAX_TX_GAS_LIMIT_OSAKA, SignedTransaction, TxTy},
    revm::context::CfgEnv,
    revm::context_interface::block::BlobExcessGasAndPrice,
    smartnet_chainspec::EthereumHardforks,
    smartnet_storage_errors::any::AnyError,
};

pub use alloy_evm::EthEvm;

mod config;
use alloy_evm::eth::spec::EthExecutorSpec;
pub use config::{revm_spec, revm_spec_by_timestamp_and_block_number};
use smartnet_ethereum_forks::Hardforks;

/// Helper type with backwards compatible methods to obtain Ethereum executor
/// providers.
#[doc(hidden)]
pub mod execute {
    use crate::EthEvmConfig;

    #[deprecated(note = "Use `EthEvmConfig` instead")]
    pub type EthExecutorProvider = EthEvmConfig;
}

mod build;
pub use build::EthBlockAssembler;

mod receipt;
pub use receipt::SmartnetReceiptBuilder;

#[cfg(feature = "test-utils")]
mod test_utils;
#[cfg(feature = "test-utils")]
pub use test_utils::*;

/// Ethereum-related EVM configuration.
#[derive(Debug, Clone)]
pub struct EthEvmConfig<C = ChainSpec, EvmFactory = EthEvmFactory> {
    /// Inner [`EthBlockExecutorFactory`].
    pub executor_factory: EthBlockExecutorFactory<SmartnetReceiptBuilder, Arc<C>, EvmFactory>,
    /// Ethereum block assembler.
    pub block_assembler: EthBlockAssembler<C>,
    /// Historical RLP import only: skip revm's upfront balance/value-max-fee check.
    /// The live/sync config never sets this; legacy geth admitted txs under lesser checks.
    import_mode: bool,
}

impl EthEvmConfig {
    /// Creates a new Ethereum EVM configuration for the ethereum mainnet.
    pub fn mainnet() -> Self {
        Self::ethereum(MAINNET.clone())
    }
}

impl<ChainSpec> EthEvmConfig<ChainSpec> {
    /// Creates a new Ethereum EVM configuration with the given chain spec.
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self::ethereum(chain_spec)
    }

    /// Creates a new Ethereum EVM configuration.
    pub fn ethereum(chain_spec: Arc<ChainSpec>) -> Self {
        Self::new_with_evm_factory(chain_spec, EthEvmFactory::default())
    }
}

impl<ChainSpec, EvmFactory> EthEvmConfig<ChainSpec, EvmFactory> {
    /// Creates a new Ethereum EVM configuration with the given chain spec and EVM factory.
    pub fn new_with_evm_factory(chain_spec: Arc<ChainSpec>, evm_factory: EvmFactory) -> Self {
        Self {
            block_assembler: EthBlockAssembler::new(chain_spec.clone()),
            executor_factory: EthBlockExecutorFactory::new(
                SmartnetReceiptBuilder::default(),
                chain_spec,
                evm_factory,
            ),
            import_mode: false,
        }
    }

    /// Marks this config as the historical-import one: revm's upfront balance check is
    /// skipped for every transaction while replaying legacy RLP history. Never use for
    /// a live node or mesh sync.
    pub const fn for_import(mut self) -> Self {
        self.import_mode = true;
        self
    }

    /// Returns the chain spec associated with this configuration.
    pub const fn chain_spec(&self) -> &Arc<ChainSpec> {
        self.executor_factory.spec()
    }
}

#[cfg(feature = "std")]
impl<ChainSpec, EvmF> EthEvmConfig<ChainSpec, EvmF>
where
    ChainSpec: EthExecutorSpec
        + EthChainSpec<Header = Header>
        + Hardforks
        + alloy_evm::eth::spec::DisableBlockRewards
        + 'static,
{
    fn evm_env_for_execution_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<EvmEnv<SpecId>, Infallible> {
        let timestamp = payload.payload.timestamp();
        let block_number = payload.payload.block_number();
        let v1 = payload.payload.as_v1();

        let blob_params = self.chain_spec().blob_params_at_timestamp(timestamp);
        let spec = revm_spec_by_timestamp_and_block_number();

        let mut cfg_env = CfgEnv::new()
            .with_chain_id(self.chain_spec().chain().id())
            .with_spec_and_mainnet_gas_params(spec);

        if let Some(blob_params) = &blob_params {
            cfg_env.set_max_blobs_per_tx(blob_params.max_blobs_per_tx);
        }

        // ONE straight path (Mars Phase B): the spec is OSAKA at every height, so the 7825
        // tx-gas cap is unconditional — no fork-haired check.
        cfg_env.tx_gas_limit_cap = Some(MAX_TX_GAS_LIMIT_OSAKA);

        // The blob market does not exist on this network: no blob params, no blob pricing.
        let blob_excess_gas_and_price = None;

        let basefee =
            smartnet_engine_primitives::ethereum::base_fee_from_payload_v1(v1).unwrap_or_default();

        let block_env = BlockEnv {
            number: U256::from(block_number),
            // geth credited fees to the seal's author (`Engine().Author`), not the
            // clique vote-marker coinbase in the envelope; see
            // `execution_beneficiary`.
            beneficiary: smartnet_engine_primitives::ethereum::execution_beneficiary(payload),
            timestamp: U256::from(timestamp),
            difficulty: U256::ZERO,
            prevrandao: None,
            gas_limit: payload.payload.gas_limit(),
            basefee,
            blob_excess_gas_and_price,
            slot_num: payload.payload.as_v4().map(|v4| v4.slot_number).unwrap_or_default(),
        };

        Ok(EvmEnv { cfg_env, block_env })
    }

    fn evm_env_for_engine_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<EvmEnv<SpecId>, Infallible> {
        let timestamp = payload.payload.timestamp();
        let block_number = payload.payload.block_number();

        // ONE straight path (Mars Phase B): same OSAKA spec + 7825 cap at every height, no
        // blob params, no blob pricing.
        let mut cfg_env = CfgEnv::new()
            .with_chain_id(self.chain_spec().chain().id())
            .with_spec_and_mainnet_gas_params(revm_spec_by_timestamp_and_block_number());
        cfg_env.tx_gas_limit_cap = Some(MAX_TX_GAS_LIMIT_OSAKA);
        let blob_excess_gas_and_price = None;

        let basefee =
            smartnet_engine_primitives::ethereum::base_fee_from_payload_v1(payload.payload.as_v1())
                .unwrap_or_default();

        let block_env = BlockEnv {
            number: U256::from(block_number),
            beneficiary: payload.payload.fee_recipient(),
            timestamp: U256::from(timestamp),
            difficulty: U256::ZERO,
            prevrandao: None,
            gas_limit: payload.payload.gas_limit(),
            basefee,
            blob_excess_gas_and_price,
            slot_num: payload.payload.as_v4().map(|v4| v4.slot_number).unwrap_or_default(),
        };

        Ok(EvmEnv { cfg_env, block_env })
    }
}

impl<ChainSpec, EvmF> ConfigureEvm for EthEvmConfig<ChainSpec, EvmF>
where
    ChainSpec: EthExecutorSpec
        + EthChainSpec<Header = Header>
        + Hardforks
        + alloy_evm::eth::spec::DisableBlockRewards
        + 'static,
    EvmF: EvmFactory<
            Tx: TransactionEnvMut
                    + FromRecoveredTx<TransactionSigned>
                    + FromTxWithEncoded<TransactionSigned>,
            Spec = SpecId,
            BlockEnv = BlockEnv,
            Precompiles = PrecompilesMap,
        > + Clone
        + Debug
        + Send
        + Sync
        + Unpin
        + 'static,
{
    type Primitives = EthPrimitives;
    type Error = Infallible;
    type NextBlockEnvCtx = NextBlockEnvAttributes;
    type BlockExecutorFactory = EthBlockExecutorFactory<SmartnetReceiptBuilder, Arc<ChainSpec>, EvmF>;
    type BlockAssembler = EthBlockAssembler<ChainSpec>;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.block_assembler
    }

    fn evm_env(&self, header: &Header) -> Result<EvmEnv<SpecId>, Self::Error> {
        let mut env = EvmEnv::for_eth_block(
            header,
            self.chain_spec(),
            self.chain_spec().chain().id(),
            self.chain_spec().blob_params_at_timestamp(header.timestamp),
        );
        // Historical geth/clique credits fees to the recovered seal signer, never to the
        // zeroed vote-marker coinbase. Only the import config rewrites the beneficiary;
        // live/sync keeps the header value byte-for-byte.
        if self.import_mode {
            env.block_env.beneficiary =
                smartnet_engine_primitives::ethereum::execution_beneficiary_for_header(header);
            // Historical fee fields are never checked — the header itself decides the gas
            // schedule and base fee used for execution. A legacy header with no baseFeePerGas
            // is Istanbul/zero (no EIP-2929 surcharge; this preserves the pinned F5 roots).
            // That includes legacy ERC-20/DeFi contracts **created under Istanbul**: they must
            // replay under the original Istanbul gas schedule, byte-exact, with no fixed-fee
            // rule applied to them. A fee-era header keeps the derived LONDON spec and its own
            // base fee as-is, even when that historical fee was zero or unlike today's fixed
            // price. Live/sync keeps LONDON for type-0 + type-2 support.
            if header.base_fee_per_gas.is_none() {
                env.cfg_env.set_spec_and_mainnet_gas_params(SpecId::ISTANBUL);
                env.block_env.basefee = 0u64;
            }
        }
        // The import-only flag rides into the EVM through CfgEnv. The gate in
        // `EthEvm::transact_raw` preserves and restores this value; on live/sync it is
        // false, so only the chair's own overdraft gets through, as before.
        env.cfg_env.disable_balance_check = self.import_mode;
        Ok(env)
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &NextBlockEnvAttributes,
    ) -> Result<EvmEnv, Self::Error> {
        Ok(EvmEnv::for_eth_next_block(
            parent,
            NextEvmEnvAttributes {
                timestamp: attributes.timestamp,
                suggested_fee_recipient: attributes.suggested_fee_recipient,
                prev_randao: attributes.prev_randao,
                gas_limit: attributes.gas_limit,
                slot_number: attributes.slot_number,
            },
            self.chain_spec().next_block_base_fee(parent, attributes.timestamp).unwrap_or_default(),
            self.chain_spec(),
            self.chain_spec().chain().id(),
            self.chain_spec().blob_params_at_timestamp(attributes.timestamp),
        ))
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<Block>,
    ) -> Result<EthBlockExecutionCtx<'a>, Self::Error> {
        Ok(EthBlockExecutionCtx {
            tx_count_hint: Some(block.transaction_count()),
            parent_hash: block.header().parent_hash,
            parent_beacon_block_root: block.header().parent_beacon_block_root,
            ommers: &block.body().ommers,
            withdrawals: block.body().withdrawals.as_ref().map(|w| Cow::Borrowed(w.as_slice())),
            extra_data: block.header().extra_data.clone(),
            slot_number: block.header().slot_number,
        })
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<EthBlockExecutionCtx<'_>, Self::Error> {
        Ok(EthBlockExecutionCtx {
            tx_count_hint: None,
            parent_hash: parent.hash(),
            parent_beacon_block_root: attributes.parent_beacon_block_root,
            ommers: &[],
            withdrawals: attributes.withdrawals.map(|w| Cow::Owned(w.into_inner())),
            extra_data: attributes.extra_data,
            slot_number: attributes.slot_number,
        })
    }
}

#[cfg(feature = "std")]
impl<ChainSpec, EvmF> ConfigureEngineEvm<ExecutionData> for EthEvmConfig<ChainSpec, EvmF>
where
    ChainSpec: EthExecutorSpec
        + EthChainSpec<Header = Header>
        + Hardforks
        + alloy_evm::eth::spec::DisableBlockRewards
        + 'static,
    EvmF: EvmFactory<
            Tx: TransactionEnvMut
                    + FromRecoveredTx<TransactionSigned>
                    + FromTxWithEncoded<TransactionSigned>,
            Spec = SpecId,
            BlockEnv = BlockEnv,
            Precompiles = PrecompilesMap,
        > + Clone
        + Debug
        + Send
        + Sync
        + Unpin
        + 'static,
{
    fn evm_env_for_payload(&self, payload: &ExecutionData) -> Result<EvmEnvFor<Self>, Self::Error> {
        if extra_data_validators(self.chain_spec().genesis().extra_data.as_ref()).is_empty() {
            self.evm_env_for_engine_payload(payload)
        } else {
            self.evm_env_for_execution_payload(payload)
        }
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a ExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        Ok(EthBlockExecutionCtx {
            tx_count_hint: Some(payload.payload.transactions().len()),
            parent_hash: payload.parent_hash(),
            parent_beacon_block_root: payload.sidecar.parent_beacon_block_root(),
            ommers: &[],
            withdrawals: payload.payload.withdrawals().map(|w| Cow::Borrowed(w.as_slice())),
            extra_data: payload.payload.as_v1().extra_data.clone(),
            slot_number: payload.payload.as_v4().map(|v4| v4.slot_number),
        })
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        let txs = payload.payload.transactions().clone();
        let convert = |tx: Bytes| {
            let tx =
                TxTy::<Self::Primitives>::decode_2718_exact(tx.as_ref()).map_err(AnyError::new)?;
            let signer = tx.try_recover().map_err(AnyError::new)?;
            Ok::<_, AnyError>(tx.with_signer(signer))
        };

        Ok((txs, convert))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_genesis::Genesis;
    use smartnet_evm::{execute::ProviderError, EvmEnv};
    use revm::{
        context::{BlockEnv, CfgEnv},
        database::CacheDB,
        database_interface::EmptyDBTyped,
        inspector::NoOpInspector,
    };
    use smartnet_chainspec::{Chain, ChainSpec};

    /// WEM chain spec (chain id 315, LONDON-only) for tests that must run against our rulebook.
    fn wem_chain_spec() -> Arc<ChainSpec> {
        let chain_spec = ChainSpec::builder()
            .chain(Chain::from_id(315))
            .genesis(Genesis::default())
            .london_activated()
            .build();
        Arc::new(chain_spec)
    }

    #[test]
    fn test_fill_cfg_and_block_env() {
        // Create a default header
        let header = Header::default();

        // Build the WEM ChainSpec (chain id 315, LONDON-only)
        let chain_spec = wem_chain_spec();

        // Use the `EthEvmConfig` to fill the `cfg_env` and `block_env` based on the ChainSpec,
        // Header, and total difficulty
        let EvmEnv { cfg_env, .. } =
            EthEvmConfig::new(Arc::new(chain_spec.clone())).evm_env(&header).unwrap();

        // Assert that the chain ID in the `cfg_env` is correctly set to the chain ID of the
        // ChainSpec
        assert_eq!(cfg_env.chain_id, chain_spec.chain().id());
    }

    /// The first tx block of the trusted chain: zero coinbase vote marker, 97-byte
    /// extraData whose trailing signature recovers to the sole clique sealer.
    const BLOCK_17214_RLP: &str = concat!(
        "f902d1f9025ba026129d98cbe54047b3b91454bb847222fe924eb07f24e021ea2b75f75a553a96a01dcc4de8dec75d7a",
        "ab85b567b6ccd41ad312451b948a7413f0a142fd40d49347940000000000000000000000000000000000000000a0ce45",
        "6f0573acf30a3e39bd8b538b59eb1214e73d5c439100feee727b9da587baa0b2d845752dd5db9caa99cbb1d2121e31da",
        "b79f5a4b664ff9b325040d632d7d7fa0056b23fbba480696b65fe5a59b8f2148a1299103c4f57df839233af2cf4ca2d2",
        "b90100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        "000000000000000000000000000000000000000282433e8401c9c38082520884684661cab861d883010d0f8467657468",
        "88676f312e32312e33856c696e757800000000000000d8bc0c59c31d12d303642f34bd531fee91cb84a764b27b0e8828",
        "7cc115dbf1c749b2a0a596717e61eb5b0c350ee1258b7584594593a72b9bd083a0ce041e269f00a00000000000000000",
        "000000000000000000000000000000000000000000000000880000000000000000f870f86e80843b9aca008252089455",
        "92a508e36703e84b6fd1b500808e3c79cc5be8890ad78ebc5ac62000008082029aa0c897ad85fc6f46d1dd26f43016c8",
        "86b152d96a6b0e15fd86cea74877bb8962c9a016c7549c8132eea34f1e3184e669d93a77eb5051ec0f8ce9c8aa51889f",
        "cd6923c0"
    );

    #[test]
    fn import_mode_env_credits_recovered_sealer_only() {
        use alloy_primitives::{address, hex, Address};

        let bytes = hex::decode(BLOCK_17214_RLP).unwrap();
        let block: Block = alloy_rlp::Decodable::decode(&mut bytes.as_slice()).unwrap();

        let live = EthEvmConfig::new(wem_chain_spec()).evm_env(&block.header).unwrap();
        assert_eq!(live.block_env.beneficiary, Address::ZERO);
        assert_eq!(live.cfg_env.spec, SpecId::LONDON);

        let import =
            EthEvmConfig::new(wem_chain_spec()).for_import().evm_env(&block.header).unwrap();
        assert_eq!(
            import.block_env.beneficiary,
            address!("0x24bf191a358c09e79b9ead2d8999d08590b5af31")
        );
        assert_ne!(import.block_env.beneficiary, block.header.beneficiary);
        // Historical import runs the trusted chain's pre-Berlin gas schedule;
        // live/sync stays on LONDON for type-0 + type-2 support.
        assert_eq!(import.cfg_env.spec, SpecId::ISTANBUL);
    }

    #[test]
    fn import_mode_fee_era_header_keeps_its_own_base_fee() {
        use alloy_primitives::hex;

        let bytes = hex::decode(BLOCK_17214_RLP).unwrap();
        let block: Block = alloy_rlp::Decodable::decode(&mut bytes.as_slice()).unwrap();
        let mut header = block.header;
        header.base_fee_per_gas = Some(123_456);

        let import =
            EthEvmConfig::new(wem_chain_spec()).for_import().evm_env(&header).unwrap();
        assert_eq!(import.cfg_env.spec, SpecId::LONDON);
        assert_eq!(import.block_env.basefee, 123_456u64);
    }

    #[test]
    fn test_evm_with_env_default_spec() {
        let evm_config = EthEvmConfig::ethereum(wem_chain_spec());

        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let evm_env = EvmEnv::default();

        let evm = evm_config.evm_with_env(db, evm_env.clone());

        // Check that the EVM environment
        assert_eq!(evm.block, evm_env.block_env);
        assert_eq!(evm.cfg, evm_env.cfg_env);
    }

    #[test]
    fn test_evm_with_env_custom_cfg() {
        let evm_config = EthEvmConfig::ethereum(wem_chain_spec());

        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        // Create a custom configuration environment with a chain ID of 111
        let cfg = CfgEnv::default().with_chain_id(111);

        let evm_env = EvmEnv { cfg_env: cfg.clone(), ..Default::default() };

        let evm = evm_config.evm_with_env(db, evm_env);

        // Check that the EVM environment is initialized with the custom environment
        assert_eq!(evm.cfg, cfg);
    }

    #[test]
    fn test_evm_with_env_custom_block_and_tx() {
        let evm_config = EthEvmConfig::ethereum(wem_chain_spec());

        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        // Create customs block and tx env
        let block = BlockEnv {
            basefee: 1000,
            gas_limit: 10_000_000,
            number: U256::from(42),
            ..Default::default()
        };

        let evm_env = EvmEnv { block_env: block, ..Default::default() };

        let evm = evm_config.evm_with_env(db, evm_env.clone());

        // Verify that the block and transaction environments are set correctly
        assert_eq!(evm.block, evm_env.block_env);

        // Default spec ID
        assert_eq!(evm.cfg.spec, SpecId::default());
    }

    #[test]
    fn test_evm_with_spec_id() {
        let evm_config = EthEvmConfig::ethereum(wem_chain_spec());

        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let evm_env = EvmEnv {
            cfg_env: CfgEnv::new().with_spec_and_mainnet_gas_params(SpecId::CONSTANTINOPLE),
            ..Default::default()
        };

        let evm = evm_config.evm_with_env(db, evm_env);

        // Check that the spec ID is setup properly
        assert_eq!(evm.cfg.spec, SpecId::CONSTANTINOPLE);
    }

    #[test]
    fn test_evm_with_env_and_default_inspector() {
        let evm_config = EthEvmConfig::ethereum(wem_chain_spec());
        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let evm_env = EvmEnv::default();

        let evm = evm_config.evm_with_env_and_inspector(db, evm_env.clone(), NoOpInspector {});

        // Check that the EVM environment is set to default values
        assert_eq!(evm.block, evm_env.block_env);
        assert_eq!(evm.cfg, evm_env.cfg_env);
    }

    #[test]
    fn test_evm_with_env_inspector_and_custom_cfg() {
        let evm_config = EthEvmConfig::ethereum(wem_chain_spec());
        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let cfg_env = CfgEnv::default().with_chain_id(111);
        let block = BlockEnv::default();
        let evm_env = EvmEnv { cfg_env: cfg_env.clone(), block_env: block };

        let evm = evm_config.evm_with_env_and_inspector(db, evm_env, NoOpInspector {});

        // Check that the EVM environment is set with custom configuration
        assert_eq!(evm.cfg, cfg_env);
        assert_eq!(evm.cfg.spec, SpecId::default());
    }

    #[test]
    fn test_evm_with_env_inspector_and_custom_block_tx() {
        let evm_config = EthEvmConfig::ethereum(wem_chain_spec());
        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        // Create custom block and tx environment
        let block = BlockEnv {
            basefee: 1000,
            gas_limit: 10_000_000,
            number: U256::from(42),
            ..Default::default()
        };
        let evm_env = EvmEnv { block_env: block, ..Default::default() };

        let evm = evm_config.evm_with_env_and_inspector(db, evm_env.clone(), NoOpInspector {});

        // Verify that the block and transaction environments are set correctly
        assert_eq!(evm.block, evm_env.block_env);
        assert_eq!(evm.cfg.spec, SpecId::default());
    }

    #[test]
    fn test_evm_with_env_inspector_and_spec_id() {
        let evm_config = EthEvmConfig::ethereum(wem_chain_spec());
        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let evm_env = EvmEnv {
            cfg_env: CfgEnv::new().with_spec_and_mainnet_gas_params(SpecId::CONSTANTINOPLE),
            ..Default::default()
        };

        let evm = evm_config.evm_with_env_and_inspector(db, evm_env.clone(), NoOpInspector {});

        // Check that the spec ID is set properly
        assert_eq!(evm.block, evm_env.block_env);
        assert_eq!(evm.cfg, evm_env.cfg_env);
        assert_eq!(evm.tx, Default::default());
    }

    /// RTBC policy defaults must mirror the authoritative chainspec constants
    /// (`SC-FILTER.md` §7.4) — fails if either side drifts.
    #[test]
    fn rtbc_policy_mirrors_chainspec_constants() {
        let policy = smartnet_evm::rtbc::RtbcPolicy::default();
        assert_eq!(policy.max_initcode_size, smartnet_chainspec::MAX_INITCODE_SIZE);
        assert_eq!(policy.max_runtime_size, smartnet_chainspec::MAX_RUNTIME_SIZE);
    }
}

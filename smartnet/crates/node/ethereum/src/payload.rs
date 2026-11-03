//! Payload component configuration for the Ethereum node.

use smartnet_ethereum_primitives::EthPrimitives;
use smartnet_evm::ConfigureEvm;
use smartnet_transaction_pool::{PoolTransaction, TransactionPool};
use smartnet_chainspec::EthereumHardforks;
use smartnet_engine_primitives::ethereum::{EthBuiltPayload, EthPayloadAttributes};
use smartnet_ethereum_payload_builder::EthereumBuilderConfig;
use smartnet_node_api::{FullNodeTypes, NodeTypes, PrimitivesTy, TxTy};
use smartnet_node_builder::{
    components::PayloadBuilderBuilder, BuilderContext, PayloadBuilderConfig, PayloadTypes,
};

/// A basic ethereum payload service.
#[derive(Clone, Default, Debug)]
#[non_exhaustive]
pub struct EthereumPayloadBuilder;

impl<Types, Node, Pool, Evm> PayloadBuilderBuilder<Node, Pool, Evm> for EthereumPayloadBuilder
where
    Types: NodeTypes<ChainSpec: EthereumHardforks, Primitives = EthPrimitives>,
    Node: FullNodeTypes<Types = Types>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
    Evm: ConfigureEvm<
            Primitives = PrimitivesTy<Types>,
            NextBlockEnvCtx = smartnet_evm::NextBlockEnvAttributes,
        > + 'static,
    Types::Payload:
        PayloadTypes<BuiltPayload = EthBuiltPayload, PayloadAttributes = EthPayloadAttributes>,
{
    type PayloadBuilder =
        smartnet_ethereum_payload_builder::EthereumPayloadBuilder<Pool, Node::Provider, Evm>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: Evm,
    ) -> eyre::Result<Self::PayloadBuilder> {
        let conf = ctx.payload_builder_config();
        let gas_limit = conf.gas_limit_for();

        Ok(smartnet_ethereum_payload_builder::EthereumPayloadBuilder::new(
            ctx.provider().clone(),
            pool,
            evm_config,
            EthereumBuilderConfig::new()
                .with_gas_limit(gas_limit)
                .with_max_blobs_per_block(conf.max_blobs_per_block())
                .with_extra_data(conf.extra_data()),
        ))
    }
}

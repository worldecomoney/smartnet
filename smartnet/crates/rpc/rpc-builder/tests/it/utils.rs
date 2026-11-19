use smartnet_ethereum_primitives::EthPrimitives;
use smartnet_consensus::noop::NoopConsensus;
use smartnet_tasks::EventSender;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use smartnet_network_api::noop::NoopNetwork;
use smartnet_transaction_pool::test_utils::{TestPool, TestPoolBuilder};
use smartnet_evm_ethereum::EthEvmConfig;
use smartnet_provider::test_utils::NoopProvider;
use smartnet_rpc_builder::{
    RpcModuleBuilder, RpcServerConfig, RpcServerHandle, TransportRpcModuleConfig,
};
use smartnet_rpc_server_types::RpcModuleSelection;
use smartnet_tasks::Runtime;

/// Localhost with port 0 so a free port is used.
pub const fn test_address() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
}

/// Launches a new server with http only with the given modules
pub async fn launch_http(modules: impl Into<RpcModuleSelection>) -> RpcServerHandle {
    let builder = test_rpc_builder();
    let eth_api = builder.bootstrap_eth_api();
    let server =
        builder.build(TransportRpcModuleConfig::set_http(modules), eth_api, EventSender::new(1));
    RpcServerConfig::http(Default::default())
        .with_http_address(test_address())
        .start(&server)
        .await
        .unwrap()
}

/// Returns an [`RpcModuleBuilder`] with testing components.
pub fn test_rpc_builder(
) -> RpcModuleBuilder<EthPrimitives, NoopProvider, TestPool, NoopNetwork, EthEvmConfig, NoopConsensus>
{
    RpcModuleBuilder::default()
        .with_provider(NoopProvider::default())
        .with_pool(TestPoolBuilder::default().into())
        .with_network(NoopNetwork::default())
        .with_executor(Runtime::test())
        .with_evm_config(EthEvmConfig::mainnet())
        .with_consensus(NoopConsensus::default())
}

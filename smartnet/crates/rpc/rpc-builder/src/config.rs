use jsonrpsee::server::ServerConfigBuilder;
use smartnet_node_core::args::RpcServerArgs;
use smartnet_rpc::ValidationApiConfig;
use smartnet_rpc_eth_types::{EthConfig, EthStateCacheConfig, GasPriceOracleConfig};
use smartnet_rpc_server_types::RpcModuleSelection;
use std::net::SocketAddr;
use tracing::warn;

use crate::{RpcModuleConfig, RpcServerConfig, TransportRpcModuleConfig};

/// A trait that provides a configured RPC server.
///
/// This provides all basic config values for the RPC server and is implemented by the
/// [`RpcServerArgs`] type.
pub trait SmartnetRpcServerConfig {
    /// The configured ethereum RPC settings.
    fn eth_config(&self) -> EthConfig;

    /// The configured ethereum RPC settings.
    fn flashbots_config(&self) -> ValidationApiConfig;

    /// Returns state cache configuration.
    fn state_cache_config(&self) -> EthStateCacheConfig;

    /// Returns the max request size in bytes.
    fn rpc_max_request_size_bytes(&self) -> u32;

    /// Returns the max response size in bytes.
    fn rpc_max_response_size_bytes(&self) -> u32;

    /// Extracts the gas price oracle config from the args.
    fn gas_price_oracle_config(&self) -> GasPriceOracleConfig;

    /// Creates the [`TransportRpcModuleConfig`] from cli args.
    ///
    /// This sets all the api modules, and configures additional settings like gas price oracle
    /// settings in the [`TransportRpcModuleConfig`].
    fn transport_rpc_module_config(&self) -> TransportRpcModuleConfig;

    /// Returns the default server config for http
    fn http_ws_server_builder(&self) -> ServerConfigBuilder;

    /// Creates the [`RpcServerConfig`] from cli args.
    fn rpc_server_config(&self) -> RpcServerConfig;

}

impl SmartnetRpcServerConfig for RpcServerArgs {
    fn eth_config(&self) -> EthConfig {
        EthConfig::default()
            .max_tracing_requests(self.rpc_max_tracing_requests)
            .max_blocking_io_requests(self.rpc_max_blocking_io_requests)
            .max_trace_filter_blocks(self.rpc_max_trace_filter_blocks)
            .max_blocks_per_filter(self.rpc_max_blocks_per_filter.unwrap_or_max())
            .max_logs_per_response(self.rpc_max_logs_per_response.unwrap_or_max() as usize)
            .eth_proof_window(self.rpc_eth_proof_window)
            .rpc_gas_cap(self.rpc_gas_cap)
            .rpc_max_simulate_blocks(self.rpc_max_simulate_blocks)
            .state_cache(self.state_cache_config())
            .gpo_config(self.gas_price_oracle_config())
            .proof_permits(self.rpc_proof_permits)
            .pending_block_kind(self.rpc_pending_block)
            .raw_tx_forwarder(self.rpc_forwarder.clone())
            .rpc_evm_memory_limit(self.rpc_evm_memory_limit)
            .force_blob_sidecar_upcasting(self.rpc_force_blob_sidecar_upcasting)
    }

    fn flashbots_config(&self) -> ValidationApiConfig {
        ValidationApiConfig {
            disallow: self.builder_disallow.clone().unwrap_or_default(),
            validation_window: self.rpc_eth_proof_window,
        }
    }

    fn state_cache_config(&self) -> EthStateCacheConfig {
        EthStateCacheConfig {
            max_blocks: self.rpc_state_cache.max_blocks,
            max_receipts: self.rpc_state_cache.max_receipts,
            max_headers: self.rpc_state_cache.max_headers,
            max_concurrent_db_requests: self.rpc_state_cache.max_concurrent_db_requests,
            max_cached_tx_hashes: self.rpc_state_cache.max_cached_tx_hashes,
        }
    }

    fn rpc_max_request_size_bytes(&self) -> u32 {
        self.rpc_max_request_size.get().saturating_mul(1024 * 1024)
    }

    fn rpc_max_response_size_bytes(&self) -> u32 {
        self.rpc_max_response_size.get().saturating_mul(1024 * 1024)
    }

    fn gas_price_oracle_config(&self) -> GasPriceOracleConfig {
        self.gas_price_oracle.gas_price_oracle_config()
    }

    fn transport_rpc_module_config(&self) -> TransportRpcModuleConfig {
        let mut config = TransportRpcModuleConfig::default().with_config(
            RpcModuleConfig::new(self.eth_config())
                .with_mesh_peers_file(self.mesh_peers_file.clone())
                .with_txpool_namespace(self.rpc_mempool)
                .with_nodeinfo(self.rpc_nodeinfo),
        );

        if self.http {
            config = config.with_http(self.http_api.clone().unwrap_or_else(|| {
                // SmartNet / debloated fork: default public HTTP API includes `txpool_*` and
                // `debug_*` (raw block/header RLP for validator tooling) without extra
                // `--http.api`.
                RpcModuleSelection::Selection(RpcModuleSelection::default_http_ws_modules())
            }));
        }

        // SmartNet: WS is intentionally never mounted as an RPC transport. The `--ws*`
        // args remain valid (future mesh transport), but no RPC modules or WS server are
        // ever configured here.

        config
    }

    fn http_ws_server_builder(&self) -> ServerConfigBuilder {
        ServerConfigBuilder::new()
            .max_connections(self.rpc_max_connections.get())
            .max_request_body_size(self.rpc_max_request_size_bytes())
            .max_response_body_size(self.rpc_max_response_size_bytes())
            .max_subscriptions_per_connection(self.rpc_max_subscriptions_per_connection.get())
    }

    fn rpc_server_config(&self) -> RpcServerConfig {
        let mut config = RpcServerConfig::default();

        if self.http_api.is_some() && !self.http {
            warn!(
                target: "smartnet::cli",
                "The --http.api flag is set but --http is not enabled. HTTP RPC API will not be exposed."
            );
        }

        if self.http {
            let socket_address = SocketAddr::new(self.http_addr, self.http_port);
            config = config
                .with_http_address(socket_address)
                .with_http(self.http_ws_server_builder())
                .with_http_cors(self.http_corsdomain.clone())
                .with_http_disable_compression(self.http_disable_compression);
        }

        // SmartNet: no WS RPC server is ever bound — WS args parse but are inert.

        config
    }
}

#[cfg(test)]
mod tests {
    use crate::config::SmartnetRpcServerConfig;
    use clap::{Args, Parser};
    use smartnet_node_core::args::RpcServerArgs;
    use smartnet_rpc_eth_types::RPC_DEFAULT_GAS_CAP;
    use smartnet_rpc_server_types::{constants, SmartnetRpcModule};
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    /// The operator CLI is config-file driven — build `RpcServerArgs` directly, the way
    /// the config loader does, and parse module selections via `FromStr`.
    fn http_args(apis: &str) -> RpcServerArgs {
        RpcServerArgs {
            http: true,
            http_api: Some(apis.parse().unwrap()),
            ..Default::default()
        }
    }

    #[test]
    fn test_rpc_gas_cap() {
        let config = RpcServerArgs::default().eth_config();
        assert_eq!(config.rpc_gas_cap, u64::from(RPC_DEFAULT_GAS_CAP));

        let args = RpcServerArgs { rpc_gas_cap: 1000, ..Default::default() };
        assert_eq!(args.eth_config().rpc_gas_cap, 1000);
    }

    #[test]
    fn test_transport_rpc_module_config() {
        let config = http_args("eth,admin,debug").transport_rpc_module_config();
        let expected = [SmartnetRpcModule::Eth, SmartnetRpcModule::Admin, SmartnetRpcModule::Debug];
        assert_eq!(config.http().cloned().unwrap().into_selection(), expected.into());
        assert!(config.ws().is_none());
    }

    #[test]
    fn test_transport_rpc_module_trim_config() {
        let config = http_args(" eth, admin, debug").transport_rpc_module_config();
        let expected = [SmartnetRpcModule::Eth, SmartnetRpcModule::Admin, SmartnetRpcModule::Debug];
        assert_eq!(config.http().cloned().unwrap().into_selection(), expected.into());
        assert!(config.ws().is_none());
    }

    #[test]
    fn test_unique_rpc_modules() {
        let config = http_args(" eth, admin, debug, eth,admin").transport_rpc_module_config();
        let expected = [SmartnetRpcModule::Eth, SmartnetRpcModule::Admin, SmartnetRpcModule::Debug];
        assert_eq!(config.http().cloned().unwrap().into_selection(), expected.into());
        assert!(config.ws().is_none());
    }

    #[test]
    fn test_rpc_server_config() {
        let config = http_args("eth,admin,debug").rpc_server_config();
        assert_eq!(
            config.http_address().unwrap(),
            SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::LOCALHOST,
                constants::DEFAULT_HTTP_RPC_PORT
            ))
        );
        assert!(config.ws_address().is_none());
    }

    #[test]
    fn test_zero_filter_limits() {
        let args = RpcServerArgs {
            rpc_max_blocks_per_filter: 0u64.into(),
            rpc_max_logs_per_response: 0u64.into(),
            ..Default::default()
        };
        let config = args.eth_config().filter_config();
        assert_eq!(config.max_blocks_per_filter, Some(u64::MAX));
        assert_eq!(config.max_logs_per_response, Some(usize::MAX));
    }

    #[test]
    fn test_custom_filter_limits() {
        let args = RpcServerArgs {
            rpc_max_blocks_per_filter: 100u64.into(),
            rpc_max_logs_per_response: 200u64.into(),
            ..Default::default()
        };
        let config = args.eth_config().filter_config();
        assert_eq!(config.max_blocks_per_filter, Some(100));
        assert_eq!(config.max_logs_per_response, Some(200));
    }

    #[test]
    fn removed_cli_flags_are_rejected() {
        for flag in ["--rpc.gascap", "--http.api", "--http", "--rpc-max-blocks-per-filter"] {
            assert!(
                CommandParser::<RpcServerArgs>::try_parse_from(["smartnet", flag, "x"]).is_err(),
                "{flag} must stay rejected (config-file-driven CLI)"
            );
        }
    }
}

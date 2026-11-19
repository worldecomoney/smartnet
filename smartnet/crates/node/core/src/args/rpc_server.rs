//! clap [Args](clap::Args) for RPC related arguments.

use crate::args::{
    types::{MaxU32, ZeroAsNoneU64},
    GasPriceOracleArgs, RpcStateCacheArgs,
};
use alloy_primitives::map::AddressSet;
use clap::Args;
use smartnet_rpc_eth_types::builder::config::PendingBlockKind;
use smartnet_rpc_server_types::{constants, RpcModuleSelection, SmartnetRpcModule};
use std::{
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    sync::OnceLock,
    time::Duration,
};
use url::Url;


/// Global static RPC server defaults
static RPC_SERVER_DEFAULTS: OnceLock<DefaultRpcServerArgs> = OnceLock::new();

/// Default max number of subscriptions per connection.
pub(crate) const RPC_DEFAULT_MAX_SUBS_PER_CONN: u32 = 1024;

/// Default max request size in MB.
pub(crate) const RPC_DEFAULT_MAX_REQUEST_SIZE_MB: u32 = 15;

/// Default max response size in MB.
///
/// This is only relevant for very large trace responses.
pub(crate) const RPC_DEFAULT_MAX_RESPONSE_SIZE_MB: u32 = 160;

/// Default number of incoming connections.
///
/// This restricts how many active connections (http) the server accepts.
/// Once exceeded, the server can reject new connections.
pub(crate) const RPC_DEFAULT_MAX_CONNECTIONS: u32 = 500;

/// Default values for RPC server that can be customized
///
/// Global defaults can be set via [`DefaultRpcServerArgs::try_init`].
#[derive(Debug, Clone)]
pub struct DefaultRpcServerArgs {
    http: bool,
    http_addr: IpAddr,
    http_port: u16,
    http_disable_compression: bool,
    http_api: Option<RpcModuleSelection>,
    http_corsdomain: Option<String>,
    rpc_mempool: bool,
    rpc_nodeinfo: bool,
    rpc_max_request_size: MaxU32,
    rpc_max_response_size: MaxU32,
    rpc_max_subscriptions_per_connection: MaxU32,
    rpc_max_connections: MaxU32,
    rpc_max_tracing_requests: usize,
    rpc_max_blocking_io_requests: usize,
    rpc_max_trace_filter_blocks: u64,
    rpc_max_blocks_per_filter: ZeroAsNoneU64,
    rpc_max_logs_per_response: ZeroAsNoneU64,
    rpc_gas_cap: u64,
    rpc_evm_memory_limit: u64,
    rpc_tx_fee_cap: u128,
    rpc_max_simulate_blocks: u64,
    rpc_eth_proof_window: u64,
    rpc_proof_permits: usize,
    rpc_pending_block: PendingBlockKind,
    rpc_forwarder: Option<Url>,
    builder_disallow: Option<AddressSet>,
    rpc_state_cache: RpcStateCacheArgs,
    gas_price_oracle: GasPriceOracleArgs,
    rpc_send_raw_transaction_sync_timeout: Duration,
}

impl DefaultRpcServerArgs {
    /// Initialize the global RPC server defaults with this configuration
    pub fn try_init(self) -> Result<(), Self> {
        RPC_SERVER_DEFAULTS.set(self)
    }

    /// Get a reference to the global RPC server defaults
    pub fn get_global() -> &'static Self {
        RPC_SERVER_DEFAULTS.get_or_init(Self::default)
    }

    /// Set the default HTTP enabled state
    pub const fn with_http(mut self, v: bool) -> Self {
        self.http = v;
        self
    }

    /// Set the default HTTP address
    pub const fn with_http_addr(mut self, v: IpAddr) -> Self {
        self.http_addr = v;
        self
    }

    /// Set the default HTTP port
    pub const fn with_http_port(mut self, v: u16) -> Self {
        self.http_port = v;
        self
    }

    /// Set whether to disable HTTP compression by default
    pub const fn with_http_disable_compression(mut self, v: bool) -> Self {
        self.http_disable_compression = v;
        self
    }

    /// Set the default HTTP API modules
    pub fn with_http_api(mut self, v: Option<RpcModuleSelection>) -> Self {
        self.http_api = v;
        self
    }

    /// Set the default HTTP CORS domain
    pub fn with_http_corsdomain(mut self, v: Option<String>) -> Self {
        self.http_corsdomain = v;
        self
    }

    /// Set the default max request size
    pub const fn with_rpc_max_request_size(mut self, v: MaxU32) -> Self {
        self.rpc_max_request_size = v;
        self
    }

    /// Set the default max response size
    pub const fn with_rpc_max_response_size(mut self, v: MaxU32) -> Self {
        self.rpc_max_response_size = v;
        self
    }

    /// Set the default max subscriptions per connection
    pub const fn with_rpc_max_subscriptions_per_connection(mut self, v: MaxU32) -> Self {
        self.rpc_max_subscriptions_per_connection = v;
        self
    }

    /// Set the default max connections
    pub const fn with_rpc_max_connections(mut self, v: MaxU32) -> Self {
        self.rpc_max_connections = v;
        self
    }

    /// Set the default max tracing requests
    pub const fn with_rpc_max_tracing_requests(mut self, v: usize) -> Self {
        self.rpc_max_tracing_requests = v;
        self
    }

    /// Set the default max blocking IO requests
    pub const fn with_rpc_max_blocking_io_requests(mut self, v: usize) -> Self {
        self.rpc_max_blocking_io_requests = v;
        self
    }

    /// Set the default max trace filter blocks
    pub const fn with_rpc_max_trace_filter_blocks(mut self, v: u64) -> Self {
        self.rpc_max_trace_filter_blocks = v;
        self
    }

    /// Set the default max blocks per filter
    pub const fn with_rpc_max_blocks_per_filter(mut self, v: ZeroAsNoneU64) -> Self {
        self.rpc_max_blocks_per_filter = v;
        self
    }

    /// Set the default max logs per response
    pub const fn with_rpc_max_logs_per_response(mut self, v: ZeroAsNoneU64) -> Self {
        self.rpc_max_logs_per_response = v;
        self
    }

    /// Set the default gas cap
    pub const fn with_rpc_gas_cap(mut self, v: u64) -> Self {
        self.rpc_gas_cap = v;
        self
    }

    /// Set the default EVM memory limit
    pub const fn with_rpc_evm_memory_limit(mut self, v: u64) -> Self {
        self.rpc_evm_memory_limit = v;
        self
    }

    /// Set the default tx fee cap
    pub const fn with_rpc_tx_fee_cap(mut self, v: u128) -> Self {
        self.rpc_tx_fee_cap = v;
        self
    }

    /// Set the default max simulate blocks
    pub const fn with_rpc_max_simulate_blocks(mut self, v: u64) -> Self {
        self.rpc_max_simulate_blocks = v;
        self
    }

    /// Set the default eth proof window
    pub const fn with_rpc_eth_proof_window(mut self, v: u64) -> Self {
        self.rpc_eth_proof_window = v;
        self
    }

    /// Set the default proof permits
    pub const fn with_rpc_proof_permits(mut self, v: usize) -> Self {
        self.rpc_proof_permits = v;
        self
    }

    /// Set the default pending block kind
    pub const fn with_rpc_pending_block(mut self, v: PendingBlockKind) -> Self {
        self.rpc_pending_block = v;
        self
    }

    /// Set the default RPC forwarder
    pub fn with_rpc_forwarder(mut self, v: Option<Url>) -> Self {
        self.rpc_forwarder = v;
        self
    }

    /// Set the default builder disallow addresses
    pub fn with_builder_disallow(mut self, v: Option<AddressSet>) -> Self {
        self.builder_disallow = v;
        self
    }

    /// Set the default RPC state cache args
    pub const fn with_rpc_state_cache(mut self, v: RpcStateCacheArgs) -> Self {
        self.rpc_state_cache = v;
        self
    }

    /// Set the default gas price oracle args
    pub const fn with_gas_price_oracle(mut self, v: GasPriceOracleArgs) -> Self {
        self.gas_price_oracle = v;
        self
    }

    /// Set the default send raw transaction sync timeout
    pub const fn with_rpc_send_raw_transaction_sync_timeout(mut self, v: Duration) -> Self {
        self.rpc_send_raw_transaction_sync_timeout = v;
        self
    }
}

impl Default for DefaultRpcServerArgs {
    fn default() -> Self {
        Self {
            http: false,
            http_addr: Ipv4Addr::LOCALHOST.into(),
            http_port: constants::DEFAULT_HTTP_RPC_PORT,
            http_disable_compression: false,
            http_api: None,
            http_corsdomain: None,
            rpc_mempool: false,
            rpc_nodeinfo: false,
            rpc_max_request_size: RPC_DEFAULT_MAX_REQUEST_SIZE_MB.into(),
            rpc_max_response_size: RPC_DEFAULT_MAX_RESPONSE_SIZE_MB.into(),
            rpc_max_subscriptions_per_connection: RPC_DEFAULT_MAX_SUBS_PER_CONN.into(),
            rpc_max_connections: RPC_DEFAULT_MAX_CONNECTIONS.into(),
            rpc_max_tracing_requests: constants::default_max_tracing_requests(),
            rpc_max_blocking_io_requests: constants::DEFAULT_MAX_BLOCKING_IO_REQUEST,
            rpc_max_trace_filter_blocks: constants::DEFAULT_MAX_TRACE_FILTER_BLOCKS,
            rpc_max_blocks_per_filter: constants::DEFAULT_MAX_BLOCKS_PER_FILTER.into(),
            rpc_max_logs_per_response: (constants::DEFAULT_MAX_LOGS_PER_RESPONSE as u64).into(),
            rpc_gas_cap: constants::gas_oracle::RPC_DEFAULT_GAS_CAP,
            rpc_evm_memory_limit: (1 << 32) - 1,
            rpc_tx_fee_cap: constants::DEFAULT_TX_FEE_CAP_WEI,
            rpc_max_simulate_blocks: constants::DEFAULT_MAX_SIMULATE_BLOCKS,
            rpc_eth_proof_window: constants::DEFAULT_ETH_PROOF_WINDOW,
            rpc_proof_permits: constants::DEFAULT_PROOF_PERMITS,
            rpc_pending_block: PendingBlockKind::Full,
            rpc_forwarder: None,
            builder_disallow: None,
            rpc_state_cache: RpcStateCacheArgs::default(),
            gas_price_oracle: GasPriceOracleArgs::default(),
            rpc_send_raw_transaction_sync_timeout:
                constants::RPC_DEFAULT_SEND_RAW_TX_SYNC_TIMEOUT_SECS,
        }
    }
}

/// Parameters for configuring the rpc more granularity via CLI
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(next_help_heading = "RPC")]
pub struct RpcServerArgs {
    /// Enable the HTTP-RPC server
    #[arg(skip)]
    pub http: bool,

    /// Http server address to listen on
    #[arg(skip = IpAddr::from([0, 0, 0, 0]))]
    pub http_addr: IpAddr,

    /// Http server port to listen on
    #[arg(skip)]
    pub http_port: u16,

    /// Disable compression for HTTP responses
    #[arg(skip)]
    pub http_disable_compression: bool,

    /// Rpc Modules to be configured for the HTTP server
    #[arg(skip)]
    pub http_api: Option<RpcModuleSelection>,

    /// Http Corsdomain to allow request from
    #[arg(skip)]
    pub http_corsdomain: Option<String>,

    /// Temporary: expose the read-only `txpool_*` namespace on the public HTTP/WS transports
    /// (operator key `RPC_MEMPOOL`). Default **false**. Read-only, but leaks pending-transaction
    /// data (front-running / MEV scrape target) and `txpool_content` is a `DoS` surface; keep the
    /// endpoint non-public and rate-limited while on. Disable before any public launch.
    #[arg(skip)]
    pub rpc_mempool: bool,

    /// Expose `web3_nodeInfo` (the closed mesh roster) on the public RPC (operator key
    /// `RPC_NODEINFO`). Default **false**. Reveals mesh membership labels/IPs/ports; the node's
    /// own public key is not reported yet (mesh identity is by listen port until the keypair
    /// lands).
    #[arg(skip)]
    pub rpc_nodeinfo: bool,

    /// Mesh gossip listen port (operator key `MESH_PORT`). `0`/unset = mesh off: the
    /// closed-list `POST /rumor` listener never starts. Must differ from `HTTP_PORT` and
    /// and must equal this node's `port_range` entry in every peer's
    /// `mainnet.jsonl` (self-identity in the mesh is by listen port).
    #[arg(skip)]
    pub mesh_port: Option<u16>,

    /// Mesh gossip listen address (operator key `MESH_ADDR`). Unset = `0.0.0.0` (all
    /// interfaces). Set explicitly to bind the listener to one interface.
    #[arg(skip)]
    pub mesh_addr: Option<IpAddr>,

    /// Mesh peer file (operator key `MESH_PEERS_FILE`; default
    /// `~/.smartnet/mainnet.jsonl`). One JSON object per line; re-read on every spread so
    /// membership changes need no restart. Missing file = outbound gossip off (logged once).
    #[arg(skip)]
    pub mesh_peers_file: Option<PathBuf>,

    /// Mesh max `WebSocket` message size in megabytes (operator key `MESH_MAX_MSG_MB`).
    /// Unset = 32 MB; `0` is rejected by the operator-config overlay. Caps every inbound
    /// and outbound frame — a large block or tx batch cannot exceed this.
    #[arg(skip)]
    pub mesh_max_msg_mb: Option<usize>,

    /// THE node heartbeat period in milliseconds (operator key `NODE_HEARTBEAT`): the
    /// ONE trigger hook every service attaches to — validator seal beat, node duties,
    /// 1 HB to bind them all. Unset = 500 ms; `0` is rejected by the operator-config
    /// overlay. The mesh keepalive is NOT this — it is its own hardcoded constant.
    #[arg(skip)]
    pub heartbeat_ms: Option<u64>,

    /// Set the maximum RPC request payload size for both HTTP and WS in megabytes.
    #[arg(skip)]
    pub rpc_max_request_size: MaxU32,

    /// Set the maximum RPC response payload size for both HTTP and WS in megabytes.
    #[arg(skip)]
    pub rpc_max_response_size: MaxU32,

    /// Set the maximum concurrent subscriptions per connection.
    #[arg(skip)]
    pub rpc_max_subscriptions_per_connection: MaxU32,

    /// Maximum number of RPC server connections.
    #[arg(skip)]
    pub rpc_max_connections: MaxU32,

    /// Maximum number of concurrent tracing requests.
    ///
    /// By default this chooses a sensible value based on the number of available cores.
    /// Tracing requests are generally CPU bound.
    /// Choosing a value that is higher than the available CPU cores can have a negative impact on
    /// the performance of the node and affect the node's ability to maintain sync.
    #[arg(skip)]
    pub rpc_max_tracing_requests: usize,

    /// Maximum number of concurrent blocking IO requests.
    ///
    /// Blocking IO requests include `eth_call`, `eth_estimateGas`, and similar methods that
    /// require EVM execution. These are spawned as blocking tasks to avoid blocking the async
    /// runtime.
    #[arg(skip)]
    pub rpc_max_blocking_io_requests: usize,

    /// Maximum number of blocks for `trace_filter` requests.
    #[arg(skip)]
    pub rpc_max_trace_filter_blocks: u64,

    /// Maximum number of blocks that could be scanned per filter request. (0 = entire chain)
    #[arg(skip)]
    pub rpc_max_blocks_per_filter: ZeroAsNoneU64,

    /// Maximum number of logs that can be returned in a single response. (0 = no limit)
    #[arg(skip)]
    pub rpc_max_logs_per_response: ZeroAsNoneU64,

    /// Maximum gas limit for `eth_call` and call tracing RPC methods.
    #[arg(skip)]
    pub rpc_gas_cap: u64,

    /// Maximum memory the EVM can allocate per RPC request.
    #[arg(skip)]
    pub rpc_evm_memory_limit: u64,

    /// Maximum eth transaction fee (in ether) that can be sent via the RPC APIs (0 = no cap)
    #[arg(skip)]
    pub rpc_tx_fee_cap: u128,

    /// Maximum number of blocks for `eth_simulateV1` call.
    #[arg(skip)]
    pub rpc_max_simulate_blocks: u64,

    /// The maximum proof window for historical proof generation.
    /// This value allows for generating historical proofs up to
    /// configured number of blocks from current tip (up to `tip - window`).
    #[arg(skip)]
    pub rpc_eth_proof_window: u64,

    /// Maximum number of concurrent getproof requests.
    #[arg(skip)]
    pub rpc_proof_permits: usize,

    /// Configures the pending block behavior for RPC responses.
    ///
    /// Options: full (include all transactions), empty (header only), none (disable pending
    /// blocks).
    #[arg(skip)]
    pub rpc_pending_block: PendingBlockKind,

    /// Endpoint to forward transactions to.
    #[arg(skip)]
    pub rpc_forwarder: Option<Url>,

    /// Path to file containing disallowed addresses, json-encoded list of strings. Block
    /// validation API will reject blocks containing transactions from these addresses.
    #[arg(skip)]
    pub builder_disallow: Option<AddressSet>,

    /// State cache configuration.
    #[command(flatten)]
    pub rpc_state_cache: RpcStateCacheArgs,

    /// Gas price oracle configuration.
    #[command(flatten)]
    pub gas_price_oracle: GasPriceOracleArgs,

    /// Timeout for `send_raw_transaction_sync` RPC method.
    #[arg(skip)]
    pub rpc_send_raw_transaction_sync_timeout: Duration,

    /// Force upcasting EIP-4844 blob sidecars to EIP-7594 format when Osaka is active.
    ///
    /// When enabled, blob transactions submitted via `eth_sendRawTransaction` with EIP-4844
    /// sidecars will be automatically converted to EIP-7594 format if the next block is Osaka.
    /// By default this is disabled, meaning transactions are submitted as-is.
    #[arg(skip)]
    pub rpc_force_blob_sidecar_upcasting: bool,
}

impl RpcServerArgs {
    /// Enables the HTTP-RPC server.
    pub const fn with_http(mut self) -> Self {
        self.http = true;
        self
    }

    /// Configures modules for the HTTP-RPC server.
    pub fn with_http_api(mut self, http_api: RpcModuleSelection) -> Self {
        self.http_api = Some(http_api);
        self
    }

    /// Configures modules for both the HTTP-RPC server and WS-RPC server.
    pub fn with_api(self, api: RpcModuleSelection) -> Self {
        self.with_http_api(api)
    }

    /// Change rpc port numbers based on the instance number, if provided.
    /// * The `http_port` is scaled by a factor of `-instance`
    ///
    /// # Panics
    /// Warning: if `instance` is zero in debug mode, this will panic.
    ///
    /// In release mode, overflow will silently wrap around.
    pub fn adjust_instance_ports(&mut self, instance: Option<u16>) {
        if let Some(instance) = instance {
            debug_assert_ne!(instance, 0, "instance must be non-zero");
            // http port is scaled by a factor of -instance
            self.http_port -= instance - 1;
        }
    }

    /// Set the http port to zero, to allow the OS to assign a random unused port when the rpc
    /// server binds to a socket.
    pub const fn with_http_unused_port(mut self) -> Self {
        self.http_port = 0;
        self
    }

    /// Configure the HTTP port to be set to a random unused port when bound.
    pub const fn with_unused_ports(mut self) -> Self {
        self = self.with_http_unused_port();
        self
    }

    /// Apply a function to the args.
    pub fn apply<F>(self, f: F) -> Self
    where
        F: FnOnce(Self) -> Self,
    {
        f(self)
    }

    /// Configures the timeout for send raw transaction sync.
    pub const fn with_send_raw_transaction_sync_timeout(mut self, timeout: Duration) -> Self {
        self.rpc_send_raw_transaction_sync_timeout = timeout;
        self
    }

    /// Returns `true` if the given RPC namespace is enabled on any transport.
    pub fn is_namespace_enabled(&self, ns: SmartnetRpcModule) -> bool {
        self.http && self.http_api.as_ref().is_some_and(|api| api.contains(&ns))
    }

    /// Enables forced blob sidecar upcasting from EIP-4844 to EIP-7594 format.
    pub const fn with_force_blob_sidecar_upcasting(mut self) -> Self {
        self.rpc_force_blob_sidecar_upcasting = true;
        self
    }
}

impl Default for RpcServerArgs {
    fn default() -> Self {
        let DefaultRpcServerArgs {
            http,
            http_addr,
            http_port,
            http_disable_compression,
            http_api,
            http_corsdomain,
            rpc_mempool,
            rpc_nodeinfo,
            rpc_max_request_size,
            rpc_max_response_size,
            rpc_max_subscriptions_per_connection,
            rpc_max_connections,
            rpc_max_tracing_requests,
            rpc_max_blocking_io_requests,
            rpc_max_trace_filter_blocks,
            rpc_max_blocks_per_filter,
            rpc_max_logs_per_response,
            rpc_gas_cap,
            rpc_evm_memory_limit,
            rpc_tx_fee_cap,
            rpc_max_simulate_blocks,
            rpc_eth_proof_window,
            rpc_proof_permits,
            rpc_pending_block,
            rpc_forwarder,
            builder_disallow,
            rpc_state_cache,
            gas_price_oracle,
            rpc_send_raw_transaction_sync_timeout,
        } = DefaultRpcServerArgs::get_global().clone();
        Self {
            http,
            http_addr,
            http_port,
            http_disable_compression,
            http_api,
            http_corsdomain,
            rpc_mempool,
            rpc_nodeinfo,
            mesh_port: None,
            mesh_addr: None,
            mesh_peers_file: None,
            mesh_max_msg_mb: None,
            heartbeat_ms: None,
            rpc_max_request_size,
            rpc_max_response_size,
            rpc_max_subscriptions_per_connection,
            rpc_max_connections,
            rpc_max_tracing_requests,
            rpc_max_blocking_io_requests,
            rpc_max_trace_filter_blocks,
            rpc_max_blocks_per_filter,
            rpc_max_logs_per_response,
            rpc_gas_cap,
            rpc_evm_memory_limit,
            rpc_tx_fee_cap,
            rpc_max_simulate_blocks,
            rpc_eth_proof_window,
            rpc_proof_permits,
            rpc_pending_block,
            rpc_forwarder,
            builder_disallow,
            rpc_state_cache,
            gas_price_oracle,
            rpc_send_raw_transaction_sync_timeout,
            rpc_force_blob_sidecar_upcasting: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Args, Parser};

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    /// The operator CLI is config-file driven: these per-flag options were removed
    /// (all struct fields are `#[arg(skip)]`). The legacy flags must stay rejected.
    #[test]
    fn removed_cli_flags_are_rejected() {
        const REMOVED: &[&str] = &[
            "--http",
            "--http.addr",
            "--http.port",
            "--http.api",
            "--http.corsdomain",
            "--rpc.max-request-size",
            "--rpc.max-response-size",
            "--rpc.max-subscriptions-per-connection",
            "--rpc.max-connections",
            "--rpc.max-tracing-requests",
            "--rpc.max-blocking-io-requests",
            "--rpc.max-trace-filter-blocks",
            "--rpc.max-blocks-per-filter",
            "--rpc.max-logs-per-response",
            "--rpc.gascap",
            "--rpc.evm-memory-limit",
            "--rpc.txfeecap",
            "--rpc.max-simulate-blocks",
            "--rpc.eth-proof-window",
            "--rpc.proof-permits",
            "--rpc.pending-block",
            "--rpc.forwarder",
            "--rpc-cache.max-blocks",
            "--rpc-cache.max-receipts",
            "--rpc-cache.max-headers",
            "--rpc-cache.max-concurrent-db-requests",
            "--gpo.blocks",
            "--gpo.ignoreprice",
            "--gpo.maxprice",
            "--gpo.percentile",
            "--rpc.send-raw-transaction-sync-timeout",
            "--rpc-max-blocks-per-filter",
            "--rpc-max-logs-per-response",
        ];
        for flag in REMOVED {
            assert!(
                CommandParser::<RpcServerArgs>::try_parse_from(["smartnet", flag, "x"]).is_err(),
                "{flag} must stay rejected (config-file-driven CLI)"
            );
        }
    }




    #[test]
    fn rpc_server_args_default_sanity_test() {
        // Defaults are config-driven (`DefaultRpcServerArgs::get_global()`); lock the
        // key operator-visible values.
        let args = RpcServerArgs::default();
        assert!(!args.http);
        assert_eq!(args.http_addr, IpAddr::from(Ipv4Addr::LOCALHOST));
        assert_eq!(args.http_port, constants::DEFAULT_HTTP_RPC_PORT);
        assert!(!args.http_disable_compression);
        assert!(args.http_api.is_none());
        assert!(!args.rpc_mempool);
        assert!(!args.rpc_nodeinfo);
        assert_eq!(args.rpc_gas_cap, constants::gas_oracle::RPC_DEFAULT_GAS_CAP);
        assert_eq!(args.rpc_tx_fee_cap, constants::DEFAULT_TX_FEE_CAP_WEI);
        assert!(args.rpc_forwarder.is_none());
        assert!(args.mesh_port.is_none());
        CommandParser::<RpcServerArgs>::parse_from(["smartnet"]);
    }

    #[test]
    fn rpc_mempool_and_nodeinfo_default_off() {
        let args = RpcServerArgs::default();
        assert!(!args.rpc_mempool, "RPC_MEMPOOL must default to false");
        assert!(!args.rpc_nodeinfo, "RPC_NODEINFO must default to false");
    }

    #[test]
    fn test_rpc_tx_fee_cap_parse_none() {
        // 1 ETH default cap comes from the struct defaults (config-loader path).
        assert_eq!(RpcServerArgs::default().rpc_tx_fee_cap, 1_000_000_000_000_000_000);
        CommandParser::<RpcServerArgs>::parse_from(["smartnet"]);
    }

}

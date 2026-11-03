use clap::Args;
use smartnet_rpc_server_types::constants::cache::{
    DEFAULT_BLOCK_CACHE_MAX_LEN, DEFAULT_CONCURRENT_DB_REQUESTS, DEFAULT_HEADER_CACHE_MAX_LEN,
    DEFAULT_MAX_CACHED_TX_HASHES, DEFAULT_RECEIPT_CACHE_MAX_LEN,
};

/// Parameters to configure RPC state cache.
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(next_help_heading = "RPC State Cache")]
pub struct RpcStateCacheArgs {
    /// Max number of blocks in cache.
    #[arg(skip)]
    pub max_blocks: u32,

    /// Max number receipts in cache.
    #[arg(skip)]
    pub max_receipts: u32,

    /// Max number of headers in cache.
    #[arg(skip)]
    pub max_headers: u32,

    /// Max number of concurrent database requests.
    #[arg(skip)]
    pub max_concurrent_db_requests: usize,

    /// Maximum number of transaction hashes to cache for transaction lookups.
    #[arg(skip)]
    pub max_cached_tx_hashes: u32,
}

impl RpcStateCacheArgs {
    /// Sets the Cache sizes to zero, effectively disabling caching.
    pub const fn set_zero_lengths(&mut self) {
        self.max_blocks = 0;
        self.max_receipts = 0;
        self.max_headers = 0;
    }
}

impl Default for RpcStateCacheArgs {
    fn default() -> Self {
        Self {
            max_blocks: DEFAULT_BLOCK_CACHE_MAX_LEN,
            max_receipts: DEFAULT_RECEIPT_CACHE_MAX_LEN,
            max_headers: DEFAULT_HEADER_CACHE_MAX_LEN,
            max_concurrent_db_requests: DEFAULT_CONCURRENT_DB_REQUESTS,
            max_cached_tx_hashes: DEFAULT_MAX_CACHED_TX_HASHES,
        }
    }
}

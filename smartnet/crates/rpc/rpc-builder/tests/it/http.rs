#![allow(unreachable_pub)]
//! Standalone http tests

use crate::utils::{launch_http, launch_http_ws, launch_ws};
use alloy_primitives::{Address, Bytes, U256, U64};
use alloy_rpc_types_eth::{
    Block, FeeHistory, Filter, Log, PendingTransactionFilterKind, SyncStatus, Transaction,
    TransactionReceipt,
};
use jsonrpsee::{
    core::{
        client::{ClientT, SubscriptionClientT},
        params::ArrayParams,
    },
    http_client::HttpClient,
    rpc_params,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use smartnet_rpc_api::{clients::NetApiClient, EthFilterApiClient, Web3ApiClient};
use smartnet_rpc_server_types::SmartnetRpcModule;
async fn test_rpc_call_ok<R>(client: &HttpClient, method_name: &str, params: ArrayParams)
where
    R: DeserializeOwned,
{
    // Make the RPC request
    match client.request::<R, _>(method_name, params).await {
        Ok(_) => {} // If the request is successful, do nothing
        Err(e) => {
            // If an error occurs, panic with the error message
            panic!("Expected successful response, got error: {e:?}");
        }
    }
}

async fn test_rpc_call_err<R>(client: &HttpClient, method_name: &str, params: ArrayParams)
where
    R: DeserializeOwned + std::fmt::Debug,
{
    // Make the RPC request
    if let Ok(resp) = client.request::<R, _>(method_name, params).await {
        // Panic if an unexpected successful response is received
        panic!("Expected error response, got successful response: {resp:?}");
    };
}

/// Represents a builder for creating JSON-RPC requests.
#[derive(Clone, Serialize, Deserialize)]
pub struct RawRpcParamsBuilder {
    method: Option<String>,
    params: Vec<Value>,
    id: i32,
}

impl RawRpcParamsBuilder {
    /// Sets the method name for the JSON-RPC request.
    pub fn method(mut self, method: impl Into<String>) -> Self {
        self.method = Some(method.into());
        self
    }

    /// Adds a parameter to the JSON-RPC request.
    pub fn add_param<S: Serialize>(mut self, param: S) -> Self {
        self.params.push(serde_json::to_value(param).expect("Failed to serialize parameter"));
        self
    }

    /// Sets the ID for the JSON-RPC request.
    pub const fn set_id(mut self, id: i32) -> Self {
        self.id = id;
        self
    }

    /// Constructs the JSON-RPC request string based on the provided configurations.
    pub fn build(self) -> String {
        let Self { method, params, id } = self;
        let method = method.unwrap_or_else(|| panic!("JSON-RPC method not set"));
        let params: Vec<String> = params.into_iter().map(|p| p.to_string()).collect();

        format!(
            r#"{{"jsonrpc":"2.0","id":{},"method":"{}","params":[{}]}}"#,
            id,
            method,
            params.join(",")
        )
    }
}

impl Default for RawRpcParamsBuilder {
    fn default() -> Self {
        Self { method: None, params: Vec::new(), id: 1 }
    }
}

async fn test_filter_calls<C>(client: &C)
where
    C: ClientT + SubscriptionClientT + Sync,
{
    EthFilterApiClient::<Transaction>::new_filter(client, Filter::default()).await.unwrap();
    EthFilterApiClient::<Transaction>::new_pending_transaction_filter(client, None).await.unwrap();
    EthFilterApiClient::<Transaction>::new_pending_transaction_filter(
        client,
        Some(PendingTransactionFilterKind::Full),
    )
    .await
    .unwrap();
    let id = EthFilterApiClient::<Transaction>::new_block_filter(client).await.unwrap();
    EthFilterApiClient::<Transaction>::filter_changes(client, id.clone()).await.unwrap();
    EthFilterApiClient::<Transaction>::logs(client, Filter::default()).await.unwrap();
    let id =
        EthFilterApiClient::<Transaction>::new_filter(client, Filter::default()).await.unwrap();
    EthFilterApiClient::<Transaction>::filter_logs(client, id.clone()).await.unwrap();
    EthFilterApiClient::<Transaction>::uninstall_filter(client, id).await.unwrap();
}
async fn test_basic_net_calls<C>(client: &C)
where
    C: ClientT + SubscriptionClientT + Sync,
{
    NetApiClient::version(client).await.unwrap();
    NetApiClient::peer_count(client).await.unwrap();
    NetApiClient::is_listening(client).await.unwrap();
}
async fn test_basic_web3_calls<C>(client: &C)
where
    C: ClientT + SubscriptionClientT + Sync,
{
    Web3ApiClient::client_version(client).await.unwrap();
    Web3ApiClient::sha3(client, Bytes::default()).await.unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn test_call_filter_functions_http() {
    smartnet_tracing::init_test_tracing();

    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();
    test_filter_calls(&client).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn test_call_net_functions_http() {
    smartnet_tracing::init_test_tracing();

    let handle = launch_http(vec![SmartnetRpcModule::Net]).await;
    let client = handle.http_client().unwrap();
    test_basic_net_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_net_functions_ws() {
    smartnet_tracing::init_test_tracing();

    let handle = launch_ws(vec![SmartnetRpcModule::Net]).await;
    let client = handle.ws_client().await.unwrap();
    test_basic_net_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_net_functions_http_and_ws() {
    smartnet_tracing::init_test_tracing();

    let handle = launch_http_ws(vec![SmartnetRpcModule::Net]).await;
    let client = handle.http_client().unwrap();
    test_basic_net_calls(&client).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn test_call_web3_functions_http() {
    smartnet_tracing::init_test_tracing();

    let handle = launch_http(vec![SmartnetRpcModule::Web3]).await;
    let client = handle.http_client().unwrap();
    test_basic_web3_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_web3_functions_ws() {
    smartnet_tracing::init_test_tracing();

    let handle = launch_ws(vec![SmartnetRpcModule::Web3]).await;
    let client = handle.ws_client().await.unwrap();
    test_basic_web3_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_web3_functions_http_and_ws() {
    smartnet_tracing::init_test_tracing();

    let handle = launch_http_ws(vec![SmartnetRpcModule::Web3]).await;
    let client = handle.http_client().unwrap();
    test_basic_web3_calls(&client).await;
}
// <https://github.com/worldecomoney/smartnet/issues/5830>
#[tokio::test(flavor = "multi_thread")]
async fn test_eth_logs_args() {
    smartnet_tracing::init_test_tracing();

    let handle = launch_http_ws(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    let mut params = ArrayParams::default();
    params.insert(serde_json::json!({"blockHash":"0x58dc57ab582b282c143424bd01e8d923cddfdcda9455bad02a29522f6274a948"})).unwrap();

    let resp = client.request::<Vec<Log>, _>("eth_getLogs", params).await;
    // block does not exist
    assert!(resp.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_block_by_number_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting block by number with proper fields
    test_rpc_call_ok::<Option<Block>>(
        &client,
        "eth_getBlockByNumber",
        rpc_params!["0x1b4", true], // Block number and full transaction object flag
    )
    .await;

    // Requesting block by number with wrong fields
    test_rpc_call_err::<Option<Block>>(
        &client,
        "eth_getBlockByNumber",
        rpc_params!["0x1b4", "0x1b4"],
    )
    .await;

    // Requesting block by number with missing fields
    test_rpc_call_err::<Option<Block>>(&client, "eth_getBlockByNumber", rpc_params!["0x1b4"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_block_by_hash_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting block by hash with proper fields
    test_rpc_call_ok::<Option<Block>>(
        &client,
        "eth_getBlockByHash",
        rpc_params!["0xdc0818cf78f21a8e70579cb46a43643f78291264dda342ae31049421c82d21ae", false],
    )
    .await;

    // Requesting block by hash with wrong fields
    test_rpc_call_err::<Option<Block>>(
        &client,
        "eth_getBlockByHash",
        rpc_params!["0xdc0818cf78f21a8e70579cb46a43643f78291264dda342ae31049421c82d21ae", "0x1b4"],
    )
    .await;

    // Requesting block by hash with missing fields
    test_rpc_call_err::<Option<Block>>(
        &client,
        "eth_getBlockByHash",
        rpc_params!["0xdc0818cf78f21a8e70579cb46a43643f78291264dda342ae31049421c82d21ae"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_code_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting code at a given address with proper fields
    test_rpc_call_ok::<Bytes>(
        &client,
        "eth_getCode",
        rpc_params![
            "0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b",
            "0x2" // 2
        ],
    )
    .await;

    // Define test cases with different default block parameters
    let default_block_params = vec!["earliest", "latest", "pending"];

    // Iterate over test cases
    for param in default_block_params {
        // Requesting code at a given address with default block parameter
        test_rpc_call_ok::<Bytes>(
            &client,
            "eth_getCode",
            rpc_params!["0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b", param],
        )
        .await;
    }

    // Without block number which is optional
    test_rpc_call_ok::<Bytes>(
        &client,
        "eth_getCode",
        rpc_params!["0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b"],
    )
    .await;

    // Requesting code at a given address with invalid default block parameter
    test_rpc_call_err::<Bytes>(
        &client,
        "eth_getCode",
        rpc_params!["0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b", "finalized"],
    )
    .await;

    // Requesting code at a given address with wrong fields
    test_rpc_call_err::<Bytes>(
        &client,
        "eth_getCode",
        rpc_params!["0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b", false],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_block_number_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting block number without any parameter
    test_rpc_call_ok::<U256>(&client, "eth_blockNumber", rpc_params![]).await;

    // Define test cases with different default block parameters
    let invalid_default_block_params = vec!["finalized", "0x2"];

    // Iterate over test cases
    for param in invalid_default_block_params {
        // Requesting block number with invalid parameter should not throw an error
        test_rpc_call_ok::<U256>(&client, "eth_blockNumber", rpc_params![param]).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_chain_id_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting chain ID without any parameter
    test_rpc_call_ok::<Option<U64>>(&client, "eth_chainId", rpc_params![]).await;

    // Define test cases with different invalid parameters
    let invalid_params = vec!["finalized", "0x2"];

    // Iterate over test cases
    for param in invalid_params {
        // Requesting chain ID with invalid parameter should not throw an error
        test_rpc_call_ok::<Option<U64>>(&client, "eth_chainId", rpc_params![param]).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_syncing_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting syncing status
    test_rpc_call_ok::<Option<SyncStatus>>(&client, "eth_syncing", rpc_params![]).await;

    // Define test cases with invalid parameters
    let invalid_params = vec!["latest", "earliest", "pending", "0x2"];

    // Iterate over test cases
    for param in invalid_params {
        // Requesting syncing status with invalid parameter should not throw an error
        test_rpc_call_ok::<Option<SyncStatus>>(&client, "eth_syncing", rpc_params![param]).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_protocol_version_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting protocol version without any parameter
    test_rpc_call_ok::<U64>(&client, "eth_protocolVersion", rpc_params![]).await;

    // Define test cases with invalid parameters
    let invalid_params = vec!["latest", "earliest", "pending", "0x2"];

    // Iterate over test cases
    for param in invalid_params {
        // Requesting protocol version with invalid parameter should not throw an error
        test_rpc_call_ok::<U64>(&client, "eth_protocolVersion", rpc_params![param]).await;
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn test_eth_accounts_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting accounts without any parameter
    test_rpc_call_ok::<Option<Vec<Address>>>(&client, "eth_accounts", rpc_params![]).await;

    // Define test cases with invalid parameters
    let invalid_params = vec!["latest", "earliest", "pending", "0x2"];

    // Iterate over test cases
    for param in invalid_params {
        // Requesting accounts with invalid parameter should not throw an error
        test_rpc_call_ok::<Option<Vec<Address>>>(&client, "eth_accounts", rpc_params![param]).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_block_transaction_count_by_hash_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting transaction count by block hash with proper fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getBlockTransactionCountByHash",
        rpc_params!["0xb903239f8543d04b5dc1ba6579132b143087c68db1b2168786408fcbce568238"],
    )
    .await;

    // Requesting transaction count by block hash with additional fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getBlockTransactionCountByHash",
        rpc_params!["0xb903239f8543d04b5dc1ba6579132b143087c68db1b2168786408fcbce568238", true],
    )
    .await;

    // Requesting transaction count by block hash with missing fields
    test_rpc_call_err::<Option<U256>>(&client, "eth_getBlockTransactionCountByHash", rpc_params![])
        .await;

    // Requesting transaction count by block hash with wrong field
    test_rpc_call_err::<Option<U256>>(
        &client,
        "eth_getBlockTransactionCountByHash",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_block_transaction_count_by_number_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting transaction count by block number with proper fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getBlockTransactionCountByNumber",
        rpc_params!["0xe8"],
    )
    .await;

    // Requesting transaction count by block number with additional fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getBlockTransactionCountByNumber",
        rpc_params!["0xe8", true],
    )
    .await;

    // Requesting transaction count by block number with missing fields
    test_rpc_call_err::<Option<U256>>(
        &client,
        "eth_getBlockTransactionCountByNumber",
        rpc_params![],
    )
    .await;

    // Requesting transaction count by block number with wrong field
    test_rpc_call_err::<Option<U256>>(
        &client,
        "eth_getBlockTransactionCountByNumber",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_uncle_count_by_block_hash_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting uncle count by block hash with proper fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getUncleCountByBlockHash",
        rpc_params!["0xb903239f8543d04b5dc1ba6579132b143087c68db1b2168786408fcbce568238"],
    )
    .await;

    // Requesting uncle count by block hash with additional fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getUncleCountByBlockHash",
        rpc_params!["0xb903239f8543d04b5dc1ba6579132b143087c68db1b2168786408fcbce568238", true],
    )
    .await;

    // Requesting uncle count by block hash with missing fields
    test_rpc_call_err::<Option<U256>>(&client, "eth_getUncleCountByBlockHash", rpc_params![]).await;

    // Requesting uncle count by block hash with wrong field
    test_rpc_call_err::<Option<U256>>(&client, "eth_getUncleCountByBlockHash", rpc_params![true])
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_uncle_count_by_block_number_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting uncle count by block number with proper fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getUncleCountByBlockNumber",
        rpc_params!["0xe8"],
    )
    .await;

    // Requesting uncle count by block number with additional fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getUncleCountByBlockNumber",
        rpc_params!["0xe8", true],
    )
    .await;

    // Requesting uncle count by block number with missing fields
    test_rpc_call_err::<Option<U256>>(&client, "eth_getUncleCountByBlockNumber", rpc_params![])
        .await;

    // Requesting uncle count by block number with wrong field
    test_rpc_call_err::<Option<U256>>(&client, "eth_getUncleCountByBlockNumber", rpc_params![true])
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_block_receipts_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting block receipts by block hash with proper fields
    test_rpc_call_ok::<Option<Vec<TransactionReceipt>>>(
        &client,
        "eth_getBlockReceipts",
        rpc_params!["0xe8"],
    )
    .await;

    // Requesting block receipts by block hash with additional fields
    test_rpc_call_ok::<Option<Vec<TransactionReceipt>>>(
        &client,
        "eth_getBlockReceipts",
        rpc_params!["0xe8", true],
    )
    .await;

    // Requesting block receipts by block hash with missing fields
    test_rpc_call_err::<Option<Vec<TransactionReceipt>>>(
        &client,
        "eth_getBlockReceipts",
        rpc_params![],
    )
    .await;

    // Requesting block receipts by block hash with wrong field
    test_rpc_call_err::<Option<Vec<TransactionReceipt>>>(
        &client,
        "eth_getBlockReceipts",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_uncle_by_block_hash_and_index_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting uncle by block hash and index with proper fields
    test_rpc_call_ok::<Option<Block>>(
        &client,
        "eth_getUncleByBlockHashAndIndex",
        rpc_params!["0xc6ef2fc5426d6ad6fd9e2a26abeab0aa2411b7ab17f30a99d3cb96aed1d1055b", "0x0"],
    )
    .await;

    // Requesting uncle by block hash and index with additional fields
    test_rpc_call_ok::<Option<Block>>(
        &client,
        "eth_getUncleByBlockHashAndIndex",
        rpc_params![
            "0xc6ef2fc5426d6ad6fd9e2a26abeab0aa2411b7ab17f30a99d3cb96aed1d1055b",
            "0x0",
            true
        ],
    )
    .await;

    // Requesting uncle by block hash and index with missing fields
    test_rpc_call_err::<Option<Block>>(&client, "eth_getUncleByBlockHashAndIndex", rpc_params![])
        .await;

    // Requesting uncle by block hash and index with wrong fields
    test_rpc_call_err::<Option<Block>>(
        &client,
        "eth_getUncleByBlockHashAndIndex",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_uncle_by_block_number_and_index_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting uncle by block number and index with proper fields
    test_rpc_call_ok::<Option<Block>>(
        &client,
        "eth_getUncleByBlockNumberAndIndex",
        rpc_params!["0x29c", "0x0"],
    )
    .await;

    // Requesting uncle by block number and index with additional fields
    test_rpc_call_ok::<Option<Block>>(
        &client,
        "eth_getUncleByBlockNumberAndIndex",
        rpc_params!["0x29c", "0x0", true],
    )
    .await;

    // Requesting uncle by block number and index with missing fields
    test_rpc_call_err::<Option<Block>>(&client, "eth_getUncleByBlockNumberAndIndex", rpc_params![])
        .await;

    // Requesting uncle by block number and index with wrong fields
    test_rpc_call_err::<Option<Block>>(
        &client,
        "eth_getUncleByBlockNumberAndIndex",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_transaction_by_hash_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting transaction by hash with proper fields
    test_rpc_call_ok::<Option<Transaction>>(
        &client,
        "eth_getTransactionByHash",
        rpc_params!["0x88df016429689c079f3b2f6ad39fa052532c56795b733da78a91ebe6a713944b"],
    )
    .await;

    // Requesting transaction by hash with additional fields
    test_rpc_call_ok::<Option<Transaction>>(
        &client,
        "eth_getTransactionByHash",
        rpc_params!["0x88df016429689c079f3b2f6ad39fa052532c56795b733da78a91ebe6a713944b", true],
    )
    .await;

    // Requesting transaction by hash with missing fields
    test_rpc_call_err::<Option<Transaction>>(&client, "eth_getTransactionByHash", rpc_params![])
        .await;

    // Requesting transaction by hash with wrong fields
    test_rpc_call_err::<Option<Transaction>>(
        &client,
        "eth_getTransactionByHash",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_transaction_by_block_hash_and_index_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting transaction by block hash and index with proper fields
    test_rpc_call_ok::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockHashAndIndex",
        rpc_params!["0x1dad0df7c027bcc86d06b3a6709ff78decd732c37b73123453ba7d9463eae60d", "0x0"],
    )
    .await;

    // Requesting transaction by block hash and index with additional fields
    test_rpc_call_ok::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockHashAndIndex",
        rpc_params![
            "0x1dad0df7c027bcc86d06b3a6709ff78decd732c37b73123453ba7d9463eae60d",
            "0x0",
            true
        ],
    )
    .await;

    // Requesting transaction by block hash and index with missing fields
    test_rpc_call_err::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockHashAndIndex",
        rpc_params![],
    )
    .await;

    // Requesting transaction by block hash and index with wrong fields
    test_rpc_call_err::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockHashAndIndex",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_transaction_by_block_number_and_index_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting transaction by block number and index with proper fields
    test_rpc_call_ok::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockNumberAndIndex",
        rpc_params!["0x29c", "0x0"],
    )
    .await;

    // Requesting transaction by block number and index with additional fields
    test_rpc_call_ok::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockNumberAndIndex",
        rpc_params!["0x29c", "0x0", true],
    )
    .await;

    // Requesting transaction by block number and index with missing fields
    test_rpc_call_err::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockNumberAndIndex",
        rpc_params![],
    )
    .await;

    // Requesting transaction by block number and index with wrong fields
    test_rpc_call_err::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockNumberAndIndex",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_transaction_receipt_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting transaction receipt by transaction hash with proper fields
    test_rpc_call_ok::<Option<TransactionReceipt>>(
        &client,
        "eth_getTransactionReceipt",
        rpc_params!["0x85d995eba9763907fdf35cd2034144dd9d53ce32cbec21349d4b12823c6860c5"],
    )
    .await;

    // Requesting transaction receipt by transaction hash with additional fields
    test_rpc_call_ok::<Option<TransactionReceipt>>(
        &client,
        "eth_getTransactionReceipt",
        rpc_params!["0x85d995eba9763907fdf35cd2034144dd9d53ce32cbec21349d4b12823c6860c5", true],
    )
    .await;

    // Requesting transaction receipt by transaction hash with missing fields
    test_rpc_call_err::<Option<TransactionReceipt>>(
        &client,
        "eth_getTransactionReceipt",
        rpc_params![],
    )
    .await;

    // Requesting transaction receipt by transaction hash with wrong fields
    test_rpc_call_err::<Option<TransactionReceipt>>(
        &client,
        "eth_getTransactionReceipt",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_balance_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Vec of block number items
    let block_number = vec!["latest", "earliest", "pending", "0x2"];

    // Iterate over test cases
    for param in block_number {
        // Requesting balance by address with proper fields
        test_rpc_call_ok::<U256>(
            &client,
            "eth_getBalance",
            rpc_params!["0x407d73d8a49eeb85d32cf465507dd71d507100c1", param],
        )
        .await;
    }

    // Requesting balance by address with additional fields
    test_rpc_call_ok::<U256>(
        &client,
        "eth_getBalance",
        rpc_params!["0x407d73d8a49eeb85d32cf465507dd71d507100c1", "latest", true],
    )
    .await;

    // Requesting balance by address without block number which is optional
    test_rpc_call_ok::<U256>(
        &client,
        "eth_getBalance",
        rpc_params!["0x407d73d8a49eeb85d32cf465507dd71d507100c1"],
    )
    .await;

    // Requesting balance by address with no field
    test_rpc_call_err::<U256>(&client, "eth_getBalance", rpc_params![]).await;

    // Requesting balance by address with wrong fields
    test_rpc_call_err::<U256>(
        &client,
        "eth_getBalance",
        rpc_params![true], // Incorrect parameters
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_storage_at_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Vec of block number items
    let block_number = vec!["latest", "earliest", "pending", "0x2"];

    // Iterate over test cases
    for param in block_number {
        // Requesting storage at a given address with proper fields
        test_rpc_call_ok::<Bytes>(
            &client,
            "eth_getStorageAt",
            rpc_params![
                "0x295a70b2de5e3953354a6a8344e616ed314d7251", // Address
                "0x0",                                        // Position in the storage
                param                                         // Block number or tag
            ],
        )
        .await;
    }

    // Requesting storage at a given address with additional fields
    test_rpc_call_ok::<Bytes>(
        &client,
        "eth_getStorageAt",
        rpc_params![
            "0x295a70b2de5e3953354a6a8344e616ed314d7251", // Address
            "0x0",                                        // Position in the storage
            "latest",                                     // Block number or tag
            true                                          // Additional field
        ],
    )
    .await;

    // Requesting storage at a given address without block number which is optional
    test_rpc_call_ok::<Bytes>(
        &client,
        "eth_getStorageAt",
        rpc_params![
            "0x295a70b2de5e3953354a6a8344e616ed314d7251", // Address
            "0x0"                                         // Position in the storage
        ],
    )
    .await;

    // Requesting storage at a given address with no field
    test_rpc_call_err::<Bytes>(&client, "eth_getStorageAt", rpc_params![]).await;

    // Requesting storage at a given address with wrong fields
    test_rpc_call_err::<Bytes>(
        &client,
        "eth_getStorageAt",
        rpc_params![
            "0x295a70b2de5e3953354a6a8344e616ed314d7251", // Address
            "0x0",                                        // Position in the storage
            "not_valid_block_number"                      // Block number or tag
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_transaction_count_rpc_call() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Vec of block number items
    let block_number = vec!["latest", "earliest", "pending", "0x2"];

    // Iterate over test cases
    for param in block_number {
        // Requesting transaction count by address with proper fields
        test_rpc_call_ok::<U256>(
            &client,
            "eth_getTransactionCount",
            rpc_params![
                "0x407d73d8a49eeb85d32cf465507dd71d507100c1", // Address
                param                                         // Block number or tag
            ],
        )
        .await;
    }

    // Requesting transaction count by address with additional fields
    test_rpc_call_ok::<U256>(
        &client,
        "eth_getTransactionCount",
        rpc_params![
            "0x407d73d8a49eeb85d32cf465507dd71d507100c1", // Address
            "latest",                                     // Block number or tag
            true                                          // Additional field
        ],
    )
    .await;

    // Requesting transaction count by address without block number which is optional
    test_rpc_call_ok::<U256>(
        &client,
        "eth_getTransactionCount",
        rpc_params![
            "0x407d73d8a49eeb85d32cf465507dd71d507100c1" // Address
        ],
    )
    .await;

    // Requesting transaction count by address with no field
    test_rpc_call_err::<U256>(&client, "eth_getTransactionCount", rpc_params![]).await;

    // Requesting transaction count by address with wrong fields
    test_rpc_call_err::<U256>(
        &client,
        "eth_getTransactionCount",
        rpc_params!["0x407d73d8a49eeb85d32cf465507dd71d507100c1", "not_valid_block_number"], // Incorrect parameters
    )
    .await;
}

#[test]
fn test_rpc_builder_basic() {
    let rpc_string = RawRpcParamsBuilder::default()
        .method("eth_getBalance")
        .add_param("0xaa00000000000000000000000000000000000000")
        .add_param("0x898753d8fdd8d92c1907ca21e68c7970abd290c647a202091181deec3f30a0b2")
        .set_id(1)
        .build();

    let expected = r#"{"jsonrpc":"2.0","id":1,"method":"eth_getBalance","params":["0xaa00000000000000000000000000000000000000","0x898753d8fdd8d92c1907ca21e68c7970abd290c647a202091181deec3f30a0b2"]}"#;
    assert_eq!(rpc_string, expected, "RPC string did not match expected format.");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_fee_history_raw() {
    smartnet_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http(vec![SmartnetRpcModule::Eth]).await;
    let client = handle.http_client().unwrap();

    // Requesting block by number with proper fields
    test_rpc_call_ok::<Option<FeeHistory>>(
        &client,
        "eth_feeHistory",
        rpc_params!["0x0", "latest", [0]],
    )
    .await;
}

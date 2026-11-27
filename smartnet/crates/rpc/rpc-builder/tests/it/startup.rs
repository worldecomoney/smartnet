//! Startup tests — HTTP only: RPC is HTTP on `SmartNet` (operator ruling 2026-11-19);
//! `WebSocket` carries the mesh binary protocol and never JSON-RPC.

use std::io;

use smartnet_rpc_builder::{
    error::{RpcError, ServerKind},
    RpcServerConfig, TransportRpcModuleConfig,
};
use smartnet_rpc_server_types::SmartnetRpcModule;
use smartnet_tasks::EventSender;

use crate::utils::{launch_http, test_rpc_builder};

fn is_addr_in_use_kind(err: &RpcError, kind: ServerKind) -> bool {
    match err {
        RpcError::AddressAlreadyInUse { kind: k, error } => {
            *k == kind && error.kind() == io::ErrorKind::AddrInUse
        }
        _ => false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_http_addr_in_use() {
    let handle = launch_http(vec![SmartnetRpcModule::Web3]).await;
    let addr = handle.http_local_addr().unwrap();
    let builder = test_rpc_builder();
    let eth_api = builder.bootstrap_eth_api();
    let server = builder.build(
        TransportRpcModuleConfig::set_http(vec![SmartnetRpcModule::Web3]),
        eth_api,
        EventSender::new(1),
    );
    let result =
        RpcServerConfig::http(Default::default()).with_http_address(addr).start(&server).await;
    let err = result.unwrap_err();
    assert!(is_addr_in_use_kind(&err, ServerKind::Http(addr)), "{err}");
}

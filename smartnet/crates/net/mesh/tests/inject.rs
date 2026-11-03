//! Local RPC injection: exact JSON-RPC methods/params the node's own endpoint receives.
//!
//! This is the temporary node import path, not mesh wire: mesh wire is binary `WebSocket`.

mod common;

use common::{install_tls, spawn_http_mock};
use smartnet_mesh::inject::{Injector, LocalRpc};
use std::time::Duration;

/// The JSON-RPC request recorded by the mock endpoint, parsed.
fn recorded_call(recorded: &common::Recorded) -> (String, serde_json::Value) {
    let requests = recorded.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    let value: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json rpc");
    let method = value["method"].as_str().expect("method").to_string();
    (method, value["params"].clone())
}

fn rpc_url(addr: std::net::SocketAddr) -> String {
    format!("http://{addr}")
}

#[tokio::test]
async fn send_raw_transaction_calls_eth_send_raw_transaction() {
    install_tls();
    let (addr, recorded) =
        spawn_http_mock(vec![(200, r#"{"jsonrpc":"2.0","id":1,"result":"0xabc"}"#.to_string())])
            .await;
    let rpc = LocalRpc::with_timeout(rpc_url(addr), Duration::from_secs(5));

    rpc.send_raw_transaction(&alloy_primitives::Bytes::from(vec![0x01, 0x02])).await.expect("ok");

    let (method, params) = recorded_call(&recorded);
    assert_eq!(method, "eth_sendRawTransaction");
    assert_eq!(params, serde_json::json!(["0x0102"]));
}

#[tokio::test]
async fn already_known_transaction_is_benign() {
    install_tls();
    let (addr, _recorded) = spawn_http_mock(vec![(
        200,
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"already known"}}"#.to_string(),
    )])
    .await;
    let rpc = LocalRpc::with_timeout(rpc_url(addr), Duration::from_secs(5));

    rpc.send_raw_transaction(&alloy_primitives::Bytes::from(vec![0x01])).await.expect("benign");
}

#[tokio::test]
async fn real_transaction_error_is_reported() {
    install_tls();
    let (addr, _recorded) = spawn_http_mock(vec![(
        200,
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"nonce too low"}}"#.to_string(),
    )])
    .await;
    let rpc = LocalRpc::with_timeout(rpc_url(addr), Duration::from_secs(5));

    let err = rpc
        .send_raw_transaction(&alloy_primitives::Bytes::from(vec![0x01]))
        .await
        .expect_err("must fail");
    assert!(err.contains("nonce too low"), "{err}");
}

#[tokio::test]
async fn new_payload_calls_smartnet_new_payload_with_flags() {
    install_tls();
    let (addr, recorded) = spawn_http_mock(vec![(
        200,
        r#"{"jsonrpc":"2.0","id":1,"result":{"status":"VALID"}}"#.to_string(),
    )])
    .await;
    let rpc = LocalRpc::with_timeout(rpc_url(addr), Duration::from_secs(5));

    rpc.new_payload(&alloy_primitives::Bytes::from(vec![0xf9, 0x01])).await.expect("ok");

    let (method, params) = recorded_call(&recorded);
    assert_eq!(method, "smartnet_newPayload");
    // [block_rlp, wait_for_persistence=true, wait_for_caches=true] — the validator path.
    assert_eq!(params, serde_json::json!(["0xf901", true, true]));
}

#[tokio::test]
async fn new_payload_error_is_reported() {
    install_tls();
    let (addr, _recorded) = spawn_http_mock(vec![(
        200,
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-38000,"message":"payload invalid"}}"#
            .to_string(),
    )])
    .await;
    let rpc = LocalRpc::with_timeout(rpc_url(addr), Duration::from_secs(5));

    let err =
        rpc.new_payload(&alloy_primitives::Bytes::from(vec![0xc0])).await.expect_err("must fail");
    assert!(err.contains("payload invalid"), "{err}");
}

#[tokio::test]
async fn garbage_response_is_reported() {
    install_tls();
    let (addr, _recorded) = spawn_http_mock(vec![(200, "not json".to_string())]).await;
    let rpc = LocalRpc::with_timeout(rpc_url(addr), Duration::from_secs(5));

    let err = rpc
        .send_raw_transaction(&alloy_primitives::Bytes::from(vec![0x01]))
        .await
        .expect_err("must fail");
    assert!(err.contains("rpc decode"), "{err}");
}

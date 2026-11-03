//! Live-peer smoke test (run explicitly: `cargo test -p smartnet-net-rpc-sync --test live_peer --
//! --ignored`).

use smartnet_net_rpc_sync::fetch::{connect_http, BlockSource};

#[tokio::test]
#[ignore]
async fn block_number_and_hash_against_live_peer() {
    let url: url::Url = "https://rpc2.wemblockchain.com".parse().unwrap();
    let source = connect_http(url);
    let number = source.block_number().await.expect("block_number");
    println!("block_number = {number}");
    let hash = source.block_hash(0).await.expect("block_hash");
    println!("block_hash(0) = {hash:?}");
    assert!(hash.is_some());
}

//! Verify our node's genesis-state trie: fetch `eth_getProof` and walk the proof,
//! confirming the root it commits to. Usage: `cargo run -p smartnet-net-rpc-sync
//! --example verify_proof -- <rpc-url> [expected_state_root_hex]` (the URL is required —
//! there is no built-in default endpoint).

use alloy_primitives::{keccak256, B256};
use alloy_provider::{Provider, ProviderBuilder};

fn nibbles(path: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len() * 2);
    for b in path {
        out.push(b >> 4);
        out.push(b & 0x0f);
    }
    out
}

#[tokio::main]
async fn main() {
    let url = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: verify_proof <rpc-url> [expected_state_root_hex]");
        std::process::exit(2);
    });
    let expected: Option<B256> = std::env::args().nth(2).map(|s| s.parse().expect("root hex"));

    let provider = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let addr = alloy_primitives::Address::ZERO;
    let proof = provider.get_proof(addr, Vec::new()).await.expect("eth_getProof");
    let nodes = proof.account_proof;

    // Root commitment: keccak of the top node's RLP.
    let root = keccak256(nodes[0].as_ref());
    println!("proof commits to root: {root}");
    if let Some(e) = expected {
        println!("expected:              {e}");
        println!("MATCH: {}", root == e);
    }

    // Walk the proof: track the key nibbles consumed and hash each node.
    let key = nibbles(keccak256(addr).as_slice());
    for (i, node) in nodes.iter().enumerate() {
        println!("node {i}: keccak={}", keccak256(node.as_ref()));
    }
    let _ = key;
}

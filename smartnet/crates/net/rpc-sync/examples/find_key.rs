//! Find which genesis alloc address hashes to a given trie key prefix.
//! Usage: `cargo run -p smartnet-net-rpc-sync --example find_key -- <genesis.json> <key_prefix_hex>`

use alloy_primitives::{keccak256, Address};

#[tokio::main]
async fn main() {
    let genesis_path = std::env::args().nth(1).unwrap_or_else(|| "genesis.json".into());
    let prefix = std::env::args().nth(2).expect("trie key prefix hex (without 0x)");
    let raw = std::fs::read_to_string(&genesis_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let Some(alloc) = v.get("alloc").and_then(|a| a.as_object()) else { panic!("no alloc") };
    for (addr, _val) in alloc {
        let address: Address = addr.parse().expect("addr");
        let hashed = keccak256(address);
        let hex = format!("{hashed:x}");
        if hex.starts_with(&prefix.to_ascii_lowercase()) {
            println!("MATCH: {address} -> keccak {hashed}");
        }
    }
    println!("done");
    let phantom: Address = "0x9400000000000000000000000000000000000000".parse().unwrap();
    println!("keccak(0x94..00) = {}", keccak256(phantom));
}
// appended check placeholder

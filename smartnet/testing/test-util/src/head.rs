//! Query canonical head for probe workflow (which `N.dat` to build next).

use eyre::{Context, Result};
use serde_json::Value;

pub(crate) async fn fetch_head(node_url: &str) -> Result<(u64, String)> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getBlockByNumber",
        "params": ["latest", false]
    });
    let client = reqwest::Client::new();
    let resp: Value = client
        .post(node_url)
        .json(&body)
        .send()
        .await
        .context("POST eth_getBlockByNumber")?
        .json()
        .await
        .context("parse JSON")?;
    let result = resp.get("result").ok_or_else(|| eyre::eyre!("missing result in RPC response"))?;
    if result.is_null() {
        eyre::bail!("no latest block (null result)");
    }
    let number_hex = result
        .get("number")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("missing number in block"))?;
    let hash = result
        .get("hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("missing hash in block"))?;
    let number = u64::from_str_radix(number_hex.strip_prefix("0x").unwrap_or(number_hex), 16)
        .map_err(|e| eyre::eyre!("parse block number: {e}"))?;
    Ok((number, hash.to_string()))
}

pub(crate) async fn print_head(node_url: &str) -> Result<()> {
    let (number, hash) = fetch_head(node_url).await?;
    let next = number + 1;
    // Probe files: block N → `(15 + N).dat` (e.g. block #1 → `16.dat`, block #2 → `17.dat`).
    let probe_n = next + 15;
    println!("node_url: {node_url}");
    println!("latest_number: {number}");
    println!("latest_hash: {hash}");
    println!("next_block_number: {next}");
    println!(
        "suggested_dump: ./{probe_n}.dat  (run-once --dump-payload ./crates/smartnet-test-util/payload-data/{probe_n}.dat)"
    );
    Ok(())
}

//! SmartNet Rust tools (test-key only — see tools/README.md for the key exception).

pub mod args;
pub mod keys;
pub mod rpc;
pub mod stats;
pub mod tx;

/// Load-test chain id (same as go-tools).
pub const CHAIN_ID: u64 = 315;
/// Chain-fixed gas price (SMARTNET_FIXED_GAS_PRICE_WEI).
pub const GAS_PRICE_WEI: u128 = 238_095_238_095;
/// Default local RPC endpoint.
pub const DEFAULT_RPC: &str = "http://127.0.0.1:8545";

/// Lowercase hex encoding with `0x` prefix (no external deps).
pub fn hex0x(bytes: &[u8]) -> String {
    const T: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        s.push(T[(b >> 4) as usize] as char);
        s.push(T[(b & 0x0f) as usize] as char);
    }
    s
}

/// Decode hex (with or without `0x`) into bytes.
pub fn unhex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if s.len() % 2 != 0 {
        return Err("hex: odd length".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| format!("hex: {e}")))
        .collect()
}

//! Legacy (EIP-155) tx build + local signing — same shape as go-tools (type-0,
//! chain-fixed gas price). Per-step timings for the benchmark.

use alloy_consensus::{SignableTransaction, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{keccak256, Address, Bytes, B256, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use std::str::FromStr;
use std::time::Instant;

use crate::{hex0x, CHAIN_ID, GAS_PRICE_WEI};

/// One signed tx plus per-step timings (microseconds).
pub struct SignedTx {
    pub hash: B256,
    /// RLP-encoded type-0 envelope, `0x`-prefixed — payload for `eth_sendRawTransaction`.
    pub raw_hex: String,
    pub sign_us: u128,
    pub encode_us: u128,
}

fn parse_key(priv_hex: &str) -> Result<PrivateKeySigner, String> {
    let b = B256::from_str(priv_hex.trim())
        .map_err(|e| format!("private key: {e}"))?;
    PrivateKeySigner::from_bytes(&b).map_err(|e| format!("signer: {e}"))
}

/// Address for a TEST key (hex, optional 0x).
pub fn key_address(priv_hex: &str) -> Result<Address, String> {
    Ok(parse_key(priv_hex)?.address())
}

/// Build + sign a legacy tx with the chain defaults (chain 315, fixed protocol gas price).
pub fn sign_legacy(
    priv_hex: &str,
    nonce: u64,
    to: TxKind,
    value: U256,
    gas_limit: u64,
    input: Bytes,
) -> Result<SignedTx, String> {
    sign_legacy_ex(priv_hex, nonce, to, value, gas_limit, input, CHAIN_ID, GAS_PRICE_WEI)
}

/// Build + sign a legacy tx with explicit chain id / gas price (chainkit flag parity:
/// `--chain`, `--gas-price N`).
pub fn sign_legacy_ex(
    priv_hex: &str,
    nonce: u64,
    to: TxKind,
    value: U256,
    gas_limit: u64,
    input: Bytes,
    chain_id: u64,
    gas_price: u128,
) -> Result<SignedTx, String> {
    let signer = parse_key(priv_hex)?;
    let tx = TxLegacy {
        chain_id: Some(chain_id),
        nonce,
        gas_price,
        gas_limit,
        to,
        value,
        input,
    };

    let t0 = Instant::now();
    let sighash = tx.signature_hash();
    let sig = signer.sign_hash_sync(&sighash).map_err(|e| format!("sign: {e}"))?;
    let sign_us = t0.elapsed().as_micros();

    let t1 = Instant::now();
    let raw = tx.into_signed(sig).encoded_2718();
    let encode_us = t1.elapsed().as_micros();

    Ok(SignedTx {
        hash: keccak256(&raw),
        raw_hex: hex0x(&raw),
        sign_us,
        encode_us,
    })
}

/// ERC-20 `transfer(address,uint256)` calldata (selector + padded args).
pub fn erc20_transfer_calldata(to: Address, amount: U256) -> Bytes {
    let mut data = Vec::with_capacity(4 + 64);
    data.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]); // keccak("transfer(address,uint256)")[..4]
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(to.as_slice());
    data.extend_from_slice(&amount.to_be_bytes::<32>());
    Bytes::from(data)
}

/// ERC-20 `balanceOf(address)` calldata.
pub fn erc20_balance_of_calldata(holder: Address) -> Bytes {
    let mut data = Vec::with_capacity(4 + 32);
    data.extend_from_slice(&[0x70, 0xa0, 0x82, 0x31]);
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(holder.as_slice());
    Bytes::from(data)
}

/// Parse a `0x…` hex quantity (balance, call result) into `U256`.
pub fn parse_u256_hex(v: &serde_json::Value) -> Result<U256, String> {
    let s = v.as_str().ok_or("expected hex string")?;
    U256::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| format!("hex U256: {e}"))
}

//! ERC-20 token metadata capture — the three view calls behind `{hash}.meta`.
//!
//! Probed at ingest — the import's range executor and the engine's block executor, the
//! same places runtime bytecode is captured (operator, 2026-11-19: "token meta must be
//! captured from import/sync same way as bytecode"). Each contract with code is asked
//! `name()` / `symbol()` / `decimals()` on the post-block state; the answers ride the
//! executed block to the commit, where the sidecar lands beside the bytecode file.
//!
//! Everything here is diagnostic, never consensus: probes are system calls (no fee, no
//! nonce, no balance rules, gas bounded by the system-call limit), their state is
//! discarded — nothing a probe does can change what the block committed — and a
//! contract that does not answer like a token simply gets no metadata.

use alloc::{collections::BTreeMap, format, string::String, vec::Vec};

use alloy_evm::Evm as _;
use alloy_primitives::{Address, Bytes, KECCAK256_EMPTY, U256};
use revm::{context::result::{ExecutionResult, Output}, database::BundleState};
use smartnet_primitives_traits::TokenMeta;

use crate::{rtbc::solc_version, rtbc_inspector::rtbc, ConfigureEvm, EvmEnvFor};

/// `name()` — `bytes4(keccak256("name()"))`.
const NAME_SELECTOR: [u8; 4] = [0x06, 0xfd, 0xde, 0x03];
/// `symbol()` — `bytes4(keccak256("symbol()"))`.
const SYMBOL_SELECTOR: [u8; 4] = [0x95, 0xd8, 0x9b, 0x41];
/// `decimals()` — `bytes4(keccak256("decimals()"))`.
const DECIMALS_SELECTOR: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];
/// `owner()` — `bytes4(keccak256("owner()"))`.
const OWNER_SELECTOR: [u8; 4] = [0x8d, 0xa5, 0xcb, 0x5b];
/// `totalSupply()` — `bytes4(keccak256("totalSupply()"))`.
const TOTAL_SUPPLY_SELECTOR: [u8; 4] = [0x18, 0x16, 0x0d, 0xdd];

/// Longest metadata string kept, in bytes — a hostile contract can return megabytes.
const MAX_STRING: usize = 256;

/// THE contract-metadata entrance — the one place both ingest paths call (operator,
/// 2026-11-19: "make sure all import and mesh sync both go through this same single
/// entrance"): the import's range executor and mesh sync's block executor both hand the
/// block's state changes and the create log here, and everything else follows — finding
/// the code-carrying contracts, pairing each with its creator, probing the views, and
/// returning the records that ride the block to the commit.
///
/// `first_sight` gates re-probes — the import passes its per-range dedupe set, the
/// engine (one block at a time) passes `|_| true`.
pub fn capture_block_meta<EvmCfg, DB>(
    evm_config: &EvmCfg,
    db: &mut DB,
    evm_env: &EvmEnvFor<EvmCfg>,
    state: &BundleState,
    creators: Vec<(Address, Address)>,
    mut first_sight: impl FnMut(Address) -> bool,
) -> Vec<(Address, TokenMeta)>
where
    EvmCfg: ConfigureEvm,
    DB: alloy_evm::Database,
    EvmEnvFor<EvmCfg>: Clone,
{
    let creators: BTreeMap<Address, Address> = creators.into_iter().collect();
    let candidates: Vec<(Address, Option<Address>)> = state
        .state
        .iter()
        .filter(|(_, account)| {
            account.info.as_ref().is_some_and(|info| info.code_hash != KECCAK256_EMPTY)
        })
        .map(|(address, _)| (*address, creators.get(address).copied()))
        .filter(|(address, _)| first_sight(*address))
        .collect();
    capture_token_meta(evm_config, db, evm_env, &candidates)
}

/// Captures the contract metadata of the given contracts on the post-block state: one
/// `(address, TokenMeta)` per contract that answers like a token.
///
/// Candidates are `(address, creator)` pairs — the creator is the address that ran the
/// create frame (from the rtbc inspector's create log), `None` when unknown. The code
/// hash and the compiler stamp come from the account's code on the post-block state.
/// `decimals()` is the token tell-tale: a contract that does not return a sane `uint8`
/// gets no entry; the other views fall back to empty/zero/`None` when absent. Probing
/// once per address is the caller's job (the sidecar is write-once, so re-probing only
/// ever loses the race with itself).
pub fn capture_token_meta<EvmCfg, DB>(
    evm_config: &EvmCfg,
    db: &mut DB,
    evm_env: &EvmEnvFor<EvmCfg>,
    contracts: &[(Address, Option<Address>)],
) -> Vec<(Address, TokenMeta)>
where
    EvmCfg: ConfigureEvm,
    DB: alloy_evm::Database,
    EvmEnvFor<EvmCfg>: Clone,
{
    let mut out = Vec::new();
    for (address, creator) in contracts {
        let Some(decimals) = probe_call(evm_config, db, evm_env, *address, DECIMALS_SELECTOR)
            .and_then(|raw| decode_decimals(&raw))
        else {
            continue;
        };
        let name = probe_call(evm_config, db, evm_env, *address, NAME_SELECTOR)
            .and_then(|raw| decode_string(&raw))
            .unwrap_or_default();
        let symbol = probe_call(evm_config, db, evm_env, *address, SYMBOL_SELECTOR)
            .and_then(|raw| decode_string(&raw))
            .unwrap_or_default();
        let owner = probe_call(evm_config, db, evm_env, *address, OWNER_SELECTOR)
            .and_then(|raw| decode_address(&raw));
        let total_supply = probe_call(evm_config, db, evm_env, *address, TOTAL_SUPPLY_SELECTOR)
            .and_then(|raw| decode_u256(&raw))
            .unwrap_or_default();
        // Code hash + compiler stamp ride the account (the code is embedded at load —
        // the bytecode store is keyed by the deployed address).
        let info = db.basic(*address).ok().flatten();
        let code_hash = info.as_ref().map(|info| info.code_hash).unwrap_or_default();
        let compiler = info
            .as_ref()
            .and_then(|info| info.code.as_ref())
            .and_then(|code| solc_version(code.original_byte_slice()))
            .map(|[a, b, c]| format!("{a}.{b}.{c}"))
            .unwrap_or_default();
        out.push((
            *address,
            TokenMeta {
                name,
                symbol,
                decimals,
                total_supply,
                owner,
                creator: *creator,
                code_hash,
                compiler,
            },
        ));
    }
    out
}

/// One view call against `target` on the post-block state. Returns the raw return data;
/// `None` on revert, halt or empty output. The result state is discarded — the probe
/// can never write what the block did not.
fn probe_call<EvmCfg, DB>(
    evm_config: &EvmCfg,
    db: &mut DB,
    evm_env: &EvmEnvFor<EvmCfg>,
    target: Address,
    selector: [u8; 4],
) -> Option<Bytes>
where
    EvmCfg: ConfigureEvm,
    DB: alloy_evm::Database,
    EvmEnvFor<EvmCfg>: Clone,
{
    let mut evm = evm_config.evm_with_env_and_inspector(&mut *db, evm_env.clone(), rtbc());
    let res =
        evm.transact_system_call(Address::ZERO, target, Bytes::copy_from_slice(&selector)).ok()?;
    match res.result {
        ExecutionResult::Success { output: Output::Call(bytes), .. } if !bytes.is_empty() => {
            Some(bytes)
        }
        _ => None,
    }
}

/// Decodes a `string` return: the standard offset/length ABI form and the legacy
/// `bytes32` form older tokens use. Strings longer than [`MAX_STRING`] are truncated.
fn decode_string(out: &[u8]) -> Option<String> {
    if out.len() == 32 {
        // legacy bytes32: trailing zeros trimmed
        let end = out.iter().position(|&b| b == 0).unwrap_or(32);
        return Some(String::from_utf8_lossy(&out[..end]).into_owned());
    }
    if out.len() >= 64 {
        let offset = usize::try_from(U256::from_be_slice(&out[..32])).ok()?;
        let len_at = offset.checked_add(32)?;
        if len_at > out.len() {
            return None;
        }
        let len = usize::try_from(U256::from_be_slice(&out[offset..len_at]))
            .ok()?
            .min(MAX_STRING);
        let end = len_at.checked_add(len)?.min(out.len());
        return Some(String::from_utf8_lossy(&out[len_at..end]).into_owned());
    }
    None
}

/// Decodes an `address` return (`owner()`): a 32-byte word holding the address.
fn decode_address(out: &[u8]) -> Option<Address> {
    if out.len() != 32 {
        return None;
    }
    Some(Address::from_slice(&out[12..32]))
}

/// Decodes a `uint256` return (`totalSupply()`): a 32-byte word.
const fn decode_u256(out: &[u8]) -> Option<U256> {
    if out.len() != 32 {
        return None;
    }
    Some(U256::from_be_slice(out))
}

/// Decodes a `uint8` return (`decimals()`): a 32-byte word holding a sane exponent.
fn decode_decimals(out: &[u8]) -> Option<u8> {
    if out.len() != 32 {
        return None;
    }
    u8::try_from(U256::from_be_slice(out)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(value: u64) -> Vec<u8> {
        let mut w = [0u8; 32];
        w[24..].copy_from_slice(&value.to_be_bytes());
        w.to_vec()
    }

    #[test]
    fn selectors_are_the_canonical_ones() {
        assert_eq!(NAME_SELECTOR, alloy_primitives::keccak256(b"name()")[..4]);
        assert_eq!(SYMBOL_SELECTOR, alloy_primitives::keccak256(b"symbol()")[..4]);
        assert_eq!(DECIMALS_SELECTOR, alloy_primitives::keccak256(b"decimals()")[..4]);
        assert_eq!(OWNER_SELECTOR, alloy_primitives::keccak256(b"owner()")[..4]);
        assert_eq!(TOTAL_SUPPLY_SELECTOR, alloy_primitives::keccak256(b"totalSupply()")[..4]);
    }

    #[test]
    fn address_and_u256_decode_a_word() {
        let mut word = [0u8; 32];
        word[12..].copy_from_slice(Address::repeat_byte(0x22).as_slice());
        assert_eq!(decode_address(&word), Some(Address::repeat_byte(0x22)));
        assert_eq!(decode_address(&word[..31]), None);

        let mut word = [0u8; 32];
        word[31] = 0x2a;
        assert_eq!(decode_u256(&word), Some(U256::from(42)));
        assert_eq!(decode_u256(&[0x01, 0x02]), None);
    }

    #[test]
    fn decimals_decodes_a_uint8_word() {
        assert_eq!(decode_decimals(&word(18)), Some(18));
        assert_eq!(decode_decimals(&word(0)), Some(0));
        assert_eq!(decode_decimals(&word(256)), None, "out of uint8 range");
        assert_eq!(decode_decimals(&word(1)[..31]), None, "not a word");
        assert_eq!(decode_decimals(&[]), None);
    }

    #[test]
    fn string_decodes_the_offset_form() {
        // "WEMX": offset 32, len 4, then the bytes right-padded to a word.
        let mut out = word(32);
        out.extend(word(4));
        out.extend(b"WEMX".to_vec());
        out.extend([0u8; 28].to_vec());
        assert_eq!(decode_string(&out).as_deref(), Some("WEMX"));
    }

    #[test]
    fn string_decodes_legacy_bytes32() {
        let mut w = [0u8; 32];
        w[..5].copy_from_slice(b"WEMX\0");
        assert_eq!(decode_string(&w).as_deref(), Some("WEMX"));
    }

    #[test]
    fn string_rejects_garbage_and_truncates_giants() {
        assert_eq!(decode_string(&[0x01, 0x02]), None);
        // offset points past the data
        let mut out = word(1024);
        out.extend(word(4));
        assert_eq!(decode_string(&out), None);
        // a giant length is truncated, never allocated in full
        let mut out = word(32);
        out.extend(word(10_000_000));
        out.extend(vec![b'x'; 300]);
        assert_eq!(decode_string(&out).unwrap().len(), MAX_STRING);
        // and a short tail is read to the end of the data, not past it
        let mut out = word(32);
        out.extend(word(10_000_000));
        out.extend(vec![b'x'; 128]);
        assert_eq!(decode_string(&out).unwrap().len(), 128);
    }
}

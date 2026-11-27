//! CALIBRI forward placeholders (chair / quorum scheduling).
//!
//! **Naming:** product docs use `getChair` / `getQuorum`; in Rust use
//! [`get_chair_addresses`] / [`get_quorum_addresses`] (always [`Vec<Address>`]
//! for chair and quorum — never collapse public authority APIs to a single
//! `Address`, even when `quorum == chair` today).
//!
//! **Interim testing:** logical quorum = **all** genesis signers in `extraData`
//! (any authorized validator may seal; the node accepts or rejects the submitted
//! block). Chair (sole sender convenience) collapses to the first authorized signer.
//! **No** parent-hash-derived subgroup yet.
//!
//! **Seal wire compatibility:** [`genesis_wire_signers`] returns the
//! full signer list encoded in genesis `extraData` (checkpoint layout). Use
//! [`get_quorum_addresses`] for **authority / who may sign** at a height.
//!
//! **Future:** chair = **sole sender**; full quorum still signs; subgroup from
//! parent block hash; `n/2+1` thresholds — implement behind these entry points.

use std::fmt;
use std::string::String;
use std::vec::Vec;

use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256};

use smartnet_chainspec::parse_extra_data_validators;

/// Block height type for CALIBRI APIs (`BlockNr` in naming discussions).
pub type BlockNr = u64;

/// Errors resolving CALIBRI schedule data from chain genesis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalibriError {
    /// Failed to parse checkpoint signers from `extraData`.
    SignersParse(String),
    /// Parsed signer list was empty.
    EmptySignerSet,
}

impl fmt::Display for CalibriError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SignersParse(e) => write!(f, "CALIBRI: failed to parse signers: {e}"),
            Self::EmptySignerSet => write!(f, "CALIBRI: no signers in genesis"),
        }
    }
}

impl std::error::Error for CalibriError {}

/// Parse the genesis signer list once; centralizes the guard + parse
/// that `interim_authority`, `genesis_wire_signers`, and public helpers all need.
fn parse_validators(genesis: &Genesis) -> Result<Vec<Address>, CalibriError> {
    // The legacy `config.clique` object is not read: a chain is validator-bearing when its
    // genesis `extraData` carries a signer section — cohort presence, not config presence.
    let signers = parse_extra_data_validators(genesis.extra_data.as_ref())
        .map_err(CalibriError::SignersParse)?;
    if signers.is_empty() {
        return Err(CalibriError::EmptySignerSet);
    }
    Ok(signers)
}

fn interim_authority(genesis: &Genesis) -> Result<Address, CalibriError> {
    parse_validators(genesis)?
        .first()
        .copied()
        .ok_or(CalibriError::EmptySignerSet)
}

fn address_display_string(addr: Address) -> String {
    format!("{addr:#x}")
}

/// Full signer list from genesis `extraData` (**checkpoint wire format**).
///
/// Used when the full encoded genesis signer set is required (checkpoint wire).
/// **Not** the same as [`get_quorum_addresses`] once CALIBRI subgroup selection exists.
///
/// Callers must **not** use raw [`parse_extra_data_validators`] outside this module;
/// keep wire parsing centralized here.
pub fn genesis_wire_signers(genesis: &Genesis) -> Result<Vec<Address>, CalibriError> {
    parse_validators(genesis)
}

/// **Sole sender** (chair) addresses for `block_nr` — always a vector API.
///
/// Interim: one element, same as first wire signer. Later: chair may be a strict
/// subset or ordering may matter.
pub fn get_chair_addresses(
    genesis: &Genesis,
    _block_nr: BlockNr,
) -> Result<Vec<Address>, CalibriError> {
    Ok(vec![interim_authority(genesis)?])
}

/// **Sole sender** (chair) as hex string — convenience over [`get_chair_addresses`].
pub fn get_chair(genesis: &Genesis, block_nr: BlockNr) -> Result<String, CalibriError> {
    let chairs = get_chair_addresses(genesis, block_nr)?;
    chairs
        .first()
        .copied()
        .map(address_display_string)
        .ok_or(CalibriError::EmptySignerSet)
}

/// First chair address — convenience only; prefer [`get_chair_addresses`] in new code.
pub fn get_chair_address(genesis: &Genesis, block_nr: BlockNr) -> Result<Address, CalibriError> {
    get_chair_addresses(genesis, block_nr)?
        .into_iter()
        .next()
        .ok_or(CalibriError::EmptySignerSet)
}

/// Quorum member list for `block_nr` as string ids (always a vector).
pub fn get_quorum(genesis: &Genesis, block_nr: BlockNr) -> Result<Vec<String>, CalibriError> {
    get_quorum_addresses(genesis, block_nr)
        .map(|v| v.into_iter().map(address_display_string).collect())
}

/// Logical **quorum** at `block_nr` — who may sign a proposal at this height.
///
/// Interim: the **full** genesis signer set (any authorized validator may seal; the
/// execution node accepts or rejects the submitted block — it never produces).
/// Preserve [`Vec<Address>`]; do not replace with a single-address API elsewhere.
pub fn get_quorum_addresses(
    genesis: &Genesis,
    _block_nr: BlockNr,
) -> Result<Vec<Address>, CalibriError> {
    genesis_wire_signers(genesis)
}

/// Interim quorum threshold: `1` until derived from quorum size.
///
/// The single source of truth for the interim value — [`quorum_threshold`] returns it and the
/// validator logs it, so a future change to the policy cannot leave the two disagreeing.
pub const INTERIM_QUORUM_THRESHOLD: u64 = 1;

/// Minimum quorum signatures required for a valid block certificate.
///
/// Interim: [`INTERIM_QUORUM_THRESHOLD`]. Future: `quorum.len() / 2 + 1` (or policy from chainspec).
pub fn quorum_threshold(_genesis: &Genesis, _block_nr: BlockNr) -> u64 {
    INTERIM_QUORUM_THRESHOLD
}

/// Whether `address` is in the logical quorum for `block_nr`.
pub fn is_quorum_member(
    genesis: &Genesis,
    block_nr: BlockNr,
    address: Address,
) -> Result<bool, CalibriError> {
    Ok(get_quorum_addresses(genesis, block_nr)?.contains(&address))
}

/// Whether `address` is the designated **sole sender** (chair) for `block_nr`.
pub fn is_chair(
    genesis: &Genesis,
    block_nr: BlockNr,
    address: Address,
) -> Result<bool, CalibriError> {
    Ok(get_chair_addresses(genesis, block_nr)?.contains(&address))
}

/// Quorum subgroup after parent hash is known (future CALIBRI selection).
pub fn get_quorum_for_parent(
    genesis: &Genesis,
    _parent_hash: B256,
    block_nr: BlockNr,
) -> Result<Vec<String>, CalibriError> {
    get_quorum(genesis, block_nr)
}

/// Address form of [`get_quorum_for_parent`].
pub fn get_quorum_addresses_for_parent(
    genesis: &Genesis,
    parent_hash: B256,
    block_nr: BlockNr,
) -> Result<Vec<Address>, CalibriError> {
    let _ = parent_hash;
    get_quorum_addresses(genesis, block_nr)
}

/// `true` when genesis exposes signers so CALIBRI stubs can resolve.
pub fn calibri_schedule_available(genesis: &Genesis) -> bool {
    genesis_wire_signers(genesis).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_genesis::{ChainConfig, CliqueConfig, Genesis};
    use alloy_primitives::{Bytes, address};

    fn minimal_sealed_genesis(signer: Address) -> Genesis {
        let mut vanity = [0u8; 32];
        vanity[0] = 1;
        let mut extra = vanity.to_vec();
        extra.extend_from_slice(signer.as_slice());
        extra.extend_from_slice(&[0u8; 65]);
        Genesis {
            config: ChainConfig {
                clique: Some(CliqueConfig {
                    period: Some(5),
                    epoch: Some(30_000),
                }),
                ..Default::default()
            },
            extra_data: Bytes::from(extra),
            ..Default::default()
        }
    }

    #[test]
    fn interim_chair_and_quorum_are_vec_of_one() {
        let s = address!("0x1111111111111111111111111111111111111111");
        let g = minimal_sealed_genesis(s);
        assert_eq!(get_chair_addresses(&g, 1).unwrap(), vec![s]);
        assert_eq!(get_quorum_addresses(&g, 42).unwrap(), vec![s]);
        assert!(is_chair(&g, 0, s).unwrap());
        assert!(calibri_schedule_available(&g));
        assert_eq!(genesis_wire_signers(&g).unwrap(), vec![s]);
    }

    #[test]
    fn quorum_accepts_all_genesis_signers() {
        let a = address!("0x1111111111111111111111111111111111111111");
        let b = address!("0x2222222222222222222222222222222222222222");
        let c = address!("0x3333333333333333333333333333333333333333");
        let vanity = [0u8; 32];
        let mut extra = vanity.to_vec();
        extra.extend_from_slice(a.as_slice());
        extra.extend_from_slice(b.as_slice());
        extra.extend_from_slice(c.as_slice());
        extra.extend_from_slice(&[0u8; 65]);
        let g = Genesis {
            config: ChainConfig {
                clique: Some(CliqueConfig {
                    period: Some(5),
                    epoch: Some(30_000),
                }),
                ..Default::default()
            },
            extra_data: Bytes::from(extra),
            ..Default::default()
        };

        let quorum = get_quorum_addresses(&g, 1).unwrap();
        assert_eq!(quorum, vec![a, b, c]);
        for signer in [a, b, c] {
            assert!(is_quorum_member(&g, 1, signer).unwrap());
        }
        // Chair (sole sender convenience) is still the first signer; every
        // genesis signer may seal in the interim model.
        assert_eq!(get_chair_addresses(&g, 1).unwrap(), vec![a]);
    }

    #[test]
    fn no_signers_returns_error() {
        // A genesis with no `extraData` signer section carries no validators — error.
        let g = Genesis::default();
        assert!(get_chair(&g, 0).is_err());
    }
}

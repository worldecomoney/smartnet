//! Pre-consensus seal primitives (ECDSA over RLP header without trailing 65-byte signature).

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, U256, keccak256};
use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};

const SEAL_SIGNATURE_BYTES: usize = 65;

/// Errors returned by pre-consensus seal / signer checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalibriLiteSealError {
    /// Header `extra_data` does not contain the trailing 65-byte seal signature.
    MissingSealSignature {
        /// Observed `extra_data` length in bytes.
        extra_data_len: usize,
    },
    /// Header signature recovery id is invalid.
    InvalidRecoveryId(u8),
    /// Header signature is malformed and cannot be decoded.
    InvalidSignature,
    /// Public key recovery from the seal hash/signature failed.
    SignatureRecoveryFailed,
    /// Recovered signer is not in the authorized signer set.
    UnauthorizedSigner(Address),
    /// Header difficulty does not match the expected seal in-turn/out-of-turn value (legacy path).
    InvalidDifficulty {
        /// Expected wire difficulty.
        expected: U256,
        /// Observed header difficulty.
        got: U256,
    },
    /// Header timestamp violates seal period spacing (legacy).
    InvalidTimestampSpacing {
        /// Parent block timestamp.
        parent_timestamp: u64,
        /// Child block timestamp.
        timestamp: u64,
        /// Seal period seconds.
        period: u64,
    },
    /// Header timestamp outside soft slot window (legacy).
    InvalidSlotTimestamp {
        /// Parent block timestamp.
        parent_timestamp: u64,
        /// Child block timestamp.
        timestamp: u64,
        /// Seal period seconds.
        period: u64,
        /// Allowed drift from expected slot.
        drift_seconds: u64,
        /// Expected timestamp.
        expected: u64,
        /// Maximum valid timestamp.
        max_valid: u64,
    },
    /// Tier 0: `timestamp` must be greater than or equal to `parent_timestamp`.
    InvalidTier0Timestamp {
        /// Parent block timestamp.
        parent_timestamp: u64,
        /// Child block timestamp.
        timestamp: u64,
    },
    /// Tier 0: optional wall-clock bound — header timestamp too far in the future.
    TimestampTooFarInFuture {
        /// Header timestamp.
        timestamp: u64,
        /// Maximum allowed timestamp.
        max_allowed: u64,
    },
    /// Signer appears in the recent signer window (legacy replay).
    SignerRecentlySigned(Address),
}

impl core::fmt::Display for CalibriLiteSealError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingSealSignature { extra_data_len } => write!(
                f,
                "header missing seal signature: extra_data length {extra_data_len} < {SEAL_SIGNATURE_BYTES}"
            ),
            Self::InvalidRecoveryId(v) => write!(f, "invalid signature recovery id byte: {v}"),
            Self::InvalidSignature => write!(f, "invalid signature bytes"),
            Self::SignatureRecoveryFailed => write!(f, "failed to recover signer from seal"),
            Self::UnauthorizedSigner(addr) => {
                write!(f, "recovered signer is not authorized: {addr}")
            }
            Self::InvalidDifficulty { expected, got } => {
                write!(f, "invalid difficulty: expected {expected}, got {got}")
            }
            Self::InvalidTimestampSpacing {
                parent_timestamp,
                timestamp,
                period,
            } => write!(
                f,
                "invalid timestamp spacing: parent={parent_timestamp}, current={timestamp}, period={period}"
            ),
            Self::InvalidSlotTimestamp {
                parent_timestamp,
                timestamp,
                period,
                drift_seconds,
                expected,
                max_valid,
            } => write!(
                f,
                "invalid soft slot timestamp: parent={parent_timestamp}, current={timestamp}, period={period}, drift={drift_seconds}, expected={expected}, max_valid={max_valid}"
            ),
            Self::InvalidTier0Timestamp {
                parent_timestamp,
                timestamp,
            } => write!(
                f,
                "invalid Tier 0 timestamp: must be >= parent; parent={parent_timestamp}, current={timestamp}"
            ),
            Self::TimestampTooFarInFuture {
                timestamp,
                max_allowed,
            } => write!(
                f,
                "invalid Tier 0 timestamp: too far in future: timestamp={timestamp}, max_allowed={max_allowed}"
            ),
            Self::SignerRecentlySigned(addr) => {
                write!(
                    f,
                    "signer is not eligible due to recent-signing rule: {addr}"
                )
            }
        }
    }
}

impl std::error::Error for CalibriLiteSealError {}

/// Computes seal hash for a header (RLP without trailing 65-byte `extra_data` suffix).
pub fn calibri_lite_seal_hash(header: &Header) -> Result<B256, CalibriLiteSealError> {
    let extra_data = header.extra_data.as_ref();
    if extra_data.len() < SEAL_SIGNATURE_BYTES {
        return Err(CalibriLiteSealError::MissingSealSignature {
            extra_data_len: extra_data.len(),
        });
    }

    let mut unsigned = header.clone();
    let unsigned_extra = &extra_data[..extra_data.len() - SEAL_SIGNATURE_BYTES];
    unsigned.extra_data = unsigned_extra.to_vec().into();

    Ok(keccak256(alloy_rlp::encode(&unsigned)))
}

/// Recovers signer address from seal signature embedded in header `extra_data`.
pub fn recover_calibri_lite_signer(header: &Header) -> Result<Address, CalibriLiteSealError> {
    let extra_data = header.extra_data.as_ref();
    if extra_data.len() < SEAL_SIGNATURE_BYTES {
        return Err(CalibriLiteSealError::MissingSealSignature {
            extra_data_len: extra_data.len(),
        });
    }

    let sig_bytes = &extra_data[extra_data.len() - SEAL_SIGNATURE_BYTES..];
    let mut compact = [0u8; 64];
    compact.copy_from_slice(&sig_bytes[..64]);
    let recovery = parse_recovery_id(sig_bytes[64])?;
    let signature =
        Signature::from_slice(&compact).map_err(|_| CalibriLiteSealError::InvalidSignature)?;

    let seal_hash = calibri_lite_seal_hash(header)?;
    let mut digest = [0u8; 32];
    digest.copy_from_slice(seal_hash.as_slice());
    let pubkey = VerifyingKey::recover_from_prehash(&digest, &signature, recovery)
        .map_err(|_| CalibriLiteSealError::SignatureRecoveryFailed)?;

    Ok(pubkey_to_address(&pubkey))
}

/// Validates that header is sealed by an authorized signer from a static signer set.
pub fn validate_static_signer(
    header: &Header,
    authorized_signers: &[Address],
) -> Result<Address, CalibriLiteSealError> {
    let signer = recover_calibri_lite_signer(header)?;
    if !authorized_signers.contains(&signer) {
        return Err(CalibriLiteSealError::UnauthorizedSigner(signer));
    }
    Ok(signer)
}

fn parse_recovery_id(raw_v: u8) -> Result<RecoveryId, CalibriLiteSealError> {
    let normalized = match raw_v {
        0 | 1 => raw_v,
        27 | 28 => raw_v - 27,
        _ => return Err(CalibriLiteSealError::InvalidRecoveryId(raw_v)),
    };
    RecoveryId::try_from(normalized).map_err(|_| CalibriLiteSealError::InvalidRecoveryId(raw_v))
}

/// Derives the Ethereum address from an uncompressed secp256k1 public key.
pub fn pubkey_to_address(pubkey: &VerifyingKey) -> Address {
    let uncompressed = pubkey.to_encoded_point(false);
    let digest = keccak256(&uncompressed.as_bytes()[1..]);
    Address::from_slice(&digest.as_slice()[12..])
}

/// Derives the signer address from a secp256k1 secret key.
///
/// Centralized here so every caller (node, validator, keystore tooling) uses the same
/// derivation and cannot drift. Equivalent to geth's account derivation.
pub fn secret_key_to_address(secret: &SigningKey) -> Address {
    pubkey_to_address(secret.verifying_key())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, keccak256};

    fn sample_secret() -> SigningKey {
        SigningKey::from_slice(&[7u8; 32]).expect("valid secret")
    }

    #[test]
    fn secret_key_to_address_matches_recovered_signer() {
        let secret = sample_secret();
        let addr = secret_key_to_address(&secret);

        // Independent derivation via the recover path on a self-sealed header must agree.
        let mut header = Header {
            extra_data: Bytes::from(vec![0u8; 97]),
            ..Default::default()
        };
        let seal_hash = calibri_lite_seal_hash(&header).unwrap();
        let mut digest = [0u8; 32];
        digest.copy_from_slice(seal_hash.as_slice());
        let (sig, rec_id) = secret
            .sign_prehash_recoverable(&digest)
            .expect("valid secret");
        let compact = sig.to_bytes();
        let mut extra = vec![0u8; 32];
        extra.extend_from_slice(&compact);
        extra.push(rec_id.to_byte());
        header.extra_data = Bytes::from(extra);

        let recovered = recover_calibri_lite_signer(&header).unwrap();
        assert_eq!(recovered, addr);
    }

    #[test]
    fn pubkey_to_address_known_vector() {
        // Verify derivation is keccak(last 20 bytes of keccak(pubkey_uncompressed[1..])).
        let secret = sample_secret();
        let pubkey = secret.verifying_key().clone();
        let uncompressed = pubkey.to_encoded_point(false);
        let digest = keccak256(&uncompressed.as_bytes()[1..]);
        let expect = Address::from_slice(&digest.as_slice()[12..]);
        assert_eq!(pubkey_to_address(&pubkey), expect);
        assert_eq!(secret_key_to_address(&secret), expect);
    }
}

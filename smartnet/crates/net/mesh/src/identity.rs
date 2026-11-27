//! Layer-2 identity: the signed `HELLO` handshake (see `doc/MESH-PAYLOAD-PROTOCOL.md`).
//!
//! One-time per connection, right after the `WebSocket` upgrade and before any other frame.
//! Authenticates the peer (proves control of an Ethereum private key); grants no roles. The
//! allowlist is the peer file's `address` field (legacy `key_id` alias accepted) — an
//! unverified `0x` address; a connection whose recovered address is not on the list is
//! dropped before any expensive work.
//!
//! Signing convention (frozen): recoverable secp256k1 over `keccak256(preimage)`, signed
//! directly (no EIP-191 prefix), 65-byte `r ‖ s ‖ v` with `v ∈ {0,1}`, low-`s` required,
//! high-`s` rejected without normalization.

use crate::protocol::HelloMessage;
use alloy_primitives::{keccak256, Address, B256};
use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use smartnet_validator::keystore_util::KeystoreMaterial;
use std::collections::{HashSet, VecDeque};

/// Domain-separation prefix for the HELLO digest (ASCII, no NUL).
pub const HELLO_DOMAIN: &[u8] = b"smartnet-mesh-hello-v1";

/// A node's mesh identity: encrypted keystore material plus the derived address and network id.
///
/// No decrypted signing key is kept here. Every HELLO signature decrypts from the two in-memory
/// buffers through the ONE shared function and drops the key immediately. The address is public
/// and safe to keep.
#[derive(Clone)]
pub struct Identity {
    key: KeystoreMaterial,
    address: Address,
    network_id: u64,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("address", &format_args!("{:#x}", self.address))
            .field("network_id", &self.network_id)
            .finish_non_exhaustive() // key material is deliberately omitted
    }
}

impl Identity {
    /// Wraps encrypted keystore material and the network (chain) id this node speaks for.
    pub const fn new(key: KeystoreMaterial, network_id: u64) -> Self {
        let address = key.address();
        Self { key, address, network_id }
    }

    /// This node's Ethereum address (derived from the key once at load time).
    pub const fn address(&self) -> Address {
        self.address
    }

    /// The network (chain) id carried in every HELLO.
    pub const fn network_id(&self) -> u64 {
        self.network_id
    }

    /// Builds and signs our own `HELLO` for a handshake we initiate.
    ///
    /// `responder`/`responder_nonce` are zeroed (we do not know them yet); the signature
    /// covers the digest with those fields zero.
    pub fn sign_hello_initiator(&self) -> HelloMessage {
        let initiator = address_bytes(&self.address());
        let initiator_nonce = rand_nonce();
        let responder = [0u8; 20];
        let responder_nonce = [0u8; 32];
        let digest = hello_digest(
            VERSION,
            self.network_id,
            &initiator,
            &responder,
            &initiator_nonce,
            &responder_nonce,
        );
        let signature = self.sign_digest(&digest);
        HelloMessage {
            version: VERSION,
            network_id: self.network_id,
            initiator,
            responder,
            initiator_nonce,
            responder_nonce,
            signature,
        }
    }

    /// Builds and signs our `HELLO` reply to a verified initiator.
    ///
    /// Echoes the initiator's nonce and identity verbatim; fills in our own address and a
    /// fresh responder nonce. The signature binds both identities and both nonces.
    pub fn sign_hello_responder(&self, initiator: &[u8; 20], initiator_nonce: &[u8; 32]) -> HelloMessage {
        let initiator_nonce = *initiator_nonce;
        let responder = address_bytes(&self.address());
        let responder_nonce = rand_nonce();
        let digest = hello_digest(
            VERSION,
            self.network_id,
            initiator,
            &responder,
            &initiator_nonce,
            &responder_nonce,
        );
        let signature = self.sign_digest(&digest);
        HelloMessage {
            version: VERSION,
            network_id: self.network_id,
            initiator: *initiator,
            responder,
            initiator_nonce,
            responder_nonce,
            signature,
        }
    }

    /// Signs a 32-byte digest with our key, returning the 65-byte `r ‖ s ‖ v` encoding
    /// (`v ∈ {0,1}`; k256 produces low-`s` signatures).
    fn sign_digest(&self, digest: &[u8; 32]) -> [u8; 65] {
        // The material was decrypted once at load, so this repeated decrypt cannot fail unless
        // memory itself is corrupt. It is dropped immediately after the signature is made.
        self.key
            .with_key(|key| {
                let (signature, rec_id) = key
                    .sign_prehash_recoverable(digest)
                    .expect("secp256k1 signing with a valid key cannot fail");
                let mut out = [0u8; 65];
                out[..64].copy_from_slice(&signature.to_bytes());
                out[64] = rec_id.to_byte();
                out
            })
            .expect("keystore material was validated when the identity was loaded")
    }
}

/// Protocol version baked into every HELLO digest.
const VERSION: u8 = crate::protocol::VERSION;

/// Builds the fixed-width HELLO digest: `keccak256` over
/// `domain ‖ version ‖ network_id ‖ initiator ‖ responder ‖ i_nonce ‖ r_nonce`, all
/// fixed-width big-endian. Zeroed fields are whatever the caller passes (zero for the
/// side that does not yet know them).
pub fn hello_digest(
    version: u8,
    network_id: u64,
    initiator: &[u8; 20],
    responder: &[u8; 20],
    initiator_nonce: &[u8; 32],
    responder_nonce: &[u8; 32],
) -> [u8; 32] {
    let mut preimage =
        Vec::with_capacity(HELLO_DOMAIN.len() + 1 + 8 + 20 + 20 + 32 + 32);
    preimage.extend_from_slice(HELLO_DOMAIN);
    preimage.push(version);
    preimage.extend_from_slice(&network_id.to_be_bytes());
    preimage.extend_from_slice(initiator);
    preimage.extend_from_slice(responder);
    preimage.extend_from_slice(initiator_nonce);
    preimage.extend_from_slice(responder_nonce);
    keccak256(&preimage).0
}

/// Result of verifying a peer's `HELLO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelloVerify {
    /// Signature recovers to a known, allowlisted address. Carries the peer's address.
    Authenticated(Address),
    /// The claimed address is not in the allowlist (checked before signature recovery).
    NotAllowlisted,
    /// Signature is malformed, high-`s`, wrong `v`, or recovers to a different address.
    BadSignature,
    /// `version` or `network_id` does not match ours.
    Mismatched {
        /// Which field disagreed ("version" or "`network_id`").
        field: &'static str,
    },
}

/// Verifies a peer's `HELLO` against our expected network id and an allowlist of known
/// addresses.
///
/// Validation order (fast-reject first, per spec): parse header → version/network check →
/// allowlist (no signature recovery) → signature recovery + low-`s` + address match.
/// `is_initiator_hello` selects which address field is the *sender* being authenticated:
/// the initiator's address on an initiator HELLO, the responder's address on a reply.
pub fn verify_hello(
    hello: &HelloMessage,
    expected_network_id: u64,
    allowlist: &[Address],
    sender_is_initiator: bool,
) -> HelloVerify {
    if hello.version != VERSION {
        return HelloVerify::Mismatched { field: "version" };
    }
    if hello.network_id != expected_network_id {
        return HelloVerify::Mismatched { field: "network_id" };
    }

    // The claimed sender address for this message: initiator on an initiator HELLO,
    // responder on a reply. This is what the signature must recover to.
    let claimed_bytes =
        if sender_is_initiator { hello.initiator } else { hello.responder };
    let claimed = match address_from_bytes(&claimed_bytes) {
        Some(address) => address,
        None => return HelloVerify::BadSignature,
    };

    // Cheap DoS gate: unknown address is dropped before any signature recovery.
    if !allowlist.contains(&claimed) {
        return HelloVerify::NotAllowlisted;
    }

    // Recover the signer from the digest and the claimed sender's signature; compare to
    // the claimed address. This is the expensive step, done only for allowlisted peers.
    let digest = hello_digest(
        hello.version,
        hello.network_id,
        &hello.initiator,
        &hello.responder,
        &hello.initiator_nonce,
        &hello.responder_nonce,
    );
    match recover_signer(&digest, &hello.signature) {
        Some(recovered) if recovered == claimed => HelloVerify::Authenticated(claimed),
        Some(_) | None => HelloVerify::BadSignature,
    }
}

/// Recovers the signer address from a 32-byte digest and a 65-byte `r ‖ s ‖ v` signature.
///
/// Enforces the frozen convention: `v ∈ {0,1}`, `1 <= r < n`, `1 <= s <= n/2` (low-`s`;
/// high-`s` is rejected, never silently normalized). Returns `None` on any violation.
fn recover_signer(digest: &[u8; 32], signature: &[u8; 65]) -> Option<Address> {
    let v = signature[64];
    if v > 1 {
        return None; // wire `v` must be 0 or 1, not 27/28 or EIP-155
    }
    let sig = Signature::from_slice(&signature[..64]).ok()?;
    // Low-`s` enforcement: reject, do not normalize (spec).
    if sig.s().to_bytes() > half_n() {
        return None;
    }
    let rec_id = RecoveryId::from_byte(v)?;
    let verifying_key =
        VerifyingKey::recover_from_prehash(digest, &sig, rec_id).ok()?;
    Some(address_from_verifying_key(&verifying_key))
}

/// The secp256k1 group order `n` as big-endian bytes, halved (for the low-`s` bound).
fn half_n() -> k256::FieldBytes {
    // n = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141
    // half_n = n >> 1 = 0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0
    let mut half = [0u8; 32];
    half[..].copy_from_slice(&[
        0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0x5D, 0x57, 0x6E, 0x73, 0x57, 0xA4, 0x50, 0x1D, 0xDF, 0xE9, 0x2F, 0x46,
        0x68, 0x1B, 0x20, 0xA0,
    ]);
    k256::FieldBytes::from(half)
}

/// Ethereum address = last 20 bytes of `keccak256(uncompressed_pubkey_without_prefix)`.
fn address_from_verifying_key(key: &VerifyingKey) -> Address {
    let encoded = key.to_encoded_point(false);
    let bytes = encoded.as_bytes(); // 65 bytes: 0x04 ‖ X ‖ Y
    let hash = keccak256(&bytes[1..]);
    Address::from_slice(&hash.as_slice()[12..])
}

/// Derives an Ethereum address from a signing key (mirrors the validator's
/// `secret_key_to_address`, using the same keccak-of-pubkey convention).
pub fn secret_key_address(key: &SigningKey) -> Address {
    address_from_verifying_key(key.verifying_key())
}

/// Converts a 20-byte array to an [`Address`].
pub fn address_from_bytes(bytes: &[u8; 20]) -> Option<Address> {
    Some(Address::from_slice(bytes))
}

/// Extracts the 20 raw bytes of an address.
pub const fn address_bytes(address: &Address) -> [u8; 20] {
    address.0 .0
}

/// A cryptographically secure random 32-byte nonce.
fn rand_nonce() -> [u8; 32] {
    let mut nonce = [0u8; 32];
    getrandom::getrandom(&mut nonce).expect("OS CSPRNG unavailable for HELLO nonce");
    nonce
}

/// Convenience: a [`B256`] view of a digest (for call sites that want the typed hash).
pub fn digest_as_b256(digest: &[u8; 32]) -> B256 {
    B256::from(*digest)
}

/// Bounded replay cache for responder-side `HELLO` nonces.
///
/// A fresh responder nonce per handshake defeats replay of a captured `HELLO_PING`; the
/// cache remembers recently accepted initiator nonces so a second presentation of the
/// same nonce is rejected. Bounded FIFO (oldest evicted first) so a long-running server
/// cannot grow it without bound — the bound is sized to outlive any honest handshake
/// round trip by orders of magnitude.
#[derive(Debug)]
pub struct NonceCache {
    set: HashSet<[u8; 32]>,
    order: VecDeque<[u8; 32]>,
    cap: usize,
}

impl NonceCache {
    /// Creates a cache retaining at most `cap` nonces (FIFO eviction).
    pub fn new(cap: usize) -> Self {
        Self { set: HashSet::with_capacity(cap), order: VecDeque::with_capacity(cap), cap }
    }

    /// Records a nonce; returns `false` when it was already present (a replay).
    pub fn insert(&mut self, nonce: [u8; 32]) -> bool {
        if !self.set.insert(nonce) {
            return false;
        }
        self.order.push_back(nonce);
        while self.order.len() > self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        true
    }

    /// Whether a nonce is already recorded — the cheap replay probe that runs before
    /// signature recovery (spec order: allowlist → replay → signature).
    pub fn contains(&self, nonce: &[u8; 32]) -> bool {
        self.set.contains(nonce)
    }

    /// Number of retained nonces (test/diagnostic helper).
    pub fn len(&self) -> usize {
        self.set.len()
    }

    /// Whether no nonces are retained.
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}

#[cfg(test)]
pub(crate) fn test_identity(seed: u8) -> Identity {
    let mut bytes = [0u8; 32];
    bytes[31] = seed;
    let key = SigningKey::from_slice(&bytes).expect("valid scalar");
    let dir = tempfile::tempdir().expect("test tempdir");
    let mut rng = rand_08::rngs::OsRng;
    alloy_signer_local::PrivateKeySigner::encrypt_keystore(
        dir.path(),
        &mut rng,
        key.to_bytes(),
        b"pw",
        Some("key.json"),
    )
    .expect("encrypt test keystore");
    let encrypted = std::fs::read(dir.path().join("key.json")).expect("read test keystore");
    let material = KeystoreMaterial::from_encrypted_bytes(encrypted, b"pw".to_vec())
        .expect("build test key material");
    Identity::new(material, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initiator_hello_verifies_against_its_own_address() {
        let a = test_identity(1);
        let hello = a.sign_hello_initiator();
        let allowlist = [a.address()];
        assert_eq!(
            verify_hello(&hello, 1, &allowlist, true),
            HelloVerify::Authenticated(a.address())
        );
    }

    #[test]
    fn responder_reply_verifies_against_responder_address() {
        let a = test_identity(1);
        let b = test_identity(2);
        let initiator_hello = a.sign_hello_initiator();
        let reply =
            b.sign_hello_responder(&initiator_hello.initiator, &initiator_hello.initiator_nonce);
        let allowlist = [a.address(), b.address()];
        // The reply authenticates the *responder* (b), not the initiator.
        assert_eq!(
            verify_hello(&reply, 1, &allowlist, false),
            HelloVerify::Authenticated(b.address())
        );
    }

    #[test]
    fn unknown_address_is_rejected_before_signature_recovery() {
        let a = test_identity(1);
        let hello = a.sign_hello_initiator();
        let empty: [Address; 0] = [];
        assert_eq!(verify_hello(&hello, 1, &empty, true), HelloVerify::NotAllowlisted);
    }

    #[test]
    fn wrong_network_id_is_mismatched() {
        let a = test_identity(1);
        let hello = a.sign_hello_initiator();
        let allowlist = [a.address()];
        assert_eq!(
            verify_hello(&hello, 999, &allowlist, true),
            HelloVerify::Mismatched { field: "network_id" }
        );
    }

    #[test]
    fn tampered_nonce_fails_signature() {
        let a = test_identity(1);
        let mut hello = a.sign_hello_initiator();
        hello.initiator_nonce[0] ^= 0xFF;
        let allowlist = [a.address()];
        assert_eq!(verify_hello(&hello, 1, &allowlist, true), HelloVerify::BadSignature);
    }

    #[test]
    fn high_s_signature_is_rejected() {
        // Sign a digest, then flip s to n - s (high-s) with the parity bit flipped, which
        // is the classic malleated form. It must be rejected, not normalized.
        let a = test_identity(1);
        let hello = a.sign_hello_initiator();
        let digest = hello_digest(
            hello.version,
            hello.network_id,
            &hello.initiator,
            &hello.responder,
            &hello.initiator_nonce,
            &hello.responder_nonce,
        );
        let sig = Signature::from_slice(&hello.signature[..64]).expect("valid sig");
        // n - s, computed on the raw 32-byte big-endian integers (n = secp256k1 order).
        let n = alloy_primitives::U256::from_be_slice(&[
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFE, 0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C,
            0xD0, 0x36, 0x41, 0x41,
        ]);
        let s_int = alloy_primitives::U256::from_be_slice(&sig.s().to_bytes());
        let n_minus_s = n - s_int;
        let s_field = k256::FieldBytes::from(n_minus_s.to_be_bytes::<32>());
        let high = Signature::from_scalars(sig.r(), s_field).expect("valid high-s");
        let mut malleated = [0u8; 65];
        malleated[..64].copy_from_slice(&high.to_bytes());
        malleated[64] = hello.signature[64] ^ 1; // flip parity with s -> n - s
        // recover_signer must reject (high-s), so the verify path sees BadSignature.
        assert!(recover_signer(&digest, &malleated).is_none());
        let mut bad = hello;
        bad.signature = malleated;
        let allowlist = [a.address()];
        assert_eq!(verify_hello(&bad, 1, &allowlist, true), HelloVerify::BadSignature);
    }

    #[test]
    fn signature_v_must_be_zero_or_one() {
        let a = test_identity(1);
        let mut hello = a.sign_hello_initiator();
        hello.signature[64] = 27; // Ethereum tx convention, forbidden on this wire
        let allowlist = [a.address()];
        assert_eq!(verify_hello(&hello, 1, &allowlist, true), HelloVerify::BadSignature);
    }

    #[test]
    fn nonce_cache_accepts_fresh_rejects_replayed() {
        let mut cache = NonceCache::new(4);
        assert!(cache.insert([1u8; 32]));
        assert!(!cache.insert([1u8; 32])); // replay
        assert!(cache.insert([2u8; 32]));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn nonce_cache_evicts_oldest_at_the_cap() {
        let mut cache = NonceCache::new(2);
        assert!(cache.insert([1u8; 32]));
        assert!(cache.insert([2u8; 32]));
        assert!(cache.insert([3u8; 32])); // evicts [1; 32]
        assert_eq!(cache.len(), 2);
        assert!(cache.insert([1u8; 32])); // accepted again: long since evicted
        assert!(!cache.insert([3u8; 32]));
    }
}

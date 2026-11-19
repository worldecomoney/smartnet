//! Node-side **native transactions**: plain value transfers, and value transfers
//! that carry a `data` field — the signalling shape the validator and the bridge
//! exchange (`0.91`/`0.92` bridge requests, the chair's `0.01` authorization, and
//! whatever else the node needs to say in calldata).
//!
//! A `SmartNet` node has no `eth_accounts` and never uses `eth_sendTransaction`
//! (a node returns `-32602: unknown account`). Everything is built and signed
//! locally here, then handed to a transport as raw bytes for
//! `eth_sendRawTransaction` — this crate owns the *shape and the signature*, not
//! the transport: the bridge submits through its failover RPC client, the
//! validator embeds the envelope in the block it is about to seal.
//!
//! Smart-contract interaction (ERC-20 calls, deployments, reads) stays in
//! `tools/` — this is only the native arm the node itself needs.
//!
//! ```text
//! NativeTx::transfer(chain_id, nonce, to, value, gas_price, gas_limit)
//!     .with_data(bytes)          // optional: the signalling variant
//!     .sign(&signer)?            // → envelope + raw bytes + tx hash
//! ```
//!
//! Signing is **deterministic**: identical inputs produce identical bytes. Every
//! re-derive path in the bridge depends on that — a lost broadcast must rebuild
//! the exact same transaction so that at most one of the two can ever be mined.

use alloy_consensus::{transaction::TxEnvelope, SignableTransaction, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{keccak256, Bytes, TxKind, B256, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;

/// One native transaction: a value transfer, optionally carrying a `data` field.
///
/// This builds a **legacy (type-0)** transaction with an explicit chain id and an
/// explicit gas price — the shape every `SmartNet` sender uses (the protocol's fixed
/// fee is passed in by the caller; this crate never invents a price).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeTx {
    /// The chain the transaction is valid on.
    pub chain_id: u64,
    /// The sender's account number.
    pub nonce: u64,
    /// Where the value goes. Always [`TxKind::Call`] — deployments are `tools/`' job.
    pub to: TxKind,
    /// Value in wei.
    pub value: U256,
    /// Calldata: empty for a plain transfer, non-empty for a signalling tx.
    pub data: Bytes,
    /// Gas price in wei — the caller supplies the protocol fixed fee.
    pub gas_price: u128,
    /// Gas limit — the caller decides (normally `eth_estimateGas`).
    pub gas_limit: u64,
}

/// A signed [`NativeTx`], in the three shapes its two consumers need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedNativeTx {
    /// The signed envelope — what the validator pushes into a sealed block body.
    pub envelope: TxEnvelope,
    /// EIP-2718 bytes — what `eth_sendRawTransaction` takes.
    pub raw: Vec<u8>,
    /// `keccak256(raw)` — the transaction hash, used as the anchor for retries.
    pub hash: B256,
}

impl NativeTx {
    /// A plain value transfer to `to`, with an empty `data` field.
    pub const fn transfer(
        chain_id: u64,
        nonce: u64,
        to: alloy_primitives::Address,
        value: U256,
        gas_price: u128,
        gas_limit: u64,
    ) -> Self {
        Self {
            chain_id,
            nonce,
            to: TxKind::Call(to),
            value,
            data: Bytes::new(),
            gas_price,
            gas_limit,
        }
    }

    /// The same transaction carrying a `data` field — the signalling variant.
    ///
    /// An empty `data` means "plain transfer"; anything else is protocol calldata
    /// (the §8 eight-field payload, …) and travels with the value.
    pub fn with_data(mut self, data: Bytes) -> Self {
        self.data = data;
        self
    }

    /// Signs with `signer` and returns the envelope, the raw bytes and the hash.
    ///
    /// Deterministic for identical inputs — the bridge's re-derive paths rely on it
    /// to rebuild byte-identical transactions after a lost broadcast.
    ///
    /// # Errors
    /// Only from the signature operation itself; a caller that can sign can use
    /// [`Self::sign_unchecked`] semantics by construction (this is the checked entry
    /// point and the only one the node uses).
    pub fn sign(&self, signer: &PrivateKeySigner) -> Result<SignedNativeTx, String> {
        let tx = TxLegacy {
            chain_id: Some(self.chain_id),
            nonce: self.nonce,
            gas_price: self.gas_price,
            gas_limit: self.gas_limit,
            to: self.to,
            value: self.value,
            input: self.data.clone(),
        };
        let sighash = tx.signature_hash();
        let signature = signer.sign_hash_sync(&sighash).map_err(|err| format!("sign: {err}"))?;
        let envelope = TxEnvelope::Legacy(tx.into_signed(signature));
        let raw = envelope.encoded_2718();
        let hash = keccak256(&raw);
        Ok(SignedNativeTx { envelope, raw, hash })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, Bytes};

    fn signer(byte: u8) -> PrivateKeySigner {
        PrivateKeySigner::from_bytes(&B256::repeat_byte(byte)).expect("test key")
    }

    fn transfer(data: Bytes) -> NativeTx {
        NativeTx::transfer(315, 7, Address::repeat_byte(0xAA), U256::from(1_000u64), 238_095_238_095, 21_000)
            .with_data(data)
    }

    #[test]
    fn a_plain_transfer_carries_no_data() {
        let tx = transfer(Bytes::new());
        assert!(tx.data.is_empty());
        assert_eq!(tx.to, TxKind::Call(Address::repeat_byte(0xAA)));
    }

    #[test]
    fn with_data_changes_the_hash_and_rides_in_the_raw_bytes() {
        let plain = transfer(Bytes::new()).sign(&signer(0x11)).expect("sign");
        let signalling = transfer(Bytes::from_static(&[0xDE, 0xAD, 0xBE, 0xEF]))
            .sign(&signer(0x11))
            .expect("sign");

        assert_ne!(plain.hash, signalling.hash, "calldata must change the identity");
        assert!(signalling.raw.windows(4).any(|w| w == [0xDE, 0xAD, 0xBE, 0xEF]));
        assert!(!plain.raw.windows(4).any(|w| w == [0xDE, 0xAD, 0xBE, 0xEF]));
    }

    #[test]
    fn signing_is_deterministic() {
        // The bridge rebuilds a lost broadcast and must produce byte-identical
        // bytes, so that at most one of the two can ever be mined.
        let a = transfer(Bytes::from_static(&[0x01])).sign(&signer(0x22)).expect("sign");
        let b = transfer(Bytes::from_static(&[0x01])).sign(&signer(0x22)).expect("sign");
        assert_eq!(a.raw, b.raw, "same inputs must sign to the same bytes");
        assert_eq!(a.hash, b.hash);
        assert_eq!(a.hash, keccak256(&a.raw), "the hash is the hash of the raw bytes");
    }

    #[test]
    fn a_different_signer_produces_a_different_transaction() {
        let a = transfer(Bytes::new()).sign(&signer(0x33)).expect("sign");
        let b = transfer(Bytes::new()).sign(&signer(0x44)).expect("sign");
        assert_ne!(a.hash, b.hash);
        assert_ne!(a.raw, b.raw);
    }

    #[test]
    fn an_empty_value_transfer_is_still_a_valid_transaction() {
        // The chair's 0.01 authorization is a value transfer with data; a delivery
        // is a value transfer without. Both must sign.
        let zero = NativeTx::transfer(315, 0, Address::repeat_byte(0xBB), U256::ZERO, 1, 21_000)
            .sign(&signer(0x55))
            .expect("sign");
        assert_eq!(zero.value_or_zero(), U256::ZERO);
    }

    trait ValueOrZero {
        fn value_or_zero(&self) -> U256;
    }
    impl ValueOrZero for SignedNativeTx {
        fn value_or_zero(&self) -> U256 {
            match &self.envelope {
                TxEnvelope::Legacy(signed) => signed.tx().value,
                _ => U256::ZERO,
            }
        }
    }
}

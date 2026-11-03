//! The wire unit of gossip: object kind, stable hash, and canonical bytes.
//!
//! Transactions use `keccak256(EIP-2718 envelope bytes)` — the Ethereum transaction hash.
//! Blocks use `keccak256(full block RLP)` as their local mesh identity alongside the block
//! number (announces are by number, not hash); the hash is the duplicate-cache key and is
//! never a transport-envelope hash. Import validation (engine `newPayload`) vets block
//! bytes independently of this local identity.

use alloy_primitives::{keccak256, Bytes};

/// What a rumor carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RumorKind {
    /// A raw signed transaction (EIP-2718 envelope bytes).
    Tx,
    /// A full block RLP payload.
    Block,
}

impl RumorKind {
    /// A human-readable label for logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tx => "tx",
            Self::Block => "block",
        }
    }
}

/// One gossip object: kind, stable object hash, and canonical bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rumor {
    /// What the payload is.
    pub kind: RumorKind,
    /// Stable object hash: the Ethereum tx hash for transactions,
    /// `keccak256(full block RLP)` for blocks.
    pub hash: [u8; 32],
    /// Canonical bytes: EIP-2718 transaction or full block RLP.
    pub data: Bytes,
    /// Announced block height for block rumors (`None` for transactions).
    pub block_nr: Option<u64>,
}

impl Rumor {
    /// Creates a transaction rumor; the hash is the Ethereum transaction hash.
    pub fn tx(data: impl Into<Bytes>) -> Self {
        let data = data.into();
        let hash = keccak256(&data).0;
        Self { kind: RumorKind::Tx, hash, data, block_nr: None }
    }

    /// Creates a block rumor for block `block_nr`; the hash is
    /// `keccak256(full block RLP)` (the mesh's local identity for the block).
    pub fn block(block_nr: u64, data: impl Into<Bytes>) -> Self {
        let data = data.into();
        let hash = keccak256(&data).0;
        Self { kind: RumorKind::Block, hash, data, block_nr: Some(block_nr) }
    }

    /// Whether the bytes match the claimed identity: the tx hash for transactions, the
    /// RLP hash for blocks. Block *validity* (parent, seal, execution) is checked by the
    /// engine import path — this is only the local mesh identity.
    pub fn hash_matches(&self) -> bool {
        keccak256(&self.data).0 == self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_hash_is_the_ethereum_transaction_hash() {
        let raw = vec![0x01u8, 0x02, 0xff];
        let rumor = Rumor::tx(raw.clone());
        assert_eq!(rumor.hash, keccak256(&raw).0);
        assert_eq!(rumor.kind, RumorKind::Tx);
        assert_eq!(rumor.block_nr, None);
        assert!(rumor.hash_matches());
    }

    #[test]
    fn tx_hash_mismatch_is_detected() {
        let mut rumor = Rumor::tx(vec![0x01]);
        rumor.data = Bytes::from(vec![0x02]);
        assert!(!rumor.hash_matches());
    }

    #[test]
    fn block_hash_is_the_keccak_of_its_rlp_and_carries_the_number() {
        let rlp = vec![0xc0u8];
        let rumor = Rumor::block(7, rlp.clone());
        assert_eq!(rumor.hash, keccak256(&rlp).0);
        assert_eq!(rumor.kind, RumorKind::Block);
        assert_eq!(rumor.block_nr, Some(7));
        assert!(rumor.hash_matches());
    }

    #[test]
    fn block_hash_mismatch_is_detected() {
        let mut rumor = Rumor::block(7, vec![0xc0]);
        rumor.data = Bytes::from(vec![0xc1]);
        assert!(!rumor.hash_matches());
    }
}

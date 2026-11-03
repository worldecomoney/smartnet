//! SmartNet proposal metadata carried in header `extraData` (vanity prefix).

use alloy_primitives::Bytes;

/// Normal block proposal.
pub const PROPOSAL_TYPE_NORMAL: u8 = 0;

/// Minimum `extraData` length for a SmartNet proposal: 32-byte vanity + 65-byte seal = 97 bytes.
/// Headers shorter than this cannot carry SmartNet proposal metadata.
pub const MIN_SMARTNET_EXTRA_DATA_LEN: usize = 32 + 65;

/// First 8 bytes of the 32-byte vanity hold SmartNet proposal metadata.
pub const META_LEN: usize = 8;

/// Minimal explicit proposal signaling (versioned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmartNetProposalMeta {
    /// Must match [`CALIBRI_LITE_RUNTIME_PROTOCOL_VERSION`](super::runtime_profile::CALIBRI_LITE_RUNTIME_PROTOCOL_VERSION).
    pub version: u8,
    /// Proposal kind (e.g. [`PROPOSAL_TYPE_NORMAL`]).
    pub proposal_type: u8,
    /// Reserved flags for future validator policy.
    pub validator_flags: u16,
}

impl SmartNetProposalMeta {
    /// Default normal proposal for the active protocol version.
    pub const fn normal_v1() -> Self {
        Self {
            version: crate::pre_consensus::runtime_profile::CALIBRI_LITE_RUNTIME_PROTOCOL_VERSION,
            proposal_type: PROPOSAL_TYPE_NORMAL,
            validator_flags: 0,
        }
    }

    /// Writes metadata into the first 8 bytes of the 32-byte vanity prefix.
    pub fn encode_into_vanity_prefix(&self, out: &mut [u8; 32]) {
        out[0] = self.version;
        out[1] = self.proposal_type;
        out[2..4].copy_from_slice(&self.validator_flags.to_be_bytes());
        out[4..META_LEN].fill(0);
        out[META_LEN..].fill(0);
    }

    /// Parses metadata from sealed header `extraData` (32-byte vanity + 65-byte signature).
    pub fn decode_from_extra_data(extra: &Bytes) -> Option<Self> {
        if extra.len() < 32 + 65 {
            return None;
        }
        let vanity = &extra[0..32];
        Some(Self {
            version: vanity[0],
            proposal_type: vanity[1],
            validator_flags: u16::from_be_bytes([vanity[2], vanity[3]]),
        })
    }
}

/// Build runtime `extra_data` = 32-byte vanity (with meta) + 65-byte seal placeholder.
pub fn build_vanity_with_meta(meta: &SmartNetProposalMeta) -> [u8; 32] {
    let mut vanity = [0u8; 32];
    meta.encode_into_vanity_prefix(&mut vanity);
    vanity
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;

    #[test]
    fn decode_valid_smartnet_extra_data() {
        let mut extra = vec![0u8; 32 + 65];
        extra[0] = 1; // version
        extra[1] = 0; // proposal_type
        let meta = SmartNetProposalMeta::decode_from_extra_data(&Bytes::from(extra));
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.version, 1);
        assert_eq!(meta.proposal_type, 0);
    }

    #[test]
    fn decode_short_extra_data_returns_none() {
        let extra = Bytes::from(vec![0u8; 50]);
        assert!(SmartNetProposalMeta::decode_from_extra_data(&extra).is_none());
    }

    #[test]
    fn decode_exactly_min_length() {
        let extra = Bytes::from(vec![0u8; MIN_SMARTNET_EXTRA_DATA_LEN]);
        assert!(SmartNetProposalMeta::decode_from_extra_data(&extra).is_some());
    }

    #[test]
    fn encode_then_decode_roundtrip() {
        let meta = SmartNetProposalMeta {
            version: 1,
            proposal_type: 2,
            validator_flags: 0xABCD,
        };
        let mut vanity = [0u8; 32];
        meta.encode_into_vanity_prefix(&mut vanity);
        let mut extra = vanity.to_vec();
        extra.extend_from_slice(&[0u8; 65]);
        let decoded = SmartNetProposalMeta::decode_from_extra_data(&Bytes::from(extra)).unwrap();
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.proposal_type, 2);
        assert_eq!(decoded.validator_flags, 0xABCD);
    }
}

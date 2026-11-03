//! Compact implementation for [`AlloyHeader`]

use crate::Compact;
use alloy_consensus::Header as AlloyHeader;
use alloy_primitives::{Address, BlockNumber, Bloom, Bytes, B256, U256};
use smartnet_codecs_derive::{add_arbitrary_tests, generate_tests};

/// Block header
///
/// This is a helper type to use derive on it instead of manually managing `bitfield`.
///
/// By deriving `Compact` here, any future changes or enhancements to the `Compact` derive
/// will automatically apply to this type.
///
/// Notice: Make sure this struct is 1:1 with [`alloy_consensus::Header`]
#[cfg_attr(
    any(test, feature = "test-utils"),
    derive(serde::Serialize, serde::Deserialize, arbitrary::Arbitrary)
)]
#[cfg_attr(feature = "test-utils", allow(unreachable_pub), visibility::make(pub))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Compact)]
#[smartnet_codecs(crate = "crate")]
#[add_arbitrary_tests(crate, compact)]
pub(crate) struct Header {
    parent_hash: B256,
    ommers_hash: B256,
    beneficiary: Address,
    state_root: B256,
    transactions_root: B256,
    receipts_root: B256,
    withdrawals_root: Option<B256>,
    logs_bloom: Bloom,
    difficulty: U256,
    number: BlockNumber,
    gas_limit: u64,
    gas_used: u64,
    timestamp: u64,
    mix_hash: B256,
    nonce: u64,
    base_fee_per_gas: Option<u64>,
    blob_gas_used: Option<u64>,
    excess_blob_gas: Option<u64>,
    parent_beacon_block_root: Option<B256>,
    extra_fields: Option<HeaderExt>,
    extra_data: Bytes,
}

/// [`Header`] extension struct.
///
/// All new fields should be added here in the form of a `Option<T>`, since `Option<HeaderExt>` is
/// used as a field of [`Header`] for backwards compatibility.
///
/// More information: <https://github.com/worldecomoney/smartnet/issues/7820> & [`smartnet_codecs_derive::Compact`].
#[cfg_attr(
    any(test, feature = "test-utils"),
    derive(serde::Serialize, serde::Deserialize, arbitrary::Arbitrary)
)]
#[cfg_attr(feature = "test-utils", allow(unreachable_pub), visibility::make(pub))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Compact)]
#[smartnet_codecs(crate = "crate")]
#[add_arbitrary_tests(crate, compact)]
pub(crate) struct HeaderExt {
    requests_hash: Option<B256>,
    block_access_list_hash: Option<B256>,
    slot_number: Option<u64>,
}

impl HeaderExt {
    /// Converts into [`Some`] if any of the field exists. Otherwise, returns [`None`].
    ///
    /// Required since [`Header`] uses `Option<HeaderExt>` as a field.
    const fn into_option(self) -> Option<Self> {
        if self.requests_hash.is_some() ||
            self.block_access_list_hash.is_some() ||
            self.slot_number.is_some()
        {
            Some(self)
        } else {
            None
        }
    }
}

impl Compact for AlloyHeader {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let extra_fields = HeaderExt {
            requests_hash: self.requests_hash,
            block_access_list_hash: self.block_access_list_hash,
            slot_number: self.slot_number,
        };

        let header = Header {
            parent_hash: self.parent_hash,
            ommers_hash: self.ommers_hash,
            beneficiary: self.beneficiary,
            state_root: self.state_root,
            transactions_root: self.transactions_root,
            receipts_root: self.receipts_root,
            withdrawals_root: self.withdrawals_root,
            logs_bloom: self.logs_bloom,
            difficulty: self.difficulty,
            number: self.number,
            gas_limit: self.gas_limit,
            gas_used: self.gas_used,
            timestamp: self.timestamp,
            mix_hash: self.mix_hash,
            nonce: self.nonce.into(),
            base_fee_per_gas: self.base_fee_per_gas,
            blob_gas_used: self.blob_gas_used,
            excess_blob_gas: self.excess_blob_gas,
            parent_beacon_block_root: self.parent_beacon_block_root,
            extra_fields: extra_fields.into_option(),
            extra_data: self.extra_data.clone(),
        };
        header.to_compact(buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (header, buf) = Header::from_compact(buf, len);
        let alloy_header = Self {
            parent_hash: header.parent_hash,
            ommers_hash: header.ommers_hash,
            beneficiary: header.beneficiary,
            state_root: header.state_root,
            transactions_root: header.transactions_root,
            receipts_root: header.receipts_root,
            withdrawals_root: header.withdrawals_root,
            logs_bloom: header.logs_bloom,
            difficulty: header.difficulty,
            number: header.number,
            gas_limit: header.gas_limit,
            gas_used: header.gas_used,
            timestamp: header.timestamp,
            mix_hash: header.mix_hash,
            nonce: header.nonce.into(),
            base_fee_per_gas: header.base_fee_per_gas,
            blob_gas_used: header.blob_gas_used,
            excess_blob_gas: header.excess_blob_gas,
            parent_beacon_block_root: header.parent_beacon_block_root,
            requests_hash: header.extra_fields.as_ref().and_then(|h| h.requests_hash),
            block_access_list_hash: header
                .extra_fields
                .as_ref()
                .and_then(|h| h.block_access_list_hash),
            slot_number: header.extra_fields.as_ref().and_then(|h| h.slot_number),
            extra_data: header.extra_data,
        };
        (alloy_header, buf)
    }
}

generate_tests!(#[crate, compact] AlloyHeader, AlloyHeaderTests);


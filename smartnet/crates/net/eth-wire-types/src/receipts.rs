//! Implements the `GetReceipts` and `Receipts` message types.

use alloc::vec::Vec;
use alloy_consensus::{ReceiptWithBloom, RlpDecodableReceipt, RlpEncodableReceipt, TxReceipt};
use alloy_primitives::B256;
use alloy_rlp::{RlpDecodableWrapper, RlpEncodableWrapper};
use derive_more::{Deref, IntoIterator};
use smartnet_codecs_derive::add_arbitrary_tests;
use smartnet_ethereum_primitives::Receipt;

/// A request for transaction receipts from the given block hashes.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    RlpEncodableWrapper,
    RlpDecodableWrapper,
    Default,
    Deref,
    IntoIterator,
)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[add_arbitrary_tests(rlp)]
pub struct GetReceipts(
    /// The block hashes to request receipts for.
    pub Vec<B256>,
);

/// The response to [`GetReceipts`], containing receipt lists that correspond to each block
/// requested.
#[derive(Clone, Debug, PartialEq, Eq, Default, Deref, IntoIterator)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[add_arbitrary_tests(rlp)]
pub struct Receipts<T = Receipt>(
    /// Each receipt hash should correspond to a block hash in the request.
    pub Vec<Vec<ReceiptWithBloom<T>>>,
);

impl<T: RlpEncodableReceipt> alloy_rlp::Encodable for Receipts<T> {
    #[inline]
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.0.encode(out)
    }
    #[inline]
    fn length(&self) -> usize {
        self.0.length()
    }
}

impl<T: RlpDecodableReceipt> alloy_rlp::Decodable for Receipts<T> {
    #[inline]
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        alloy_rlp::Decodable::decode(buf).map(Self)
    }
}

/// Eth/70 `Receipts` response payload.
///
/// The payload carries `last_block_incomplete` followed by the per-block receipt lists.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
pub struct Receipts70<T = Receipt> {
    /// Whether the receipts list for the last block is incomplete.
    pub last_block_incomplete: bool,
    /// Receipts grouped by block.
    pub receipts: Vec<Vec<T>>,
}

impl<T> alloy_rlp::Encodable for Receipts70<T>
where
    T: alloy_rlp::Encodable,
{
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.last_block_incomplete.encode(out);
        self.receipts.encode(out);
    }

    fn length(&self) -> usize {
        self.last_block_incomplete.length() + self.receipts.length()
    }
}

impl<T> alloy_rlp::Decodable for Receipts70<T>
where
    T: alloy_rlp::Decodable,
{
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let last_block_incomplete = bool::decode(buf)?;
        let receipts = Vec::<Vec<T>>::decode(buf)?;
        Ok(Self { last_block_incomplete, receipts })
    }
}

impl<T: TxReceipt> Receipts70<T> {
    /// Encodes all receipts with the bloom filter.
    ///
    /// Just like eth/69, eth/70 does not transmit bloom filters over the wire.
    /// When higher layers still expect the older bloom-bearing [`Receipts`]
    /// type, this helper converts the eth/70 payload into that shape by
    /// recomputing the bloom locally from the contained receipts.
    ///
    /// Note: This is an expensive operation that recalculates the bloom for
    /// every receipt.
    pub fn into_with_bloom(self) -> Receipts<T> {
        Receipts(
            self.receipts
                .into_iter()
                .map(|receipts| receipts.into_iter().map(|r| r.into_with_bloom()).collect())
                .collect(),
        )
    }
}

impl<T: TxReceipt> From<Receipts70<T>> for Receipts<T> {
    fn from(receipts: Receipts70<T>) -> Self {
        receipts.into_with_bloom()
    }
}

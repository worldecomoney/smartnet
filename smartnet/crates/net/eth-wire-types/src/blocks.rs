//! Implements the `GetBlockHeaders`, `BlockHeaders`, and `BlockBodies` message types.

use crate::HeadersDirection;
use alloc::vec::Vec;
use alloy_eips::BlockHashOrNumber;
use alloy_rlp::{RlpDecodable, RlpDecodableWrapper, RlpEncodable, RlpEncodableWrapper};
use derive_more::{Deref, IntoIterator};
use smartnet_codecs_derive::{add_arbitrary_tests, generate_tests};

/// A request for a peer to return block headers starting at the requested block.
/// The peer must return at most [`limit`](#structfield.limit) headers.
/// If the [`reverse`](#structfield.reverse) field is `true`, the headers will be returned starting
/// at [`start_block`](#structfield.start_block), traversing towards the genesis block.
/// Otherwise, headers will be returned starting at [`start_block`](#structfield.start_block),
/// traversing towards the latest block.
///
/// If the [`skip`](#structfield.skip) field is non-zero, the peer must skip that amount of headers
/// in the direction specified by [`reverse`](#structfield.reverse).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[add_arbitrary_tests(rlp)]
pub struct GetBlockHeaders {
    /// The block number or hash that the peer should start returning headers from.
    pub start_block: BlockHashOrNumber,

    /// The maximum number of headers to return.
    pub limit: u64,

    /// The number of blocks that the node should skip while traversing and returning headers.
    /// A skip value of zero denotes that the peer should return contiguous headers, starting from
    /// [`start_block`](#structfield.start_block) and returning at most
    /// [`limit`](#structfield.limit) headers.
    pub skip: u32,

    /// The direction in which the headers should be returned in.
    pub direction: HeadersDirection,
}

/// The response to [`GetBlockHeaders`], containing headers if any headers were found.
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
pub struct BlockHeaders<H = alloy_consensus::Header>(
    /// The requested headers.
    pub Vec<H>,
);

generate_tests!(#[rlp, 10] BlockHeaders<alloy_consensus::Header>, EthBlockHeadersTests);

impl<H> From<Vec<H>> for BlockHeaders<H> {
    fn from(headers: Vec<H>) -> Self {
        Self(headers)
    }
}

/// The response to a block-bodies request, containing the block bodies that the peer knows about
/// if any were found.
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
pub struct BlockBodies<B = smartnet_ethereum_primitives::BlockBody>(
    /// The requested block bodies, each of which should correspond to a hash in the request.
    pub Vec<B>,
);

generate_tests!(#[rlp, 16] BlockBodies<smartnet_ethereum_primitives::BlockBody>, EthBlockBodiesTests);

impl<B> From<Vec<B>> for BlockBodies<B> {
    fn from(bodies: Vec<B>) -> Self {
        Self(bodies)
    }
}

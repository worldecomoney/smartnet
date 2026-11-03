//! Implements the `GetPooledTransactions` and `PooledTransactions` message types.

use alloc::vec::Vec;
use alloy_consensus::transaction::PooledTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::B256;
use alloy_rlp::{RlpDecodableWrapper, RlpEncodableWrapper};
use derive_more::{Constructor, Deref, IntoIterator};
use smartnet_codecs_derive::add_arbitrary_tests;

/// A list of transaction hashes that the peer would like transaction bodies for.
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
pub struct GetPooledTransactions(
    /// The transaction hashes to request transaction bodies for.
    pub Vec<B256>,
);

impl<T> From<Vec<T>> for GetPooledTransactions
where
    T: Into<B256>,
{
    fn from(hashes: Vec<T>) -> Self {
        Self(hashes.into_iter().map(|h| h.into()).collect())
    }
}

/// The response to [`GetPooledTransactions`], containing the transaction bodies associated with
/// the requested hashes.
///
/// This response may not contain all bodies requested, but the bodies should be in the same order
/// as the request's hashes. Hashes may be skipped, and the client should ensure that each body
/// corresponds to a requested hash. Hashes may need to be re-requested if the bodies are not
/// included in the response.
// #[derive_arbitrary(rlp, 10)]
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    RlpEncodableWrapper,
    RlpDecodableWrapper,
    IntoIterator,
    Deref,
    Constructor,
)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PooledTransactions<T = PooledTransaction>(
    /// The transaction bodies, each of which should correspond to a requested hash.
    pub Vec<T>,
);

impl<T: Encodable2718> PooledTransactions<T> {
    /// Returns an iterator over the transaction hashes in this response.
    pub fn hashes(&self) -> impl Iterator<Item = B256> + '_ {
        self.iter().map(|tx| tx.trie_hash())
    }
}

impl<T, U> TryFrom<Vec<U>> for PooledTransactions<T>
where
    T: TryFrom<U>,
{
    type Error = T::Error;

    fn try_from(txs: Vec<U>) -> Result<Self, Self::Error> {
        txs.into_iter().map(T::try_from).collect()
    }
}

impl<T> FromIterator<T> for PooledTransactions<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<T> Default for PooledTransactions<T> {
    fn default() -> Self {
        Self(Default::default())
    }
}

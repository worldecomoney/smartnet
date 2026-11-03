//! Standalone crate for ethereum-specific `SmartNet` primitive types.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

// Feature-only dep: activated by `smartnet-codec` feature for downstream consumers.
#[cfg(feature = "smartnet-codec")]
use smartnet_codecs as _;

mod receipt;
pub use receipt::*;

pub use alloy_consensus::{transaction::PooledTransaction, TxType};
use alloy_consensus::{TxEip4844, TxEip4844WithSidecar};
use alloy_eips::eip7594::BlobTransactionSidecarVariant;

/// Typed Transaction type without a signature
pub type Transaction = alloy_consensus::EthereumTypedTransaction<TxEip4844>;

/// Signed transaction.
pub type TransactionSigned = alloy_consensus::EthereumTxEnvelope<TxEip4844>;

/// A type alias for [`PooledTransaction`] that's also generic over blob sidecar.
pub type PooledTransactionVariant =
    alloy_consensus::EthereumTxEnvelope<TxEip4844WithSidecar<BlobTransactionSidecarVariant>>;

/// Type alias for the ethereum block
pub type Block = alloy_consensus::Block<TransactionSigned>;

/// Type alias for the ethereum blockbody
pub type BlockBody = alloy_consensus::BlockBody<TransactionSigned>;

/// Helper struct that specifies the ethereum
/// [`NodePrimitives`](smartnet_primitives_traits::NodePrimitives) types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct EthPrimitives;

impl smartnet_primitives_traits::NodePrimitives for EthPrimitives {
    type Block = crate::Block;
    type BlockHeader = alloy_consensus::Header;
    type BlockBody = crate::BlockBody;
    type SignedTx = crate::TransactionSigned;
    type Receipt = crate::Receipt;
}

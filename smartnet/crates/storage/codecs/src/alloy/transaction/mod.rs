//! Compact implementation for transaction types
use crate::Compact;
use alloy_consensus::{
    transaction::{RlpEcdsaEncodableTx, TxEip1559, TxEip2930, TxEip7702, TxLegacy},
    EthereumTypedTransaction, TxType,
};
use alloy_primitives::bytes::BufMut;

impl<Eip4844> Compact for EthereumTypedTransaction<Eip4844>
where
    Eip4844: Compact + RlpEcdsaEncodableTx,
{
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let identifier = self.tx_type().to_compact(buf);
        match self {
            Self::Legacy(tx) => tx.to_compact(buf),
            Self::Eip2930(tx) => tx.to_compact(buf),
            Self::Eip1559(tx) => tx.to_compact(buf),
            Self::Eip4844(tx) => tx.to_compact(buf),
            Self::Eip7702(tx) => tx.to_compact(buf),
        };
        identifier
    }

    fn from_compact(buf: &[u8], identifier: usize) -> (Self, &[u8]) {
        let (tx_type, buf) = TxType::from_compact(buf, identifier);

        match tx_type {
            TxType::Legacy => {
                let (tx, buf) = TxLegacy::from_compact(buf, buf.len());
                (Self::Legacy(tx), buf)
            }
            TxType::Eip4844 => {
                let (tx, buf) = Eip4844::from_compact(buf, buf.len());
                (Self::Eip4844(tx), buf)
            }
            TxType::Eip7702 => {
                let (tx, buf) = TxEip7702::from_compact(buf, buf.len());
                (Self::Eip7702(tx), buf)
            }
            TxType::Eip1559 => {
                let (tx, buf) = TxEip1559::from_compact(buf, buf.len());
                (Self::Eip1559(tx), buf)
            }
            TxType::Eip2930 => {
                let (tx, buf) = TxEip2930::from_compact(buf, buf.len());
                (Self::Eip2930(tx), buf)
            }
        }
    }
}

cond_mod!(eip1559, eip2930, eip4844, eip7702, legacy, txtype);

mod ethereum;
pub use ethereum::{CompactEnvelope, Envelope, FromTxCompact, ToTxCompact};


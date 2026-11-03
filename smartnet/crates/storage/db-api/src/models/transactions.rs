//! Transaction related models and types.

use alloy_primitives::{Address, BlockNumber};
use bytes::BufMut;
use smartnet_codecs::{add_arbitrary_tests, Compact};
use smartnet_ethereum_primitives::TransactionSigned;
use serde::{Deserialize, Serialize};

/// A canonical transaction row keyed by tx number (SQLITE.md §11 `transactions`, §13.5 3B.2).
///
/// `transaction` is the authoritative payload; `sender` and `block_number` absorb the
/// former `TransactionSenders` and `TransactionBlocks` tables. `sender` stays `None`
/// until the sender-recovery stage has run for the transaction's block.
#[derive(Debug, Default, Eq, PartialEq, Clone, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[add_arbitrary_tests(compact)]
pub struct StoredTransaction<T = TransactionSigned> {
    /// The recovered sender; `None` until sender recovery has processed the transaction.
    pub sender: Option<Address>,
    /// Number of the canonical block that contains the transaction.
    pub block_number: BlockNumber,
    /// The transaction payload. Encoded LAST: the Compact codec's `Bytes` handling
    /// consumes the remainder of the buffer, so a `Bytes`-bearing element may only be
    /// the final field of a layout.
    pub transaction: T,
}

impl<T: Compact> Compact for StoredTransaction<T> {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let mut buffer = bytes::BytesMut::new();
        match &self.sender {
            Some(sender) => {
                buffer.put_u8(1);
                sender.to_compact(&mut buffer);
            }
            None => buffer.put_u8(0),
        }
        // The varint length of `block_number` is stored ahead of the value — see
        // `from_compact`.
        let mut block_buf = bytes::BytesMut::new();
        let block_len = self.block_number.to_compact(&mut block_buf);
        buffer.put_u8(block_len as u8);
        buffer.put(block_buf);
        // NOTE: the transaction is encoded LAST — see the field doc above.
        self.transaction.to_compact(&mut buffer);
        let total_length = buffer.len();
        buf.put(buffer);
        total_length
    }

    fn from_compact(mut buf: &[u8], _len: usize) -> (Self, &[u8]) {
        use bytes::Buf;
        let (sender, rest) = if buf.get_u8() == 1 {
            let (sender, rest) = Address::from_compact(buf, buf.len());
            (Some(sender), rest)
        } else {
            (None, buf)
        };
        buf = rest;
        // `block_number` is a varint whose encoded length is not recoverable from the
        // remaining buffer (the transaction may itself start with zero bytes), so the
        // length is stored immediately before the value.
        let block_len = buf.get_u8() as usize;
        let (block_number, rest) = u64::from_compact(buf, block_len);
        buf = rest;
        // The transaction is last: its decode consumes the remainder of the buffer.
        let (transaction, rest) = T::from_compact(buf, buf.len());
        (Self { sender, block_number, transaction }, rest)
    }
}

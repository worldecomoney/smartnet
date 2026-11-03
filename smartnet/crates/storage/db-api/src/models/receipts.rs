//! Receipt and log related models and types.

use alloy_consensus::{RlpDecodableReceipt, RlpEncodableReceipt, TxReceipt};
use alloy_primitives::{Address, Bloom, Bytes, TxNumber, B256};
use bytes::BufMut;
use smartnet_codecs::{add_arbitrary_tests, Compact};
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// Number of low bits reserved for the per-transaction log index in a [`Logs`] key.
///
/// This bounds the number of logs a single transaction may emit to `2^16 - 1 = 65535`,
/// which no Ethereum transaction can exceed under the block gas limit.
pub const LOG_INDEX_BITS: u32 = 16;

/// Encodes a composite `(tx_number, log_index)` key for the [`Logs`](crate::tables::Logs) table.
///
/// The composite keeps logs ordered by transaction and lets an unwind delete every log of
/// every transaction at or above a threshold with a single range delete
/// (`logs_key_range`). See [`LOG_INDEX_BITS`] for the index bound.
#[inline]
pub const fn logs_key(tx_number: TxNumber, log_index: u16) -> u64 {
    (tx_number << LOG_INDEX_BITS) | (log_index as u64)
}

/// Exclusive end of the log-key range that covers `tx_number..to_tx_number` (end-exclusive).
///
/// Pass `u64::MAX` as `to_tx_number` to cover every transaction from `from_tx_number` upward.
#[inline]
pub const fn logs_key_range(from_tx_number: TxNumber, to_tx_number: TxNumber) -> Range<u64> {
    (from_tx_number << LOG_INDEX_BITS)..(to_tx_number << LOG_INDEX_BITS)
}

/// A canonical receipt row keyed by transaction number (SQLITE.md §11 `receipts`, §13.5 3B.3).
///
/// `receipt_rlp` is the authoritative payload: it is the receipt encoded exactly as it
/// appears on the wire (EIP-2718 with bloom), from which the typed receipt — including
/// its logs — is fully rebuildable. The extracted columns are derived and exist only for
/// point queries that should not pay a full RLP decode.
///
/// [`Logs`](crate::tables::Logs) rows are a second derived index over the same logs,
/// rebuildable by decoding `receipt_rlp` and re-deriving the per-transaction log index.
#[derive(Debug, Default, Eq, PartialEq, Clone, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[add_arbitrary_tests(compact)]
pub struct StoredReceipt {
    /// Whether the transaction executed successfully (the receipt `statusCode`).
    pub status: bool,
    /// Cumulative gas used in the block up to and including this transaction.
    pub cumulative_gas_used: u64,
    /// Gas used by this transaction alone (`cumulative_gas_used` minus the previous
    /// transaction's cumulative in the same block).
    pub gas_used: u64,
    /// Address of the contract created by this transaction, if any.
    ///
    /// Not populated by the execution write path — receipts are written without their
    /// transactions, and the create address is derived from sender + nonce. Present so a
    /// future writer that has the transaction can fill it (SQLITE.md §11 D).
    pub contract_address: Option<Address>,
    /// The authoritative receipt payload (EIP-2718 encoding with bloom). Encoded LAST:
    /// the Compact codec's `Bytes` handling consumes the remainder of the buffer, so a
    /// `Bytes`-bearing element may only be the final field of a layout.
    pub receipt_rlp: Bytes,
}

impl StoredReceipt {
    /// Builds a row from a typed receipt, encoding its EIP-2718 payload as the
    /// authoritative `receipt_rlp` and extracting the derived columns.
    ///
    /// `prev_cumulative_gas_used` is the `cumulative_gas_used` of the previous receipt in
    /// the same block (zero for the block's first receipt); `gas_used` is derived from it.
    ///
    /// `contract_address` and the log bloom are supplied by the caller rather than read
    /// off the trait object: the node's receipt type is only known through the
    /// [`TxReceipt`] trait, which does not expose either.
    pub fn encode_new<R: RlpEncodableReceipt + TxReceipt>(
        receipt: &R,
        prev_cumulative_gas_used: u64,
        contract_address: Option<Address>,
        logs_bloom: Bloom,
    ) -> Self {
        let mut receipt_rlp = Vec::new();
        receipt.rlp_encode_with_bloom(&logs_bloom, &mut receipt_rlp);

        Self {
            status: receipt.status(),
            cumulative_gas_used: receipt.cumulative_gas_used(),
            gas_used: receipt.cumulative_gas_used().saturating_sub(prev_cumulative_gas_used),
            contract_address,
            receipt_rlp: receipt_rlp.into(),
        }
    }

    /// Rebuilds the typed receipt from the authoritative `receipt_rlp`.
    ///
    /// This is the documented rebuild rule for both the extracted columns and the
    /// derived [`Logs`](crate::tables::Logs) rows: decode `receipt_rlp`, then read the
    /// fields/logs straight off the receipt.
    pub fn decode_receipt<R: RlpDecodableReceipt>(&self) -> alloy_rlp::Result<R> {
        R::rlp_decode_with_bloom(&mut self.receipt_rlp.as_ref())
            .map(|with_bloom| with_bloom.receipt)
    }
}

impl Compact for StoredReceipt {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let mut buffer = bytes::BytesMut::new();
        buffer.put_u8(self.status as u8);
        // Fixed-width so the decode cursor never has to recover a varint length from the
        // tail of the buffer.
        buffer.put_u64(self.cumulative_gas_used);
        buffer.put_u64(self.gas_used);
        match &self.contract_address {
            Some(address) => {
                buffer.put_u8(1);
                address.to_compact(&mut buffer);
            }
            None => buffer.put_u8(0),
        }
        // NOTE: the receipt RLP is encoded LAST — see the field doc above.
        self.receipt_rlp.to_compact(&mut buffer);
        let total_length = buffer.len();
        buf.put(buffer);
        total_length
    }

    fn from_compact(mut buf: &[u8], _len: usize) -> (Self, &[u8]) {
        use bytes::Buf;
        let status = buf.get_u8() == 1;
        let cumulative_gas_used = buf.get_u64();
        let gas_used = buf.get_u64();
        let contract_address = (buf.get_u8() == 1).then(|| {
            let (address, rest) = Address::from_compact(buf, buf.len());
            buf = rest;
            address
        });
        // The receipt RLP is last: its decode consumes the remainder of the buffer.
        let (receipt_rlp, rest) = Bytes::from_compact(buf, buf.len());
        (Self { status, cumulative_gas_used, gas_used, contract_address, receipt_rlp }, rest)
    }
}

/// A single log entry, denormalized out of [`StoredReceipt::receipt_rlp`] into the
/// derived [`Logs`](crate::tables::Logs) index (SQLITE.md §11 `logs`, §13.5 3B.3).
///
/// Fully rebuildable by decoding the owning receipt's `receipt_rlp` and enumerating its
/// logs in order. `tx_number` and `log_index` live in the table key — see [`logs_key`].
#[derive(Debug, Default, Eq, PartialEq, Clone, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[add_arbitrary_tests(compact)]
pub struct StoredLog {
    /// Address of the contract that emitted the log.
    pub address: Address,
    /// The log's topics (at most four).
    pub topics: Vec<B256>,
    /// The log's data payload. Encoded LAST: the Compact codec's `Bytes` handling
    /// consumes the remainder of the buffer, so a `Bytes`-bearing element may only be
    /// the final field of a layout.
    pub data: Bytes,
}

impl From<&alloy_primitives::Log> for StoredLog {
    fn from(log: &alloy_primitives::Log) -> Self {
        Self {
            address: log.address,
            topics: log.data.topics().to_vec(),
            data: log.data.data.clone(),
        }
    }
}

impl Compact for StoredLog {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let mut buffer = bytes::BytesMut::new();
        self.address.to_compact(&mut buffer);
        // Topic count is fixed-width and each topic is a fixed 32 bytes, so the decode
        // cursor never has to recover a length from the tail of the buffer.
        buffer.put_u8(self.topics.len() as u8);
        for topic in &self.topics {
            topic.to_compact(&mut buffer);
        }
        // NOTE: the data is encoded LAST — see the field doc above.
        self.data.to_compact(&mut buffer);
        let total_length = buffer.len();
        buf.put(buffer);
        total_length
    }

    fn from_compact(mut buf: &[u8], _len: usize) -> (Self, &[u8]) {
        use bytes::Buf;
        let (address, rest) = Address::from_compact(buf, buf.len());
        buf = rest;
        let topic_count = buf.get_u8() as usize;
        let mut topics = Vec::with_capacity(topic_count);
        for _ in 0..topic_count {
            let (topic, rest) = B256::from_compact(buf, buf.len());
            topics.push(topic);
            buf = rest;
        }
        // The data is last: its decode consumes the remainder of the buffer.
        let (data, rest) = Bytes::from_compact(buf, buf.len());
        (Self { address, topics, data }, rest)
    }
}

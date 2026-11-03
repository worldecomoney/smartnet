//! Block related models and types.

use alloy_consensus::Header;
use alloy_eips::eip4895::Withdrawals;
use alloy_primitives::B256;
use bytes::BufMut;
use smartnet_codecs::{add_arbitrary_tests, Compact};
use serde::{Deserialize, Serialize};
use smartnet_db_models::StoredBlockBodyIndices;

/// The storage representation of a block's ommers.
///
/// It is stored as the headers of the block's uncles.
#[derive(Debug, Default, Eq, PartialEq, Clone, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[add_arbitrary_tests(compact)]
pub struct StoredBlockOmmers<H = Header> {
    /// The block headers of this block's uncles.
    pub ommers: Vec<H>,
}

impl<H: Compact> Compact for StoredBlockOmmers<H> {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let mut buffer = bytes::BytesMut::new();
        self.ommers.to_compact(&mut buffer);
        let total_length = buffer.len();
        buf.put(buffer);
        total_length
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let (ommers, new_buf) = Vec::from_compact(buf, buf.len());
        (Self { ommers }, new_buf)
    }
}

/// A full block row keyed by block hash (SQLITE.md §11 `blocks`, §13.5 3B.1).
///
/// `header` is authoritative for hashing; ommers/withdrawals/body indices are the
/// absorbed `BlockOmmers`/`BlockWithdrawals`/`BlockBodyIndices` payloads. Rows are
/// written header-first (headers stage); `body_indices` stays `None` until the body
/// for the block is written.
#[derive(Debug, Default, Eq, PartialEq, Clone, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[add_arbitrary_tests(compact)]
pub struct StoredBlock<H = Header> {
    /// The block header. Authoritative payload: its hash is the row key.
    pub header: H,
    /// The block's uncles/ommers (empty post-merge).
    pub ommers: Vec<H>,
    /// The block withdrawals. `None` for pre-Shanghai or not-yet-written bodies.
    pub withdrawals: Option<Withdrawals>,
    /// Transaction range of the block. `None` while the row is header-only.
    pub body_indices: Option<StoredBlockBodyIndices>,
}

impl<H: Compact> Compact for StoredBlock<H> {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let mut buffer = bytes::BytesMut::new();
        // NOTE: the header is encoded LAST — the Compact codec's `Bytes` handling
        // (`extra_data` inside the header) consumes the remainder of the buffer, so a
        // `Bytes`-bearing struct may only ever be the final element of a layout.
        self.ommers.to_compact(&mut buffer);
        match &self.withdrawals {
            Some(withdrawals) => {
                buffer.put_u8(1);
                withdrawals.to_compact(&mut buffer);
            }
            None => buffer.put_u8(0),
        }
        match &self.body_indices {
            Some(indices) => {
                buffer.put_u8(1);
                indices.to_compact(&mut buffer);
            }
            None => buffer.put_u8(0),
        }
        self.header.to_compact(&mut buffer);
        let total_length = buffer.len();
        buf.put(buffer);
        total_length
    }

    fn from_compact(mut buf: &[u8], _len: usize) -> (Self, &[u8]) {
        use bytes::Buf;
        let (ommers, rest) = Vec::from_compact(buf, buf.len());
        buf = rest;
        let (withdrawals, rest) = if buf.get_u8() == 1 {
            let (withdrawals, rest) = Withdrawals::from_compact(buf, buf.len());
            (Some(withdrawals), rest)
        } else {
            (None, buf)
        };
        buf = rest;
        let (body_indices, rest) = if buf.get_u8() == 1 {
            let (indices, rest) = StoredBlockBodyIndices::from_compact(buf, buf.len());
            (Some(indices), rest)
        } else {
            (None, buf)
        };
        buf = rest;
        // The header is last: its decode consumes the remainder of the buffer.
        let (header, rest) = H::from_compact(buf, buf.len());
        (Self { header, ommers, withdrawals, body_indices }, rest)
    }
}

/// Hash of the block header.
pub type HeaderHash = B256;


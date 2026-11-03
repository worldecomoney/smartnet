//! Implements the `BlockAccessLists` message type.

use alloc::vec::Vec;
use alloy_primitives::Bytes;
use alloy_rlp::{BufMut, Decodable, Encodable, Header};
use smartnet_codecs_derive::add_arbitrary_tests;

/// Response containing one BAL per requested block hash.
///
/// The inner `Bytes` values store raw BAL RLP payloads and are encoded as nested RLP items, not
/// as RLP byte strings.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[add_arbitrary_tests(rlp)]
pub struct BlockAccessLists(
    /// The requested block access lists as raw RLP blobs. Per EIP-8159, unavailable entries are
    /// represented by an RLP-encoded empty list (`0xc0`).
    pub Vec<Bytes>,
);

impl Encodable for BlockAccessLists {
    fn encode(&self, out: &mut dyn BufMut) {
        let payload_length = self.0.iter().map(|bytes| bytes.len()).sum();
        Header { list: true, payload_length }.encode(out);
        for bal in &self.0 {
            out.put_slice(bal);
        }
    }

    fn length(&self) -> usize {
        let payload_length = self.0.iter().map(|bytes| bytes.len()).sum();
        Header { list: true, payload_length }.length_with_payload()
    }
}

impl Decodable for BlockAccessLists {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        let (mut payload, rest) = buf.split_at(header.payload_length);
        *buf = rest;
        let mut bals = Vec::new();

        while !payload.is_empty() {
            let item_start = payload;
            let item_header = Header::decode(&mut payload)?;
            if !item_header.list {
                return Err(alloy_rlp::Error::UnexpectedString);
            }

            let item_length = item_header.length_with_payload();
            bals.push(Bytes::copy_from_slice(&item_start[..item_length]));
            payload = &payload[item_header.payload_length..];
        }

        Ok(Self(bals))
    }
}

#[cfg(any(test, feature = "arbitrary"))]
impl<'a> arbitrary::Arbitrary<'a> for BlockAccessLists {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let entries = Vec::<Vec<alloy_eip7928::AccountChanges>>::arbitrary(u)?
            .into_iter()
            .map(|entry| {
                let mut out = Vec::new();
                alloy_rlp::encode_list(&entry, &mut out);
                Bytes::from(out)
            })
            .collect();
        Ok(Self(entries))
    }
}


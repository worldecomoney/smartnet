//! Compact implementation for [`AlloyAuthorization`]

use crate::Compact;
use alloy_eips::eip7702::{Authorization as AlloyAuthorization, SignedAuthorization};
use alloy_primitives::{Address, U256};
use bytes::Buf;
use core::ops::Deref;
use smartnet_codecs_derive::add_arbitrary_tests;

/// Authorization acts as bridge which simplifies Compact implementation for `AlloyAuthorization`.
///
/// Notice: Make sure this struct is 1:1 with `alloy_eips::eip7702::Authorization`
#[derive(Debug, Clone, PartialEq, Eq, Default, Compact)]
#[smartnet_codecs(crate = "crate")]
#[cfg_attr(
    any(test, feature = "test-utils"),
    derive(arbitrary::Arbitrary, serde::Serialize, serde::Deserialize)
)]
#[cfg_attr(feature = "test-utils", allow(unreachable_pub), visibility::make(pub))]
#[add_arbitrary_tests(crate, compact)]
pub(crate) struct Authorization {
    chain_id: U256,
    address: Address,
    nonce: u64,
}

impl Compact for AlloyAuthorization {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let authorization =
            Authorization { chain_id: self.chain_id, address: self.address, nonce: self.nonce() };
        authorization.to_compact(buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (authorization, buf) = Authorization::from_compact(buf, len);
        let alloy_authorization = Self {
            chain_id: authorization.chain_id,
            address: authorization.address,
            nonce: authorization.nonce,
        };
        (alloy_authorization, buf)
    }
}

impl Compact for SignedAuthorization {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        buf.put_u8(self.y_parity());
        buf.put_slice(self.r().as_le_slice());
        buf.put_slice(self.s().as_le_slice());

        // to_compact doesn't write the len to buffer.
        // By placing it as last, we don't need to store it either.
        1 + 32 + 32 + self.deref().to_compact(buf)
    }

    fn from_compact(mut buf: &[u8], len: usize) -> (Self, &[u8]) {
        let y_parity = buf.get_u8();
        let r = U256::from_le_slice(&buf[0..32]);
        buf.advance(32);
        let s = U256::from_le_slice(&buf[0..32]);
        buf.advance(32);

        let (auth, buf) = AlloyAuthorization::from_compact(buf, len);

        (Self::new_unchecked(auth, y_parity, r, s), buf)
    }
}


//! Compact implementation for [`AccessList`]

use crate::Compact;
use alloc::vec::Vec;
use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::Address;

/// Implement `Compact` for `AccessListItem` and `AccessList`.
impl Compact for AccessListItem {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let mut buffer = Vec::new();
        self.address.to_compact(&mut buffer);
        self.storage_keys.specialized_to_compact(&mut buffer);
        buf.put(&buffer[..]);
        buffer.len()
    }

    fn from_compact(mut buf: &[u8], _: usize) -> (Self, &[u8]) {
        let (address, new_buf) = Address::from_compact(buf, buf.len());
        buf = new_buf;
        let (storage_keys, new_buf) = Vec::specialized_from_compact(buf, buf.len());
        buf = new_buf;
        let access_list_item = Self { address, storage_keys };
        (access_list_item, buf)
    }
}

impl Compact for AccessList {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let mut buffer = Vec::new();
        self.0.to_compact(&mut buffer);
        buf.put(&buffer[..]);
        buffer.len()
    }

    fn from_compact(mut buf: &[u8], _: usize) -> (Self, &[u8]) {
        let (access_list_items, new_buf) = Vec::from_compact(buf, buf.len());
        buf = new_buf;
        let access_list = Self(access_list_items);
        (access_list, buf)
    }
}


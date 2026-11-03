//! Native Compact codec impl for primitive alloy log types.

use crate::Compact;
use alloc::vec::Vec;
use alloy_primitives::{Address, Bytes, Log, LogData};
use bytes::BufMut;

/// Implement `Compact` for `LogData` and `Log`.
impl Compact for LogData {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let mut buffer = Vec::new();

        self.topics().specialized_to_compact(&mut buffer);
        self.data.to_compact(&mut buffer);
        buf.put(&buffer[..]);
        buffer.len()
    }

    fn from_compact(mut buf: &[u8], _: usize) -> (Self, &[u8]) {
        let (topics, new_buf) = Vec::specialized_from_compact(buf, buf.len());
        buf = new_buf;
        let (data, buf) = Bytes::from_compact(buf, buf.len());
        let log_data = Self::new_unchecked(topics, data);
        (log_data, buf)
    }
}

impl Compact for Log {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let mut buffer = Vec::new();
        self.address.to_compact(&mut buffer);
        self.data.to_compact(&mut buffer);
        buf.put(&buffer[..]);
        buffer.len()
    }

    fn from_compact(mut buf: &[u8], _: usize) -> (Self, &[u8]) {
        let (address, new_buf) = Address::from_compact(buf, buf.len());
        buf = new_buf;
        let (log_data, new_buf) = LogData::from_compact(buf, buf.len());
        buf = new_buf;
        let log = Self { address, data: log_data };
        (log, buf)
    }
}


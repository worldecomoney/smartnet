//! Compact codec.
//!
//! *Warning*: The `Compact` encoding format and its implementations are
//! designed for storing and retrieving data internally. They are not hardened
//! to safely read potentially malicious data.
//!
//! ## Feature Flags
//!
//! - `alloy`: [Compact] implementation for various alloy types.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub use smartnet_codecs_derive::*;
use serde as _;

use alloy_primitives::{Address, Bloom, Bytes, FixedBytes, U256};
use bytes::{Buf, BufMut};

use alloc::{
    borrow::{Cow, ToOwned},
    vec::Vec,
};
use core::num::NonZeroU64;

#[cfg(feature = "alloy")]
pub mod alloy;
#[cfg(feature = "alloy")]
pub mod lz4;

pub mod compress;
pub use compress::{Compress, Decompress, DecompressError};

pub mod txtype;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

// Used by generated code and doc tests. Not public API.
#[doc(hidden)]
#[path = "private.rs"]
pub mod __private;

/// Trait that implements the `Compact` codec.
///
/// When deriving the trait for custom structs, be aware of certain limitations/recommendations:
/// * Works best with structs that only have native types (eg. u64, B256, U256).
/// * Fixed array types (B256, Address, Bloom) are not compacted.
/// * Max size of `T` in `Option<T>` or `Vec<T>` shouldn't exceed `0xffff`.
/// * Any `Bytes` field **should be placed last**.
/// * Any other type which is not known to the derive module **should be placed last** in they
///   contain a `Bytes` field.
///
/// The last two points make it easier to decode the data without saving the length on the
/// `StructFlags`. It will fail compilation if it's not respected. If they're alias to known types,
/// add their definitions to `get_bit_size()` or `known_types` in `generator.rs`.
///
/// Regarding the `specialized_to/from_compact` methods: Mainly used as a workaround for not being
/// able to specialize an impl over certain types like `Vec<T>`/`Option<T>` where `T` is a fixed
/// size array like `Vec<B256>`.
///
/// ## Caution
///
/// Due to the bitfields, every type change on the rust type (e.g. `U256` to `u64`) is a breaking
/// change and will lead to a new, incompatible [`Compact`] implementation. Implementers must take
/// special care when changing or rearranging fields.
pub trait Compact: Sized {
    /// Takes a buffer which can be written to. *Ideally*, it returns the length written to.
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>;

    /// Takes a buffer which can be read from. Returns the object and `buf` with its internal cursor
    /// advanced (eg.`.advance(len)`).
    ///
    /// `len` can either be the `buf` remaining length, or the length of the compacted type.
    ///
    /// It will panic, if `len` is smaller than `buf.len()`.
    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]);

    /// "Optional": If there's no good reason to use it, don't.
    #[inline]
    fn specialized_to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.to_compact(buf)
    }

    /// "Optional": If there's no good reason to use it, don't.
    #[inline]
    fn specialized_from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        Self::from_compact(buf, len)
    }
}

impl Compact for alloc::string::String {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.as_bytes().to_compact(buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (vec, buf) = Vec::<u8>::from_compact(buf, len);
        let string = Self::from_utf8(vec).unwrap(); // Safe conversion
        (string, buf)
    }
}

impl<T: Compact> Compact for &T {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        (*self).to_compact(buf)
    }

    fn from_compact(_: &[u8], _: usize) -> (Self, &[u8]) {
        unimplemented!()
    }
}

/// To be used with `Option<CompactPlaceholder>` to place or replace one bit on the bitflag struct.
pub type CompactPlaceholder = ();

impl Compact for CompactPlaceholder {
    #[inline]
    fn to_compact<B>(&self, _: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        0
    }

    #[inline]
    fn from_compact(buf: &[u8], _: usize) -> (Self, &[u8]) {
        ((), buf)
    }
}

macro_rules! impl_uint_compact {
    ($($name:tt),+) => {
        $(
            impl Compact for $name {
                #[inline]
                fn to_compact<B>(&self, buf: &mut B) -> usize
                    where B: bytes::BufMut + AsMut<[u8]>
                {
                    let leading = self.leading_zeros() as usize / 8;
                    buf.put_slice(&self.to_be_bytes()[leading..]);
                    core::mem::size_of::<$name>() - leading
                }

                #[inline]
                fn from_compact(mut buf: &[u8], len: usize) -> (Self, &[u8]) {
                    if len == 0 {
                        return (0, buf);
                    }

                    let mut arr = [0; core::mem::size_of::<$name>()];
                    arr[core::mem::size_of::<$name>() - len..].copy_from_slice(&buf[..len]);
                    buf.advance(len);
                    ($name::from_be_bytes(arr), buf)
                }
            }
        )+
    };
}

impl_uint_compact!(u8, u64, u128);

impl Compact for NonZeroU64 {
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.get().to_compact(buf)
    }

    #[inline]
    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (value, buf) = u64::from_compact(buf, len);
        (Self::new(value).expect("compact non-zero integer cannot decode zero"), buf)
    }
}

impl<T> Compact for Vec<T>
where
    T: Compact,
{
    /// Returns 0 since we won't include it in the `StructFlags`.
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.as_slice().to_compact(buf)
    }

    #[inline]
    fn from_compact(buf: &[u8], _: usize) -> (Self, &[u8]) {
        let (length, mut buf) = decode_varuint(buf);
        let mut list = Self::with_capacity(length);
        for _ in 0..length {
            let len;
            (len, buf) = decode_varuint(buf);

            let (element, _) = T::from_compact(&buf[..len], len);
            buf.advance(len);

            list.push(element);
        }

        (list, buf)
    }

    /// To be used by fixed sized types like `Vec<B256>`.
    #[inline]
    fn specialized_to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.as_slice().specialized_to_compact(buf)
    }

    /// To be used by fixed sized types like `Vec<B256>`.
    #[inline]
    fn specialized_from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (length, mut buf) = decode_varuint(buf);
        let mut list = Self::with_capacity(length);

        for _ in 0..length {
            let element;
            (element, buf) = T::from_compact(buf, len);
            list.push(element);
        }

        (list, buf)
    }
}

impl<T> Compact for &[T]
where
    T: Compact,
{
    /// Returns 0 since we won't include it in the `StructFlags`.
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        encode_varuint(self.len(), buf);

        let mut tmp: Vec<u8> = Vec::with_capacity(64);

        for element in *self {
            tmp.clear();

            // We don't know the length until we compact it
            let length = element.to_compact(&mut tmp);
            encode_varuint(length, buf);

            buf.put_slice(&tmp);
        }

        0
    }

    #[inline]
    fn from_compact(_: &[u8], _: usize) -> (Self, &[u8]) {
        unimplemented!()
    }

    /// To be used by fixed sized types like `&[B256]`.
    #[inline]
    fn specialized_to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        encode_varuint(self.len(), buf);
        for element in *self {
            element.to_compact(buf);
        }
        0
    }

    #[inline]
    fn specialized_from_compact(_: &[u8], _: usize) -> (Self, &[u8]) {
        unimplemented!()
    }
}

impl<T> Compact for Option<T>
where
    T: Compact,
{
    /// Returns 0 for `None` and 1 for `Some(_)`.
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let Some(element) = self else { return 0 };

        // We don't know the length of the element until we compact it.
        let mut tmp = Vec::with_capacity(64);
        let length = element.to_compact(&mut tmp);

        encode_varuint(length, buf);

        buf.put_slice(&tmp);

        1
    }

    #[inline]
    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        if len == 0 {
            return (None, buf);
        }

        let (len, buf) = decode_varuint(buf);

        let (element, buf) = T::from_compact(buf, len);

        (Some(element), buf)
    }

    /// To be used by fixed sized types like `Option<B256>`.
    #[inline]
    fn specialized_to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        if let Some(element) = self {
            element.to_compact(buf);
            1
        } else {
            0
        }
    }

    /// To be used by fixed sized types like `Option<B256>`.
    #[inline]
    fn specialized_from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        if len == 0 {
            return (None, buf);
        }

        let (element, buf) = T::from_compact(buf, len);
        (Some(element), buf)
    }
}

impl<T: Compact + ToOwned<Owned = T>> Compact for Cow<'_, T> {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.as_ref().to_compact(buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (element, buf) = T::from_compact(buf, len);
        (Cow::Owned(element), buf)
    }

    fn specialized_to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.as_ref().specialized_to_compact(buf)
    }

    fn specialized_from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (element, buf) = T::specialized_from_compact(buf, len);
        (Cow::Owned(element), buf)
    }
}

impl Compact for U256 {
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let inner = self.to_be_bytes::<32>();
        let size = 32 - (self.leading_zeros() / 8);
        buf.put_slice(&inner[32 - size..]);
        size
    }

    #[inline]
    fn from_compact(mut buf: &[u8], len: usize) -> (Self, &[u8]) {
        if len == 0 {
            return (Self::ZERO, buf);
        }

        let mut arr = [0; 32];
        arr[(32 - len)..].copy_from_slice(&buf[..len]);
        buf.advance(len);
        (Self::from_be_bytes(arr), buf)
    }
}

impl Compact for Bytes {
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let len = self.len();
        buf.put_slice(&self.0);
        len
    }

    #[inline]
    fn from_compact(mut buf: &[u8], len: usize) -> (Self, &[u8]) {
        (buf.copy_to_bytes(len).into(), buf)
    }
}

impl<const N: usize> Compact for [u8; N] {
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        buf.put_slice(&self[..]);
        N
    }

    #[inline]
    fn from_compact(mut buf: &[u8], len: usize) -> (Self, &[u8]) {
        if len == 0 {
            return ([0; N], buf);
        }

        let v = buf[..N].try_into().unwrap();
        buf.advance(N);
        (v, buf)
    }
}

/// Implements the [`Compact`] trait for wrappers over fixed size byte array types.
#[macro_export]
macro_rules! impl_compact_for_wrapped_bytes {
    ($($name:tt),+) => {
        $(
            impl Compact for $name {
                #[inline]
                fn to_compact<B>(&self, buf: &mut B) -> usize
                where
                    B: bytes::BufMut + AsMut<[u8]>
                {
                    self.0.to_compact(buf)
                }

                #[inline]
                fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
                    let (v, buf) = <[u8; core::mem::size_of::<$name>()]>::from_compact(buf, len);
                    (Self::from(v), buf)
                }
            }
        )+
    };
}
impl_compact_for_wrapped_bytes!(Address, Bloom);

impl<const N: usize> Compact for FixedBytes<N> {
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.0.to_compact(buf)
    }

    #[inline]
    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (v, buf) = <[u8; N]>::from_compact(buf, len);
        (Self::from(v), buf)
    }
}

impl Compact for bool {
    /// `bool` vars go directly to the `StructFlags` and are not written to the buffer.
    #[inline]
    fn to_compact<B>(&self, _: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        *self as usize
    }

    /// `bool` expects the real value to come in `len`, and does not advance the cursor.
    #[inline]
    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        (len != 0, buf)
    }
}

fn encode_varuint<B>(mut n: usize, buf: &mut B)
where
    B: bytes::BufMut + AsMut<[u8]>,
{
    while n >= 0x80 {
        buf.put_u8((n as u8) | 0x80);
        n >>= 7;
    }
    buf.put_u8(n as u8);
}

fn decode_varuint(buf: &[u8]) -> (usize, &[u8]) {
    let mut value = 0;

    for i in 0..33 {
        let byte = buf[i];
        value |= usize::from(byte & 0x7F) << (i * 7);
        if byte < 0x80 {
            return (value, &buf[i + 1..]);
        }
    }

    decode_varuint_panic();
}

#[inline(never)]
#[cold]
const fn decode_varuint_panic() -> ! {
    panic!("could not decode varuint");
}


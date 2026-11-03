//! LZ4 block compression for Compact-encoded values (merged from the former
//! `smartnet-lz4-compressors` crate).
//!
//! Commonly used LZ4 [`Compressor`] and [`ReusableDecompressor`] for `SmartNet` types.
//!
//! Compressed values are laid out as `[u32 LE uncompressed length][lz4 block]`, so a value can be
//! decompressed without knowing its target size upfront.



use alloc::vec::Vec;

/// LZ4 block compressor.
#[derive(Debug, Clone, Copy, Default)]
pub struct Compressor;

impl Compressor {
    /// Creates a new compressor.
    pub const fn new() -> Self {
        Self
    }

    /// Compresses `data`, prepending its uncompressed length.
    pub fn compress(&mut self, data: &[u8]) -> Vec<u8> {
        lz4_flex::block::compress_prepend_size(data)
    }
}

/// LZ4 block decompressor that reuses its internal buffer between calls.
#[expect(missing_debug_implementations)]
pub struct ReusableDecompressor {
    /// The buffer to decompress into.
    buf: Vec<u8>,
}

impl ReusableDecompressor {
    fn new() -> Self {
        Self { buf: Vec::with_capacity(4096) }
    }

    /// Decompresses `src` reusing the internal buffer.
    #[track_caller]
    pub fn decompress(&mut self, src: &[u8]) -> &[u8] {
        assert!(src.len() >= 4, "compressed value is missing its length prefix");

        let uncompressed_len =
            u32::from_le_bytes(src[..4].try_into().expect("length prefix is exactly four bytes"))
                as usize;

        self.buf.clear();
        if let Err(err) = self.buf.try_reserve(uncompressed_len) {
            panic!(
                "failed to reserve {uncompressed_len} bytes for decompressing {} bytes: {err}",
                src.len(),
            );
        }
        self.buf.resize(uncompressed_len, 0);

        let written = lz4_flex::block::decompress_into(&src[4..], &mut self.buf)
            .unwrap_or_else(|err| panic!("failed to decompress {} bytes: {err}", src.len()));
        self.buf.truncate(written);
        &self.buf
    }
}

/// Creates a transaction [`Compressor`].
pub const fn create_tx_compressor() -> Compressor {
    Compressor::new()
}

/// Creates a transaction [`ReusableDecompressor`].
pub fn create_tx_decompressor() -> ReusableDecompressor {
    ReusableDecompressor::new()
}

/// Creates a receipt [`Compressor`].
pub const fn create_receipt_compressor() -> Compressor {
    Compressor::new()
}

/// Creates a receipt [`ReusableDecompressor`].
pub fn create_receipt_decompressor() -> ReusableDecompressor {
    ReusableDecompressor::new()
}

#[cfg(feature = "std")]
pub use locals::*;
#[cfg(feature = "std")]
mod locals {
    use super::*;
    use core::cell::RefCell;

    std::thread_local! {
        /// Thread Transaction compressor.
        pub static TRANSACTION_COMPRESSOR: RefCell<Compressor> = const { RefCell::new(Compressor::new()) };

        /// Thread Transaction decompressor.
        pub static TRANSACTION_DECOMPRESSOR: RefCell<ReusableDecompressor> =
            RefCell::new(ReusableDecompressor::new());

        /// Thread receipt compressor.
        pub static RECEIPT_COMPRESSOR: RefCell<Compressor> = const { RefCell::new(Compressor::new()) };

        /// Thread receipt decompressor.
        pub static RECEIPT_DECOMPRESSOR: RefCell<ReusableDecompressor> =
            RefCell::new(ReusableDecompressor::new());
    }
}

/// Executes `f` with the thread-local transaction compressor on `std`, otherwise creates a new one.
#[inline]
pub fn with_tx_compressor<R>(f: impl FnOnce(&mut Compressor) -> R) -> R {
    #[cfg(feature = "std")]
    {
        TRANSACTION_COMPRESSOR.with_borrow_mut(f)
    }
    #[cfg(not(feature = "std"))]
    {
        f(&mut create_tx_compressor())
    }
}

/// Executes `f` with the thread-local transaction decompressor on `std`, otherwise creates a new
/// one.
#[inline]
pub fn with_tx_decompressor<R>(f: impl FnOnce(&mut ReusableDecompressor) -> R) -> R {
    #[cfg(feature = "std")]
    {
        TRANSACTION_DECOMPRESSOR.with_borrow_mut(f)
    }
    #[cfg(not(feature = "std"))]
    {
        f(&mut create_tx_decompressor())
    }
}

/// Executes `f` with the thread-local receipt compressor on `std`, otherwise creates a new one.
#[inline]
pub fn with_receipt_compressor<R>(f: impl FnOnce(&mut Compressor) -> R) -> R {
    #[cfg(feature = "std")]
    {
        RECEIPT_COMPRESSOR.with_borrow_mut(f)
    }
    #[cfg(not(feature = "std"))]
    {
        f(&mut create_receipt_compressor())
    }
}

/// Executes `f` with the thread-local receipt decompressor on `std`, otherwise creates a new
/// one.
#[inline]
pub fn with_receipt_decompressor<R>(f: impl FnOnce(&mut ReusableDecompressor) -> R) -> R {
    #[cfg(feature = "std")]
    {
        RECEIPT_DECOMPRESSOR.with_borrow_mut(f)
    }
    #[cfg(not(feature = "std"))]
    {
        f(&mut create_receipt_decompressor())
    }
}


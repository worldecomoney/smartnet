use alloc::vec::Vec;
use core::cmp::Ordering;
use derive_more::Deref;
pub use nybbles::Nibbles;

/// Compares two [`Nibbles`] in depth-first order.
///
/// In depth-first ordering:
/// - Descendants come before their ancestors (children before parents)
/// - Siblings are ordered lexicographically
pub fn depth_first_cmp(a: &Nibbles, b: &Nibbles) -> Ordering {
    if a.len() == b.len() {
        return a.cmp(b);
    }

    let common_prefix_len = a.common_prefix_length(b);
    if a.len() == common_prefix_len {
        return Ordering::Greater;
    } else if b.len() == common_prefix_len {
        return Ordering::Less;
    }

    a.get_unchecked(common_prefix_len).cmp(&b.get_unchecked(common_prefix_len))
}

/// The representation of nibbles of the merkle trie stored in the database.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, derive_more::Index)]
#[cfg_attr(any(test, feature = "serde"), derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "test-utils", derive(arbitrary::Arbitrary))]
pub struct StoredNibbles(pub Nibbles);

impl From<Nibbles> for StoredNibbles {
    #[inline]
    fn from(value: Nibbles) -> Self {
        Self(value)
    }
}

impl From<Vec<u8>> for StoredNibbles {
    #[inline]
    fn from(value: Vec<u8>) -> Self {
        Self(Nibbles::from_nibbles_unchecked(value))
    }
}

#[cfg(any(test, feature = "smartnet-codec"))]
impl smartnet_codecs::Compact for StoredNibbles {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let bytes = self.0.iter().collect::<arrayvec::ArrayVec<u8, 64>>();
        buf.put_slice(&bytes);
        bytes.len()
    }

    fn from_compact(mut buf: &[u8], len: usize) -> (Self, &[u8]) {
        use bytes::Buf;

        let nibbles = &buf[..len];
        buf.advance(len);
        (Self(Nibbles::from_nibbles_unchecked(nibbles)), buf)
    }
}

#[cfg(any(test, feature = "smartnet-codec"))]
smartnet_codecs::impl_compression_for_compact!(StoredNibbles);

/// The representation of nibbles of the merkle trie stored in the database.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deref)]
#[cfg_attr(any(test, feature = "serde"), derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "test-utils", derive(arbitrary::Arbitrary))]
pub struct StoredNibblesSubKey(pub Nibbles);

impl From<Nibbles> for StoredNibblesSubKey {
    #[inline]
    fn from(value: Nibbles) -> Self {
        Self(value)
    }
}

impl From<Vec<u8>> for StoredNibblesSubKey {
    #[inline]
    fn from(value: Vec<u8>) -> Self {
        Self(Nibbles::from_nibbles_unchecked(value))
    }
}

impl From<StoredNibblesSubKey> for Nibbles {
    #[inline]
    fn from(value: StoredNibblesSubKey) -> Self {
        value.0
    }
}

impl StoredNibblesSubKey {
    /// Encodes the nibbles into a fixed-size `[u8; 65]` array.
    ///
    /// The first 64 bytes contain the nibble values (right-padded with zeros),
    /// and the 65th byte stores the actual nibble count.
    pub fn to_compact_array(&self) -> [u8; 65] {
        assert!(self.0.len() <= 64);
        let mut buf = [0u8; 65];
        for (i, nibble) in self.0.iter().enumerate() {
            buf[i] = nibble;
        }
        buf[64] = self.0.len() as u8;
        buf
    }
}

#[cfg(any(test, feature = "smartnet-codec"))]
impl smartnet_codecs::Compact for StoredNibblesSubKey {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        assert!(self.0.len() <= 64);

        let bytes = self.0.iter().collect::<arrayvec::ArrayVec<u8, 64>>();
        buf.put_slice(&bytes);

        // Right-pad with zeros
        static ZERO: &[u8; 64] = &[0; 64];
        buf.put_slice(&ZERO[bytes.len()..]);

        buf.put_u8(bytes.len() as u8);
        64 + 1
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let len = buf[64] as usize;
        (Self(Nibbles::from_nibbles_unchecked(&buf[..len])), &buf[65..])
    }
}

#[cfg(any(test, feature = "smartnet-codec"))]
smartnet_codecs::impl_compression_for_compact!(StoredNibblesSubKey);

/// Packed representation of nibbles for the `AccountsTrie` (storage v2).
///
/// Stores 2 nibbles per byte via [`Nibbles::pack`], right-padded to 32 bytes + 1 nibble-count
/// byte = 33 bytes fixed. This halves the key size compared to [`StoredNibbles`] while
/// preserving sort order under `memcmp`.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, derive_more::Index)]
#[cfg_attr(any(test, feature = "serde"), derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "test-utils", derive(arbitrary::Arbitrary))]
pub struct PackedStoredNibbles(pub Nibbles);

impl From<Nibbles> for PackedStoredNibbles {
    #[inline]
    fn from(value: Nibbles) -> Self {
        Self(value)
    }
}

impl From<Vec<u8>> for PackedStoredNibbles {
    #[inline]
    fn from(value: Vec<u8>) -> Self {
        Self(Nibbles::from_nibbles_unchecked(value))
    }
}

impl From<StoredNibbles> for PackedStoredNibbles {
    #[inline]
    fn from(value: StoredNibbles) -> Self {
        Self(value.0)
    }
}

impl From<PackedStoredNibbles> for StoredNibbles {
    #[inline]
    fn from(value: PackedStoredNibbles) -> Self {
        Self(value.0)
    }
}

impl PackedStoredNibbles {
    /// Encodes the nibbles into a fixed-size `[u8; 33]` array.
    ///
    /// The first 32 bytes contain the packed nibbles (2 per byte, right-padded with zeros),
    /// and the 33rd byte stores the actual nibble count.
    pub fn to_compact_array(&self) -> [u8; 33] {
        assert!(self.0.len() <= 64);
        let mut buf = [0u8; 33];
        self.0.pack_to(&mut buf[..32]);
        buf[32] = self.0.len() as u8;
        buf
    }
}

#[cfg(any(test, feature = "smartnet-codec"))]
impl smartnet_codecs::Compact for PackedStoredNibbles {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        assert!(self.0.len() <= 64);

        let mut packed = [0u8; 32];
        self.0.pack_to(&mut packed);
        buf.put_slice(&packed);
        buf.put_u8(self.0.len() as u8);
        33
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let nibble_count = buf[32] as usize;
        let packed_len = nibble_count.div_ceil(2);
        (Self(Nibbles::unpack(&buf[..packed_len]).slice(..nibble_count)), &buf[33..])
    }
}

#[cfg(any(test, feature = "smartnet-codec"))]
smartnet_codecs::impl_compression_for_compact!(PackedStoredNibbles);

/// Packed representation of nibbles as a `DupSort` subkey for `StoragesTrie` (storage v2).
///
/// Stores 2 nibbles per byte via [`Nibbles::pack`], right-padded to 32 bytes + 1 nibble-count
/// byte = 33 bytes fixed. This halves the subkey size compared to [`StoredNibblesSubKey`]
/// (65 → 33 bytes) while preserving sort order under `memcmp`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deref)]
#[cfg_attr(any(test, feature = "serde"), derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "test-utils", derive(arbitrary::Arbitrary))]
pub struct PackedStoredNibblesSubKey(pub Nibbles);

impl From<Nibbles> for PackedStoredNibblesSubKey {
    #[inline]
    fn from(value: Nibbles) -> Self {
        Self(value)
    }
}

impl From<Vec<u8>> for PackedStoredNibblesSubKey {
    #[inline]
    fn from(value: Vec<u8>) -> Self {
        Self(Nibbles::from_nibbles_unchecked(value))
    }
}

impl From<PackedStoredNibblesSubKey> for Nibbles {
    #[inline]
    fn from(value: PackedStoredNibblesSubKey) -> Self {
        value.0
    }
}

impl From<StoredNibblesSubKey> for PackedStoredNibblesSubKey {
    #[inline]
    fn from(value: StoredNibblesSubKey) -> Self {
        Self(value.0)
    }
}

impl From<PackedStoredNibblesSubKey> for StoredNibblesSubKey {
    #[inline]
    fn from(value: PackedStoredNibblesSubKey) -> Self {
        Self(value.0)
    }
}

impl PackedStoredNibblesSubKey {
    /// Encodes the nibbles into a fixed-size `[u8; 33]` array.
    ///
    /// The first 32 bytes contain the packed nibbles (2 per byte, right-padded with zeros),
    /// and the 33rd byte stores the actual nibble count.
    pub fn to_compact_array(&self) -> [u8; 33] {
        assert!(self.0.len() <= 64);
        let mut buf = [0u8; 33];
        self.0.pack_to(&mut buf[..32]);
        buf[32] = self.0.len() as u8;
        buf
    }
}

#[cfg(any(test, feature = "smartnet-codec"))]
impl smartnet_codecs::Compact for PackedStoredNibblesSubKey {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        assert!(self.0.len() <= 64);

        let mut packed = [0u8; 32];
        self.0.pack_to(&mut packed);
        buf.put_slice(&packed);
        buf.put_u8(self.0.len() as u8);
        33
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let nibble_count = buf[32] as usize;
        let packed_len = nibble_count.div_ceil(2);
        (Self(Nibbles::unpack(&buf[..packed_len]).slice(..nibble_count)), &buf[33..])
    }
}

#[cfg(any(test, feature = "smartnet-codec"))]
smartnet_codecs::impl_compression_for_compact!(PackedStoredNibblesSubKey);

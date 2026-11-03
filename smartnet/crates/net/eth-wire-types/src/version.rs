//! Support for representing the version of the `eth`

use crate::alloc::string::ToString;
use alloc::string::String;
use alloy_rlp::{Decodable, Encodable, Error as RlpError};
use bytes::BufMut;
use core::str::FromStr;
use derive_more::Display;

/// Error thrown when failed to parse a valid [`EthVersion`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Unknown eth protocol version: {0}")]
pub struct ParseVersionError(String);

/// The `eth` protocol version.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Display)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
pub enum EthVersion {
    /// The `eth` protocol version 66.
    Eth66 = 66,
    /// The `eth` protocol version 67.
    Eth67 = 67,
    /// The `eth` protocol version 68.
    Eth68 = 68,
    /// The `eth` protocol version 69.
    Eth69 = 69,
    /// The `eth` protocol version 70.
    Eth70 = 70,
    /// The `eth` protocol version 71.
    Eth71 = 71,
}

impl EthVersion {
    /// The latest known eth version
    pub const LATEST: Self = Self::Eth69;

    /// All known eth versions
    pub const ALL_VERSIONS: &'static [Self] = &[Self::Eth69, Self::Eth68, Self::Eth67, Self::Eth66];

    /// Returns true if the version is eth/66
    pub const fn is_eth66(&self) -> bool {
        matches!(self, Self::Eth66)
    }

    /// Returns true if the version is eth/67
    pub const fn is_eth67(&self) -> bool {
        matches!(self, Self::Eth67)
    }

    /// Returns true if the version is eth/68
    pub const fn is_eth68(&self) -> bool {
        matches!(self, Self::Eth68)
    }

    /// Returns true if the version is eth/69
    pub const fn is_eth69(&self) -> bool {
        matches!(self, Self::Eth69)
    }

    /// Returns true if the version is eth/70
    pub const fn is_eth70(&self) -> bool {
        matches!(self, Self::Eth70)
    }

    /// Returns true if the version is eth/71
    pub const fn is_eth71(&self) -> bool {
        matches!(self, Self::Eth71)
    }

    /// Returns true if the version is eth/69 or newer.
    pub const fn is_eth69_or_newer(&self) -> bool {
        matches!(self, Self::Eth69 | Self::Eth70 | Self::Eth71)
    }
}

/// RLP encodes `EthVersion` as a single byte (66-71).
impl Encodable for EthVersion {
    fn encode(&self, out: &mut dyn BufMut) {
        (*self as u8).encode(out)
    }

    fn length(&self) -> usize {
        (*self as u8).length()
    }
}

/// RLP decodes a single byte into `EthVersion`.
/// Returns error if byte is not a valid version (66-71).
impl Decodable for EthVersion {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let version = u8::decode(buf)?;
        Self::try_from(version).map_err(|_| RlpError::Custom("invalid eth version"))
    }
}

/// Allow for converting from a `&str` to an `EthVersion`.
///
/// # Example
/// ```
/// use smartnet_eth_wire_types::EthVersion;
///
/// let version = EthVersion::try_from("67").unwrap();
/// assert_eq!(version, EthVersion::Eth67);
/// ```
impl TryFrom<&str> for EthVersion {
    type Error = ParseVersionError;

    #[inline]
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "66" => Ok(Self::Eth66),
            "67" => Ok(Self::Eth67),
            "68" => Ok(Self::Eth68),
            "69" => Ok(Self::Eth69),
            "70" => Ok(Self::Eth70),
            "71" => Ok(Self::Eth71),
            _ => Err(ParseVersionError(s.to_string())),
        }
    }
}

/// Allow for converting from a u8 to an `EthVersion`.
///
/// # Example
/// ```
/// use smartnet_eth_wire_types::EthVersion;
///
/// let version = EthVersion::try_from(67).unwrap();
/// assert_eq!(version, EthVersion::Eth67);
/// ```
impl TryFrom<u8> for EthVersion {
    type Error = ParseVersionError;

    #[inline]
    fn try_from(u: u8) -> Result<Self, Self::Error> {
        match u {
            66 => Ok(Self::Eth66),
            67 => Ok(Self::Eth67),
            68 => Ok(Self::Eth68),
            69 => Ok(Self::Eth69),
            70 => Ok(Self::Eth70),
            71 => Ok(Self::Eth71),
            _ => Err(ParseVersionError(u.to_string())),
        }
    }
}

impl FromStr for EthVersion {
    type Err = ParseVersionError;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s)
    }
}

impl From<EthVersion> for u8 {
    #[inline]
    fn from(v: EthVersion) -> Self {
        v as Self
    }
}

impl From<EthVersion> for &'static str {
    #[inline]
    fn from(v: EthVersion) -> &'static str {
        match v {
            EthVersion::Eth66 => "66",
            EthVersion::Eth67 => "67",
            EthVersion::Eth68 => "68",
            EthVersion::Eth69 => "69",
            EthVersion::Eth70 => "70",
            EthVersion::Eth71 => "71",
        }
    }
}

//! Implements data structures specific to the database

use crate::{
    table::{Decode, Encode},
    DatabaseError,
};
use alloy_primitives::{Address, B256, U256};
use smartnet_codecs::{add_arbitrary_tests, impl_compression_for_compact, Compact};
use smartnet_prune_types::PruneSegment;
use smartnet_trie_common::{StoredNibbles, StoredNibblesSubKey, *};
use serde::{Deserialize, Serialize};

pub mod accounts;
pub mod blocks;
pub mod integer_list;
pub mod metadata;
pub mod receipts;
pub mod sharded_key;
pub mod storage_sharded_key;
pub mod transactions;

pub use accounts::*;
pub use blocks::*;
pub use integer_list::IntegerList;
pub use metadata::*;
pub use receipts::*;
pub use sharded_key::ShardedKey;
pub use smartnet_db_models::{
    AccountBeforeTx, ClientVersion, PersistenceWatermark, StorageBeforeTx, StoredBlockBodyIndices,
    StoredBlockWithdrawals, StoredMigration, StoredSchemaInfo, StoredTransactionLocation,
};
pub use transactions::*;

/// Macro that implements [`Encode`] and [`Decode`] for uint types.
macro_rules! impl_uints {
    ($($name:tt),+) => {
        $(
            impl Encode for $name {
                type Encoded = [u8; std::mem::size_of::<$name>()];

                fn encode(self) -> Self::Encoded {
                    self.to_be_bytes()
                }
            }

            impl Decode for $name {
                fn decode(value: &[u8]) -> Result<Self, $crate::DatabaseError> {
                    Ok(
                        $name::from_be_bytes(
                            value.try_into().map_err(|_| $crate::DatabaseError::Decode)?
                        )
                    )
                }
            }
        )+
    };
}

impl_uints!(u64, u32, u16, u8);

impl Encode for Vec<u8> {
    type Encoded = Self;

    fn encode(self) -> Self::Encoded {
        self
    }
}

impl Decode for Vec<u8> {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        Ok(value.to_vec())
    }

    fn decode_owned(value: Vec<u8>) -> Result<Self, DatabaseError> {
        Ok(value)
    }
}

impl Encode for Address {
    type Encoded = [u8; 20];

    fn encode(self) -> Self::Encoded {
        self.0 .0
    }
}

impl Decode for Address {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        Ok(Self::from_slice(value))
    }
}

impl Encode for B256 {
    type Encoded = [u8; 32];

    fn encode(self) -> Self::Encoded {
        self.0
    }
}

impl Decode for B256 {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        Ok(Self::new(value.try_into().map_err(|_| DatabaseError::Decode)?))
    }
}

impl Encode for String {
    type Encoded = Vec<u8>;

    fn encode(self) -> Self::Encoded {
        self.into_bytes()
    }
}

impl Decode for String {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        Self::decode_owned(value.to_vec())
    }

    fn decode_owned(value: Vec<u8>) -> Result<Self, DatabaseError> {
        Self::from_utf8(value).map_err(|_| DatabaseError::Decode)
    }
}

impl Encode for StoredNibbles {
    type Encoded = arrayvec::ArrayVec<u8, 64>;

    fn encode(self) -> Self::Encoded {
        self.0.iter().collect()
    }
}

impl Decode for StoredNibbles {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        Ok(Self::from_compact(value, value.len()).0)
    }
}

impl Encode for StoredNibblesSubKey {
    type Encoded = [u8; 65];

    fn encode(self) -> Self::Encoded {
        self.to_compact_array()
    }
}

impl Decode for StoredNibblesSubKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        Ok(Self::from_compact(value, value.len()).0)
    }
}

impl Encode for PackedStoredNibbles {
    type Encoded = [u8; 33];

    fn encode(self) -> Self::Encoded {
        self.to_compact_array()
    }
}

impl Decode for PackedStoredNibbles {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        Ok(Self::from_compact(value, value.len()).0)
    }
}

impl Encode for PackedStoredNibblesSubKey {
    type Encoded = [u8; 33];

    fn encode(self) -> Self::Encoded {
        self.to_compact_array()
    }
}

impl Decode for PackedStoredNibblesSubKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        Ok(Self::from_compact(value, value.len()).0)
    }
}

impl Encode for PruneSegment {
    type Encoded = [u8; 1];

    fn encode(self) -> Self::Encoded {
        let mut buf = [0u8];
        self.to_compact(&mut buf.as_mut());
        buf
    }
}

impl Decode for PruneSegment {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        Ok(Self::from_compact(value, value.len()).0)
    }
}

impl Encode for ClientVersion {
    type Encoded = Vec<u8>;

    // Delegate to the Compact implementation
    fn encode(self) -> Self::Encoded {
        let mut buf = vec![];
        self.to_compact(&mut buf);
        buf
    }
}

impl Decode for ClientVersion {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        Ok(Self::from_compact(value, value.len()).0)
    }
}

impl_compression_for_compact!(StoredBlockOmmers<H>, CompactU256);
impl_compression_for_compact!(StoredBlock<H>);
impl_compression_for_compact!(StoredTransaction<T>);
impl_compression_for_compact!(StoredReceipt);
impl_compression_for_compact!(StoredLog);

/// Adds wrapper structs for some primitive types so they can use `StructFlags` from Compact, when
/// used as pure table values.
macro_rules! add_wrapper_struct {
    ($(($name:tt, $wrapper:tt)),+) => {
        $(
            /// Wrapper struct so it can use `StructFlags` from Compact, when used as pure table values.
            #[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, Compact)]
            #[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
            #[add_arbitrary_tests(compact)]
            pub struct $wrapper(pub $name);

            impl From<$name> for $wrapper {
                fn from(value: $name) -> Self {
                    $wrapper(value)
                }
            }

            impl From<$wrapper> for $name {
                fn from(value: $wrapper) -> Self {
                    value.0
                }
            }

            impl std::ops::Deref for $wrapper {
                type Target = $name;

                fn deref(&self) -> &Self::Target {
                    &self.0
                }
            }

        )+
    };
}

add_wrapper_struct!((U256, CompactU256));
add_wrapper_struct!((u64, CompactU64));
add_wrapper_struct!((ClientVersion, CompactClientVersion));

//! Tables and data models.
//!
//! # Overview
//!
//! This module defines the tables, as well as some table-related abstractions:
//!
//! - [`codecs`] integrates different codecs into [`Encode`] and [`Decode`]
//! - [`models`](crate::models) defines the values written to tables
//!
//! # Database Tour
//!
//! TODO(onbjerg): Find appropriate format for this...

pub mod codecs;

mod raw;
pub use raw::{RawDupSort, RawKey, RawTable, RawValue, TableRawRow};

use crate::{
    models::{
        accounts::BlockNumberAddress, storage_sharded_key::StorageShardedKey, AccountBeforeTx,
        ClientVersion, IntegerList, PersistenceWatermark, ShardedKey, StoredMigration,
        StoredSchemaInfo, StoredTransactionLocation,
    },
    table::{Decode, DupSort, Encode, Table, TableInfo},
};
use alloy_primitives::{Address, BlockHash, BlockNumber, TxHash, TxNumber, B256};
use smartnet_primitives_traits::{Account, Bytecode, StorageEntry};
use smartnet_prune_types::{PruneCheckpoint, PruneSegment};
use smartnet_stages_types::StageCheckpoint;
use smartnet_trie_common::{
    BranchNodeCompact, PackedStorageTrieEntry, PackedStoredNibbles, PackedStoredNibblesSubKey,
    StorageTrieEntry, StoredNibbles, StoredNibblesSubKey,
};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Enum for the types of tables present in the primary database.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum TableType {
    /// key value table
    Table,
    /// Duplicate key value table
    DupSort,
}

/// Keyspace group of a table — the PROP5 ownership/lifecycle classification, fused
/// with the SQLITE.md §11 target schema.
///
/// One database, two groups (F-series user decision): every table belongs to exactly
/// one group; the storage adapter derives per-group keyspace options from it, and the
/// F3 commit/recovery watermark spans both groups inside one atomic transaction.
///
/// * [`State`](Self::State) — active execution state, unwind data, and control/ recovery (PROP5 §A
///   current state, §C changesets "keep in state engine", §D checkpoints/metadata/watermark "keep
///   in state engine").
/// * [`History`](Self::History) — historical chain data: block payloads, transactions, receipts,
///   and the historical state indexes (PROP5 §B payloads "historical store", §H history indexes).
#[derive(Debug, PartialEq, Eq, Copy, Clone, Hash)]
pub enum TableGroup {
    /// Active execution state, unwind data, control and recovery (PROP5 A/C/D).
    State,
    /// Historical chain data: payloads, transactions, receipts, history indexes
    /// (PROP5 B/H).
    History,
}

/// The general purpose of this is to use with a combination of Tables enum,
/// by implementing a `TableViewer` trait you can operate on db tables in an abstract way.
///
/// # Example
///
/// ```
/// use smartnet_db_api::{
///     table::{DupSort, Table},
///     TableViewer, Tables,
/// };
///
/// struct MyTableViewer;
///
/// impl TableViewer<()> for MyTableViewer {
///     type Error = &'static str;
///
///     fn view<T: Table>(&self) -> Result<(), Self::Error> {
///         // operate on table in a generic way
///         Ok(())
///     }
///
///     fn view_dupsort<T: DupSort>(&self) -> Result<(), Self::Error> {
///         // operate on a dupsort table in a generic way
///         Ok(())
///     }
/// }
///
/// let viewer = MyTableViewer {};
///
/// let _ = Tables::CanonicalChain.view(&viewer);
/// let _ = Tables::TxLocations.view(&viewer);
/// ```
pub trait TableViewer<R> {
    /// The error type returned by the viewer.
    type Error;

    /// Calls `view` with the correct table type.
    fn view_rt(&self, table: Tables) -> Result<R, Self::Error> {
        table.view(self)
    }

    /// Operate on the table in a generic way.
    fn view<T: Table>(&self) -> Result<R, Self::Error>;

    /// Operate on the dupsort table in a generic way.
    ///
    /// By default, the `view` function is invoked unless overridden.
    fn view_dupsort<T: DupSort>(&self) -> Result<R, Self::Error>
    where
        T::Value: smartnet_primitives_traits::ValueWithSubKey<SubKey = T::SubKey>,
    {
        self.view::<T>()
    }
}

/// General trait for defining the set of tables
/// Used to initialize database
pub trait TableSet {
    /// Returns an iterator over the tables
    fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>>;
}

/// Defines all the tables in the database.
#[macro_export]
macro_rules! tables {
    (@bool) => { false };
    (@bool $($t:tt)+) => { true };

    (@view $name:ident $v:ident) => { $v.view::<$name>() };
    (@view $name:ident $v:ident $_subkey:ty) => { $v.view_dupsort::<$name>() };

    (@value_doc $key:ty, $value:ty) => {
        concat!("[`", stringify!($value), "`]")
    };
    // Don't generate links if we have generics
    (@value_doc $key:ty, $value:ty, $($generic:ident),*) => {
        concat!("`", stringify!($value), "`")
    };

    ($($(#[$attr:meta])* table $name:ident$(<$($generic:ident $(= $default:ty)?),*>)? { group = $group:ident; type Key = $key:ty; type Value = $value:ty; $(type SubKey = $subkey:ty;)? } )*) => {
        // Table marker types.
        $(
            $(#[$attr])*
            ///
            #[doc = concat!("Marker type representing a database table mapping [`", stringify!($key), "`] to ", tables!(@value_doc $key, $value, $($($generic),*)?), ".")]
            $(
                #[doc = concat!("\n\nThis table's `DUPSORT` subkey is [`", stringify!($subkey), "`].")]
            )?
            pub struct $name$(<$($generic $( = $default)?),*>)? {
                _private: std::marker::PhantomData<($($($generic,)*)?)>,
            }

            // Ideally this implementation wouldn't exist, but it is necessary to derive `Debug`
            // when a type is generic over `T: Table`. See: https://github.com/rust-lang/rust/issues/26925
            impl$(<$($generic),*>)? fmt::Debug for $name$(<$($generic),*>)? {
                fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
                    unreachable!("this type cannot be instantiated")
                }
            }

            impl$(<$($generic),*>)? $crate::table::Table for $name$(<$($generic),*>)?
            where
                $value: $crate::table::Value + 'static
                $($(,$generic: Send + Sync)*)?
            {
                const NAME: &'static str = table_names::$name;
                const DUPSORT: bool = tables!(@bool $($subkey)?);
                const GROUP: $crate::tables::TableGroup =
                    $crate::tables::TableGroup::$group;

                type Key = $key;
                type Value = $value;
            }

            $(
                impl DupSort for $name {
                    type SubKey = $subkey;
                }
            )?
        )*

        // Tables enum.

        /// A table in the database.
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Tables {
            $(
                #[doc = concat!("The [`", stringify!($name), "`] database table.")]
                $name,
            )*
        }

        impl Tables {
            /// All the tables in the database.
            pub const ALL: &'static [Self] = &[$(Self::$name,)*];

            /// The number of tables in the database.
            pub const COUNT: usize = Self::ALL.len();

            /// Returns the name of the table as a string.
            pub const fn name(&self) -> &'static str {
                match self {
                    $(
                        Self::$name => table_names::$name,
                    )*
                }
            }

            /// Returns `true` if the table is a `DUPSORT` table.
            pub const fn is_dupsort(&self) -> bool {
                match self {
                    $(
                        Self::$name => tables!(@bool $($subkey)?),
                    )*
                }
            }

            /// Returns the keyspace group ([`TableGroup`]) of the table.
            pub const fn group(&self) -> TableGroup {
                match self {
                    $(
                        Self::$name => TableGroup::$group,
                    )*
                }
            }

            /// The type of the given table in database.
            pub const fn table_type(&self) -> TableType {
                if self.is_dupsort() {
                    TableType::DupSort
                } else {
                    TableType::Table
                }
            }

            /// Allows to operate on specific table type
            pub fn view<T, R>(&self, visitor: &T) -> Result<R, T::Error>
            where
                T: ?Sized + TableViewer<R>,
            {
                match self {
                    $(
                        Self::$name => tables!(@view $name visitor $($subkey)?),
                    )*
                }
            }
        }

        impl fmt::Debug for Tables {
            #[inline]
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.name())
            }
        }

        impl fmt::Display for Tables {
            #[inline]
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.name().fmt(f)
            }
        }

        impl std::str::FromStr for Tables {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $(
                        table_names::$name => Ok(Self::$name),
                    )*
                    s => Err(format!("unknown table: {s:?}")),
                }
            }
        }

        impl TableInfo for Tables {
            fn name(&self) -> &'static str {
                self.name()
            }

            fn is_dupsort(&self) -> bool {
                self.is_dupsort()
            }

            fn group(&self) -> TableGroup {
                self.group()
            }
        }

        impl TableSet for Tables {
            fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>> {
                Box::new(Self::ALL.iter().map(|table| Box::new(*table) as Box<dyn TableInfo>))
            }
        }

        // Need constants to match on in the `FromStr` implementation.
        #[expect(non_upper_case_globals)]
        mod table_names {
            $(
                pub(super) const $name: &'static str = stringify!($name);
            )*
        }

        /// Maps a run-time [`Tables`] enum value to its corresponding compile-time [`Table`] type.
        ///
        /// This is a simpler alternative to [`TableViewer`].
        ///
        /// # Examples
        ///
        /// ```
        /// use smartnet_db_api::{table::Table, Tables, tables_to_generic};
        ///
        /// let table = Tables::CanonicalChain;
        /// let result = tables_to_generic!(table, |GenericTable| <GenericTable as Table>::NAME);
        /// assert_eq!(result, table.name());
        /// ```
        #[macro_export]
        macro_rules! tables_to_generic {
            ($table:expr, |$generic_name:ident| $e:expr) => {
                match $table {
                    $(
                        Tables::$name => {
                            use $crate::tables::$name as $generic_name;
                            $e
                        },
                    )*
                }
            };
        }
    };
}

tables! {
    /// Stores the canonical chain index: block number → block hash
    /// (SQLITE.md §11 `canonical_chain`, §13.5 3B.1). Exactly one canonical block per
    /// height; absorbs the former `CanonicalHeaders` table.
    table CanonicalChain {
        group = History;
        type Key = BlockNumber;
        type Value = BlockHash;
    }

    /// Stores the mapping of the transaction hash to the transaction number.
    table TransactionHashNumbers {
        group = History;
        type Key = TxHash;
        type Value = TxNumber;
    }

    /// Stores the block hash → block number index (`fjall4a.md` §7 `header_numbers`).
    ///
    /// Part of the index `save_block_state_and_index` writes: `header(hash)` and
    /// `block_number(hash)` resolve through it before the block payload is read from the
    /// file tier. The payload itself never lives in a [`Blocks`] row.
    table HeaderNumbers {
        group = History;
        type Key = BlockHash;
        type Value = BlockNumber;
    }

    /// Stores the transaction number → (block number, index) location
    /// (`fjall4a.md` §7 `canonical_tx_lookup`, the `tx_num`-keyed half).
    ///
    /// Part of the index `save_block_state_and_index` writes: together with
    /// [`TransactionHashNumbers`] (`hash → tx_num`) it makes every `tx_num`- and
    /// hash-keyed lookup resolvable, with the transaction payload itself read from the
    /// file tier rather than a [`Transactions`] row.
    table TxLocations {
        group = History;
        type Key = TxNumber;
        type Value = StoredTransactionLocation;
    }

    /// Stores all smart contract bytecodes.
    /// There will be multiple accounts that have same bytecode
    /// So we would need to introduce reference counter.
    /// This will be small optimization on state.
    table Bytecodes {
        group = State;
        type Key = B256;
        type Value = Bytecode;
    }

    /// Stores the current state of an [`Account`].
    table PlainAccountState {
        group = State;
        type Key = Address;
        type Value = Account;
    }

    /// Stores the current value of a storage key.
    table PlainStorageState {
        group = State;
        type Key = Address;
        type Value = StorageEntry;
        type SubKey = B256;
    }

    /// Stores pointers to block changeset with changes for each account key.
    ///
    /// Last shard key of the storage will contain `u64::MAX` `BlockNumber`,
    /// this would allows us small optimization on db access when change is in plain state.
    ///
    /// Imagine having shards as:
    /// * `Address | 100`
    /// * `Address | u64::MAX`
    ///
    /// What we need to find is number that is one greater than N. Db `seek` function allows us to fetch
    /// the shard that equal or more than asked. For example:
    /// * For N=50 we would get first shard.
    /// * for N=150 we would get second shard.
    /// * If max block number is 200 and we ask for N=250 we would fetch last shard and know that needed entry is in `AccountPlainState`.
    /// * If there were no shard we would get `None` entry or entry of different storage key.
    ///
    /// Code example can be found in `smartnet_provider::HistoricalStateProviderRef`
    table AccountsHistory {
        group = History;
        type Key = ShardedKey<Address>;
        type Value = BlockNumberList;
    }

    /// Stores pointers to block number changeset with changes for each storage key.
    ///
    /// Last shard key of the storage will contain `u64::MAX` `BlockNumber`,
    /// this would allows us small optimization on db access when change is in plain state.
    ///
    /// Imagine having shards as:
    /// * `Address | StorageKey | 100`
    /// * `Address | StorageKey | u64::MAX`
    ///
    /// What we need to find is number that is one greater than N. Db `seek` function allows us to fetch
    /// the shard that equal or more than asked. For example:
    /// * For N=50 we would get first shard.
    /// * for N=150 we would get second shard.
    /// * If max block number is 200 and we ask for N=250 we would fetch last shard and know that needed entry is in `StoragePlainState`.
    /// * If there were no shard we would get `None` entry or entry of different storage key.
    ///
    /// Code example can be found in `smartnet_provider::HistoricalStateProviderRef`
    table StoragesHistory {
        group = History;
        type Key = StorageShardedKey;
        type Value = BlockNumberList;
    }

    /// Stores the state of an account before a certain transaction changed it.
    /// Change on state can be: account is created, selfdestructed, touched while empty
    /// or changed balance,nonce.
    table AccountChangeSets {
        group = State;
        type Key = BlockNumber;
        type Value = AccountBeforeTx;
        type SubKey = Address;
    }

    /// Stores the state of a storage key before a certain transaction changed it.
    /// If [`StorageEntry::value`] is zero, this means storage was not existing
    /// and needs to be removed.
    table StorageChangeSets {
        group = State;
        type Key = BlockNumberAddress;
        type Value = StorageEntry;
        type SubKey = B256;
    }

    /// Stores the current state of an [`Account`] indexed with `keccak256Address`
    /// This table is in preparation for merklization and calculation of state root.
    /// We are saving whole account data as it is needed for partial update when
    /// part of storage is changed. Benefit for merklization is that hashed addresses are sorted.
    table HashedAccounts {
        group = State;
        type Key = B256;
        type Value = Account;
    }

    /// Stores the current storage values indexed with `keccak256Address` and
    /// hash of storage key `keccak256key`.
    /// This table is in preparation for merklization and calculation of state root.
    /// Benefit for merklization is that hashed addresses/keys are sorted.
    table HashedStorages {
        group = State;
        type Key = B256;
        type Value = StorageEntry;
        type SubKey = B256;
    }

    /// Stores the current state's Merkle Patricia Tree.
    table AccountsTrie {
        group = State;
        type Key = StoredNibbles;
        type Value = BranchNodeCompact;
    }

    /// From `HashedAddress` => `NibblesSubKey` => Intermediate value
    table StoragesTrie {
        group = State;
        type Key = B256;
        type Value = StorageTrieEntry;
        type SubKey = StoredNibblesSubKey;
    }

    /// Stores the highest synced block number and stage-specific checkpoint of each stage.
    table StageCheckpoints {
        group = State;
        type Key = StageId;
        type Value = StageCheckpoint;
    }

    /// Stores arbitrary data to keep track of a stage first-sync progress.
    table StageCheckpointProgresses {
        group = State;
        type Key = StageId;
        type Value = Vec<u8>;
    }

    /// Stores the highest pruned block number and prune mode of each prune segment.
    table PruneCheckpoints {
        group = State;
        type Key = PruneSegment;
        type Value = PruneCheckpoint;
    }

    /// Stores the history of client versions that have accessed the database with write
    /// privileges by unix timestamp in seconds (SQLITE.md §11 `client_versions`, §13.5 3B.4).
    table ClientVersions {
        group = State;
        type Key = u64;
        type Value = ClientVersion;
    }

    /// Stores generic chain state info, like the last finalized block.
    table ChainState {
        group = State;
        type Key = ChainStateKey;
        type Value = BlockNumber;
    }

    /// Stores generic node metadata as key-value pairs.
    /// Can store feature flags, configuration markers, and other node-specific data.
    table Metadata {
        group = State;
        type Key = String;
        type Value = Vec<u8>;
    }

    /// Stores network settings derived from the genesis — the chain id and the genesis signer
    /// list — as string key -> bytes. Written once at genesis init and read once at bootstrap
    /// through the single accessors `get_chain_id()` / `get_validators()`. The database is
    /// the single source of truth for both — there is no operator-config fallback.
    table Settings {
        group = State;
        type Key = String;
        type Value = Vec<u8>;
    }

    /// Stores the persistence watermark of the last committed transaction — the single
    /// durable commit watermark (`commit_sequence`, `last_persisted_block`,
    /// `last_persisted_tx`), written inside the same transaction as the data it
    /// describes. One row only; see [`PersistenceProgressKey`].
    table PersistenceProgress {
        group = State;
        type Key = PersistenceProgressKey;
        type Value = PersistenceWatermark;
    }

    /// Stores the database schema version — the single row that replaces the former
    /// `database.version` sidecar file (SQLITE.md §11 A `schema_info`, §13.5 3B.4).
    /// Written once on fresh init; refused (never migrated) on mismatch. See
    /// [`SchemaInfoKey`].
    table SchemaInfo {
        group = State;
        type Key = SchemaInfoKey;
        type Value = StoredSchemaInfo;
    }

    /// Stores applied schema migrations (SQLITE.md §11 A `migrations`, §13.5 3B.4).
    /// Empty under the fresh-init-only policy — the log table exists for a future
    /// migration framework.
    table Migrations {
        group = State;
        type Key = u64;
        type Value = StoredMigration;
    }
}

/// Packed-encoding view of the [`AccountsTrie`] table.
///
/// Uses [`PackedStoredNibbles`] (33-byte) keys instead of [`StoredNibbles`] (65-byte).
/// Shares the same underlying table - this is a type-level view for storage v2.
#[derive(Debug)]
pub struct PackedAccountsTrie;

impl Table for PackedAccountsTrie {
    const NAME: &'static str = <AccountsTrie as Table>::NAME;
    const DUPSORT: bool = false;
    const GROUP: TableGroup = <AccountsTrie as Table>::GROUP;
    type Key = PackedStoredNibbles;
    type Value = BranchNodeCompact;
}

/// Packed-encoding view of the [`StoragesTrie`] table.
///
/// Uses [`PackedStoredNibblesSubKey`] (33-byte) subkeys instead of [`StoredNibblesSubKey`]
/// (65-byte). Shares the same underlying table - this is a type-level view for storage v2.
#[derive(Debug)]
pub struct PackedStoragesTrie;

impl Table for PackedStoragesTrie {
    const NAME: &'static str = <StoragesTrie as Table>::NAME;
    const DUPSORT: bool = true;
    const GROUP: TableGroup = <StoragesTrie as Table>::GROUP;
    type Key = B256;
    type Value = PackedStorageTrieEntry;
}

impl DupSort for PackedStoragesTrie {
    type SubKey = PackedStoredNibblesSubKey;
}

/// Keys for the `ChainState` table.
#[derive(Ord, Clone, Eq, PartialOrd, PartialEq, Debug, Deserialize, Serialize, Hash)]
pub enum ChainStateKey {
    /// Last finalized block key
    LastFinalizedBlock,
    /// Last safe block key
    LastSafeBlock,
}

impl Encode for ChainStateKey {
    type Encoded = [u8; 1];

    fn encode(self) -> Self::Encoded {
        match self {
            Self::LastFinalizedBlock => [0],
            Self::LastSafeBlock => [1],
        }
    }
}

impl Decode for ChainStateKey {
    fn decode(value: &[u8]) -> Result<Self, crate::DatabaseError> {
        match value {
            [0] => Ok(Self::LastFinalizedBlock),
            [1] => Ok(Self::LastSafeBlock),
            _ => Err(crate::DatabaseError::Decode),
        }
    }
}

/// Keys for the `PersistenceProgress` table: a singleton — exactly one row exists.
#[derive(Ord, Clone, Eq, PartialOrd, PartialEq, Debug, Deserialize, Serialize, Hash)]
pub enum PersistenceProgressKey {
    /// The single persistence watermark row.
    Singleton,
}

impl Encode for PersistenceProgressKey {
    type Encoded = [u8; 1];

    fn encode(self) -> Self::Encoded {
        match self {
            Self::Singleton => [0],
        }
    }
}

impl Decode for PersistenceProgressKey {
    fn decode(value: &[u8]) -> Result<Self, crate::DatabaseError> {
        match value {
            [0] => Ok(Self::Singleton),
            _ => Err(crate::DatabaseError::Decode),
        }
    }
}

/// Keys for the `SchemaInfo` table: a singleton — exactly one row exists.
#[derive(Ord, Clone, Eq, PartialOrd, PartialEq, Debug, Deserialize, Serialize, Hash)]
pub enum SchemaInfoKey {
    /// The single schema-version row.
    Singleton,
}

impl Encode for SchemaInfoKey {
    type Encoded = [u8; 1];

    fn encode(self) -> Self::Encoded {
        match self {
            Self::Singleton => [0],
        }
    }
}

impl Decode for SchemaInfoKey {
    fn decode(value: &[u8]) -> Result<Self, crate::DatabaseError> {
        match value {
            [0] => Ok(Self::Singleton),
            _ => Err(crate::DatabaseError::Decode),
        }
    }
}

// Alias types.

/// List with transaction numbers.
pub type BlockNumberList = IntegerList;

/// Encoded stage id.
pub type StageId = String;

/// The [`StageCheckpoints`] key of the Execution stage's row.
///
/// This is the checkpoint the persistence watermark's block number is derived from at
/// commit time; it is exactly the string the pipeline writes for
/// `smartnet_stages_types::StageId::Execution`.
pub const EXECUTION_STAGE_KEY: &str = smartnet_stages_types::StageId::Execution.as_str();

//! An integration of `smartnet-trie` with `smartnet-db`.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod changesets;
pub use changesets::*;
mod hashed_cursor;
mod prefix_set;
mod proof;
mod state;
mod storage;
mod trie_cursor;

pub use hashed_cursor::{
    DatabaseHashedAccountCursor, DatabaseHashedCursorFactory, DatabaseHashedStorageCursor,
};
pub use prefix_set::load_prefix_sets_with_provider;
pub use proof::{DatabaseProof, DatabaseStorageProof};
pub use smartnet_db_api::tables::{PackedAccountsTrie, PackedStoragesTrie};
pub use state::{from_reverts_auto, DatabaseHashedPostState, DatabaseStateRoot};
pub use storage::{hashed_storage_from_reverts_with_provider, DatabaseStorageRoot};
pub use trie_cursor::{
    DatabaseAccountTrieCursor, DatabaseStorageTrieCursor, DatabaseTrieCursorFactory,
    LegacyKeyAdapter, PackedKeyAdapter, StorageTrieEntryLike, TrieKeyAdapter, TrieTableAdapter,
};

/// Dispatches a trie operation using the [`LegacyKeyAdapter`].
///
/// The first argument must implement
/// [`StorageSettingsCache`](smartnet_storage_api::StorageSettingsCache). Inside the closure body,
/// `$A` is a type alias for [`LegacyKeyAdapter`].
///
/// # Example
///
/// ```ignore
/// smartnet_trie_db::with_adapter!(provider, |A| {
///     let factory = DatabaseTrieCursorFactory::<_, A>::new(tx);
///     // ...
/// })
/// ```
#[macro_export]
macro_rules! with_adapter {
    ($settings_provider:expr, |$A:ident| $body:expr) => {{
        type $A = $crate::LegacyKeyAdapter;
        $body
    }};
}

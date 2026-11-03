//! Chain state related types and functionality.
//!
//! ExEx notification types were merged in from the former `smartnet-exex-types` crate.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod exex;
pub use exex::{
    ExExHead, ExExManagerHandle, ExExNotification, ExExNotificationSource, FinishedExExHeight,
};

#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub use exex::serde_bincode_compat;

mod execution_stats;
pub use execution_stats::ExecutionTimingStats;

mod in_memory;
pub use in_memory::*;

mod deferred_trie;
pub use deferred_trie::*;

mod lazy_overlay;
pub use lazy_overlay::*;

mod noop;

mod chain_info;
pub use chain_info::ChainInfoTracker;

mod notifications;
pub use notifications::{
    CanonStateNotification, CanonStateNotificationSender, CanonStateNotificationStream,
    CanonStateNotifications, CanonStateSubscriptions, ForkChoiceNotifications, ForkChoiceStream,
    ForkChoiceSubscriptions, PersistedBlockNotifications, PersistedBlockSubscriptions,
    WatchValueStream,
};

mod memory_overlay;
pub use memory_overlay::{MemoryOverlayStateProvider, MemoryOverlayStateProviderRef};

#[cfg(any(test, feature = "test-utils"))]
/// Common test helpers
pub mod test_utils;

// todo: remove when generic data prim integration complete
pub use smartnet_ethereum_primitives::EthPrimitives;

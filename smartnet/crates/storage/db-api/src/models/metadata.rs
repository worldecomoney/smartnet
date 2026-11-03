//! Storage metadata models.

use smartnet_codecs::{add_arbitrary_tests, Compact};
use serde::{Deserialize, Serialize};

/// Calibri Lite **node/runtime** persistence profile (`smartnet` owns the primary database; validators
/// do not).
///
/// Separates **canonical runtime** persistence (state, trie, changesets, receipts) from
/// **Ethereum archival** assumptions (account/storage history indices, explorer replay).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Compact, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[add_arbitrary_tests(compact)]
pub enum PersistenceProfile {
    /// Calibri Lite runtime: minimal canonical persistence; **no** history index tables.
    #[default]
    #[serde(alias = "SmartNet")]
    CalibriLite,
    /// Full archive: history indices at genesis and on each state change (upstream Ethereum).
    Archive,
}

/// Storage configuration settings for this node.
///
/// Uses v1/legacy layout (everything in the primary database), and controls which
/// persistence features are active.
///
/// These should be set during `init_genesis` or `init_db` depending on whether we want dictate
/// behaviour of new or old nodes respectively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Compact, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[add_arbitrary_tests(compact)]
pub struct StorageSettings {
    /// Calibri Lite runtime vs archive persistence (history indexing, genesis history).
    #[serde(default)]
    pub persistence_profile: PersistenceProfile,
}

impl StorageSettings {
    /// Default for new Calibri Lite nodes: v1 layout + runtime profile (no archival history).
    pub const fn base() -> Self {
        Self::calibri_lite_runtime()
    }

    /// Calibri Lite node/runtime: canonical state, trie, changesets; history indices disabled.
    pub const fn calibri_lite_runtime() -> Self {
        Self { persistence_profile: PersistenceProfile::CalibriLite }
    }

    /// v1 layout with **archive** history indexing (upstream Ethereum defaults).
    pub const fn v1() -> Self {
        Self { persistence_profile: PersistenceProfile::Archive }
    }

    /// Archive / full-history profile explicitly.
    pub const fn archive() -> Self {
        Self::v1()
    }

    /// Whether account history indices (`AccountsHistory`) are persisted.
    pub const fn persist_account_history_index(&self) -> bool {
        matches!(self.persistence_profile, PersistenceProfile::Archive)
    }

    /// Whether storage history indices (`StoragesHistory`) are persisted.
    pub const fn persist_storage_history_index(&self) -> bool {
        matches!(self.persistence_profile, PersistenceProfile::Archive)
    }

    /// Startup log label for [`PersistenceProfile`] (Calibri Lite node/runtime vs archive).
    pub const fn profile_name(&self) -> &'static str {
        match self.persistence_profile {
            PersistenceProfile::CalibriLite => "calibri_lite_runtime",
            PersistenceProfile::Archive => "archive",
        }
    }

    /// Whether account/storage history indexing is active (both indices for archive profile).
    pub const fn history_indexing_enabled(&self) -> bool {
        self.persist_account_history_index() && self.persist_storage_history_index()
    }

    /// Whether this node uses the archive/full-history persistence profile.
    pub const fn archive_mode(&self) -> bool {
        matches!(self.persistence_profile, PersistenceProfile::Archive)
    }
}


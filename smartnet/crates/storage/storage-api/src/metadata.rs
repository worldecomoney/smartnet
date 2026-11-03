//! Metadata provider trait for reading and writing node metadata.

use alloc::vec::Vec;
use smartnet_db_api::models::StorageSettings;
use smartnet_storage_errors::provider::{ProviderError, ProviderResult};

/// Metadata keys.
pub mod keys {
    /// Storage configuration settings for this node.
    pub const STORAGE_SETTINGS: &str = "storage_settings";

    /// Changeset-window floor: the lowest block whose `AccountChangeSets`/`StorageChangeSets`
    /// rows are retained, written by the Stage 5 limiter in the same transaction that trims them.
    ///
    /// The read path (`try_into_history_at_block`) reads this to refuse historical queries below
    /// the floor with an explicit error instead of silently falling through to plain state.
    pub const CHANGESET_WINDOW_FLOOR: &str = "changeset_window_floor";

    /// Network chain id, written from the genesis at init. Stored in the `Settings` table and
    /// read once at bootstrap through the single accessor `get_chain_id()`; the database is the
    /// single source of truth — there is no operator-config fallback.
    pub const CHAIN_ID: &str = "chain_id";

    /// Validators — the authorized sealing set, written at init from the genesis `extraData`
    /// signer section (today exactly the genesis signers; the name is deliberately genesis-free:
    /// validators are validators, however many there are and wherever they come from). Lives
    /// **only here in the database** — never a file, never a config key — added by node init and
    /// enhanced/extended by further protocol as the validator set grows. Stored in the `Settings`
    /// table as an array of 0x addresses (index 0 the default validator) and read once at
    /// bootstrap through the single accessor `get_validators()`; the database is the single
    /// source of truth — there is no operator-config fallback.
    pub const VALIDATORS: &str = "validators";

    /// Full genesis spec, serde-JSON-encoded `alloy_genesis::Genesis` — the exact genesis the
    /// genesis file declared at init (config incl. clique period/epoch + fork blocks, header
    /// fields, the original alloc). Written once at init (same transaction as the genesis) and
    /// rebuilt into the runtime chainspec at every boot from the same struct — the database is
    /// the single source of truth for the entire chain rulebook, so any well-formed genesis
    /// works and the boot spec always matches what was initialised. A database without this row
    /// (old identity format) is a fail-closed boot error (`smartnet init` run required, after
    /// wiping the datadir).
    pub const GENESIS_SPEC: &str = "genesis_spec";

    /// Genesis block hash, written once at init alongside the other genesis KV rows. The
    /// database is the single source of truth for the genesis identity; the block-0 payload
    /// itself lives only in the file tier (same shape as every later block).
    pub const GENESIS_HASH: &str = "genesis_hash";

    /// Graceful-shutdown marker: present iff the node's last exit was a clean shutdown
    /// (ctrl+c, SIGTERM, or a warm reboot) during which both stores were flushed. The value
    /// is the unix timestamp of that exit (for logging); only its presence matters.
    ///
    /// **This is the only recovery flag** (RECOVERY.md). Boot reads it, then clears it (so a
    /// crash from here on is caught next boot): present means "skip recovery", absent means
    /// "run recovery" — no server-boot-time or clock comparison. The clean-shutdown handler
    /// flushes the FS and DB, then writes this key last; a crash never reaches that path, so
    /// it stays absent and recovery runs.
    pub const SHUTDOWN_CLEAN: &str = "shutdown_clean";
}

/// Client trait for reading node metadata from the database.
#[auto_impl::auto_impl(&, Arc)]
pub trait MetadataProvider: Send {
    /// Get a metadata value by key
    fn get_metadata(&self, key: &str) -> ProviderResult<Option<Vec<u8>>>;

    /// Get storage settings for this node.
    ///
    /// If the stored metadata can't be deserialized (e.g. the format changed),
    /// this returns `None` instead of an error so commands like `db clear` can
    /// still operate without requiring a compatible metadata schema.
    fn storage_settings(&self) -> ProviderResult<Option<StorageSettings>> {
        Ok(self
            .get_metadata(keys::STORAGE_SETTINGS)?
            .and_then(|bytes| serde_json::from_slice(&bytes).ok()))
    }
}

/// Client trait for writing node metadata to the database.
pub trait MetadataWriter: Send {
    /// Write a metadata value
    fn write_metadata(&self, key: &str, value: Vec<u8>) -> ProviderResult<()>;

    /// Write storage settings for this node
    ///
    /// Be sure to update provider factory cache with
    /// [`StorageSettingsCache::set_storage_settings_cache`].
    fn write_storage_settings(&self, settings: StorageSettings) -> ProviderResult<()> {
        self.write_metadata(
            keys::STORAGE_SETTINGS,
            serde_json::to_vec(&settings).map_err(ProviderError::other)?,
        )
    }
}

/// Trait for caching storage settings on a provider factory.
pub trait StorageSettingsCache: Send {
    /// Gets the cached storage settings.
    fn cached_storage_settings(&self) -> StorageSettings;

    /// Sets the storage settings of this `ProviderFactory`.
    ///
    /// IMPORTANT: It does not save settings in storage, that should be done by
    /// [`MetadataWriter::write_storage_settings`]
    fn set_storage_settings_cache(&self, settings: StorageSettings);
}

/// Trait for accessing the database directory path.
#[cfg(feature = "std")]
pub trait StoragePath: Send {
    /// Returns the path to the database directory (e.g. `<datadir>/db`).
    fn storage_path(&self) -> std::path::PathBuf;
}

//! Calibri Lite node/runtime storage profile startup logging.

use smartnet_db_api::models::StorageSettings;
use tracing::info;

/// Emit a single INFO line identifying the active persistence profile.
///
/// Intended once per node startup (and after genesis init) so mixed fleets
/// (runtime, explorer, archive) are easy to distinguish in logs.
pub fn log_calibri_lite_storage_profile(settings: StorageSettings) {
    info!(
        target: "smartnet::storage",
        event = "CALIBRI_LITE_STORAGE_PROFILE_ACTIVE",
        profile = settings.profile_name(),
        history_indexing = settings.history_indexing_enabled(),
        archive_mode = settings.archive_mode(),
    );
}

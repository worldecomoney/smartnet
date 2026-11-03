//! Commonly used ExEx types (merged from the former `smartnet-exex-types` crate).


mod finished_height;
mod head;
mod manager_handle;
mod notification;
mod notification_source;

pub use finished_height::FinishedExExHeight;
pub use head::ExExHead;
pub use manager_handle::ExExManagerHandle;
pub use notification::ExExNotification;
pub use notification_source::ExExNotificationSource;

/// Bincode-compatible serde implementations for commonly used ExEx types.
///
/// `bincode` crate doesn't work with optionally serializable serde fields, but some of the
/// ExEx types require optional serialization for RPC compatibility. This module makes so that
/// all fields are serialized.
///
/// Read more: <https://github.com/bincode-org/bincode/issues/326>
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub mod serde_bincode_compat {
    pub use super::notification::serde_bincode_compat::*;
}

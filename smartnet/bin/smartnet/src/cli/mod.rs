//! CLI definition and entrypoint to executable

/// Re-export of the [`smartnet_node_core`] types specifically in the `cli` module.
///
/// This is re-exported because the types in `smartnet_node_core::cli` originally existed in
/// `smartnet::cli` but were moved to the [`smartnet_node_core`] crate. This re-export avoids a
/// breaking change.
pub use crate::core::cli::*;

/// Re-export of the [`smartnet_cli`] types specifically in the `interface` module.
///
/// This is re-exported because the types in [`smartnet_cli::interface`] originally
/// existed in `smartnet::cli` but were moved to the [`smartnet_cli`] crate. This re-export
/// avoids a breaking change.
pub use smartnet_cli::interface::*;

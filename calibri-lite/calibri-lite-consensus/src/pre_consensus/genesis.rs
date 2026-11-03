//! Genesis-only signer set parsing (the chain JSON seal key is opaque; semantics live here).

/// Parse authorized signers from genesis `extraData` (the genesis JSON seal key is opaque).
pub use smartnet_chainspec::parse_extra_data_validators;

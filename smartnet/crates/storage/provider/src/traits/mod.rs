//! Collection of common provider traits.

// Re-export all the traits
pub use smartnet_storage_api::*;

pub use smartnet_chainspec::ChainSpecProvider;

mod aux_db_provider;
pub use aux_db_provider::AuxDbProviderFactory;

mod full;
pub use full::FullProvider;

//! Genesis initialization: on-disk state bootstrap (`genesis`) and history-index writers
//! (`history`).

/// Genesis state bootstrap — network settings, validators, state-root import.
pub mod genesis;
/// History-index writers used while seeding genesis accounts and storage.
pub mod history;

pub use genesis::*;

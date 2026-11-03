//! Implementation of exotic state root computation approaches.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod storage_root_targets;
pub use storage_root_targets::StorageRootTargets;

/// Parallel trie calculation stats.
pub mod stats;

/// Implementation of parallel state root computation.
pub mod root;

/// Implementation of parallel proof computation.
pub mod proof_task;

/// State root task interface types shared between the engine tree and the payload builder.
pub mod state_root_task;

/// Async value encoder for V2 proofs.
pub(crate) mod value_encoder;

/// Parallel state root metrics.
#[cfg(feature = "metrics")]
pub mod metrics;

/// Proof task manager metrics.
#[cfg(feature = "metrics")]
pub mod proof_task_metrics;

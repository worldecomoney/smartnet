//! Staged syncing primitives for `SmartNet`.
//!
//! This crate contains the syncing primitives [`Pipeline`] and [`Stage`], as well as all stages
//! that the node uses to sync.
//!
//! A pipeline can be configured using [`Pipeline::builder()`].
//!
//! For ease of use, this crate also exposes a set of [`StageSet`]s, which are collections of stages
//! that perform specific functions during sync. Stage sets can be customized; it is possible to
//! add, disable and replace stages in the set.
//!
//! # Examples
//!
//! ```
//! # use std::sync::Arc;
//! # use smartnet_chainspec::MAINNET;
//! # use smartnet_prune_types::PruneModes;
//! # use smartnet_stages::Pipeline;
//! # use smartnet_stages::sets::DefaultStages;
//! # use smartnet_evm_ethereum::EthEvmConfig;
//! # use smartnet_provider::test_utils::{create_test_provider_factory, MockNodeTypesWithDB};
//! # use smartnet_config::config::StageConfig;
//! # use smartnet_consensus::test_utils::TestConsensus;
//! # use smartnet_consensus::FullConsensus;
//! #
//! # let consensus: Arc<dyn FullConsensus<smartnet_ethereum_primitives::EthPrimitives>> = Arc::new(TestConsensus::default());
//! # let provider_factory = create_test_provider_factory();
//! # let executor_provider = EthEvmConfig::mainnet();
//! // Create a pipeline with the default (offline) stages
//! # let pipeline =
//! Pipeline::<MockNodeTypesWithDB>::builder()
//!     .add_stages(DefaultStages::new(
//!         executor_provider,
//!         consensus,
//!         StageConfig::default(),
//!         PruneModes::default(),
//!     ))
//!     .build(provider_factory);
//! ```
//!
//! ## Feature Flags
//!
//! - `test-utils`: Export utilities for testing

#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
// Used only by the cfg(test / feature = "test-utils") helpers — marked as
// intentionally present so the default build does not flag the dependency.
use smartnet_tasks as _;

#[expect(missing_docs)]
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

/// A re-export of common structs and traits.
pub mod prelude;

/// Implementations of stages.
pub mod stages;

pub mod sets;

// re-export the stages API
pub use smartnet_stages_api::*;

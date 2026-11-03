//! Standalone crate for ethereum-specific `SmartNet` configuration and builder types.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

use smartnet_revm as _;
use revm as _;

pub use smartnet_engine_primitives::ethereum::EthEngineTypes;

pub mod evm;
pub use evm::EthEvmConfig;

#[allow(deprecated)]
pub use evm::EthExecutorProvider;

pub use smartnet_ethereum_consensus as consensus;
pub mod node;
pub use node::*;

pub mod payload;

pub mod engine;
pub use engine::EthereumEngineValidator;

/// Streaming file import: read the original chain RLP in place, submit every block
/// through the engine (`smartnet import <file>`).
pub mod import;

/// The merged validator's in-process node interface (`SEAL = true`).
pub mod validator;

/// Mesh service — closed-list all-to-all gossip over `POST /rumor` (replaces devp2p).
pub(crate) mod mesh;

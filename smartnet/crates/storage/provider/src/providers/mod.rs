//! Contains the main provider types and traits for interacting with the blockchain's storage.

use smartnet_chainspec::EthereumHardforks;
use smartnet_db_api::table::Value;
use smartnet_node_types::{NodePrimitives, NodeTypes, NodeTypesWithDB};

mod database;
pub use database::*;

mod state;
pub use state::{
    historical::{
        compute_history_rank, history_info, needs_prev_shard_check, HistoricalStateProvider,
        HistoricalStateProviderRef, HistoryInfo, LowestAvailableBlocks,
    },
    latest::{LatestStateProvider, LatestStateProviderRef},
    overlay::{OverlayStateProvider, OverlayStateProviderFactory},
};

mod consistent_view;
pub use consistent_view::{ConsistentDbView, ConsistentViewError};

mod blockchain_provider;
pub use blockchain_provider::BlockchainProvider;

mod consistent;
pub use consistent::ConsistentProvider;

pub(crate) mod aux_db;

pub use aux_db::{
    AuxDbBatch, AuxDbBuilder, AuxDbIter, AuxDbProvider, AuxDbReadSnapshot, PrunedIndices,
};

/// Helper trait to bound [`NodeTypes`] so that combined with database they satisfy
/// [`ProviderNodeTypes`].
///
/// The file-tier read path (6B, `fjall4a.md` §6) requires the primitives to be
/// rebuildable from records: an handed-off block's payload rows are gone, so headers,
/// transactions, receipts, and the block body itself must be constructible from the
/// `b`/`txs`/`receipts`/`logs` records read back from the sealed `.gz` files.
pub trait NodeTypesForProvider
where
    Self: NodeTypes<
        ChainSpec: EthereumHardforks,
        Storage: ChainStorage<Self::Primitives>,
        Primitives: NodePrimitives<
            SignedTx: Value + for<'a> From<&'a smartnet_file_tier::TxRecord>,
            Receipt: Value + From<smartnet_file_tier::ReceiptBuild>,
            BlockHeader: Value + for<'a> From<&'a smartnet_file_tier::BtRecord>,
            BlockBody: smartnet_file_tier::BuildBody,
        >,
    >,
{
}

impl<T> NodeTypesForProvider for T where
    T: NodeTypes<
        ChainSpec: EthereumHardforks,
        Storage: ChainStorage<T::Primitives>,
        Primitives: NodePrimitives<
            SignedTx: Value + for<'a> From<&'a smartnet_file_tier::TxRecord>,
            Receipt: Value + From<smartnet_file_tier::ReceiptBuild>,
            BlockHeader: Value + for<'a> From<&'a smartnet_file_tier::BtRecord>,
            BlockBody: smartnet_file_tier::BuildBody,
        >,
    >
{
}

/// Helper trait keeping common requirements of providers for [`NodeTypesWithDB`].
pub trait ProviderNodeTypes
where
    Self: NodeTypesForProvider + NodeTypesWithDB,
{
}
impl<T> ProviderNodeTypes for T where T: NodeTypesForProvider + NodeTypesWithDB {}

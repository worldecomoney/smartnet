//! Helper provider traits to encapsulate all provider traits for simplicity.

use crate::{
    AccountReader, AuxDbProviderFactory, BlockReader, BlockReaderIdExt, ChainSpecProvider,
    ChangeSetReader, DatabaseProviderFactory, HashedPostStateProvider, PruneCheckpointReader,
    StageCheckpointReader, StateProviderFactory, StateReader,
};
use smartnet_chain_state::{
    CanonStateSubscriptions, ForkChoiceSubscriptions, PersistedBlockSubscriptions,
};
use smartnet_node_types::{BlockTy, HeaderTy, NodeTypesWithDB, ReceiptTy, TxTy};
use smartnet_storage_api::{NodePrimitivesProvider, StorageChangeSetReader, StorageSettingsCache};
use std::fmt::Debug;

/// Helper trait to unify all provider traits for simplicity.
pub trait FullProvider<N: NodeTypesWithDB>:
    DatabaseProviderFactory<
        DB = N::DB,
        Provider: BlockReader
                      + StageCheckpointReader
                      + PruneCheckpointReader
                      + ChangeSetReader
                      + StorageChangeSetReader
                      + StorageSettingsCache,
    > + NodePrimitivesProvider<Primitives = N::Primitives>
    + AuxDbProviderFactory
    + BlockReaderIdExt<
        Transaction = TxTy<N>,
        Block = BlockTy<N>,
        Receipt = ReceiptTy<N>,
        Header = HeaderTy<N>,
    > + AccountReader
    + StateProviderFactory
    + StateReader
    + HashedPostStateProvider
    + ChainSpecProvider<ChainSpec = N::ChainSpec>
    + ChangeSetReader
    + StorageChangeSetReader
    + CanonStateSubscriptions
    + ForkChoiceSubscriptions<Header = HeaderTy<N>>
    + PersistedBlockSubscriptions
    + StageCheckpointReader
    + Clone
    + Debug
    + Unpin
    + 'static
{
}

impl<T, N: NodeTypesWithDB> FullProvider<N> for T where
    T: DatabaseProviderFactory<
            DB = N::DB,
            Provider: BlockReader
                          + StageCheckpointReader
                          + PruneCheckpointReader
                          + ChangeSetReader
                          + StorageChangeSetReader
                          + StorageSettingsCache,
        > + NodePrimitivesProvider<Primitives = N::Primitives>
            + AuxDbProviderFactory
        + BlockReaderIdExt<
            Transaction = TxTy<N>,
            Block = BlockTy<N>,
            Receipt = ReceiptTy<N>,
            Header = HeaderTy<N>,
        > + AccountReader
        + StateProviderFactory
        + StateReader
        + HashedPostStateProvider
        + ChainSpecProvider<ChainSpec = N::ChainSpec>
        + ChangeSetReader
        + StorageChangeSetReader
        + CanonStateSubscriptions
        + ForkChoiceSubscriptions<Header = HeaderTy<N>>
        + PersistedBlockSubscriptions
        + StageCheckpointReader
        + Clone
        + Debug
        + Unpin
        + 'static
{
}

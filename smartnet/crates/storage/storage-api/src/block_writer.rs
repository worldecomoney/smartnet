use crate::NodePrimitivesProvider;
use alloy_primitives::BlockNumber;
use smartnet_primitives_traits::{Block, NodePrimitives};
use smartnet_storage_errors::provider::ProviderResult;

/// `BlockExecution` Writer
pub trait BlockExecutionWriter:
    NodePrimitivesProvider<Primitives: NodePrimitives<Block = Self::Block>> + BlockWriter
{
    /// Remove all of the blocks above the provided number and their execution result
    ///
    /// The passed block number will stay in the database.
    fn remove_block_and_execution_above(&self, block: BlockNumber) -> ProviderResult<()>;
}

impl<T: BlockExecutionWriter> BlockExecutionWriter for &T {
    fn remove_block_and_execution_above(&self, block: BlockNumber) -> ProviderResult<()> {
        (*self).remove_block_and_execution_above(block)
    }
}

/// Block Writer.
///
/// **Payload writers are gone.** The engine is the only production block writer
/// (`StorageEngine::save_blocks` writes payload to the file tier and index + state to the
/// database). The former `insert_block` / `append_block_bodies` /
/// `append_blocks_with_state` Fjall payload writers were removed; this trait now carries only
/// the removal hooks used by `restore_unwind(2)` cold-shutdown recovery — the only
/// sanctioned unwind.
#[auto_impl::auto_impl(&, Arc, Box)]
pub trait BlockWriter {
    /// The block this writer can unwind.
    type Block: Block;
    /// The receipt type.
    type Receipt: Send + Sync;

    /// Removes all blocks above the given block number from the database.
    ///
    /// Note: This does not remove state or execution data.
    fn remove_blocks_above(&self, block: BlockNumber) -> ProviderResult<()>;

    /// Removes all block bodies above the given block number from the database.
    fn remove_bodies_above(&self, block: BlockNumber) -> ProviderResult<()>;
}

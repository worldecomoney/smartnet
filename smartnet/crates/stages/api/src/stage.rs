use crate::{error::StageError, StageCheckpoint, StageId};
use alloy_primitives::{BlockNumber, TxNumber};
use smartnet_db_api::{cursor::DbCursorRO, tables, transaction::DbTx};
use smartnet_provider::{BlockReader, DBProvider, ProviderError};
use std::{
    cmp::min,
    future::{poll_fn, Future},
    ops::{Range, RangeInclusive},
    task::{Context, Poll},
};
use tracing::instrument;

/// Stage execution input, see [`Stage::execute`].
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct ExecInput {
    /// The target block number the stage needs to execute towards.
    pub target: Option<BlockNumber>,
    /// The checkpoint of this stage the last time it was executed.
    pub checkpoint: Option<StageCheckpoint>,
}

/// Return type for [`ExecInput::next_block_range_with_threshold`].
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct BlockRangeOutput {
    /// The block range to execute.
    pub block_range: RangeInclusive<BlockNumber>,
    /// Whether this is the final range to execute.
    pub is_final_range: bool,
}

/// Return type for [`ExecInput::next_block_range_with_transaction_threshold`].
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct TransactionRangeOutput {
    /// The transaction range to execute.
    pub tx_range: Range<TxNumber>,
    /// The block range to execute.
    pub block_range: RangeInclusive<BlockNumber>,
    /// Whether this is the final range to execute.
    pub is_final_range: bool,
}

impl ExecInput {
    /// Return the checkpoint of the stage or default.
    pub fn checkpoint(&self) -> StageCheckpoint {
        self.checkpoint.unwrap_or_default()
    }

    /// Return the next block number after the current
    /// +1 is needed to skip the present block and always start from block number 1, not 0.
    pub fn next_block(&self) -> BlockNumber {
        let current_block = self.checkpoint();
        current_block.block_number + 1
    }

    /// Returns `true` if the target block number has already been reached.
    pub fn target_reached(&self) -> bool {
        self.checkpoint().block_number >= self.target()
    }

    /// Return the target block number or default.
    pub fn target(&self) -> BlockNumber {
        self.target.unwrap_or_default()
    }

    /// Return next block range that needs to be executed.
    pub fn next_block_range(&self) -> RangeInclusive<BlockNumber> {
        self.next_block_range_with_threshold(u64::MAX).block_range
    }

    /// Return true if this is the first block range to execute.
    pub const fn is_first_range(&self) -> bool {
        self.checkpoint.is_none()
    }

    /// Return the next block range to execute.
    pub fn next_block_range_with_threshold(&self, threshold: u64) -> BlockRangeOutput {
        let current_block = self.checkpoint();
        let start = current_block.block_number + 1;
        let target = self.target();

        let end = min(target, current_block.block_number.saturating_add(threshold));

        let is_final_range = end == target;
        BlockRangeOutput { block_range: start..=end, is_final_range }
    }

    /// Return the next block range determined the number of transactions within it.
    /// This function walks the block indices until either the end of the range is reached or
    /// the number of transactions exceeds the threshold.
    ///
    /// Returns [`None`] if no transactions are found for the current execution input.
    #[instrument(level = "debug", target = "sync::stages", skip(provider), ret)]
    pub fn next_block_range_with_transaction_threshold<Provider>(
        &self,
        provider: &Provider,
        tx_threshold: u64,
    ) -> Result<Option<TransactionRangeOutput>, StageError>
    where
        Provider: DBProvider + BlockReader,
    {
        // `TxLocations` is the transaction-number index; the payload lives in the file tier.
        // Get the lowest indexed transaction — transactions below it cannot be processed
        // (they may have been pruned or removed manually).
        let Some(first_tx) = provider
            .tx_ref()
            .cursor_read::<tables::TxLocations>()?
            .first()?
            .map(|(first_tx, _)| first_tx)
        else {
            return Ok(None);
        };
        let Some(lowest_transactions_block) = provider.block_by_transaction_id(first_tx)? else {
            return Ok(None);
        };

        let start_block = self.next_block().max(lowest_transactions_block);
        let target_block = self.target();

        // If the start block is greater than the target, then there's no transactions to process
        // and we return early. It's possible to trigger this scenario when running a stage
        // manually for a range of transactions that doesn't exist.
        if start_block > target_block {
            return Ok(None);
        }

        let start_block_body = provider
            .block_body_indices(start_block)?
            .ok_or(ProviderError::BlockBodyIndicesNotFound(start_block))?;
        let first_tx_num = start_block_body.first_tx_num();

        let target_block_body = provider
            .block_body_indices(target_block)?
            .ok_or(ProviderError::BlockBodyIndicesNotFound(target_block))?;

        // number of transactions left to execute.
        let all_tx_cnt = target_block_body.next_tx_num() - first_tx_num;

        if all_tx_cnt == 0 {
            // if there is no more transaction return back.
            return Ok(None);
        }

        // get block of this tx
        let (end_block, is_final_range, next_tx_num) = if all_tx_cnt <= tx_threshold {
            (target_block, true, target_block_body.next_tx_num())
        } else {
            // get tx block number. next_tx_num in this case will be less than all_tx_cnt.
            // So we are sure that transaction must exist.
            let end_block_number = provider
                .block_by_transaction_id(first_tx_num + tx_threshold)?
                .expect("block of tx must exist");
            // we want to get range of all transactions of this block, so we are fetching block
            // body.
            let end_block_body = provider
                .block_body_indices(end_block_number)?
                .ok_or(ProviderError::BlockBodyIndicesNotFound(end_block_number))?;
            (end_block_number, false, end_block_body.next_tx_num())
        };

        let tx_range = first_tx_num..next_tx_num;
        Ok(Some(TransactionRangeOutput {
            tx_range,
            block_range: start_block..=end_block,
            is_final_range,
        }))
    }
}

/// The output of a stage execution.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ExecOutput {
    /// How far the stage got.
    pub checkpoint: StageCheckpoint,
    /// Whether or not the stage is done.
    pub done: bool,
}

impl ExecOutput {
    /// Mark the stage as not done, checkpointing at the given place.
    pub const fn in_progress(checkpoint: StageCheckpoint) -> Self {
        Self { checkpoint, done: false }
    }

    /// Mark the stage as done, checkpointing at the given place.
    pub const fn done(checkpoint: StageCheckpoint) -> Self {
        Self { checkpoint, done: true }
    }
}

/// A stage is a segmented part of the syncing process of the node.
///
/// Each stage takes care of a well-defined task, such as downloading headers or executing
/// transactions, and persist their results to a database.
///
/// Stages must have a unique [ID][StageId] and implement a way to "roll forwards"
/// ([`Stage::execute`]).
///
/// `SmartNet`: there is no stage unwind — sync failures drop the run (recoverable stage
/// errors retry in the pipeline; everything else fails the pipeline). Only
/// `restore_unwind(2)` may unwind, on cold-shutdown recovery.
///
/// Stages are executed as part of a pipeline where they are executed serially.
///
/// Stages receive [`DBProvider`](smartnet_provider::DBProvider).
#[auto_impl::auto_impl(Box)]
pub trait Stage<Provider>: Send {
    /// Get the ID of the stage.
    ///
    /// Stage IDs must be unique.
    fn id(&self) -> StageId;

    /// Returns `Poll::Ready(Ok(()))` when the stage is ready to execute the given range.
    ///
    /// This method is heavily inspired by [tower](https://crates.io/crates/tower)'s `Service` trait.
    /// Any asynchronous tasks or communication should be handled in `poll_execute_ready`, e.g.
    /// moving downloaded items from downloaders to an internal buffer in the stage.
    ///
    /// If the stage has any pending external state, then `Poll::Pending` is returned.
    ///
    /// If `Poll::Ready(Err(_))` is returned, the stage may not be able to execute anymore
    /// depending on the specific error. In that case the pipeline drops the run.
    ///
    /// Once `Poll::Ready(Ok(()))` is returned, the stage may be executed once using `execute`.
    /// Until the stage has been executed, repeated calls to `poll_execute_ready` must return either
    /// `Poll::Ready(Ok(()))` or `Poll::Ready(Err(_))`.
    ///
    /// Note that `poll_execute_ready` may reserve shared resources that are consumed in a
    /// subsequent call of `execute`, e.g. internal buffers. It is crucial for implementations
    /// to not assume that `execute` will always be invoked and to ensure that those resources
    /// are appropriately released if the stage is dropped before `execute` is called.
    ///
    /// For the same reason, it is also important that any shared resources do not exhibit
    /// unbounded growth on repeated calls to `poll_execute_ready`.
    fn poll_execute_ready(
        &mut self,
        _cx: &mut Context<'_>,
        _input: ExecInput,
    ) -> Poll<Result<(), StageError>> {
        Poll::Ready(Ok(()))
    }

    /// Execute the stage.
    /// It is expected that the stage will write all necessary data to the database
    /// upon invoking this method.
    fn execute(&mut self, provider: &Provider, input: ExecInput) -> Result<ExecOutput, StageError>;

    /// Post execution commit hook.
    ///
    /// This is called after the stage has been executed and the data has been committed by the
    /// provider. The stage may want to pass some data from [`Self::execute`] via the internal
    /// field.
    fn post_execute_commit(&mut self) -> Result<(), StageError> {
        Ok(())
    }
}

/// [Stage] trait extension.
pub trait StageExt<Provider>: Stage<Provider> {
    /// Utility extension for the `Stage` trait that invokes `Stage::poll_execute_ready`
    /// with [`poll_fn`] context. For more information see [`Stage::poll_execute_ready`].
    fn execute_ready(
        &mut self,
        input: ExecInput,
    ) -> impl Future<Output = Result<(), StageError>> + Send {
        poll_fn(move |cx| self.poll_execute_ready(cx, input))
    }
}

impl<Provider, S: Stage<Provider> + ?Sized> StageExt<Provider> for S {}

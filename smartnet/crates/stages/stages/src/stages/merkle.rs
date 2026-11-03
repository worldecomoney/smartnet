use alloy_consensus::{constants::KECCAK_EMPTY, BlockHeader};
use alloy_primitives::{BlockNumber, Sealable, B256};
use smartnet_codecs::Compact;
use smartnet_primitives_traits::{GotExpected, SealedHeader};
use smartnet_stages_api::{
    BlockErrorKind, EntitiesCheckpoint, ExecInput, ExecOutput, MerkleCheckpoint, Stage,
    StageCheckpoint, StageError, StageId, StorageRootMerkleCheckpoint,
};
use smartnet_trie::{IntermediateStateRootState, StateRoot, StateRootProgress, StoredSubNode};
use smartnet_trie_db::DatabaseStateRoot;
use smartnet_consensus::ConsensusError;
use smartnet_db_api::{
    tables,
    transaction::DbTxMut,
};
use smartnet_provider::{
    ChangeSetReader, DBProvider, HeaderProvider, ProviderError, StageCheckpointReader,
    StageCheckpointWriter, StatsReader, StorageChangeSetReader, StorageSettingsCache, TrieWriter,
};

use std::fmt::Debug;

type DbStateRoot<'a, TX, A> = StateRoot<
    smartnet_trie_db::DatabaseTrieCursorFactory<&'a TX, A>,
    smartnet_trie_db::DatabaseHashedCursorFactory<&'a TX>,
>;
use tracing::*;

// TODO: automate the process outlined below so the user can just send in a debugging package
/// The error message that we include in invalid state root errors to tell users what information
/// they should include in a bug report, since true state root errors can be impossible to debug
/// with just basic logs.
pub const INVALID_STATE_ROOT_ERROR_MESSAGE: &str = r#"
Invalid state root error on stage verification!
This is an error that likely requires a bug report with additional information.
Please include the following information in your report:
 * This error message
 * The state root of the block that was rejected
 * 50-100 lines of logs before and after the first occurrence of the log message with the state root of the block that was rejected.
 * The debug logs from __the same time period__ (from the configured log directory).

Once you have this information, please submit an issue in this repository's tracker.
"#;

/// The default threshold (in number of blocks) for switching from incremental trie building
/// of changes to whole rebuild.
pub const MERKLE_STAGE_DEFAULT_REBUILD_THRESHOLD: u64 = 100_000;

/// The default threshold (in number of blocks) to run the stage in incremental mode. The
/// incremental mode will calculate the state root for a large range of blocks by calculating the
/// new state root for this many blocks, in batches, repeating until we reach the desired block
/// number.
pub const MERKLE_STAGE_DEFAULT_INCREMENTAL_THRESHOLD: u64 = 7_000;

/// The merkle hashing stage uses input from
/// [`AccountHashingStage`][crate::stages::AccountHashingStage] and
/// [`StorageHashingStage`][crate::stages::StorageHashingStage] to calculate intermediate hashes
/// and state roots.
///
/// This stage should be run with the above two stages, otherwise it is a no-op.
///
/// This stage is split in two: one for calculating hashes and one for unwinding.
///
/// When run in execution, it's going to be executed AFTER the hashing stages, to generate
/// the state root. When run in unwind mode, it's going to be executed BEFORE the hashing stages,
/// so that it unwinds the intermediate hashes based on the unwound hashed state from the hashing
/// stages. The order of these two variants is important. The unwind variant should be added to the
/// pipeline before the execution variant.
///
/// An example pipeline to only hash state would be:
///
/// - [`MerkleStage::Unwind`]
/// - [`AccountHashingStage`][crate::stages::AccountHashingStage]
/// - [`StorageHashingStage`][crate::stages::StorageHashingStage]
/// - [`MerkleStage::Execution`]
#[derive(Debug, Clone)]
pub enum MerkleStage {
    /// The execution portion of the merkle stage.
    Execution {
        // TODO: make struct for holding incremental settings, for code reuse between `Execution`
        // variant and `Both`
        /// The threshold (in number of blocks) for switching from incremental trie building
        /// of changes to whole rebuild.
        rebuild_threshold: u64,
        /// The threshold (in number of blocks) to run the stage in incremental mode. The
        /// incremental mode will calculate the state root by calculating the new state root for
        /// some number of blocks, repeating until we reach the desired block number.
        incremental_threshold: u64,
    },
    /// The unwind portion of the merkle stage.
    Unwind,
    /// Able to execute and unwind. Used for tests
    #[cfg(any(test, feature = "test-utils"))]
    Both {
        /// The threshold (in number of blocks) for switching from incremental trie building
        /// of changes to whole rebuild.
        rebuild_threshold: u64,
        /// The threshold (in number of blocks) to run the stage in incremental mode. The
        /// incremental mode will calculate the state root by calculating the new state root for
        /// some number of blocks, repeating until we reach the desired block number.
        incremental_threshold: u64,
    },
}

impl MerkleStage {
    /// Stage default for the [`MerkleStage::Execution`].
    pub const fn default_execution() -> Self {
        Self::Execution {
            rebuild_threshold: MERKLE_STAGE_DEFAULT_REBUILD_THRESHOLD,
            incremental_threshold: MERKLE_STAGE_DEFAULT_INCREMENTAL_THRESHOLD,
        }
    }

    /// Stage default for the [`MerkleStage::Unwind`].
    pub const fn default_unwind() -> Self {
        Self::Unwind
    }

    /// Create new instance of [`MerkleStage::Execution`].
    pub const fn new_execution(rebuild_threshold: u64, incremental_threshold: u64) -> Self {
        Self::Execution { rebuild_threshold, incremental_threshold }
    }

    /// Gets the hashing progress
    pub fn get_execution_checkpoint(
        &self,
        provider: &impl StageCheckpointReader,
    ) -> Result<Option<MerkleCheckpoint>, StageError> {
        let buf =
            provider.get_stage_checkpoint_progress(StageId::MerkleExecute)?.unwrap_or_default();

        if buf.is_empty() {
            return Ok(None);
        }

        let (checkpoint, _) = MerkleCheckpoint::from_compact(&buf, buf.len());
        Ok(Some(checkpoint))
    }

    /// Saves the hashing progress
    pub fn save_execution_checkpoint(
        &self,
        provider: &impl StageCheckpointWriter,
        checkpoint: Option<MerkleCheckpoint>,
    ) -> Result<(), StageError> {
        let mut buf = vec![];
        if let Some(checkpoint) = checkpoint {
            debug!(
                target: "sync::stages::merkle::exec",
                last_account_key = ?checkpoint.last_account_key,
                "Saving inner merkle checkpoint"
            );
            checkpoint.to_compact(&mut buf);
        }
        Ok(provider.save_stage_checkpoint_progress(StageId::MerkleExecute, buf)?)
    }
}

impl<Provider> Stage<Provider> for MerkleStage
where
    Provider: DBProvider<Tx: DbTxMut>
        + TrieWriter
        + StatsReader
        + HeaderProvider
        + ChangeSetReader
        + StorageChangeSetReader
        + StorageSettingsCache
        + StageCheckpointReader
        + StageCheckpointWriter,
{
    /// Return the id of the stage
    fn id(&self) -> StageId {
        match self {
            Self::Execution { .. } => StageId::MerkleExecute,
            Self::Unwind => StageId::MerkleUnwind,
            #[cfg(any(test, feature = "test-utils"))]
            Self::Both { .. } => StageId::Other("MerkleBoth"),
        }
    }

    /// Execute the stage.
    fn execute(&mut self, provider: &Provider, input: ExecInput) -> Result<ExecOutput, StageError> {
        let (threshold, incremental_threshold) = match self {
            Self::Unwind => {
                info!(target: "sync::stages::merkle::unwind", "Stage is always skipped");
                return Ok(ExecOutput::done(StageCheckpoint::new(input.target())));
            }
            Self::Execution { rebuild_threshold, incremental_threshold } => {
                (*rebuild_threshold, *incremental_threshold)
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::Both { rebuild_threshold, incremental_threshold } => {
                (*rebuild_threshold, *incremental_threshold)
            }
        };

        let range = input.next_block_range();
        let (from_block, to_block) = range.clone().into_inner();
        let current_block_number = input.checkpoint().block_number;

        let target_block = provider
            .header_by_number(to_block)?
            .ok_or_else(|| ProviderError::HeaderNotFound(to_block.into()))?;
        let target_block_root = target_block.state_root();

        let (trie_root, entities_checkpoint) = if range.is_empty() {
            (target_block_root, input.checkpoint().entities_stage_checkpoint().unwrap_or_default())
        } else if to_block - from_block > threshold || from_block == 1 {
            let mut checkpoint = self.get_execution_checkpoint(provider)?;

            // if there are more blocks than threshold it is faster to rebuild the trie
            let mut entities_checkpoint = if let Some(checkpoint) =
                checkpoint.as_ref().filter(|c| c.target_block == to_block)
            {
                debug!(
                    target: "sync::stages::merkle::exec",
                    current = ?current_block_number,
                    target = ?to_block,
                    last_account_key = ?checkpoint.last_account_key,
                    "Continuing inner merkle checkpoint"
                );

                input.checkpoint().entities_stage_checkpoint()
            } else {
                debug!(
                    target: "sync::stages::merkle::exec",
                    current = ?current_block_number,
                    target = ?to_block,
                    previous_checkpoint = ?checkpoint,
                    "Rebuilding trie"
                );
                // Reset the checkpoint and clear trie tables
                checkpoint = None;
                self.save_execution_checkpoint(provider, None)?;
                provider.tx_ref().clear::<tables::AccountsTrie>()?;
                provider.tx_ref().clear::<tables::StoragesTrie>()?;

                None
            }
            .unwrap_or(EntitiesCheckpoint {
                processed: 0,
                total: (provider.count_entries::<tables::HashedAccounts>()? +
                    provider.count_entries::<tables::HashedStorages>()?)
                    as u64,
            });

            let tx = provider.tx_ref();
            let progress = smartnet_trie_db::with_adapter!(provider, |A| {
                DbStateRoot::<_, A>::from_tx(tx)
                    .with_intermediate_state(checkpoint.map(IntermediateStateRootState::from))
                    .root_with_progress()
            })
            .map_err(|e| {
                error!(target: "sync::stages::merkle", %e, ?current_block_number, ?to_block, "State root with progress failed! {INVALID_STATE_ROOT_ERROR_MESSAGE}");
                StageError::Fatal(Box::new(e))
            })?;
            match progress {
                StateRootProgress::Progress(state, hashed_entries_walked, updates) => {
                    provider.write_trie_updates(updates)?;

                    let mut checkpoint = MerkleCheckpoint::new(
                        to_block,
                        state.account_root_state.last_hashed_key,
                        state
                            .account_root_state
                            .walker_stack
                            .into_iter()
                            .map(StoredSubNode::from)
                            .collect(),
                        state.account_root_state.hash_builder.into(),
                    );

                    // Save storage root state if present
                    if let Some(storage_state) = state.storage_root_state {
                        checkpoint.storage_root_checkpoint =
                            Some(StorageRootMerkleCheckpoint::new(
                                storage_state.state.last_hashed_key,
                                storage_state
                                    .state
                                    .walker_stack
                                    .into_iter()
                                    .map(StoredSubNode::from)
                                    .collect(),
                                storage_state.state.hash_builder.into(),
                                storage_state.account.nonce,
                                storage_state.account.balance,
                                storage_state.account.bytecode_hash.unwrap_or(KECCAK_EMPTY),
                            ));
                    }
                    self.save_execution_checkpoint(provider, Some(checkpoint))?;

                    entities_checkpoint.processed += hashed_entries_walked as u64;

                    return Ok(ExecOutput {
                        checkpoint: input
                            .checkpoint()
                            .with_entities_stage_checkpoint(entities_checkpoint),
                        done: false,
                    });
                }
                StateRootProgress::Complete(root, hashed_entries_walked, updates) => {
                    provider.write_trie_updates(updates)?;

                    entities_checkpoint.processed += hashed_entries_walked as u64;

                    (root, entities_checkpoint)
                }
            }
        } else {
            debug!(target: "sync::stages::merkle::exec", current = ?current_block_number, target = ?to_block, "Updating trie in chunks");
            let mut final_root = None;
            for start_block in range.step_by(incremental_threshold as usize) {
                let chunk_to = std::cmp::min(start_block + incremental_threshold, to_block);
                let chunk_range = start_block..=chunk_to;
                debug!(
                    target: "sync::stages::merkle::exec",
                    current = ?current_block_number,
                    target = ?to_block,
                    incremental_threshold,
                    chunk_range = ?chunk_range,
                    "Processing chunk"
                );
                let (root, updates) = smartnet_trie_db::with_adapter!(provider, |A| {
                    DbStateRoot::<_, A>::incremental_root_with_updates(provider, chunk_range)
                })
                .map_err(|e| {
                    error!(target: "sync::stages::merkle", %e, ?current_block_number, ?to_block, "Incremental state root failed! {INVALID_STATE_ROOT_ERROR_MESSAGE}");
                    StageError::Fatal(Box::new(e))
                })?;
                provider.write_trie_updates(updates)?;
                final_root = Some(root);
            }

            // if we had no final root, we must have not looped above, which should not be possible
            let final_root = final_root.ok_or(StageError::Fatal(
                "Incremental merkle hashing did not produce a final root".into(),
            ))?;

            let total_hashed_entries = (provider.count_entries::<tables::HashedAccounts>()? +
                provider.count_entries::<tables::HashedStorages>()?)
                as u64;

            let entities_checkpoint = EntitiesCheckpoint {
                // This is fine because `range` doesn't have an upper bound, so in this `else`
                // branch we're just hashing all remaining accounts and storage slots we have in the
                // database.
                processed: total_hashed_entries,
                total: total_hashed_entries,
            };
            // Save the checkpoint
            (final_root, entities_checkpoint)
        };

        // Reset the checkpoint
        self.save_execution_checkpoint(provider, None)?;

        validate_state_root(trie_root, SealedHeader::seal_slow(target_block), to_block)?;

        Ok(ExecOutput {
            checkpoint: StageCheckpoint::new(to_block)
                .with_entities_stage_checkpoint(entities_checkpoint),
            done: true,
        })
    }
}

/// Check that the computed state root matches the root in the expected header.
#[inline]
fn validate_state_root<H: BlockHeader + Sealable + Debug>(
    got: B256,
    expected: SealedHeader<H>,
    target_block: BlockNumber,
) -> Result<(), StageError> {
    if got == expected.state_root() {
        Ok(())
    } else {
        error!(target: "sync::stages::merkle", ?target_block, ?got, ?expected, "Failed to verify block state root! {INVALID_STATE_ROOT_ERROR_MESSAGE}");
        Err(StageError::Block {
            error: BlockErrorKind::Validation(ConsensusError::BodyStateRootDiff(
                GotExpected { got, expected: expected.state_root() }.into(),
            )),
            block: Box::new(expected.block_with_parent()),
        })
    }
}

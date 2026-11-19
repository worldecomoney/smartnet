use alloy_primitives::{keccak256, B256};
use itertools::Itertools;
use smartnet_primitives_traits::Account;
use smartnet_stages_api::{
    AccountHashingCheckpoint, BlockRangeOutput, EntitiesCheckpoint, ExecInput, ExecOutput, Stage,
    StageCheckpoint, StageError, StageId,
};
use smartnet_config::config::{EtlConfig, HashingConfig};
use smartnet_db_api::{
    cursor::{DbCursorRO, DbCursorRW},
    tables,
    transaction::{DbTx, DbTxMut},
    RawKey, RawTable, RawValue,
};
use smartnet_provider::etl::Collector;
use smartnet_provider::{
    AccountExtReader, DBProvider, HashingWriter, StatsReader, StorageSettingsCache,
};
use smartnet_storage_errors::provider::ProviderResult;
use std::{
    collections::BTreeSet,
    fmt::Debug,
    io,
    ops::{Range, RangeInclusive},
    sync::mpsc::{self, Receiver},
};
use tracing::*;

/// Maximum number of channels that can exist in memory.
const MAXIMUM_CHANNELS: usize = 10_000;

/// Maximum number of accounts to hash per rayon worker job.
const WORKER_CHUNK_SIZE: usize = 100;

/// One worker's result stream: fallible entries (raw-key decode) plus the expected
/// entry count, so a worker that dies without sending its error is detected instead
/// of silently truncating the collector.
type HashChannel = Receiver<Result<(RawKey<B256>, RawValue<Account>), StageError>>;

/// Account hashing stage hashes plain account.
/// This is preparation before generating intermediate hashes and calculating Merkle tree root.
#[derive(Clone, Debug)]
pub struct AccountHashingStage {
    /// The threshold (in number of blocks) for switching between incremental
    /// hashing and full storage hashing.
    pub clean_threshold: u64,
    /// The maximum number of accounts to process before committing during unwind.
    pub commit_threshold: u64,
    /// The maximum number of changeset entries to process before committing. The stage commits
    /// after either `commit_threshold` blocks or `commit_entries` entries, whichever comes first.
    pub commit_entries: u64,
    /// ETL configuration
    pub etl_config: EtlConfig,
}

impl AccountHashingStage {
    /// Create new instance of [`AccountHashingStage`].
    pub const fn new(config: HashingConfig, etl_config: EtlConfig) -> Self {
        Self {
            clean_threshold: config.clean_threshold,
            commit_threshold: config.commit_threshold,
            commit_entries: config.commit_entries,
            etl_config,
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl AccountHashingStage {
    /// Initializes the `PlainAccountState` table with `num_accounts` having some random state
    /// at the target block, with `txs_range` transactions in each block.
    ///
    /// Proceeds to go to the `BlockTransitionIndex` end, go back `transitions` and change the
    /// account state in the `AccountChangeSets` table.
    pub fn seed<Tx: DbTx + DbTxMut + 'static, N: smartnet_provider::providers::ProviderNodeTypes>(
        provider: &smartnet_provider::DatabaseProvider<Tx, N>,
        opts: SeedOpts,
    ) -> Result<Vec<(alloy_primitives::Address, Account)>, StageError>
    where
        N::Primitives: smartnet_primitives_traits::NodePrimitives<
            Block = smartnet_ethereum_primitives::Block,
            BlockHeader = smartnet_primitives_traits::Header,
        >,
    {
        use alloy_primitives::U256;
        use smartnet_testing_utils::{
            generators,
            generators::{random_block_range, random_eoa_accounts, BlockRangeParams},
        };
        use smartnet_db_api::models::AccountBeforeTx;

        let mut rng = generators::rng();

        let blocks = random_block_range(
            &mut rng,
            opts.blocks.clone(),
            BlockRangeParams { parent: Some(B256::ZERO), tx_count: opts.txs, ..Default::default() },
        );

        for block in blocks {
            provider.insert_block(&block.try_recover().unwrap()).unwrap();
        }
        let mut accounts = random_eoa_accounts(&mut rng, opts.accounts);
        {
            // Account State generator
            let mut account_cursor =
                provider.tx_ref().cursor_write::<tables::PlainAccountState>()?;
            accounts.sort_by_key(|a| a.0);
            for (addr, acc) in &accounts {
                account_cursor.append(*addr, acc)?;
            }

            let mut acc_changeset_cursor =
                provider.tx_ref().cursor_write::<tables::AccountChangeSets>()?;
            for (t, (addr, acc)) in opts.blocks.zip(&accounts) {
                let Account { nonce, balance, .. } = acc;
                let prev_acc = Account {
                    nonce: nonce - 1,
                    balance: balance - U256::from(1),
                    bytecode_hash: None,
                };
                let acc_before_tx = AccountBeforeTx { address: *addr, info: Some(prev_acc) };
                acc_changeset_cursor.append(t, &acc_before_tx)?;
            }
        }

        Ok(accounts)
    }
}

impl Default for AccountHashingStage {
    fn default() -> Self {
        Self {
            clean_threshold: 500_000,
            commit_threshold: 100_000,
            commit_entries: 30_000_000,
            etl_config: EtlConfig::default(),
        }
    }
}

impl<Provider> Stage<Provider> for AccountHashingStage
where
    Provider: DBProvider<Tx: DbTxMut>
        + HashingWriter
        + AccountExtReader
        + StatsReader
        + StorageSettingsCache,
{
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::AccountHashing
    }

    /// Execute the stage.
    ///
    /// Hashes plain state to populate the hashed tables.
    fn execute(&mut self, provider: &Provider, input: ExecInput) -> Result<ExecOutput, StageError> {
        if input.target_reached() {
            return Ok(ExecOutput::done(input.checkpoint()));
        }

        // Use the total remaining range to decide clean vs incremental.
        let total_range = input.target() - input.checkpoint().block_number;
        let from_block = input.next_block();

        if total_range > self.clean_threshold || from_block == 1 {
            // if there are more blocks than threshold it is faster to go over Plain state and
            // hash all accounts otherwise take changesets aggregate the sets and apply
            // hashing to HashedAccounts table. Also, if we start from genesis, we need to
            // hash from scratch, as genesis accounts are not in changeset.
            let tx = provider.tx_ref();

            // clear table, load all accounts and hash them
            tx.clear::<tables::HashedAccounts>()?;

            let mut accounts_cursor = tx.cursor_read::<RawTable<tables::PlainAccountState>>()?;
            let mut collector =
                Collector::new(self.etl_config.file_size, self.etl_config.dir.clone());
            let mut channels: Vec<(HashChannel, usize)> = Vec::with_capacity(MAXIMUM_CHANNELS);

            // channels used to return result of account hashing
            for chunk in &accounts_cursor.walk(None)?.chunks(WORKER_CHUNK_SIZE) {
                // An _unordered_ channel to receive results from a rayon job
                let chunk = chunk.collect::<Result<Vec<_>, _>>()?;
                let (tx, rx) = mpsc::sync_channel(chunk.len());
                let expected = chunk.len();
                channels.push((rx, expected));
                // Spawn the hashing task onto the global rayon pool
                rayon::spawn(move || {
                    for (address, account) in chunk {
                        // A malformed key is an error on the channel, never a panic:
                        // a panicking worker would drop the sender and `collect` would
                        // otherwise treat the disconnect as successful completion.
                        match address.key() {
                            Ok(address) => {
                                if tx
                                    .send(Ok((RawKey::new(keccak256(address)), account)))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(err) => {
                                let _ = tx.send(Err(err.into()));
                                break;
                            }
                        }
                    }
                });

                // Flush to ETL when channels length reaches MAXIMUM_CHANNELS
                if !channels.is_empty() && channels.len().is_multiple_of(MAXIMUM_CHANNELS) {
                    collect(&mut channels, &mut collector)?;
                }
            }

            collect(&mut channels, &mut collector)?;

            let mut hashed_account_cursor =
                tx.cursor_write::<RawTable<tables::HashedAccounts>>()?;

            let total_hashes = collector.len();
            let interval = (total_hashes / 10).max(1);
            for (index, item) in collector.iter()?.enumerate() {
                if index > 0 && index.is_multiple_of(interval) {
                    info!(
                        target: "sync::stages::hashing_account",
                        progress = %format!("{:.2}%", (index as f64 / total_hashes as f64) * 100.0),
                        "Inserting hashes"
                    );
                }

                let (key, value) = item?;
                hashed_account_cursor
                    .append(RawKey::<B256>::from_vec(key), &RawValue::<Account>::from_vec(value))?;
            }

            let checkpoint = StageCheckpoint::new(input.target())
                .with_account_hashing_stage_checkpoint(AccountHashingCheckpoint {
                    progress: stage_checkpoint_progress(provider)?,
                    ..Default::default()
                });

            Ok(ExecOutput { checkpoint, done: true })
        } else {
            // Stream changesets entry-by-entry, bounded by both block count
            // (commit_threshold) and entry count (commit_entries), whichever comes first.
            let BlockRangeOutput { block_range, is_final_range } =
                input.next_block_range_with_threshold(self.commit_threshold);
            let (from_block, to_block) = block_range.into_inner();

            let tx = provider.tx_ref();
            let mut changeset_cursor = tx.cursor_read::<tables::AccountChangeSets>()?;
            let mut changed = BTreeSet::new();
            let mut total_entries = 0u64;
            let mut last_block = from_block;

            for entry in changeset_cursor.walk_range(from_block..=to_block)? {
                let (block_number, account_before) = entry?;

                // Check the entry limit only at block boundaries so we never
                // checkpoint mid-block (which would skip the remaining entries
                // for that block on the next invocation).
                if block_number != last_block && total_entries >= self.commit_entries {
                    break;
                }

                last_block = block_number;
                changed.insert(account_before.address);
                total_entries += 1;
            }

            let accounts = provider.basic_accounts(changed)?;
            provider.insert_account_for_hashing(accounts)?;

            let exhausted = total_entries < self.commit_entries;
            let done = exhausted && is_final_range;
            let progress_block = if exhausted { to_block } else { last_block };

            let checkpoint = StageCheckpoint::new(progress_block)
                .with_account_hashing_stage_checkpoint(AccountHashingCheckpoint {
                    progress: stage_checkpoint_progress(provider)?,
                    ..Default::default()
                });

            Ok(ExecOutput { checkpoint, done })
        }
    }
}

/// Flushes channels hashes to ETL collector.
///
/// Iterates each channel to its natural end (a cleanly finished worker drops its
/// sender after the last entry) and fails loudly when a worker died early: an entry
/// error propagates, and a short count means the worker panicked mid-chunk — either
/// way the stage must not report success over a partially collected set.
fn collect(
    channels: &mut Vec<(HashChannel, usize)>,
    collector: &mut Collector<RawKey<B256>, RawValue<Account>>,
) -> Result<(), StageError> {
    for (channel, expected) in channels.iter_mut() {
        let mut received = 0usize;
        loop {
            match channel.recv() {
                Ok(Ok((key, v))) => {
                    collector.insert(key, v)?;
                    received += 1;
                }
                // The worker reported a failure (e.g. malformed key) — propagate it.
                Ok(Err(err)) => return Err(err),
                // Sender dropped: either the worker finished every entry or it died
                // mid-chunk; the count check below tells the two apart.
                Err(_) => break,
            }
        }
        if received != *expected {
            return Err(io::Error::other(format!(
                "account hashing worker channel closed after {received}/{expected} entries"
            ))
            .into());
        }
    }
    info!(target: "sync::stages::hashing_account", "Hashed {} entries", collector.len());
    channels.clear();
    Ok(())
}

// TODO: Rewrite this
/// `SeedOpts` provides configuration parameters for calling `AccountHashingStage::seed`
/// in unit tests or benchmarks to generate an initial database state for running the
/// stage.
///
/// In order to check the "full hashing" mode of the stage you want to generate more
/// transitions than `AccountHashingStage.clean_threshold`. This requires:
/// 1. Creating enough blocks so there's enough transactions to generate the required transition
///    keys in the `BlockTransitionIndex` (which depends on the `TxTransitionIndex` internally)
/// 2. Setting `blocks.len() > clean_threshold` so that there's enough diffs to actually take the
///    2nd codepath
#[derive(Clone, Debug)]
pub struct SeedOpts {
    /// The range of blocks to be generated
    pub blocks: RangeInclusive<u64>,
    /// The number of accounts to be generated
    pub accounts: usize,
    /// The range of transactions to be generated per block.
    pub txs: Range<u8>,
}

fn stage_checkpoint_progress(provider: &impl StatsReader) -> ProviderResult<EntitiesCheckpoint> {
    Ok(EntitiesCheckpoint {
        processed: provider.count_entries::<tables::HashedAccounts>()? as u64,
        total: provider.count_entries::<tables::PlainAccountState>()? as u64,
    })
}

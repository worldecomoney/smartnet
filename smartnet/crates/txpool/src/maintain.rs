//! Support for maintaining the state of the transaction pool

use crate::{
    blobstore::{BlobSidecarConverter, BlobStoreCanonTracker, BlobStoreUpdates},
    error::PoolError,
    metrics::MaintainPoolMetrics,
    traits::{CanonicalStateUpdate, EthPoolTransaction, TransactionPool, TransactionPoolExt},
    validate::ValidPoolTransaction,
    AllPoolTransactions, BlobTransactionSidecarVariant, BlockInfo, PoolTransaction, PoolUpdateKind,
    TransactionOrigin,
};
use alloy_consensus::{transaction::TxHashRef, BlockHeader, Typed2718};
use alloy_eips::{BlockNumberOrTag, Decodable2718, Encodable2718};
use alloy_primitives::{
    map::{AddressSet, HashSet},
    Address, BlockHash, BlockNumber, Bytes,
};
use alloy_rlp::Encodable;
use futures_util::{
    future::{BoxFuture, Fuse, FusedFuture},
    FutureExt, Stream, StreamExt,
};
use smartnet_execution_types::ChangedAccount;
use smartnet_primitives_traits::{
    transaction::signed::SignedTransaction, NodePrimitives, SealedHeader,
};
use serde::{Deserialize, Serialize};
use smartnet_chain_state::CanonStateNotification;
use smartnet_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use smartnet_fs_util::FsPathError;
use smartnet_storage_api::{
    errors::provider::ProviderError, BlockReaderIdExt, StateProviderFactory,
};
use smartnet_tasks::Runtime;
use std::{
    borrow::Borrow,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    sync::oneshot,
    time::{interval, Duration, MissedTickBehavior},
};
use tracing::{debug, error, info, trace, warn};

/// Default/ceiling for how long non-executable (future-nonce) transactions stay queued.
///
/// The node pins the real TTL from `TXPOOL_QUEUED_TTL_SECS` (validated `1..=600` at boot) and
/// passes it into [`MaintainPoolConfig`]. This constant is only the fallback for programmatic
/// launches: a future-nonce tx that cannot ever execute must not park the pool indefinitely.
pub const MAX_QUEUED_TRANSACTION_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// Default pending (executable) tx lifetime cap: **none** — pending txs stay until included,
/// replaced, or invalidated. The node sets a cap only with `TXPOOL_PENDING_TTL_SECS > 0`.
pub const DEFAULT_PENDING_TX_LIFETIME: Option<Duration> = None;

/// Additional settings for maintaining the transaction pool
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintainPoolConfig {
    /// Maximum (reorg) depth we handle when updating the transaction pool: `new.number -
    /// last_seen.number`
    ///
    /// Default: 64 (2 epochs)
    pub max_update_depth: u64,
    /// Maximum number of accounts to reload from state at once when updating the transaction pool.
    ///
    /// Default: 100
    pub max_reload_accounts: usize,

    /// Non-executable (queued) tx lifetime, supplied by the node's
    /// `TXPOOL_QUEUED_TTL_SECS` policy (fallback: 10 minutes). Applies to every tx: in a
    /// mesh-only network there is no meaningful local/external distinction.
    pub queued_ttl: Duration,

    /// Executable (pending) tx lifetime cap; `None` = no cap. Supplied by the node's
    /// `TXPOOL_PENDING_TTL_SECS` policy.
    pub pending_ttl: Option<Duration>,

    /// How often the node's cleansing sweep runs, even with zero canonical events.
    pub ttl_sweep_interval: Duration,
}

impl Default for MaintainPoolConfig {
    fn default() -> Self {
        Self {
            max_update_depth: 64,
            max_reload_accounts: 100,
            queued_ttl: MAX_QUEUED_TRANSACTION_LIFETIME,
            pending_ttl: DEFAULT_PENDING_TX_LIFETIME,
            ttl_sweep_interval: Duration::from_secs(60),
        }
    }
}

/// Pure staleness check the TTL sweeps use (testable): `true` when the tx is past its TTL.
/// Origin is irrelevant — in a mesh-only network every tx reaches the pool through the same
/// in-process RPC semantics, so there is no meaningful local/external distinction.
/// `tx.timestamp` is pool-insert time.
fn is_stale_by_ttl<T>(
    tx: &Arc<ValidPoolTransaction<T>>,
    now: std::time::Instant,
    ttl: Duration,
) -> bool
where
    T: PoolTransaction,
{
    now.duration_since(tx.timestamp) > ttl
}

/// TTL stale-eviction sweep: removes pool transactions past their bucket TTL.
///
/// Invoked from the chain-event pass on every canonical commit/reorg **and** from a periodic
/// timer in the maintenance loop. The timer exists because a stalled chain emits no canonical
/// event; the TTLs still bound *what* is evicted, the two triggers bound *when* we look.
fn sweep_stale_by_ttl<P>(
    pool: &P,
    queued_ttl: Duration,
    pending_ttl: Option<Duration>,
) where
    P: TransactionPoolExt,
{
    let mut stale_blobs = Vec::new();
    let now = std::time::Instant::now();
    // Queued bucket: future-nonce txs past their TTL, regardless of origin.
    let stale_queued: Vec<_> = pool
        .queued_transactions()
        .into_iter()
        .filter(|tx| is_stale_by_ttl(tx, now, queued_ttl))
        .map(|tx| {
            if tx.is_eip4844() {
                stale_blobs.push(*tx.hash());
            }
            *tx.hash()
        })
        .collect();
    // Pending bucket: only when the operator set a pending cap.
    let stale_pending: Vec<_> = match pending_ttl {
        Some(pending) => pool
            .pending_transactions()
            .into_iter()
            .filter(|tx| is_stale_by_ttl(tx, now, pending))
            .map(|tx| *tx.hash())
            .collect(),
        None => Vec::new(),
    };
    debug!(target: "txpool", queued=%stale_queued.len(), pending=%stale_pending.len(), "removing stale transactions");
    let mut all = stale_queued;
    all.extend(stale_pending);
    pool.remove_transactions(all);
    if !stale_blobs.is_empty() {
        pool.delete_blobs(stale_blobs);
    }
}

/// Settings for local transaction backup task
#[derive(Debug, Clone, Default)]
pub struct LocalTransactionBackupConfig {
    /// Path to transactions backup file
    pub transactions_path: Option<PathBuf>,
    /// Queued-tx TTL applied by maintenance, used to discard stale backups on reload.
    pub queued_ttl: Duration,
    /// Pending-tx TTL cap applied by maintenance, used to discard stale backups on reload.
    pub pending_ttl: Option<Duration>,
}

impl LocalTransactionBackupConfig {
    /// Receive path to transactions backup and return initialized config
    pub const fn with_local_txs_backup(transactions_path: PathBuf) -> Self {
        Self {
            transactions_path: Some(transactions_path),
            queued_ttl: MAX_QUEUED_TRANSACTION_LIFETIME,
            pending_ttl: DEFAULT_PENDING_TX_LIFETIME,
        }
    }
}

/// Returns a spawnable future for maintaining the state of the transaction pool.
pub fn maintain_transaction_pool_future<N, Client, P, St>(
    client: Client,
    pool: P,
    events: St,
    task_spawner: Runtime,
    config: MaintainPoolConfig,
) -> BoxFuture<'static, ()>
where
    N: NodePrimitives,
    Client: StateProviderFactory
        + BlockReaderIdExt<Header = N::BlockHeader>
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = N::BlockHeader> + EthereumHardforks>
        + Clone
        + 'static,
    P: TransactionPoolExt<Transaction: PoolTransaction<Consensus = N::SignedTx>, Block = N::Block>
        + 'static,
    St: Stream<Item = CanonStateNotification<N>> + Send + Unpin + 'static,
{
    async move {
        maintain_transaction_pool(client, pool, events, task_spawner, config).await;
    }
    .boxed()
}

/// Maintains the state of the transaction pool by handling new blocks and reorgs.
///
/// This listens for any new blocks and reorgs and updates the transaction pool's state accordingly
pub async fn maintain_transaction_pool<N, Client, P, St>(
    client: Client,
    pool: P,
    mut events: St,
    task_spawner: Runtime,
    config: MaintainPoolConfig,
) where
    N: NodePrimitives,
    Client: StateProviderFactory
        + BlockReaderIdExt<Header = N::BlockHeader>
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = N::BlockHeader> + EthereumHardforks>
        + Clone
        + 'static,
    P: TransactionPoolExt<Transaction: PoolTransaction<Consensus = N::SignedTx>, Block = N::Block>
        + 'static,
    St: Stream<Item = CanonStateNotification<N>> + Send + Unpin + 'static,
{
    let metrics = MaintainPoolMetrics::default();
    let MaintainPoolConfig { max_update_depth, max_reload_accounts, .. } = config;
    // Loop-carried state lives in one struct so the canonical/reorg handlers
    // (extracted below) can borrow it mutably without a 10-parameter handshake.
    let mut mstate = MaintainState {
        blob_store_tracker: BlobStoreCanonTracker::default(),
        last_finalized_block: FinalizedBlockTracker::new(client.finalized_block_number().ok().flatten()),
        dirty_addresses: AddressSet::default(),
        maintained_state: MaintainedPoolState::InSync,
        first_event: true,
    };
    // ensure the pool points to latest state
    if let Ok(Some(latest)) = client.header_by_number_or_tag(BlockNumberOrTag::Latest) {
        let latest = SealedHeader::seal_slow(latest);
        let chain_spec = client.chain_spec();
        let info = BlockInfo {
            block_gas_limit: latest.gas_limit(),
            last_seen_block_hash: latest.hash(),
            last_seen_block_number: latest.number(),
            pending_basefee: chain_spec
                .next_block_base_fee(latest.header(), latest.timestamp())
                .unwrap_or_default(),
            pending_blob_fee: latest
                .maybe_next_block_blob_fee(chain_spec.blob_params_at_timestamp(latest.timestamp())),
        };
        pool.set_block_info(info);
    }

    // the future that reloads accounts from state
    let mut reload_accounts_fut = Fuse::terminated();

    // Node-owned cleansing policy: TTLs and cadence arrive in `MaintainPoolConfig`, never from
    // hidden globals. Applied both on canonical events and on the timer below.
    let MaintainPoolConfig { queued_ttl, pending_ttl, ttl_sweep_interval, .. } = config;
    let mut ttl_tick = interval(ttl_sweep_interval);
    ttl_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tracing::debug!(
        target: "txpool",
        queued_ttl_secs = queued_ttl.as_secs(),
        pending_ttl_secs = ?pending_ttl.map(|d| d.as_secs()),
        sweep_interval_secs = ttl_sweep_interval.as_secs(),
        "txpool TTL sweep configured by node policy (canonical events + timer)"
    );

    // The update loop that waits for new blocks and reorgs and performs pool updated
    // Listen for new chain events and derive the update action for the pool
    loop {
        trace!(target: "txpool", state=?mstate.maintained_state, "awaiting new block or reorg");

        metrics.set_dirty_accounts_len(mstate.dirty_addresses.len());
        let pool_info = pool.block_info();

        // after performing a pool update after a new block we have some time to properly update
        // dirty accounts and correct if the pool drifted from current state, for example after
        // restart or a pipeline run
        if mstate.maintained_state.is_drifted() {
            metrics.inc_drift();
            // assuming all senders are dirty
            mstate.dirty_addresses = pool.unique_senders();
            // make sure we toggle the state back to in sync
            mstate.maintained_state = MaintainedPoolState::InSync;
        }

        // if we have accounts that are out of sync with the pool, we reload them in chunks
        if !mstate.dirty_addresses.is_empty() && reload_accounts_fut.is_terminated() {
            let (tx, rx) = oneshot::channel();
            let c = client.clone();
            let at = pool_info.last_seen_block_hash;
            let fut = if mstate.dirty_addresses.len() > max_reload_accounts {
                // need to chunk accounts to reload
                let accs_to_reload =
                    mstate.dirty_addresses.iter().copied().take(max_reload_accounts).collect::<Vec<_>>();
                for acc in &accs_to_reload {
                    // make sure we remove them from the dirty set
                    mstate.dirty_addresses.remove(acc);
                }
                async move {
                    let res = load_accounts(c, at, accs_to_reload);
                    let _ = tx.send(res);
                }
                .boxed()
            } else {
                // can fetch all dirty accounts at once
                let accs_to_reload = std::mem::take(&mut mstate.dirty_addresses);
                async move {
                    let res = load_accounts(c, at, accs_to_reload);
                    let _ = tx.send(res);
                }
                .boxed()
            };
            reload_accounts_fut = rx.fuse();
            task_spawner.spawn_blocking_task(fut);
        }

        // check if we have a new finalized block
        if let Some(finalized) =
            mstate.last_finalized_block.update(client.finalized_block_number().ok().flatten()) &&
            let BlobStoreUpdates::Finalized(blobs) =
                mstate.blob_store_tracker.on_finalized_block(finalized)
        {
            metrics.inc_deleted_tracked_blobs(blobs.len());
            // remove all finalized blobs from the blob store
            pool.delete_blobs(blobs);
            // and also do periodic cleanup
            let pool = pool.clone();
            task_spawner.spawn_blocking_task(async move {
                debug!(target: "txpool", finalized_block = %finalized, "cleaning up blob store");
                pool.cleanup_blobs();
            });
        }

        // outcomes of the futures we are waiting on
        let mut event = None;
        let mut reloaded = None;

        // select of account reloads, the TTL timer, and new canonical state updates (which
        // should arrive at the rate of the block time — but may not when the chain stalls)
        tokio::select! {
            res = &mut reload_accounts_fut =>  {
                reloaded = Some(res);
            }
            // Self-healing sweep: runs even with no canonical event and no account reload.
            _ = ttl_tick.tick() => {
                sweep_stale_by_ttl(&pool, queued_ttl, pending_ttl);
                continue;
            }
            ev = events.next() =>  {
                 if ev.is_none() {
                    // the stream ended, we are done
                    break;
                }
                event = ev;
                // on receiving the first event on start up, mark the pool as drifted to explicitly
                // trigger revalidation and clear out outdated txs.
                if mstate.first_event {
                    mstate.maintained_state = MaintainedPoolState::Drifted;
                    mstate.first_event = false
                }
            }
        }
        // handle the result of the account reload
        match reloaded {
            Some(Ok(Ok(LoadedAccounts { accounts, failed_to_load }))) => {
                // reloaded accounts successfully
                // extend accounts we failed to load from database
                mstate.dirty_addresses.extend(failed_to_load);
                // update the pool with the loaded accounts
                pool.update_accounts(accounts);
            }
            Some(Ok(Err(res))) => {
                // Failed to load accounts from state
                let (accs, err) = *res;
                debug!(target: "txpool", %err, "failed to load accounts");
                mstate.dirty_addresses.extend(accs);
            }
            Some(Err(_)) => {
                // failed to receive the accounts, sender dropped, only possible if task panicked
                mstate.maintained_state = MaintainedPoolState::Drifted;
            }
            None => {}
        }

        // handle the new block or reorg
        let Some(event) = event else { continue };

        // Canonical event arrived: sweep here too so eviction is not delayed by up to one
        // timer tick while the chain is moving. The timer branch above covers the stalled case.
        sweep_stale_by_ttl(&pool, queued_ttl, pending_ttl);
        match event {
            CanonStateNotification::Reorg { old, new } => {
                handle_canonical_reorg(
                    &client,
                    &pool,
                    &mut mstate,
                    &metrics,
                    &pool_info,
                    old,
                    new,
                )
                .await;
            }
            CanonStateNotification::Commit { new } => {
                handle_canonical_commit(
                    &client,
                    &pool,
                    &mut mstate,
                    &pool_info,
                    max_update_depth,
                    &task_spawner,
                    new,
                );
            }
        }
    }
}

/// Loop-carried maintenance state — the extraction seam for the canonical/reorg
/// handlers (HUGE-FN Tier-B split of `maintain_transaction_pool`).
struct MaintainState {
    blob_store_tracker: BlobStoreCanonTracker,
    last_finalized_block: FinalizedBlockTracker,
    dirty_addresses: AddressSet,
    maintained_state: MaintainedPoolState,
    first_event: bool,
}

/// Canonical-event phase (reorg): reload only the accounts the old chain touched and the
/// new chain did not, update the pool with the new canonical state, and re-inject the
/// transactions the old chain mined but the new one does not.
async fn handle_canonical_reorg<N, C, P>(
    client: &C,
    pool: &P,
    mstate: &mut MaintainState,
    metrics: &MaintainPoolMetrics,
    pool_info: &BlockInfo,
    old: std::sync::Arc<smartnet_execution_types::Chain<N>>,
    new: std::sync::Arc<smartnet_execution_types::Chain<N>>,
) where
    N: NodePrimitives,
    C: StateProviderFactory
        + BlockReaderIdExt<Header = N::BlockHeader>
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = N::BlockHeader> + EthereumHardforks>
        + Clone
        + 'static,
    P: TransactionPoolExt<Transaction: PoolTransaction<Consensus = N::SignedTx>, Block = N::Block>
        + 'static,
{
                let (old_blocks, old_state) = old.inner();
                let (new_blocks, new_state) = new.inner();
                let new_tip = new_blocks.tip();
                let new_first = new_blocks.first();
                let old_first = old_blocks.first();

                // check if the reorg is not canonical with the pool's block
                if !(old_first.parent_hash() == pool_info.last_seen_block_hash ||
                    new_first.parent_hash() == pool_info.last_seen_block_hash)
                {
                    // the new block points to a higher block than the oldest block in the old chain
                    mstate.maintained_state = MaintainedPoolState::Drifted;
                }

                let chain_spec = client.chain_spec();

                // fees for the next block: `new_tip+1`
                let pending_block_base_fee = chain_spec
                    .next_block_base_fee(new_tip.header(), new_tip.timestamp())
                    .unwrap_or_default();
                let pending_block_blob_fee = new_tip.header().maybe_next_block_blob_fee(
                    chain_spec.blob_params_at_timestamp(new_tip.timestamp()),
                );

                // we know all changed account in the new chain
                let new_changed_accounts: HashSet<_> =
                    new_state.changed_accounts().map(ChangedAccountEntry).collect();

                // find all accounts that were changed in the old chain but _not_ in the new chain
                let missing_changed_acc = old_state
                    .accounts_iter()
                    .map(|(a, _)| a)
                    .filter(|addr| !new_changed_accounts.contains(addr));

                // for these we need to fetch the nonce+balance from the db at the new tip
                let mut changed_accounts =
                    match load_accounts(client.clone(), new_tip.hash(), missing_changed_acc) {
                        Ok(LoadedAccounts { accounts, failed_to_load }) => {
                            // extend accounts we failed to load from database
                            mstate.dirty_addresses.extend(failed_to_load);

                            accounts
                        }
                        Err(err) => {
                            let (addresses, err) = *err;
                            debug!(
                                target: "txpool",
                                %err,
                                "failed to load missing changed accounts at new tip: {:?}",
                                new_tip.hash()
                            );
                            mstate.dirty_addresses.extend(addresses);
                            vec![]
                        }
                    };

                // also include all accounts from new chain
                // we can use extend here because they are unique
                changed_accounts.extend(new_changed_accounts.into_iter().map(|entry| entry.0));

                // all transactions mined in the new chain
                let new_mined_transactions: HashSet<_> = new_blocks.transaction_hashes().collect();

                // update the pool then re-inject the pruned transactions
                // find all transactions that were mined in the old chain but not in the new chain
                let pruned_old_transactions = old_blocks
                    .transactions_ecrecovered()
                    .filter(|tx| !new_mined_transactions.contains(tx.tx_hash()))
                    .filter_map(|tx| {
                        if tx.is_eip4844() {
                            // reorged blobs no longer include the blob, which is necessary for
                            // validating the transaction. Even though the transaction could have
                            // been validated previously, we still need the blob in order to
                            // accurately set the transaction's
                            // encoded-length which is propagated over the network.
                            pool.get_blob(*tx.tx_hash())
                                .ok()
                                .flatten()
                                .map(Arc::unwrap_or_clone)
                                .and_then(|sidecar| {
                                    <P as TransactionPool>::Transaction::try_from_eip4844(
                                        tx, sidecar,
                                    )
                                })
                        } else {
                            <P as TransactionPool>::Transaction::try_from_consensus(tx).ok()
                        }
                    })
                    .collect::<Vec<_>>();

                // update the pool first
                let update = CanonicalStateUpdate {
                    new_tip: new_tip.sealed_block(),
                    pending_block_base_fee,
                    pending_block_blob_fee,
                    changed_accounts,
                    // all transactions mined in the new chain need to be removed from the pool
                    mined_transactions: new_blocks.transaction_hashes().collect(),
                    update_kind: PoolUpdateKind::Reorg,
                };
                pool.on_canonical_state_change(update);

                // all transactions that were mined in the old chain but not in the new chain need
                // to be re-injected
                //
                // Note: we no longer know if the tx was local or external
                // Because the transactions are not finalized, the corresponding blobs are still in
                // blob store (if we previously received them from the network)
                let reinjection_results =
                    pool.add_external_transactions(pruned_old_transactions).await;

                let mut reinserted = 0usize;
                let mut rejected = 0usize;
                for result in reinjection_results {
                    match result {
                        Ok(_) => reinserted += 1,
                        Err(err) => {
                            rejected += 1;
                            warn!(target: "txpool", hash = %err.hash, %err, "failed to reinsert transaction after reorg");
                        }
                    }
                }
                // The existing counter tracks successful reinsertions only; there is no
                // rejection counter, so failures are reported by the log line above.
                metrics.inc_reinserted_transactions(reinserted);
                debug!(target: "txpool", reinserted, rejected, "re-injected transactions after reorg");

                // keep track of new mined blob transactions
                mstate.blob_store_tracker.add_new_chain_blocks(&new_blocks);

}

/// Canonical-event phase (commit): depth-gated pool update for the new chain segment,
/// clear dirty flags for the accounts it changed, track mined blobs, and start the
/// Osaka blob-conversion task when the transition is imminent.
fn handle_canonical_commit<N, C, P>(
    client: &C,
    pool: &P,
    mstate: &mut MaintainState,
    pool_info: &BlockInfo,
    max_update_depth: u64,
    task_spawner: &Runtime,
    new: std::sync::Arc<smartnet_execution_types::Chain<N>>,
) where
    N: NodePrimitives,
    C: StateProviderFactory
        + BlockReaderIdExt<Header = N::BlockHeader>
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = N::BlockHeader> + EthereumHardforks>
        + Clone
        + 'static,
    P: TransactionPoolExt<Transaction: PoolTransaction<Consensus = N::SignedTx>, Block = N::Block>
        + 'static,
{
                let (blocks, state) = new.inner();
                let tip = blocks.tip();
                let chain_spec = client.chain_spec();

                // fees for the next block: `tip+1`
                let pending_block_base_fee = chain_spec
                    .next_block_base_fee(tip.header(), tip.timestamp())
                    .unwrap_or_default();
                let pending_block_blob_fee = tip.header().maybe_next_block_blob_fee(
                    chain_spec.blob_params_at_timestamp(tip.timestamp()),
                );

                let first_block = blocks.first();
                trace!(
                    target: "txpool",
                    first = first_block.number(),
                    tip = tip.number(),
                    pool_block = pool_info.last_seen_block_number,
                    "update pool on new commit"
                );

                // check if the depth is too large and should be skipped, this could happen after
                // initial sync or long re-sync
                let depth = tip.number().abs_diff(pool_info.last_seen_block_number);
                if depth > max_update_depth {
                    mstate.maintained_state = MaintainedPoolState::Drifted;
                    debug!(target: "txpool", ?depth, "skipping deep canonical update");
                    let info = BlockInfo {
                        block_gas_limit: tip.header().gas_limit(),
                        last_seen_block_hash: tip.hash(),
                        last_seen_block_number: tip.number(),
                        pending_basefee: pending_block_base_fee,
                        pending_blob_fee: pending_block_blob_fee,
                    };
                    pool.set_block_info(info);

                    // keep track of mined blob transactions
                    mstate.blob_store_tracker.add_new_chain_blocks(&blocks);

                    return;
                }

                let mut changed_accounts = Vec::with_capacity(state.state().len());
                for acc in state.changed_accounts() {
                    // we can always clear the dirty flag for this account
                    mstate.dirty_addresses.remove(&acc.address);
                    changed_accounts.push(acc);
                }

                let mined_transactions = blocks.transaction_hashes().collect();

                // check if the range of the commit is canonical with the pool's block
                if first_block.parent_hash() != pool_info.last_seen_block_hash {
                    // we received a new canonical chain commit but the commit is not canonical with
                    // the pool's block, this could happen after initial sync or
                    // long re-sync
                    mstate.maintained_state = MaintainedPoolState::Drifted;
                }

                // Canonical update
                let update = CanonicalStateUpdate {
                    new_tip: tip.sealed_block(),
                    pending_block_base_fee,
                    pending_block_blob_fee,
                    changed_accounts,
                    mined_transactions,
                    update_kind: PoolUpdateKind::Commit,
                };
                pool.on_canonical_state_change(update);

                // keep track of mined blob transactions
                mstate.blob_store_tracker.add_new_chain_blocks(&blocks);

                // If Osaka activates in 2 slots we need to convert blobs to new format.
                if !chain_spec.is_osaka_active_at_timestamp(tip.timestamp()) &&
                    !chain_spec.is_osaka_active_at_timestamp(tip.timestamp().saturating_add(12)) &&
                    chain_spec.is_osaka_active_at_timestamp(tip.timestamp().saturating_add(24))
                {
                    let pool = pool.clone();
                    let spawner = task_spawner.clone();
                    let client = client.clone();
                    task_spawner.spawn_task(async move {
                        // Start converting not eaerlier than 4 seconds into current slot to ensure
                        // that our pool only contains valid transactions for the next block (as
                        // it's not Osaka yet).
                        tokio::time::sleep(Duration::from_secs(4)).await;

                        let mut interval = tokio::time::interval(Duration::from_secs(1));
                        loop {
                            // Loop and replace blob transactions until we reach Osaka transition
                            // block after which no legacy blobs are going to be accepted.
                            let last_iteration =
                                client.latest_header().ok().flatten().is_none_or(|header| {
                                    client
                                        .chain_spec()
                                        .is_osaka_active_at_timestamp(header.timestamp())
                                });

                            let AllPoolTransactions { pending, queued } = pool.all_transactions();
                            for tx in pending.into_iter().chain(queued).filter(|tx| tx.is_eip4844())
                            {
                                let tx_hash = *tx.hash();

                                // Fetch sidecar from the pool
                                let Ok(Some(sidecar)) = pool.get_blob(tx_hash) else {
                                    continue;
                                };
                                // Ensure it is a legacy blob
                                if !sidecar.is_eip4844() {
                                    continue;
                                }
                                // Remove transaction and sidecar from the pool, both are in memory
                                // now
                                let Some(tx) = pool.remove_transactions(vec![tx_hash]).pop() else {
                                    continue;
                                };
                                pool.delete_blob(tx_hash);

                                let BlobTransactionSidecarVariant::Eip4844(sidecar) =
                                    Arc::unwrap_or_clone(sidecar)
                                else {
                                    continue;
                                };

                                let converter = BlobSidecarConverter::new();
                                let pool = pool.clone();
                                spawner.spawn_task(async move {
                                    // Convert sidecar to EIP-7594 format
                                    let Some(sidecar) = converter.convert(sidecar).await else {
                                        return;
                                    };

                                    // Re-insert transaction with the new sidecar
                                    let origin = tx.origin;
                                    let Some(tx) = EthPoolTransaction::try_from_eip4844(
                                        tx.to_consensus(),
                                        sidecar.into(),
                                    ) else {
                                        return;
                                    };
                                    let _ = pool.add_transaction(origin, tx).await;
                                });
                            }

                            if last_iteration {
                                break;
                            }

                            interval.tick().await;
                        }
                    });
                }

}

struct FinalizedBlockTracker {
    last_finalized_block: Option<BlockNumber>,
}

impl FinalizedBlockTracker {
    const fn new(last_finalized_block: Option<BlockNumber>) -> Self {
        Self { last_finalized_block }
    }

    /// Updates the tracked finalized block and returns the new finalized block if it changed
    fn update(&mut self, finalized_block: Option<BlockNumber>) -> Option<BlockNumber> {
        let finalized = finalized_block?;
        self.last_finalized_block.is_none_or(|last| last < finalized).then(|| {
            self.last_finalized_block = Some(finalized);
            finalized
        })
    }
}

/// Keeps track of the pool's state, whether the accounts in the pool are in sync with the actual
/// state.
#[derive(Debug, PartialEq, Eq)]
enum MaintainedPoolState {
    /// Pool is assumed to be in sync with the current state
    InSync,
    /// Pool could be out of sync with the state
    Drifted,
}

impl MaintainedPoolState {
    /// Returns `true` if the pool is assumed to be out of sync with the current state.
    #[inline]
    const fn is_drifted(&self) -> bool {
        matches!(self, Self::Drifted)
    }
}

/// A unique [`ChangedAccount`] identified by its address that can be used for deduplication
#[derive(Eq)]
struct ChangedAccountEntry(ChangedAccount);

impl PartialEq for ChangedAccountEntry {
    fn eq(&self, other: &Self) -> bool {
        self.0.address == other.0.address
    }
}

impl Hash for ChangedAccountEntry {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.address.hash(state);
    }
}

impl Borrow<Address> for ChangedAccountEntry {
    fn borrow(&self) -> &Address {
        &self.0.address
    }
}

#[derive(Default)]
struct LoadedAccounts {
    /// All accounts that were loaded
    accounts: Vec<ChangedAccount>,
    /// All accounts that failed to load
    failed_to_load: Vec<Address>,
}

/// Loads all accounts at the given state
///
/// Returns an error with all given addresses if the state is not available.
///
/// Note: this expects _unique_ addresses
fn load_accounts<Client, I>(
    client: Client,
    at: BlockHash,
    addresses: I,
) -> Result<LoadedAccounts, Box<(AddressSet, ProviderError)>>
where
    I: IntoIterator<Item = Address>,
    Client: StateProviderFactory,
{
    let addresses = addresses.into_iter();
    let mut res = LoadedAccounts::default();
    let state = match client.history_by_block_hash(at) {
        Ok(state) => state,
        Err(err) => return Err(Box::new((addresses.collect(), err))),
    };
    for addr in addresses {
        if let Ok(maybe_acc) = state.basic_account(&addr) {
            let acc = maybe_acc
                .map(|acc| ChangedAccount { address: addr, nonce: acc.nonce, balance: acc.balance })
                .unwrap_or_else(|| ChangedAccount::empty(addr));
            res.accounts.push(acc)
        } else {
            // failed to load account.
            res.failed_to_load.push(addr);
        }
    }
    Ok(res)
}

/// Loads transactions from a file, decodes them from the JSON or RLP format, and
/// inserts them into the transaction pool on node boot up.
/// The file is removed after the transactions have been successfully processed.
async fn load_and_reinsert_transactions<P>(
    pool: P,
    file_path: &Path,
    queued_ttl: Duration,
    pending_ttl: Option<Duration>,
) -> Result<(), TransactionsBackupError>
where
    P: TransactionPool<Transaction: PoolTransaction<Consensus: SignedTransaction>>,
{
    if !file_path.exists() {
        return Ok(());
    }

    // The backup format has no per-tx insertion timestamp, so the file's mtime is the only
    // age signal available here. Use the shortest TTL maintenance applies: a backup older
    // than that holds txs which would already have been swept had the node stayed up.
    let backup_age = smartnet_fs_util::metadata(file_path)?
        .modified()
        .map_err(|err| FsPathError::metadata(err, file_path))?
        .elapsed()
        .unwrap_or_default();
    let max_age = queued_ttl.min(pending_ttl.unwrap_or(queued_ttl));
    if backup_age > max_age {
        info!(target: "txpool", txs_file=?file_path, ?backup_age, ?max_age, "skipping stale transaction backup");
        smartnet_fs_util::remove_file(file_path)?;
        return Ok(());
    }

    debug!(target: "txpool", txs_file =?file_path, "Check local persistent storage for saved transactions");
    let data = smartnet_fs_util::read(file_path)?;

    if data.is_empty() {
        return Ok(());
    }

    let pool_transactions: Vec<(TransactionOrigin, <P as TransactionPool>::Transaction)> =
        if let Ok(tx_backups) = serde_json::from_slice::<Vec<TxBackup>>(&data) {
            tx_backups
                .into_iter()
                .filter_map(|backup| {
                    let tx_signed =
                        <P::Transaction as PoolTransaction>::Consensus::decode_2718_exact(
                            backup.rlp.as_ref(),
                        )
                        .ok()?;
                    let recovered = tx_signed.try_into_recovered().ok()?;
                    let pool_tx =
                        <P::Transaction as PoolTransaction>::try_from_consensus(recovered).ok()?;

                    Some((backup.origin, pool_tx))
                })
                .collect()
        } else {
            let txs_signed: Vec<<P::Transaction as PoolTransaction>::Consensus> =
                alloy_rlp::Decodable::decode(&mut data.as_slice())?;

            txs_signed
                .into_iter()
                .filter_map(|tx| tx.try_into_recovered().ok())
                .filter_map(|tx| {
                    <P::Transaction as PoolTransaction>::try_from_consensus(tx)
                        .ok()
                        .map(|pool_tx| (TransactionOrigin::Local, pool_tx))
                })
                .collect()
        };

    let inserted = futures_util::future::join_all(
        pool_transactions.into_iter().map(|(origin, tx)| pool.add_transaction(origin, tx)),
    )
    .await;

    info!(target: "txpool", txs_file =?file_path, num_txs=%inserted.len(), "Successfully reinserted local transactions from file");
    smartnet_fs_util::remove_file(file_path)?;
    Ok(())
}

fn save_local_txs_backup<P>(pool: P, file_path: &Path)
where
    P: TransactionPool<Transaction: PoolTransaction<Consensus: Encodable>>,
{
    let local_transactions = pool.get_local_transactions();
    if local_transactions.is_empty() {
        trace!(target: "txpool", "no local transactions to save");
        return;
    }

    let local_transactions = local_transactions
        .into_iter()
        .map(|tx| {
            let consensus_tx = tx.to_consensus().into_inner();
            let rlp_data = consensus_tx.encoded_2718();

            TxBackup { rlp: rlp_data.into(), origin: tx.origin }
        })
        .collect::<Vec<_>>();

    let json_data = match serde_json::to_string(&local_transactions) {
        Ok(data) => data,
        Err(err) => {
            warn!(target: "txpool", %err, txs_file=?file_path, "failed to serialize local transactions to json");
            return;
        }
    };

    info!(target: "txpool", txs_file =?file_path, num_txs=%local_transactions.len(), "Saving current local transactions");
    let parent_dir = file_path.parent().map(std::fs::create_dir_all).transpose();

    match parent_dir.map(|_| smartnet_fs_util::write(file_path, json_data)) {
        Ok(_) => {
            info!(target: "txpool", txs_file=?file_path, "Wrote local transactions to file");
        }
        Err(err) => {
            warn!(target: "txpool", %err, txs_file=?file_path, "Failed to write local transactions to file");
        }
    }
}

/// A transaction backup that is saved as json to a file for
/// reinsertion into the pool
#[derive(Debug, Deserialize, Serialize)]
pub struct TxBackup {
    /// Encoded transaction
    pub rlp: Bytes,
    /// The origin of the transaction
    pub origin: TransactionOrigin,
}

/// Errors possible during txs backup load and decode
#[derive(thiserror::Error, Debug)]
pub enum TransactionsBackupError {
    /// Error during RLP decoding of transactions
    #[error("failed to apply transactions backup. Encountered RLP decode error: {0}")]
    Decode(#[from] alloy_rlp::Error),
    /// Error during json decoding of transactions
    #[error("failed to apply transactions backup. Encountered JSON decode error: {0}")]
    Json(#[from] serde_json::Error),
    /// Error during file upload
    #[error("failed to apply transactions backup. Encountered file error: {0}")]
    FsPath(#[from] FsPathError),
    /// Error adding transactions to the transaction pool
    #[error("failed to insert transactions to the transactions pool. Encountered pool error: {0}")]
    Pool(#[from] PoolError),
}

/// Task which manages saving local transactions to the persistent file in case of shutdown.
/// Reloads the transactions from the file on the boot up and inserts them into the pool.
pub async fn backup_local_transactions_task<P>(
    shutdown: smartnet_tasks::shutdown::GracefulShutdown,
    pool: P,
    config: LocalTransactionBackupConfig,
) where
    P: TransactionPool<Transaction: PoolTransaction<Consensus: SignedTransaction>> + Clone,
{
    let Some(transactions_path) = config.transactions_path else {
        // nothing to do
        return;
    };

    if let Err(err) = load_and_reinsert_transactions(
        pool.clone(),
        &transactions_path,
        config.queued_ttl,
        config.pending_ttl,
    )
    .await
    {
        error!(target: "txpool", "{}", err)
    }

    let graceful_guard = shutdown.await;

    // write transactions to disk
    save_local_txs_backup(pool, &transactions_path);

    drop(graceful_guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        blobstore::InMemoryBlobStore, validate::EthTransactionValidatorBuilder,
        CoinbaseTipOrdering, EthPooledTransaction, Pool, TransactionOrigin,
    };
    use alloy_eips::eip2718::Decodable2718;
    use alloy_primitives::{hex, U256};
    use smartnet_ethereum_primitives::PooledTransactionVariant;
    use smartnet_evm_ethereum::EthEvmConfig;
    use smartnet_fs_util as fs;
    use smartnet_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use smartnet_tasks::Runtime;

    #[test]
    fn changed_acc_entry() {
        let changed_acc = ChangedAccountEntry(ChangedAccount::empty(Address::random()));
        let mut copy = changed_acc.0;
        copy.nonce = 10;
        assert!(changed_acc.eq(&ChangedAccountEntry(copy)));
    }

    const EXTENSION: &str = "json";
    const FILENAME: &str = "test_transactions_backup";

    #[tokio::test(flavor = "multi_thread")]
    async fn test_save_local_txs_backup() {
        let temp_dir = tempfile::tempdir().unwrap();
        let transactions_path = temp_dir.path().join(FILENAME).with_extension(EXTENSION);
        let tx_bytes = hex!(
            "02f87201830655c2808505ef61f08482565f94388c818ca8b9251b393131c08a736a67ccb192978801049e39c4b5b1f580c001a01764ace353514e8abdfb92446de356b260e3c1225b73fc4c8876a6258d12a129a04f02294aa61ca7676061cd99f29275491218b4754b46a0248e5e42bc5091f507"
        );
        let tx = PooledTransactionVariant::decode_2718(&mut &tx_bytes[..]).unwrap();
        // The fixture transaction is an Ethereum mainnet (chain id 1) signed transaction;
        // the validator's chain id is derived from the provider's chain spec.
        let provider = MockEthProvider::default()
            .with_chain_spec(
                smartnet_chainspec::ChainSpecBuilder::mainnet()
                    .chain(smartnet_chainspec::Chain::from(1u64))
                    .build(),
            )
            .with_genesis_block();
        let transaction = EthPooledTransaction::from_pooled(tx.try_into_recovered().unwrap());
        let tx_to_cmp = transaction.clone();
        let sender = hex!("1f9090aaE28b8a3dCeaDf281B0F12828e676c326").into();
        provider.add_account(sender, ExtendedAccount::new(42, U256::MAX));
        let blob_store = InMemoryBlobStore::default();
        let validator = EthTransactionValidatorBuilder::new(provider, EthEvmConfig::mainnet())
            .build(blob_store.clone());

        let txpool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            blob_store.clone(),
            Default::default(),
        );

        txpool.add_transaction(TransactionOrigin::Local, transaction.clone()).await.unwrap();

        let rt = Runtime::test();
        let config = LocalTransactionBackupConfig::with_local_txs_backup(transactions_path.clone());
        rt.spawn_critical_with_graceful_shutdown_signal("test task", |shutdown| {
            backup_local_transactions_task(shutdown, txpool.clone(), config)
        });

        let mut txns = txpool.get_local_transactions();
        let tx_on_finish = txns.pop().expect("there should be 1 transaction");

        assert_eq!(*tx_to_cmp.hash(), *tx_on_finish.hash());

        rt.graceful_shutdown();

        let data = fs::read(transactions_path).unwrap();

        let txs: Vec<TxBackup> = serde_json::from_slice::<Vec<TxBackup>>(&data).unwrap();
        assert_eq!(txs.len(), 1);

        temp_dir.close().unwrap();
    }

    #[test]
    fn test_update_with_higher_finalized_block() {
        let mut tracker = FinalizedBlockTracker::new(Some(10));
        assert_eq!(tracker.update(Some(15)), Some(15));
        assert_eq!(tracker.last_finalized_block, Some(15));
    }

    #[test]
    fn test_update_with_lower_finalized_block() {
        let mut tracker = FinalizedBlockTracker::new(Some(20));
        assert_eq!(tracker.update(Some(15)), None);
        // finalized block should NOT go backwards
        assert_eq!(tracker.last_finalized_block, Some(20));
    }

    #[test]
    fn test_update_with_equal_finalized_block() {
        let mut tracker = FinalizedBlockTracker::new(Some(20));
        assert_eq!(tracker.update(Some(20)), None);
        assert_eq!(tracker.last_finalized_block, Some(20));
    }

    #[test]
    fn test_update_with_no_last_finalized_block() {
        let mut tracker = FinalizedBlockTracker::new(None);
        assert_eq!(tracker.update(Some(10)), Some(10));
        assert_eq!(tracker.last_finalized_block, Some(10));
    }

    #[test]
    fn test_update_with_no_new_finalized_block() {
        let mut tracker = FinalizedBlockTracker::new(Some(10));
        assert_eq!(tracker.update(None), None);
        assert_eq!(tracker.last_finalized_block, Some(10));
    }

    #[test]
    fn test_update_with_no_finalized_blocks() {
        let mut tracker = FinalizedBlockTracker::new(None);
        assert_eq!(tracker.update(None), None);
        assert_eq!(tracker.last_finalized_block, None);
    }
}

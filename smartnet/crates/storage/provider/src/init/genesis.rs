//! SmartNet genesis initialization utility functions.

use crate::{
    errors::provider::ProviderResult, keys, log_calibri_lite_storage_profile, AuxDbProviderFactory,
    BlockHashReader, BlockNumReader, BundleStateInit, ChainSpecProvider, DBProvider,
    DatabaseProviderFactory, ExecutionOutcome, HashingWriter, HeaderProvider, HistoryWriter,
    MetadataProvider, MetadataWriter, NodePrimitivesProvider, OriginalValuesKnown, ProviderError,
    RevertsInit, StageCheckpointReader, StageCheckpointWriter, StateWriteConfig, StateWriter,
    StoragePath, StorageSettings, StorageSettingsCache, TrieWriter,
};
use alloy_consensus::BlockHeader;
use std::path::Path;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{
    keccak256,
    map::{AddressMap, B256Map, HashMap},
    Address, B256, U256,
};
use smartnet_chainspec::EthChainSpec;
use smartnet_codecs::Compact;
use smartnet_config::config::EtlConfig;
use smartnet_db_api::{
    cursor::{DbCursorRW, DbDupCursorRW},
    models::{
        storage_sharded_key::StorageShardedKey, AccountBeforeTx, BlockNumberAddress, IntegerList,
        ShardedKey,
    },
    tables,
    transaction::{DbTx, DbTxMut},
    Database, DatabaseError,
};
use crate::etl::Collector;
use smartnet_trie::StateRootError;
use smartnet_primitives_traits::{
    Account, Bytecode, GotExpected, NodePrimitives, SealedHeader, StorageEntry,
};
use smartnet_stages_types::{StageCheckpoint, StageId};
use smartnet_trie::{
    prefix_set::TriePrefixSets, IntermediateStateRootState, StateRoot as StateRootComputer,
    StateRootProgress,
};
use smartnet_trie_db::DatabaseStateRoot;

type DbStateRoot<'a, TX, A> = StateRootComputer<
    smartnet_trie_db::DatabaseTrieCursorFactory<&'a TX, A>,
    smartnet_trie_db::DatabaseHashedCursorFactory<&'a TX>,
>;

use serde::{Deserialize, Serialize};
use std::io::BufRead;
use tracing::{debug, error, info, trace, warn};

pub use super::history::{
    insert_account_history, insert_genesis_account_history, insert_genesis_history,
    insert_genesis_storage_history, insert_history, insert_storage_history,
};

/// Default soft limit for number of bytes to read from state dump file, before inserting into
/// database.
///
/// Default is 1 GB.
pub const DEFAULT_SOFT_LIMIT_BYTE_LEN_ACCOUNTS_CHUNK: usize = 1_000_000_000;

/// Soft limit for the number of flushed updates after which to log progress summary.
const SOFT_LIMIT_COUNT_FLUSHED_UPDATES: usize = 1_000_000;

/// Max number of storage "units" (1 per account + 1 per storage slot) before committing
/// the current transaction and opening a new one. This bounds dirty page accumulation
/// and prevents OOM on large state imports.
const STORAGE_COMMIT_THRESHOLD: usize = 500_000;

/// Storage initialization error type.
#[derive(Debug, thiserror::Error, Clone)]
pub enum InitStorageError {
    /// Genesis header found on static files but the database is empty.
    #[error(
        "static files found, but the database is uninitialized. If attempting to re-syncing, delete both."
    )]
    UninitializedDatabase,
    /// An existing genesis block was found in the database, and its hash did not match the hash of
    /// the chainspec.
    #[error(
        "genesis hash in the storage does not match the specified chainspec: chainspec is {chainspec_hash}, database is {storage_hash}"
    )]
    GenesisHashMismatch {
        /// Expected genesis hash.
        chainspec_hash: B256,
        /// Actual genesis hash.
        storage_hash: B256,
    },
    /// A genesis carrying `private_key` is forbidden: fail explicitly, never strip it silently.
    #[error("genesis must not contain private_key (found at {path})")]
    GenesisContainsPrivateKey {
        /// JSON path of the forbidden key; never its value.
        path: String,
    },
    /// Provider error.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// State root error while computing the state root
    #[error(transparent)]
    StateRootError(#[from] StateRootError),
    /// State root doesn't match the expected one.
    #[error("state root mismatch: {_0}")]
    StateRootMismatch(GotExpected<B256>),
}

impl From<DatabaseError> for InitStorageError {
    fn from(error: DatabaseError) -> Self {
        Self::Provider(ProviderError::Database(error))
    }
}

/// Genesis is never allowed to carry a private key. Both the parsed accounts and the verbatim
/// raw spec are checked: a contradiction is a fatal error, never a silent strip/filter. The
/// error names only the JSON path, never the key value.
fn ensure_no_private_key_in_genesis(
    genesis: &Genesis,
    genesis_raw: Option<&[u8]>,
) -> Result<(), InitStorageError> {
    if let Some((address, _)) =
        genesis.alloc.iter().find(|(_, account)| account.private_key.is_some())
    {
        return Err(InitStorageError::GenesisContainsPrivateKey {
            path: format!("alloc.{address:#x}.private_key"),
        });
    }

    let Some(raw) = genesis_raw else {
        return Ok(());
    };
    let value: serde_json::Value = serde_json::from_slice(raw).map_err(|e| {
        InitStorageError::Provider(ProviderError::Database(DatabaseError::Other(format!(
            "genesis spec does not parse for private_key scan: {e}"
        ))))
    })?;
    if let Some(path) = find_json_key(&value, "private_key", "") {
        return Err(InitStorageError::GenesisContainsPrivateKey { path });
    }
    Ok(())
}

fn find_json_key(value: &serde_json::Value, key: &str, path: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for (name, child) in map {
                let child_path =
                    if path.is_empty() { name.clone() } else { format!("{path}.{name}") };
                if name == key {
                    return Some(child_path);
                }
                if let Some(found) = find_json_key(child, key, &child_path) {
                    return Some(found);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                if let Some(found) = find_json_key(child, key, &format!("{path}[{index}]")) {
                    return Some(found);
                }
            }
        }
        _ => {}
    }
    None
}

/// Write the genesis block if it has not already been written
pub fn init_genesis<PF>(factory: &PF) -> Result<B256, InitStorageError>
where
    PF: DatabaseProviderFactory
        + ChainSpecProvider
        + StageCheckpointReader
        + BlockNumReader
        + MetadataProvider
        + NodePrimitivesProvider
        + StorageSettingsCache,
    PF::Provider: StoragePath,
    PF::ProviderRW: StageCheckpointWriter
        + HistoryWriter
        + HeaderProvider
        + HashingWriter
        + StateWriter
        + TrieWriter
        + MetadataWriter
        + ChainSpecProvider
        + StorageSettingsCache
        + AuxDbProviderFactory
        + NodePrimitivesProvider<Primitives = PF::Primitives>
        + AsRef<PF::ProviderRW>,
    PF::ChainSpec: EthChainSpec<Header = <PF::Primitives as NodePrimitives>::BlockHeader>,
{
    init_genesis_with_settings(factory, StorageSettings::base(), None)
}

/// Write the genesis block if it has not already been written with [`StorageSettings`].
///
/// `genesis_raw`: the verbatim bytes of the genesis file the operator fed to `init` (stored
/// byte-for-byte in the `GENESIS_SPEC` row — the database keeps the network's self-declared
/// record without losing any key the parser does not read). A `private_key` anywhere in that
/// record is rejected before any write. `None` when the spec was built programmatically.
pub fn init_genesis_with_settings<PF>(
    factory: &PF,
    genesis_storage_settings: StorageSettings,
    genesis_raw: Option<Vec<u8>>,
) -> Result<B256, InitStorageError>
where
    PF: DatabaseProviderFactory
        + ChainSpecProvider
        + StageCheckpointReader
        + BlockNumReader
        + MetadataProvider
        + NodePrimitivesProvider
        + StorageSettingsCache,
    PF::Provider: StoragePath,
    PF::ProviderRW: StageCheckpointWriter
        + HistoryWriter
        + HeaderProvider
        + HashingWriter
        + StateWriter
        + TrieWriter
        + MetadataWriter
        + ChainSpecProvider
        + StorageSettingsCache
        + AuxDbProviderFactory
        + NodePrimitivesProvider<Primitives = PF::Primitives>
        + AsRef<PF::ProviderRW>,
    PF::ChainSpec: EthChainSpec<Header = <PF::Primitives as NodePrimitives>::BlockHeader>,
{
    let chain = factory.chain_spec();

    let genesis = chain.genesis();
    ensure_no_private_key_in_genesis(genesis, genesis_raw.as_deref())?;
    let hash = chain.genesis_hash();

    // Get the genesis block number from the chain spec
    let genesis_block_number = chain.genesis_header().number();

    // Check if we already have the genesis header or if we have the wrong one.
    match factory.block_hash(genesis_block_number) {
        Ok(None) => {}
        Ok(Some(block_hash)) => {
            if block_hash == hash {
                // Some users will at times attempt to re-sync from scratch by just deleting the
                // database. Make sure that our database has been written to, and throw error if
                // it's empty.
                if factory.get_stage_checkpoint(StageId::Headers)?.is_none() {
                    error!(target: "smartnet::storage", "Genesis header found on static files, but database is uninitialized.");
                    return Err(InitStorageError::UninitializedDatabase);
                }

                let stored = factory.storage_settings()?.unwrap_or_else(StorageSettings::v1);
                if stored != genesis_storage_settings {
                    warn!(
                        target: "smartnet::storage",
                        ?stored,
                        requested = ?genesis_storage_settings,
                        "Storage settings mismatch detected. Using the stored settings from the existing database."
                    );
                }
                factory.set_storage_settings_cache(stored);

                debug!("Genesis already written, skipping.");
                return Ok(hash);
            }

            return Err(InitStorageError::GenesisHashMismatch {
                chainspec_hash: hash,
                storage_hash: block_hash,
            });
        }
        Err(e) => {
            debug!(?e);
            return Err(e.into());
        }
    }

    debug!("Writing genesis block.");

    // Make sure to set storage settings before anything writes
    factory.set_storage_settings_cache(genesis_storage_settings);
    log_calibri_lite_storage_profile(genesis_storage_settings);

    let alloc = &genesis.alloc;

    // use transaction to insert genesis header
    let provider_rw = factory.database_provider_rw()?;

    // Behaviour reserved only for new nodes should be set in the storage settings.
    provider_rw.write_storage_settings(genesis_storage_settings)?;

    // Network settings taken from the genesis — the chain id and, when the genesis carries one,
    // the validator set. Written once here (the same transaction as the genesis) and read once at
    // bootstrap through the single accessors `get_chain_id()` / `get_validators()`. A genesis
    // with no signer section is valid: the validators row is then simply absent and the boot
    // fails closed (no config fallback). This is generic — it makes no consensus assumption.
    provider_rw.tx_ref().put::<tables::Settings>(
        keys::CHAIN_ID.to_string(),
        chain.chain_id().to_be_bytes().to_vec(),
    )?;
    let validators = validators_from_extra_data(&genesis.extra_data);
    if !validators.is_empty() {
        let json = serde_json::to_vec(&validators)
            .map_err(|e| DatabaseError::Other(format!("validators do not serialize: {e}")))?;
        provider_rw.tx_ref().put::<tables::Settings>(keys::VALIDATORS.to_string(), json)?;
    }
    // The full genesis spec — the EXACT bytes of the genesis file the init run consumed
    // (verbatim: every key it carried, including ones the parser does not read, e.g. a
    // `calibri-lite { period, epoch, quorum }` object) ends up in the database (same
    // transaction). Nothing in the genesis is dropped: it is the network's record of itself.
    // The boot then rebuilds the runtime chainspec by parsing this raw row; a re-serialized
    // fallback (spec built programmatically) is used only when no raw file exists.
    let spec_json = match genesis_raw {
        Some(raw) => raw,
        None => serde_json::to_vec(&genesis)
            .map_err(|e| DatabaseError::Other(format!("genesis spec does not serialize: {e}")))?,
    };
    provider_rw.tx_ref().put::<tables::Settings>(keys::GENESIS_SPEC.to_string(), spec_json)?;
    // The genesis hash is part of the genesis KV set in Fjall — not file-tier payload.
    provider_rw
        .tx_ref()
        .put::<tables::Settings>(keys::GENESIS_HASH.to_string(), hash.0.to_vec())?;

    insert_genesis_hashes(&provider_rw, alloc.iter())?;
    insert_genesis_history(&provider_rw, alloc.iter())?;

    // Insert header
    // Scope the global base to this factory's datadir (the db dir and the file-tier base
    // share the chain ancestor): a foreign datadir's base from a parallel test must never
    // be written to.
    let file_tier_base = smartnet_file_tier::base().filter(|base| {
        factory
            .database_provider_ro()
            .map(|provider| provider.storage_path().starts_with(base))
            .unwrap_or(false)
    });
    insert_genesis_header(&provider_rw, &chain, file_tier_base, &chain.genesis_tx_records())?;

    insert_genesis_state(&provider_rw, alloc.iter())?;

    // compute state root to populate trie tables
    compute_state_root(&provider_rw, None)?;

    // set stage checkpoint to genesis block number for all stages
    let checkpoint = StageCheckpoint::new(genesis_block_number);
    for stage in StageId::ALL {
        provider_rw.save_stage_checkpoint(stage, checkpoint)?;
    }
    provider_rw.commit()?;

    Ok(hash)
}

/// The validator address(es) encoded in a genesis `extraData` signer section: vanity (32 bytes)
/// + one or more 20-byte addresses + a seal (65 bytes). Generic — makes no consensus assumption, so
///   any genesis is accepted; empty when the genesis carries no signer list (not an error).
pub fn validators_from_extra_data(extra_data: &[u8]) -> Vec<Address> {
    const VANITY: usize = 32;
    const SEAL: usize = 65;
    const ADDR: usize = 20;
    if extra_data.len() < VANITY + ADDR + SEAL {
        return Vec::new();
    }
    let body = &extra_data[VANITY..extra_data.len() - SEAL];
    if !body.len().is_multiple_of(ADDR) {
        return Vec::new();
    }
    body.as_chunks::<ADDR>().0.iter().map(|chunk| Address::from_slice(chunk)).collect()
}

/// Reads the network settings written at genesis init from an open database: the chain id and,
/// when the genesis carried one, the validator set. Each is `None` when the genesis did not
/// provide it — the caller then fails the boot closed (no config fallback). This is the read
/// side of the single source of truth ([`keys::CHAIN_ID`] / [`keys::VALIDATORS`] in
/// `tables::Settings`).
pub fn read_network_settings<DB: Database>(
    db: &DB,
) -> Result<(Option<u64>, Option<Vec<Address>>), DatabaseError> {
    let tx = db.tx()?;
    let chain_id = tx
        .get::<tables::Settings>(keys::CHAIN_ID.to_string())?
        .and_then(|b| <[u8; 8]>::try_from(b.as_slice()).ok())
        .map(u64::from_be_bytes);
    let validators = match tx.get::<tables::Settings>(keys::VALIDATORS.to_string())? {
        Some(bytes) => Some(serde_json::from_slice::<Vec<Address>>(&bytes).map_err(|e| {
            DatabaseError::Other(format!("validators row is not a 0x-address array: {e}"))
        })?),
        None => None,
    };
    Ok((chain_id, validators))
}

/// Reads the full genesis spec — the exact `Genesis` the genesis file declared at `smartnet
/// init` — from an open database. `None` when the database was written by an older node build
/// that did not store the spec (fail-closed at boot: wipe the datadir and re-init).
pub fn read_genesis_spec<DB: Database>(db: &DB) -> Result<Option<Genesis>, DatabaseError> {
    let tx = db.tx()?;
    tx.get::<tables::Settings>(keys::GENESIS_SPEC.to_string())?
        .map(|bytes| {
            // Idempotent calibri* normalization: DBs initialized before the defaults
            // landed store unnormalized rows — every boot parse agrees with init.
            let mut value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|e| DatabaseError::Other(format!("genesis spec row is corrupt: {e}")))?;
            smartnet_chainspec::normalize_calibri_genesis_raw(&mut value)
                .map_err(|e| DatabaseError::Other(format!("genesis spec row invalid: {e}")))?;
            serde_json::from_value(value)
                .map_err(|e| DatabaseError::Other(format!("genesis spec row is corrupt: {e}")))
        })
        .transpose()
}

/// Inserts the genesis state into the database.
pub fn insert_genesis_state<'a, 'b, Provider>(
    provider: &Provider,
    alloc: impl Iterator<Item = (&'a Address, &'b GenesisAccount)>,
) -> ProviderResult<()>
where
    Provider: DBProvider<Tx: DbTxMut>
        + HeaderProvider
        + StateWriter
        + ChainSpecProvider
        + AsRef<Provider>,
{
    let genesis_block_number = provider.chain_spec().genesis_header().number();
    insert_state(provider, alloc, genesis_block_number)
}

/// Inserts state at given block into database.
pub fn insert_state<'a, 'b, Provider>(
    provider: &Provider,
    alloc: impl Iterator<Item = (&'a Address, &'b GenesisAccount)>,
    block: u64,
) -> ProviderResult<()>
where
    Provider: DBProvider<Tx: DbTxMut>
        + HeaderProvider
        + StateWriter
        + AsRef<Provider>,
{
    let capacity = alloc.size_hint().1.unwrap_or(0);
    let mut state_init: BundleStateInit =
        AddressMap::with_capacity_and_hasher(capacity, Default::default());
    let mut reverts_init: AddressMap<_> =
        AddressMap::with_capacity_and_hasher(capacity, Default::default());
    let mut contracts: B256Map<Bytecode> =
        B256Map::with_capacity_and_hasher(capacity, Default::default());

    for (address, account) in alloc {
        let bytecode_hash = if let Some(code) = &account.code {
            match Bytecode::new_raw_checked(code.clone()) {
                Ok(bytecode) => {
                    let hash = bytecode.hash_slow();
                    contracts.insert(hash, bytecode);
                    Some(hash)
                }
                Err(err) => {
                    error!(%address, %err, "Failed to decode genesis bytecode.");
                    return Err(DatabaseError::Other(err.to_string()).into());
                }
            }
        } else {
            None
        };

        // get state
        let storage = account
            .storage
            .as_ref()
            .map(|m| {
                m.iter()
                    .map(|(key, value)| {
                        let value = U256::from_be_bytes(value.0);
                        (*key, (U256::ZERO, value))
                    })
                    .collect::<B256Map<_>>()
            })
            .unwrap_or_default();

        reverts_init.insert(
            *address,
            (Some(None), storage.keys().map(|k| StorageEntry::new(*k, U256::ZERO)).collect()),
        );

        state_init.insert(
            *address,
            (
                None,
                Some(Account {
                    nonce: account.nonce.unwrap_or_default(),
                    balance: account.balance,
                    bytecode_hash,
                }),
                storage,
            ),
        );
    }
    let all_reverts_init: RevertsInit = HashMap::from_iter([(block, reverts_init)]);

    let execution_outcome = ExecutionOutcome::new_init(
        state_init,
        all_reverts_init,
        contracts,
        Vec::default(),
        block,
        Vec::new(),
    );

    provider.write_state(
        &execution_outcome,
        OriginalValuesKnown::Yes,
        StateWriteConfig::default(),
    )?;

    trace!(target: "smartnet::cli", "Inserted state");

    Ok(())
}

/// Inserts hashes for the genesis state.
pub fn insert_genesis_hashes<'a, 'b, Provider>(
    provider: &Provider,
    alloc: impl Iterator<Item = (&'a Address, &'b GenesisAccount)> + Clone,
) -> ProviderResult<()>
where
    Provider: DBProvider<Tx: DbTxMut> + HashingWriter,
{
    // insert and hash accounts to hashing table
    let alloc_accounts = alloc.clone().map(|(addr, account)| (*addr, Some(Account::from(account))));
    provider.insert_account_for_hashing(alloc_accounts)?;

    trace!(target: "smartnet::cli", "Inserted account hashes");

    let alloc_storage = alloc.filter_map(|(addr, account)| {
        // only return Some if there is storage
        account.storage.as_ref().map(|storage| {
            (*addr, storage.iter().map(|(&key, &value)| StorageEntry { key, value: value.into() }))
        })
    });
    provider.insert_storage_for_hashing(alloc_storage)?;

    trace!(target: "smartnet::cli", "Inserted storage hashes");

    Ok(())
}

/// Inserts the genesis index rows and, when the file tier is active, the block-0 payload.
///
/// The immutable block payload is never stored in the database — not even genesis. When the
/// file tier is enabled (every production init) block 0 is written to
/// `<base>/blocks/0/00/b.jsonl` via [`crate::storage_engine::save_genesis_payload`], exactly
/// like every later block; only the index rows (`CanonicalChain`, `HeaderNumbers`) go to Fjall.
/// A disabled file tier (tests) therefore has no genesis payload row either.
pub fn insert_genesis_header<Provider, Spec>(
    provider: &Provider,
    chain: &Spec,
    // The file-tier base *scoped to this factory's datadir* (the process-global base only
    // when it contains this datadir; a foreign datadir's base — parallel tests — is None).
    file_tier_base: Option<&'static Path>,
    // The calibri* pre-fund records (GENESIS.md): the alloc entries as real tx listings
    // in block 0 — the state still comes from the alloc, no doubles. Empty for legacy
    // clique genesis (byte-perfect).
    txs: &[smartnet_ethereum_primitives::TransactionSigned],
) -> ProviderResult<()>
where
    Provider: NodePrimitivesProvider + DBProvider<Tx: DbTxMut>,
    Spec: EthChainSpec<Header = <Provider::Primitives as NodePrimitives>::BlockHeader>,
{
    let (header, block_hash) = (chain.genesis_header(), chain.genesis_hash());

    // Get the actual genesis block number from the header
    let genesis_block_number = header.number();

    if let Some(base) = file_tier_base {
        // Production: the payload lives only in the file tier. Write it once (launch may call
        // this on an empty database with a pre-existing file tree); a present record is
        // authoritative and is never duplicated.
        crate::storage_engine::save_genesis_payload(base, header, block_hash, txs)?;
    }

    provider.tx_ref().put::<tables::CanonicalChain>(genesis_block_number, block_hash)?;
    provider.tx_ref().put::<tables::HeaderNumbers>(block_hash, genesis_block_number)?;

    // The records' DB index rows (the tx-counter values 0..N-1): RPC lookups
    // (`eth_getTransactionByHash`) resolve through these, exactly like later blocks.
    for (idx, tx) in txs.iter().enumerate() {
        let tx_num = idx as u64;
        provider
            .tx_ref()
            .put::<tables::TxLocations>(
                tx_num,
                smartnet_db_api::models::StoredTransactionLocation::new(
                    genesis_block_number,
                    idx as u64,
                ),
            )?;
        provider
            .tx_ref()
            .put::<tables::TransactionHashNumbers>(*tx.tx_hash(), tx_num)?;
    }

    Ok(())
}

/// Reads account state from a [`BufRead`] reader and initializes it at the highest block that can
/// be found on database.
///
/// It's similar to [`init_genesis`] but supports importing state too big to fit in memory, and can
/// be set to the highest block present. One practical usecase is to import OP mainnet state at
/// bedrock transition block.
pub fn init_from_state_dump<PF>(
    mut reader: impl BufRead,
    provider_factory: &PF,
    etl_config: EtlConfig,
) -> eyre::Result<B256>
where
    PF: DatabaseProviderFactory<
        ProviderRW: DBProvider<Tx: DbTxMut>
                        + BlockNumReader
                        + BlockHashReader
                        + ChainSpecProvider
                        + StageCheckpointWriter
                        + HistoryWriter
                        + HeaderProvider
                        + HashingWriter
                        + TrieWriter
                        + StateWriter
                        + StorageSettingsCache
                        + AuxDbProviderFactory
                        + NodePrimitivesProvider
                        + AsRef<PF::ProviderRW>,
    >,
{
    if etl_config.file_size == 0 {
        return Err(eyre::eyre!("ETL file size cannot be zero"));
    }

    let (block, hash, expected_state_root) = {
        let provider_rw = provider_factory.database_provider_rw()?;
        let block = provider_rw.last_block_number()?;
        let hash = provider_rw
            .block_hash(block)?
            .ok_or_else(|| eyre::eyre!("Block hash not found for block {}", block))?;
        let header = provider_rw
            .header_by_number(block)?
            .map(|h| SealedHeader::new(h, hash))
            .ok_or_else(|| ProviderError::HeaderNotFound(block.into()))?;
        let state_root = header.state_root();

        debug!(target: "smartnet::cli",
            block,
            chain=%provider_rw.chain_spec().chain(),
            "Initializing state at block"
        );

        (block, hash, state_root)
    };

    // first line can be state root
    let dump_state_root = parse_state_root(&mut reader)?;
    if expected_state_root != dump_state_root {
        error!(target: "smartnet::cli",
            ?dump_state_root,
            ?expected_state_root,
            "State root from state dump does not match state root in current header."
        );
        return Err(InitStorageError::StateRootMismatch(GotExpected {
            got: dump_state_root,
            expected: expected_state_root,
        })
        .into());
    }

    // remaining lines are accounts
    let collector = parse_accounts(&mut reader, etl_config)?;

    // write state to db with chunked commits to avoid OOM
    dump_state(collector, provider_factory, block)?;

    info!(target: "smartnet::cli", "All accounts written to database, starting state root computation (may take some time)");

    // clear trie tables so state root is computed from scratch
    {
        let provider_rw = provider_factory.database_provider_rw()?;
        provider_rw.tx_ref().clear::<tables::AccountsTrie>()?;
        provider_rw.tx_ref().clear::<tables::StoragesTrie>()?;
        provider_rw.commit()?;
    }

    // compute and compare state root
    let computed_state_root = compute_state_root_chunked(provider_factory)?;
    if computed_state_root == expected_state_root {
        info!(target: "smartnet::cli",
            ?computed_state_root,
            "Computed state root matches state root in state dump"
        );
    } else {
        error!(target: "smartnet::cli",
            ?computed_state_root,
            ?expected_state_root,
            "Computed state root does not match state root in state dump"
        );

        return Err(InitStorageError::StateRootMismatch(GotExpected {
            got: computed_state_root,
            expected: expected_state_root,
        })
        .into());
    }

    // insert sync stages for stages that require state
    {
        let provider_rw = provider_factory.database_provider_rw()?;
        for stage in StageId::STATE_REQUIRED {
            provider_rw.save_stage_checkpoint(stage, StageCheckpoint::new(block))?;
        }
        provider_rw.commit()?;
    }

    Ok(hash)
}

/// Parses and returns expected state root.
fn parse_state_root(reader: &mut impl BufRead) -> eyre::Result<B256> {
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let expected_state_root = serde_json::from_str::<StateRoot>(&line)?.root;
    trace!(target: "smartnet::cli",
        root=%expected_state_root,
        "Read state root from file"
    );
    Ok(expected_state_root)
}

/// Parses accounts and pushes them to a [`Collector`].
fn parse_accounts(
    mut reader: impl BufRead,
    etl_config: EtlConfig,
) -> Result<Collector<Address, GenesisAccount>, eyre::Error> {
    let mut line = String::new();
    let mut collector = Collector::new(etl_config.file_size, etl_config.dir);

    loop {
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }

        let GenesisAccountWithAddress { genesis_account, address } = serde_json::from_str(&line)?;
        collector.insert(address, genesis_account)?;

        line.clear();
    }

    Ok(collector)
}

/// Takes a [`Collector`] and writes all accounts directly to database tables.
///
/// This bypasses the higher-level `insert_state`/`insert_genesis_hashes`/`insert_history`
/// functions which build intermediate structures (`BundleStateInit`, `RevertsInit`,
/// `ExecutionOutcome`) that duplicate all storage data 2-3x in memory. For accounts with
/// millions of storage entries this causes OOM.
///
/// Instead, each account is written directly to all required tables using cursor operations,
/// using `append`/`append_dup` for sorted tables where possible (a fast path that skips
/// B-tree traversal). Commits happen every [`STORAGE_COMMIT_THRESHOLD`] storage units to
/// bound dirty page accumulation.
///
/// NOTE: This function is not idempotent. If the process crashes mid-import, the database
/// must be wiped before retrying.
fn dump_state<PF>(
    mut collector: Collector<Address, GenesisAccount>,
    provider_factory: &PF,
    block: u64,
) -> Result<(), eyre::Error>
where
    PF: DatabaseProviderFactory<ProviderRW: DBProvider<Tx: DbTxMut>>,
{
    let accounts_len = collector.len();
    let mut total_accounts: usize = 0;
    let mut storage_units: usize = 0;

    // pre-allocate the history list once — every entry uses the same single-block bitmap
    let history_list = IntegerList::new([block])?;

    // track seen bytecode hashes to avoid re-hashing and re-writing duplicates

    let mut provider_rw = provider_factory.database_provider_rw()?;

    for entry in collector.iter()? {
        let (address_raw, account_raw) = entry?;
        let (address, _) = Address::from_compact(address_raw.as_slice(), address_raw.len());
        let (account, _) = GenesisAccount::from_compact(account_raw.as_slice(), account_raw.len());

        let account_storage_len = account.storage.as_ref().map_or(0, |s| s.len());
        let account_units = 1 + account_storage_len;

        // commit before this account would push us over the threshold
        if storage_units > 0 && storage_units + account_units > STORAGE_COMMIT_THRESHOLD {
            provider_rw.commit()?;
            provider_rw = provider_factory.database_provider_rw()?;
            info!(target: "smartnet::cli",
                total_accounts,
                accounts_len,
                storage_units,
                "Committed chunk"
            );
            storage_units = 0;
        }

        write_account_to_db(
            provider_rw.tx_ref(),
            &address,
            &account,
            block,
            &history_list,
        )?;

        total_accounts += 1;
        storage_units += account_units;

        if total_accounts.is_multiple_of(100_000) {
            info!(target: "smartnet::cli", total_accounts, accounts_len, "Writing accounts...");
        }
    }

    // commit final batch
    provider_rw.commit()?;

    info!(target: "smartnet::cli", total_accounts, "All accounts written to database");

    Ok(())
}

/// Writes a single account and all its storage to every required DB table directly,
/// without building intermediary structures.
///
/// Uses `append_dup` for `DupSort` tables where insertion order matches key order (the ETL
/// collector sorts by address, so `AccountChangeSets`, `PlainStorageState`, and
/// `StorageChangeSets` receive data in sorted order within each account). For `HashedAccounts`
/// and `HashedStorages`, insertion order is unsorted (keccak scrambles address order), so we
/// use `put`/`upsert` which do a full B-tree lookup.
fn write_account_to_db<TX: DbTxMut>(
    tx: &TX,
    address: &Address,
    genesis_account: &GenesisAccount,
    block: u64,
    history_list: &IntegerList,
) -> Result<(), eyre::Error> {
    let bytecode_hash = if let Some(code) = &genesis_account.code {
        let bytecode = Bytecode::new_raw_checked(code.clone())
            .map_err(|e| eyre::eyre!("Invalid bytecode for {address}: {e}"))?;
        let hash = bytecode.hash_slow();
        // Strict store split: bytecode is file-tier only when the tier is enabled
        // (no DB fallback). The file is keyed by the deployed address (operator
        // layout); the `Bytecodes` table is written solely in the tier-disabled test
        // mode, where the table (hash-keyed) is the only bytecode store.
        match crate::storage_engine::write_bytecode(
            smartnet_file_tier::base(),
            address,
            bytecode.0.original_byte_slice(),
        )? {
            crate::storage_engine::BytecodeDestination::FileTier => {}
            crate::storage_engine::BytecodeDestination::DatabaseTable => {
                tx.put::<tables::Bytecodes>(hash, bytecode)?;
            }
        }
        Some(hash)
    } else {
        None
    };

    let account = Account {
        nonce: genesis_account.nonce.unwrap_or_default(),
        balance: genesis_account.balance,
        bytecode_hash,
    };

    let hashed_address = keccak256(address);

    // plain state — sorted by address (ETL order), use append
    tx.put::<tables::PlainAccountState>(*address, account)?;

    // hashed state — unsorted (keccak scrambles order), must use put
    tx.put::<tables::HashedAccounts>(hashed_address, account)?;

    // account changeset — DupSort keyed by block, subkey sorted by address (ETL order)
    let mut acct_cs_cursor = tx.cursor_dup_write::<tables::AccountChangeSets>()?;
    acct_cs_cursor.append_dup(block, AccountBeforeTx { address: *address, info: None })?;

    // account history
    tx.put::<tables::AccountsHistory>(ShardedKey::new(*address, u64::MAX), history_list.clone())?;

    // storage entries
    if let Some(storage) = &genesis_account.storage {
        let mut hashed_storage_cursor = tx.cursor_dup_write::<tables::HashedStorages>()?;
        let mut plain_storage_cursor = tx.cursor_dup_write::<tables::PlainStorageState>()?;
        let mut storage_cs_cursor = tx.cursor_dup_write::<tables::StorageChangeSets>()?;

        // sort storage slots by key so we can use append_dup for plain/changeset tables
        let mut sorted_slots: Vec<_> = storage.iter().collect();
        sorted_slots.sort_unstable_by_key(|(k, _)| *k);

        for &(&key, &value) in &sorted_slots {
            let value_u256 = U256::from_be_bytes(value.0);

            // plain storage — sorted by (address, key), use append_dup
            plain_storage_cursor.append_dup(*address, StorageEntry { key, value: value_u256 })?;

            // hashed storage — unsorted keccak order, use upsert
            let hashed_key = keccak256(key);
            hashed_storage_cursor
                .upsert(hashed_address, &StorageEntry { key: hashed_key, value: value_u256 })?;

            // storage changeset — sorted by (block, address), then by key via append_dup
            storage_cs_cursor.append_dup(
                BlockNumberAddress((block, *address)),
                StorageEntry { key, value: U256::ZERO },
            )?;

            // storage history
            tx.put::<tables::StoragesHistory>(
                StorageShardedKey::new(*address, key, u64::MAX),
                history_list.clone(),
            )?;
        }
    }

    Ok(())
}

/// Computes the state root (from scratch) based on the accounts and storages present in the
/// database.
fn compute_state_root<Provider>(
    provider: &Provider,
    prefix_sets: Option<TriePrefixSets>,
) -> Result<B256, InitStorageError>
where
    Provider: DBProvider<Tx: DbTxMut> + TrieWriter + StorageSettingsCache,
{
    smartnet_trie_db::with_adapter!(provider, |A| {
        compute_state_root_inner::<_, A>(provider, prefix_sets)
    })
}

fn compute_state_root_inner<Provider, A>(
    provider: &Provider,
    prefix_sets: Option<TriePrefixSets>,
) -> Result<B256, InitStorageError>
where
    Provider: DBProvider<Tx: DbTxMut> + TrieWriter + StorageSettingsCache,
    A: smartnet_trie_db::TrieTableAdapter,
{
    trace!(target: "smartnet::cli", "Computing state root");

    let tx = provider.tx_ref();
    let mut intermediate_state: Option<IntermediateStateRootState> = None;
    let mut total_flushed_updates = 0;

    loop {
        let mut state_root =
            DbStateRoot::<_, A>::from_tx(tx).with_intermediate_state(intermediate_state);

        if let Some(sets) = prefix_sets.clone() {
            state_root = state_root.with_prefix_sets(sets);
        }

        match state_root.root_with_progress()? {
            StateRootProgress::Progress(state, _, updates) => {
                let updated_len = provider.write_trie_updates(updates)?;
                total_flushed_updates += updated_len;

                trace!(target: "smartnet::cli",
                    last_account_key = %state.account_root_state.last_hashed_key,
                    updated_len,
                    total_flushed_updates,
                    "Flushing trie updates"
                );

                intermediate_state = Some(*state);

                if total_flushed_updates.is_multiple_of(SOFT_LIMIT_COUNT_FLUSHED_UPDATES) {
                    info!(target: "smartnet::cli",
                        total_flushed_updates,
                        "Flushing trie updates"
                    );
                }
            }
            StateRootProgress::Complete(root, _, updates) => {
                let updated_len = provider.write_trie_updates(updates)?;
                total_flushed_updates += updated_len;

                trace!(target: "smartnet::cli",
                    %root,
                    updated_len,
                    total_flushed_updates,
                    "State root has been computed"
                );

                return Ok(root);
            }
        }
    }
}

/// Computes the state root (from scratch) with periodic commits to free dirty pages.
///
/// Opens a fresh transaction each iteration to release dirty pages, preventing OOM on large
/// states where trie updates accumulate gigabytes of dirty pages.
fn compute_state_root_chunked<PF>(provider_factory: &PF) -> Result<B256, InitStorageError>
where
    PF: DatabaseProviderFactory<
        ProviderRW: DBProvider<Tx: DbTxMut> + TrieWriter + StorageSettingsCache,
    >,
{
    let provider_rw = provider_factory.database_provider_rw().map_err(provider_db_err)?;

    smartnet_trie_db::with_adapter!(&provider_rw, |A| {
        drop(provider_rw);
        compute_state_root_chunked_inner::<PF, A>(provider_factory)
    })
}

fn compute_state_root_chunked_inner<PF, A>(provider_factory: &PF) -> Result<B256, InitStorageError>
where
    PF: DatabaseProviderFactory<
        ProviderRW: DBProvider<Tx: DbTxMut> + TrieWriter + StorageSettingsCache,
    >,
    A: smartnet_trie_db::TrieTableAdapter,
{
    trace!(target: "smartnet::cli", "Computing state root");

    let mut intermediate_state: Option<IntermediateStateRootState> = None;
    let mut total_flushed_updates = 0;

    loop {
        let provider_rw = provider_factory.database_provider_rw().map_err(provider_db_err)?;
        let tx = provider_rw.tx_ref();

        let state_root =
            DbStateRoot::<_, A>::from_tx(tx).with_intermediate_state(intermediate_state.take());

        match state_root.root_with_progress()? {
            StateRootProgress::Progress(state, _, updates) => {
                let updated_len = provider_rw.write_trie_updates(updates)?;
                total_flushed_updates += updated_len;

                info!(target: "smartnet::cli",
                    last_account_key = %state.account_root_state.last_hashed_key,
                    updated_len,
                    total_flushed_updates,
                    "Flushing trie updates (committing to free memory)"
                );

                intermediate_state = Some(*state);
                provider_rw.commit().map_err(provider_db_err)?;
            }
            StateRootProgress::Complete(root, _, updates) => {
                let updated_len = provider_rw.write_trie_updates(updates)?;
                total_flushed_updates += updated_len;

                info!(target: "smartnet::cli",
                    %root,
                    updated_len,
                    total_flushed_updates,
                    "State root computation complete"
                );

                provider_rw.commit().map_err(provider_db_err)?;
                return Ok(root);
            }
        }
    }
}

/// Converts a provider error into an [`InitStorageError`].
fn provider_db_err(e: impl std::fmt::Display) -> InitStorageError {
    InitStorageError::from(StateRootError::Database(DatabaseError::Other(e.to_string())))
}

/// Type to deserialize state root from state dump file.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct StateRoot {
    root: B256,
}

/// An account as in the state dump file. This contains a [`GenesisAccount`] and the account's
/// address.
#[derive(Debug, Serialize, Deserialize)]
struct GenesisAccountWithAddress {
    /// The account's balance, nonce, code, and storage.
    #[serde(flatten)]
    genesis_account: GenesisAccount,
    /// The account's address.
    address: Address,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_utils::{create_test_provider_factory_with_chain_spec, MockNodeTypesWithDB},
        AuxDbProviderFactory, ProviderFactory,
    };
    use alloy_genesis::{Genesis, GenesisAccount};
    use smartnet_chainspec::{dev_chain_spec, Chain, ChainSpec, MAINNET};
    use smartnet_db::DatabaseEnv;
    use smartnet_db_api::{
        cursor::DbCursorRO,
        models::{storage_sharded_key::StorageShardedKey, IntegerList, ShardedKey},
        table::{Table, TableRow},
        transaction::DbTx,
        Database,
    };
    use std::{collections::BTreeMap, sync::Arc};

    fn collect_table_entries<DB, T>(
        tx: &<DB as Database>::TX,
    ) -> Result<Vec<TableRow<T>>, InitStorageError>
    where
        DB: Database,
        T: Table,
    {
        Ok(tx.cursor_read::<T>()?.walk_range(..)?.collect::<Result<Vec<_>, _>>()?)
    }

    #[test]
    fn success_init_genesis_mainnet() {
        let genesis_hash =
            init_genesis(&create_test_provider_factory_with_chain_spec(MAINNET.clone())).unwrap();

        assert_eq!(genesis_hash, MAINNET.genesis_hash());
    }

    #[test]
    fn fail_init_inconsistent_db() {
        let dev = dev_chain_spec();
        let factory = create_test_provider_factory_with_chain_spec(dev.clone());
        let aux_db_provider = factory.aux_db_provider();
        init_genesis(&factory).unwrap();

        // Try to init db with a different genesis block
        let genesis_hash = init_genesis(
            &ProviderFactory::<MockNodeTypesWithDB>::new(
                factory.into_db(),
                MAINNET.clone(),
                Some(aux_db_provider),
                smartnet_tasks::Runtime::test(),
            )
            .unwrap(),
        );

        assert!(matches!(
            genesis_hash.unwrap_err(),
            InitStorageError::GenesisHashMismatch { chainspec_hash, storage_hash }
                if chainspec_hash == MAINNET.genesis_hash() && storage_hash == dev.genesis_hash()
        ))
    }

    #[test]
    fn init_genesis_history() {
        let address_with_balance = Address::with_last_byte(1);
        let address_with_storage = Address::with_last_byte(2);
        let storage_key = B256::with_last_byte(1);
        let chain_spec = Arc::new(ChainSpec {
            chain: Chain::from_id(1),
            genesis: Genesis {
                alloc: BTreeMap::from([
                    (
                        address_with_balance,
                        GenesisAccount { balance: U256::from(1), ..Default::default() },
                    ),
                    (
                        address_with_storage,
                        GenesisAccount {
                            storage: Some(BTreeMap::from([(storage_key, B256::random())])),
                            ..Default::default()
                        },
                    ),
                ]),
                ..Default::default()
            },
            hardforks: Default::default(),
            paris_block_and_final_difficulty: None,
            deposit_contract: None,
            ..Default::default()
        });

        let factory = create_test_provider_factory_with_chain_spec(chain_spec);
        init_genesis_with_settings(&factory, StorageSettings::archive(), None).unwrap();

        let expected_accounts = vec![
            (ShardedKey::new(address_with_balance, u64::MAX), IntegerList::new([0]).unwrap()),
            (ShardedKey::new(address_with_storage, u64::MAX), IntegerList::new([0]).unwrap()),
        ];
        let expected_storages = vec![(
            StorageShardedKey::new(address_with_storage, storage_key, u64::MAX),
            IntegerList::new([0]).unwrap(),
        )];

        let collect_from_db = |factory: &ProviderFactory<MockNodeTypesWithDB>| {
            let provider = factory.provider().unwrap();
            let tx = provider.tx_ref();
            (
                collect_table_entries::<DatabaseEnv, tables::AccountsHistory>(tx).unwrap(),
                collect_table_entries::<DatabaseEnv, tables::StoragesHistory>(tx).unwrap(),
            )
        };

        {
            let (accounts, storages) = collect_from_db(&factory);
            assert_eq!(accounts, expected_accounts);
            assert_eq!(storages, expected_storages);
        }
    }

    #[test]
    fn warn_storage_settings_mismatch() {
        let factory = create_test_provider_factory_with_chain_spec(MAINNET.clone());
        init_genesis_with_settings(&factory, StorageSettings::v1(), None).unwrap();

        // Request different settings - should warn but succeed
        let result =
            init_genesis_with_settings(&factory, StorageSettings::calibri_lite_runtime(), None);

        // Should succeed (warning is logged, not an error)
        assert!(result.is_ok());
    }

    #[test]
    fn allow_same_storage_settings() {
        let factory = create_test_provider_factory_with_chain_spec(MAINNET.clone());
        let settings = StorageSettings::calibri_lite_runtime();
        init_genesis_with_settings(&factory, settings, None).unwrap();

        let result = init_genesis_with_settings(&factory, settings, None);

        assert!(result.is_ok());
    }

    #[test]
    fn reject_genesis_with_private_key_in_alloc() {
        let spec = Arc::new(ChainSpec {
            chain: Chain::from_id(1),
            genesis: Genesis {
                alloc: BTreeMap::from([(
                    Address::with_last_byte(1),
                    GenesisAccount {
                        private_key: Some(B256::with_last_byte(1)),
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
            hardforks: Default::default(),
            paris_block_and_final_difficulty: None,
            deposit_contract: None,
            ..Default::default()
        });
        let factory = create_test_provider_factory_with_chain_spec(spec);

        let err = init_genesis_with_settings(&factory, StorageSettings::v1(), None).unwrap_err();
        assert!(
            matches!(err, InitStorageError::GenesisContainsPrivateKey { .. }),
            "genesis private_key must fail explicitly: {err:?}"
        );
    }

    #[test]
    fn reject_raw_genesis_with_private_key_anywhere() {
        let raw = br#"{"alloc":{"0x0000000000000000000000000000000000000001":{"private_key":"0xdeadbeef"}}}"#;
        let err = ensure_no_private_key_in_genesis(&Genesis::default(), Some(raw)).unwrap_err();
        assert!(
            matches!(err, InitStorageError::GenesisContainsPrivateKey { ref path } if path.contains("private_key")),
            "raw genesis private_key must fail explicitly: {err:?}"
        );
        assert!(!format!("{err}").contains("deadbeef"), "error must not carry the key value");
    }
}

use crate::{
    providers::{AuxDbBuilder, NodeTypesForProvider, ProviderNodeTypes},
    HashingWriter, ProviderFactory, TrieWriter,
};
use alloy_primitives::B256;
use smartnet_chainspec::{ChainSpec, MAINNET};
use smartnet_db::{test_utils::TempDatabase, DatabaseArguments, DatabaseEnv};
use smartnet_errors::ProviderResult;
use smartnet_engine_primitives::ethereum::EthEngineTypes;
use smartnet_node_types::NodeTypesWithDBAdapter;
use smartnet_primitives_traits::{Account, StorageEntry};
use smartnet_trie::StateRoot;
use smartnet_trie_db::DatabaseStateRoot;
use std::sync::Arc;

type DbStateRoot<'a, TX, A> = StateRoot<
    smartnet_trie_db::DatabaseTrieCursorFactory<&'a TX, A>,
    smartnet_trie_db::DatabaseHashedCursorFactory<&'a TX>,
>;

pub mod blocks;
mod mock;
mod noop;

pub use mock::{ExtendedAccount, MockEthProvider};
pub use noop::NoopProvider;
pub use smartnet_chain_state::test_utils::TestCanonStateSubscriptions;

/// Mock [`smartnet_node_types::NodeTypes`] for testing.
pub type MockNodeTypes = smartnet_node_types::AnyNodeTypesWithEngine<
    smartnet_ethereum_primitives::EthPrimitives,
    smartnet_engine_primitives::ethereum::EthEngineTypes,
    smartnet_chainspec::ChainSpec,
    crate::EthStorage,
    EthEngineTypes,
>;

/// Mock [`smartnet_node_types::NodeTypesWithDB`] for testing.
pub type MockNodeTypesWithDB<DB = Arc<TempDatabase<DatabaseEnv>>> =
    NodeTypesWithDBAdapter<MockNodeTypes, DB>;

/// Creates test provider factory with mainnet chain spec.
pub fn create_test_provider_factory() -> ProviderFactory<MockNodeTypesWithDB> {
    create_test_provider_factory_with_chain_spec(MAINNET.clone())
}

/// Creates test provider factory with provided chain spec.
pub fn create_test_provider_factory_with_chain_spec(
    chain_spec: Arc<ChainSpec>,
) -> ProviderFactory<MockNodeTypesWithDB> {
    create_test_provider_factory_with_node_types::<MockNodeTypes>(chain_spec)
}

/// Creates test provider factory with provided chain spec.
pub fn create_test_provider_factory_with_node_types<N: NodeTypesForProvider>(
    chain_spec: Arc<N::ChainSpec>,
) -> ProviderFactory<NodeTypesWithDBAdapter<N, Arc<TempDatabase<DatabaseEnv>>>> {
    create_test_provider_factory_at_with_node_types(
        smartnet_db::test_utils::tempdir_path(),
        chain_spec,
    )
}

/// Same as [`create_test_provider_factory_with_node_types`], but rooted at a caller-chosen
/// `datadir_path`, so a child process can reopen the same database after the parent (or the
/// child itself) was killed — the kill-9 crash gates.
pub fn create_test_provider_factory_at_with_node_types<N: NodeTypesForProvider>(
    datadir_path: std::path::PathBuf,
    chain_spec: Arc<N::ChainSpec>,
) -> ProviderFactory<NodeTypesWithDBAdapter<N, Arc<TempDatabase<DatabaseEnv>>>> {
    // A single directory holds all data dirs (db, aux_db).
    // TempDatabase will clean up the entire directory on drop — a SIGKILLed child never
    // drops, so the parent can reopen what it left behind.
    let aux_db_path = datadir_path.join("aux_db");

    // Create database with the datadir path so TempDatabase cleans up everything on drop
    let db = smartnet_db::test_utils::create_test_rw_db_with_datadir(&datadir_path);

    ProviderFactory::new(
        db,
        chain_spec,
        Some(
            AuxDbBuilder::new(&aux_db_path)
                .with_default_tables()
                .build()
                .expect("failed to create test aux DB provider"),
        ),
        smartnet_tasks::Runtime::test(),
    )
    .expect("failed to create test provider factory")
}

/// [`create_test_provider_factory`] rooted at a caller-chosen datadir (kill-9 crash gates:
/// the child seeds this path and the parent reopens it after the child dies).
pub fn create_test_provider_factory_at(
    datadir_path: impl Into<std::path::PathBuf>,
) -> ProviderFactory<MockNodeTypesWithDB> {
    create_test_provider_factory_at_with_node_types::<MockNodeTypes>(
        datadir_path.into(),
        MAINNET.clone(),
    )
}

/// Creates test provider factory with provided chain spec and custom database arguments.
///
/// Same as [`create_test_provider_factory_with_chain_spec`] but allows overriding the default
/// test database arguments (e.g. to override them for heavy benchmarks).
pub fn create_test_provider_factory_with_chain_spec_and_db_args(
    chain_spec: Arc<ChainSpec>,
    db_args: DatabaseArguments,
) -> ProviderFactory<MockNodeTypesWithDB> {
    let datadir_path = smartnet_db::test_utils::tempdir_path();

    let db_path = datadir_path.join("db");
    let aux_db_path = datadir_path.join("aux_db");

    let db = smartnet_db::init_db(&db_path, db_args).expect("failed to init db");
    let db = Arc::new(TempDatabase::new(db, datadir_path));

    ProviderFactory::new(
        db,
        chain_spec,
        Some(
            AuxDbBuilder::new(&aux_db_path)
                .with_default_tables()
                .build()
                .expect("failed to create test aux DB provider"),
        ),
        smartnet_tasks::Runtime::test(),
    )
    .expect("failed to create test provider factory")
}

/// Inserts the genesis alloc from the provided chain spec into the trie.
pub fn insert_genesis<N: ProviderNodeTypes<ChainSpec = ChainSpec>>(
    provider_factory: &ProviderFactory<N>,
    chain_spec: Arc<N::ChainSpec>,
) -> ProviderResult<B256> {
    let provider = provider_factory.provider_rw()?;

    // Hash accounts and insert them into hashing table.
    let genesis = chain_spec.genesis();
    let alloc_accounts =
        genesis.alloc.iter().map(|(addr, account)| (*addr, Some(Account::from(account))));
    provider.insert_account_for_hashing(alloc_accounts).unwrap();

    let alloc_storage = genesis.alloc.clone().into_iter().filter_map(|(addr, account)| {
        // Only return `Some` if there is storage.
        account.storage.map(|storage| {
            (
                addr,
                storage.into_iter().map(|(key, value)| StorageEntry { key, value: value.into() }),
            )
        })
    });
    provider.insert_storage_for_hashing(alloc_storage)?;

    let (root, updates) = smartnet_trie_db::with_adapter!(provider, |A| {
        DbStateRoot::<_, A>::from_tx(provider.tx_ref()).root_with_updates()?
    });
    provider.write_trie_updates(updates).unwrap();

    provider.commit()?;

    Ok(root)
}

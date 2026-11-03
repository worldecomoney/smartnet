//! Helper builder entrypoint to instantiate a [`ProviderFactory`].

use crate::{
    providers::{AuxDbProvider, NodeTypesForProvider},
    ProviderFactory,
};
use smartnet_db::{open_db_read_only, DatabaseArguments, DatabaseEnv};
use smartnet_node_types::NodeTypesWithDBAdapter;
use std::{
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Helper type to create a [`ProviderFactory`].
///
/// See [`ProviderFactoryBuilder::open_read_only`] for usage examples.
#[derive(Debug)]
pub struct ProviderFactoryBuilder<N> {
    _types: PhantomData<N>,
}

impl<N> ProviderFactoryBuilder<N> {
    /// Maps the [`smartnet_node_types::NodeTypes`] of this builder.
    pub fn types<T>(self) -> ProviderFactoryBuilder<T> {
        ProviderFactoryBuilder::default()
    }

    /// Opens the database with the given chainspec and [`ReadOnlyConfig`].
    ///
    /// # Open a monitored instance
    ///
    /// This is recommended when the new read-only instance is used with an active node.
    ///
    /// ```no_run
    /// use smartnet_chainspec::MAINNET;
    /// use smartnet_provider::providers::{NodeTypesForProvider, ProviderFactoryBuilder};
    ///
    /// fn demo<N: NodeTypesForProvider<ChainSpec = smartnet_chainspec::ChainSpec>>(
    ///     runtime: smartnet_tasks::Runtime,
    /// ) {
    ///     let provider_factory = ProviderFactoryBuilder::<N>::default()
    ///         .open_read_only(MAINNET.clone(), "datadir", runtime)
    ///         .unwrap();
    /// }
    /// ```
    ///
    /// # Open an unmonitored instance
    ///
    /// This is recommended when no changes to the database are expected (e.g. no active node)
    ///
    /// ```no_run
    /// use smartnet_chainspec::MAINNET;
    /// use smartnet_provider::providers::{
    ///     NodeTypesForProvider, ProviderFactoryBuilder, ReadOnlyConfig,
    /// };
    ///
    /// fn demo<N: NodeTypesForProvider<ChainSpec = smartnet_chainspec::ChainSpec>>(
    ///     runtime: smartnet_tasks::Runtime,
    /// ) {
    ///     let provider_factory = ProviderFactoryBuilder::<N>::default()
    ///         .open_read_only(MAINNET.clone(), "datadir", runtime)
    ///         .unwrap();
    /// }
    /// ```
    ///
    /// # Open an instance with disabled read-transaction timeout
    ///
    /// By default, read transactions are automatically terminated after a timeout to prevent
    /// database free list growth. However, if the database is static (no writes occurring), this
    /// safety mechanism can be disabled using
    /// [`ReadOnlyConfig::disable_long_read_transaction_safety`].
    ///
    /// ```no_run
    /// use smartnet_chainspec::MAINNET;
    /// use smartnet_provider::providers::{
    ///     NodeTypesForProvider, ProviderFactoryBuilder, ReadOnlyConfig,
    /// };
    ///
    /// fn demo<N: NodeTypesForProvider<ChainSpec = smartnet_chainspec::ChainSpec>>(
    ///     runtime: smartnet_tasks::Runtime,
    /// ) {
    ///     let provider_factory = ProviderFactoryBuilder::<N>::default()
    ///         .open_read_only(
    ///             MAINNET.clone(),
    ///             ReadOnlyConfig::from_datadir("datadir").disable_long_read_transaction_safety(),
    ///             runtime,
    ///         )
    ///         .unwrap();
    /// }
    /// ```
    pub fn open_read_only(
        self,
        chainspec: Arc<N::ChainSpec>,
        config: impl Into<ReadOnlyConfig>,
        runtime: smartnet_tasks::Runtime,
    ) -> eyre::Result<ProviderFactory<NodeTypesWithDBAdapter<N, DatabaseEnv>>>
    where
        N: NodeTypesForProvider,
    {
        let ReadOnlyConfig { db_dir, db_args, aux_db_dir } = config.into();
        let db = open_db_read_only(db_dir, db_args)?;
        let aux_db_provider = AuxDbProvider::builder(&aux_db_dir)
            .with_default_tables()
            .with_read_only(true)
            .build()?;
        let factory = ProviderFactory::new(db, chainspec, Some(aux_db_provider), runtime)?;
        Ok(factory)
    }
}

impl<N> Default for ProviderFactoryBuilder<N> {
    fn default() -> Self {
        Self { _types: Default::default() }
    }
}

/// Settings for how to open the database and `aux DB`.
///
/// The default derivation from a path assumes the path is the datadir:
/// [`ReadOnlyConfig::from_datadir`]
#[derive(Debug, Clone)]
pub struct ReadOnlyConfig {
    /// The path to the database directory.
    pub db_dir: PathBuf,
    /// How to open the database
    pub db_args: DatabaseArguments,
    /// The path to the `aux DB` directory
    pub aux_db_dir: PathBuf,
}

impl ReadOnlyConfig {
    /// Derives the [`ReadOnlyConfig`] from the datadir.
    ///
    /// By default this assumes the following datadir layout:
    ///
    /// ```text
    ///  -`datadir`
    ///    |__db
    ///    |__aux_db
    /// ```
    pub fn from_datadir(datadir: impl AsRef<Path>) -> Self {
        let datadir = datadir.as_ref();
        Self {
            db_dir: datadir.join("db"),
            db_args: Default::default(),
            aux_db_dir: datadir.join("aux_db"),
        }
    }

    /// No-op kept for API compatibility.
    ///
    /// Fjall does not hold long-lived read transactions, so there is no read-transaction timeout
    /// to disable.
    pub const fn disable_long_read_transaction_safety(self) -> Self {
        self
    }

    /// Derives the [`ReadOnlyConfig`] from the database dir.
    ///
    /// By default this assumes the following datadir layout:
    ///
    /// ```text
    ///    - db
    ///    - aux_db
    /// ```
    ///
    /// # Panics
    ///
    /// If the path does not exist
    pub fn from_db_dir(db_dir: impl AsRef<Path>) -> Self {
        let db_dir = db_dir.as_ref();
        let datadir = std::fs::canonicalize(db_dir).unwrap().parent().unwrap().to_path_buf();
        let aux_db_dir = datadir.join("aux_db");
        Self::from_dirs(db_dir, aux_db_dir)
    }

    /// Creates the config for the given paths.
    pub fn from_dirs(db_dir: impl AsRef<Path>, aux_db_dir: impl AsRef<Path>) -> Self {
        Self {
            db_dir: db_dir.as_ref().into(),
            db_args: Default::default(),
            aux_db_dir: aux_db_dir.as_ref().into(),
        }
    }

    /// Configures the db arguments used when opening the database.
    pub fn with_db_args(mut self, db_args: impl Into<DatabaseArguments>) -> Self {
        self.db_args = db_args.into();
        self
    }

    /// Configures the db directory.
    pub fn with_db_dir(mut self, db_dir: impl Into<PathBuf>) -> Self {
        self.db_dir = db_dir.into();
        self
    }
}

impl<T> From<T> for ReadOnlyConfig
where
    T: AsRef<Path>,
{
    fn from(value: T) -> Self {
        Self::from_datadir(value.as_ref())
    }
}

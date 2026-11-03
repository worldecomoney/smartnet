use crate::DBProvider;
use alloc::{vec, vec::Vec};
use alloy_consensus::Header;
use alloy_primitives::BlockNumber;
use core::marker::PhantomData;
use smartnet_ethereum_primitives::TransactionSigned;
use smartnet_primitives_traits::{
    Block, BlockBody, FullBlockHeader, NodePrimitives, SignedTransaction,
};
use smartnet_chainspec::{ChainSpecProvider, EthereumHardforks};
use smartnet_db_api::transaction::DbTxMut;
use smartnet_storage_errors::provider::{ProviderError, ProviderResult};

/// Trait that implements how block bodies are written to the storage.
///
/// Note: Within the current abstraction, this should only write to tables unrelated to
/// transactions. Writing of transactions is handled separately.
#[auto_impl::auto_impl(&, Arc)]
pub trait BlockBodyWriter<Provider, Body: BlockBody> {
    /// Writes a set of block bodies to the storage.
    fn write_block_bodies(
        &self,
        provider: &Provider,
        bodies: Vec<(BlockNumber, Option<&Body>)>,
    ) -> ProviderResult<()>;

    /// Removes all block bodies above the given block number from the database.
    fn remove_block_bodies_above(
        &self,
        provider: &Provider,
        block: BlockNumber,
    ) -> ProviderResult<()>;
}

/// Trait that implements how chain-specific types are written to the storage.
pub trait ChainStorageWriter<Provider, Primitives: NodePrimitives>:
    BlockBodyWriter<Provider, <Primitives::Block as Block>::Body>
{
}
impl<T, Provider, Primitives: NodePrimitives> ChainStorageWriter<Provider, Primitives> for T where
    T: BlockBodyWriter<Provider, <Primitives::Block as Block>::Body>
{
}

/// Input for reading a block body. Contains a header of block being read and a list of pre-fetched
/// transactions.
pub type ReadBodyInput<'a, B> =
    (&'a <B as Block>::Header, Vec<<<B as Block>::Body as BlockBody>::Transaction>);

/// Trait that implements how block bodies are read from the storage.
///
/// Note: Within the current abstraction, transactions persistence is handled separately, thus this
/// trait is provided with transactions read beforehand and is expected to construct the block body
/// from those transactions and additional data read from elsewhere.
#[auto_impl::auto_impl(&, Arc)]
pub trait BlockBodyReader<Provider> {
    /// The block type.
    type Block: Block;

    /// Receives a list of block headers along with block transactions and returns the block bodies.
    fn read_block_bodies(
        &self,
        provider: &Provider,
        inputs: Vec<ReadBodyInput<'_, Self::Block>>,
    ) -> ProviderResult<Vec<<Self::Block as Block>::Body>>;
}

/// Trait that implements how chain-specific types are read from storage.
pub trait ChainStorageReader<Provider, Primitives: NodePrimitives>:
    BlockBodyReader<Provider, Block = Primitives::Block>
{
}
impl<T, Provider, Primitives: NodePrimitives> ChainStorageReader<Provider, Primitives> for T where
    T: BlockBodyReader<Provider, Block = Primitives::Block>
{
}

/// Ethereum storage implementation.
#[derive(Debug, Clone, Copy)]
pub struct EthStorage<T = TransactionSigned, H = Header>(PhantomData<(T, H)>);

impl<T, H> Default for EthStorage<T, H> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<Provider, T, H> BlockBodyWriter<Provider, alloy_consensus::BlockBody<T, H>>
    for EthStorage<T, H>
where
    Provider: DBProvider<Tx: DbTxMut>,
    T: SignedTransaction,
    H: FullBlockHeader,
{
    fn write_block_bodies(
        &self,
        _provider: &Provider,
        _bodies: Vec<(u64, Option<&alloy_consensus::BlockBody<T, H>>)>,
    ) -> ProviderResult<()> {
        // Block bodies (ommers/withdrawals) live in the file-tier bundle, written with the
        // block by the engine. There is no database body row to update.
        Ok(())
    }

    fn remove_block_bodies_above(
        &self,
        _provider: &Provider,
        _block: BlockNumber,
    ) -> ProviderResult<()> {
        // The file tier owns payload unwinds; the DB only drops its index rows.
        Ok(())
    }
}

impl<Provider, T, H> BlockBodyReader<Provider> for EthStorage<T, H>
where
    Provider: DBProvider + ChainSpecProvider<ChainSpec: EthereumHardforks>,
    T: SignedTransaction,
    H: FullBlockHeader,
{
    type Block = alloy_consensus::Block<T, H>;

    fn read_block_bodies(
        &self,
        _provider: &Provider,
        _inputs: Vec<ReadBodyInput<'_, Self::Block>>,
    ) -> ProviderResult<Vec<<Self::Block as Block>::Body>> {
        // Bodies are composed from the file-tier bundle by the provider; there is no
        // database payload row to read here.
        Err(ProviderError::UnsupportedProvider)
    }
}

/// A noop storage for chains that don’t have custom body storage.
///
/// This will never read nor write additional body content such as withdrawals or ommers.
/// But will respect the optionality of withdrawals if activated and fill them if the corresponding
/// hardfork is activated.
#[derive(Debug, Clone, Copy)]
pub struct EmptyBodyStorage<T, H>(PhantomData<(T, H)>);

impl<T, H> Default for EmptyBodyStorage<T, H> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<Provider, T, H> BlockBodyWriter<Provider, alloy_consensus::BlockBody<T, H>>
    for EmptyBodyStorage<T, H>
where
    T: SignedTransaction,
    H: FullBlockHeader,
{
    fn write_block_bodies(
        &self,
        _provider: &Provider,
        _bodies: Vec<(u64, Option<&alloy_consensus::BlockBody<T, H>>)>,
    ) -> ProviderResult<()> {
        // noop
        Ok(())
    }

    fn remove_block_bodies_above(
        &self,
        _provider: &Provider,
        _block: BlockNumber,
    ) -> ProviderResult<()> {
        // noop
        Ok(())
    }
}

impl<Provider, T, H> BlockBodyReader<Provider> for EmptyBodyStorage<T, H>
where
    Provider: ChainSpecProvider<ChainSpec: EthereumHardforks>,
    T: SignedTransaction,
    H: FullBlockHeader,
{
    type Block = alloy_consensus::Block<T, H>;

    fn read_block_bodies(
        &self,
        provider: &Provider,
        inputs: Vec<ReadBodyInput<'_, Self::Block>>,
    ) -> ProviderResult<Vec<<Self::Block as Block>::Body>> {
        let chain_spec = provider.chain_spec();

        Ok(inputs
            .into_iter()
            .map(|(header, transactions)| {
                alloy_consensus::BlockBody {
                    transactions,
                    ommers: vec![], // Empty storage never has ommers
                    withdrawals: chain_spec
                        .is_shanghai_active_at_timestamp(header.timestamp())
                        .then(Default::default),
                }
            })
            .collect())
    }
}

//! No-op auxiliary DB stub (upstream API surface; Calibri Lite uses the primary database only).
#![allow(missing_docs)]

use super::HistoryInfo;
use alloy_primitives::{Address, BlockNumber, TxNumber, B256};
use parking_lot::Mutex;
use smartnet_chain_state::ExecutedBlock;
use smartnet_db_api::{models::StorageSettings, table::Table};
use smartnet_prune_types::PruneMode;
use smartnet_storage_errors::provider::ProviderResult;
use std::{fmt, marker::PhantomData, path::Path, sync::Arc};

pub(crate) type RawAuxDbBatch = ();
pub(crate) type PendingAuxDbBatches = Arc<Mutex<Vec<RawAuxDbBatch>>>;

#[derive(Clone, Debug)]
pub struct AuxDbWriteCtx {
    pub first_block_number: BlockNumber,
    pub prune_tx_lookup: Option<PruneMode>,
    pub storage_settings: StorageSettings,
    pub pending_batches: PendingAuxDbBatches,
}

#[derive(Debug, Default, Clone)]
pub struct AuxDbProvider;

impl AuxDbProvider {
    pub fn builder(path: impl AsRef<Path>) -> AuxDbBuilder {
        AuxDbBuilder::new(path)
    }

    pub fn new(_path: impl AsRef<Path>) -> ProviderResult<Self> {
        Ok(Self)
    }

    pub const fn snapshot(&self) -> AuxDbReadSnapshot<'_> {
        AuxDbReadSnapshot { _pd: PhantomData }
    }

    pub const fn batch(&self) -> AuxDbBatch<'_> {
        AuxDbBatch { _pd: PhantomData }
    }

    pub const fn batch_with_auto_commit(&self) -> AuxDbBatch<'_> {
        AuxDbBatch { _pd: PhantomData }
    }

    pub const fn try_catch_up_with_primary(&self) -> ProviderResult<()> {
        Ok(())
    }

    pub const fn check_consistency<T>(&self, _provider_ro: &T) -> ProviderResult<Option<u64>> {
        Ok(None)
    }

    pub const fn commit_batch(&self, _batch: RawAuxDbBatch) -> ProviderResult<()> {
        Ok(())
    }

    pub fn write_blocks_data<N: smartnet_node_types::NodePrimitives>(
        &self,
        _blocks: &[ExecutedBlock<N>],
        _tx_nums: &[TxNumber],
        _ctx: AuxDbWriteCtx,
        _runtime: &smartnet_tasks::Runtime,
    ) -> ProviderResult<()> {
        Ok(())
    }

    pub const fn unwind_account_history_indices(
        &self,
        _last_indices: &[(Address, BlockNumber)],
    ) -> ProviderResult<RawAuxDbBatch> {
        Ok(())
    }

    pub const fn unwind_storage_history_indices(
        &self,
        _storage_changesets: &[(Address, B256, BlockNumber)],
    ) -> ProviderResult<RawAuxDbBatch> {
        Ok(())
    }

    pub const fn flush(&self, _tables: &[&str]) -> ProviderResult<()> {
        Ok(())
    }

    pub const fn clear<T: Table>(&self) -> ProviderResult<()> {
        let _ = T::NAME;
        Ok(())
    }

    pub const fn iter<T: Table>(&self) -> ProviderResult<AuxDbIter<'_, T>> {
        Ok(AuxDbIter { _pd: PhantomData })
    }
}

#[derive(Debug, Clone)]
pub struct AuxDbBuilder {
    _path: Arc<std::path::PathBuf>,
}

impl AuxDbBuilder {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self { _path: Arc::new(path.as_ref().to_path_buf()) }
    }

    pub const fn with_default_tables(self) -> Self {
        self
    }

    pub const fn with_read_only(self, _read_only: bool) -> Self {
        self
    }

    pub fn build(self) -> ProviderResult<AuxDbProvider> {
        Ok(AuxDbProvider)
    }
}

pub struct AuxDbReadSnapshot<'db> {
    _pd: PhantomData<&'db ()>,
}
impl fmt::Debug for AuxDbReadSnapshot<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuxDbReadSnapshot").finish()
    }
}
impl AuxDbReadSnapshot<'_> {
    pub fn get<T: Table>(&self, _key: T::Key) -> ProviderResult<Option<T::Value>> {
        Ok(None)
    }

    pub const fn account_history_info(
        &self,
        _address: Address,
        _block_number: BlockNumber,
        _lowest_available_block_number: Option<BlockNumber>,
        _visible_tip: BlockNumber,
    ) -> ProviderResult<HistoryInfo> {
        Ok(HistoryInfo::NotYetWritten)
    }

    pub const fn storage_history_info(
        &self,
        _address: Address,
        _storage_key: B256,
        _block_number: BlockNumber,
        _lowest_available_block_number: Option<BlockNumber>,
        _visible_tip: BlockNumber,
    ) -> ProviderResult<HistoryInfo> {
        Ok(HistoryInfo::NotYetWritten)
    }
}

pub struct AuxDbBatch<'a> {
    _pd: PhantomData<&'a ()>,
}
impl fmt::Debug for AuxDbBatch<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuxDbBatch").finish()
    }
}
impl AuxDbBatch<'_> {
    pub const fn into_inner(self) -> RawAuxDbBatch {}
    pub fn put<T: Table>(&mut self, _key: T::Key, _value: &T::Value) -> ProviderResult<()> {
        Ok(())
    }
    pub fn delete<T: Table>(&mut self, _key: T::Key) -> ProviderResult<()> {
        Ok(())
    }
    pub fn get<T: Table>(&self, _key: T::Key) -> ProviderResult<Option<T::Value>> {
        Ok(None)
    }

    pub fn append_account_history_shard(
        &mut self,
        _address: Address,
        _indices: impl IntoIterator<Item = u64>,
    ) -> ProviderResult<()> {
        Ok(())
    }

    pub fn append_storage_history_shard(
        &mut self,
        _address: Address,
        _storage_key: B256,
        _indices: impl IntoIterator<Item = u64>,
    ) -> ProviderResult<()> {
        Ok(())
    }

    pub fn prune_account_history_batch(
        &mut self,
        _targets: &[(Address, BlockNumber)],
    ) -> ProviderResult<PrunedIndices> {
        Ok(PrunedIndices::default())
    }

    pub fn prune_storage_history_batch(
        &mut self,
        _targets: &[((Address, B256), BlockNumber)],
    ) -> ProviderResult<PrunedIndices> {
        Ok(PrunedIndices::default())
    }
}

pub struct AuxDbIter<'db, T: Table> {
    _pd: PhantomData<(&'db (), T)>,
}
impl<T: Table> fmt::Debug for AuxDbIter<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuxDbIter").finish()
    }
}
impl<T: Table> Iterator for AuxDbIter<'_, T> {
    type Item = ProviderResult<(T::Key, T::Value)>;
    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrunedIndices {
    pub deleted: usize,
    pub updated: usize,
    pub unchanged: usize,
}

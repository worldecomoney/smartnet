//! Additional testing support for `NoopProvider`.

use crate::{
    either_writer::RawAuxDbBatch,
    providers::AuxDbProvider,
    AuxDbProviderFactory,
};
use smartnet_errors::ProviderResult;
use smartnet_primitives_traits::NodePrimitives;
use std::path::PathBuf;

/// Re-exported for convenience
pub use smartnet_storage_api::noop::NoopProvider;

impl<C: Send + Sync, N: NodePrimitives> AuxDbProviderFactory for NoopProvider<C, N> {
    fn aux_db_provider(&self) -> AuxDbProvider {
        AuxDbProvider::builder(PathBuf::default()).build().unwrap()
    }

    fn set_pending_aux_db_batch(&self, _batch: RawAuxDbBatch) {}

    fn commit_pending_aux_db_batches(&self) -> ProviderResult<()> {
        Ok(())
    }
}

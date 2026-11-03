//! Common receipts pruning logic.
//!
//! - [`crate::segments::user::Receipts`] is responsible for pruning receipts according to the
//!   user-configured settings (for example, on a full node or with a custom prune config)

use crate::{segments::PruneInput, PrunerError};
use smartnet_prune_types::{PruneCheckpoint, PruneSegment, SegmentOutput};
use smartnet_provider::{errors::provider::ProviderResult, PruneCheckpointWriter};
use tracing::debug;

pub(crate) fn prune<Provider>(
    provider: &Provider,
    input: PruneInput,
) -> Result<SegmentOutput, PrunerError> {
    let _ = (provider, input);
    debug!(target: "pruner", "Receipts live in the file tier; nothing to prune in the database.");
    Ok(SegmentOutput::done())
}

pub(crate) fn save_checkpoint(
    provider: impl PruneCheckpointWriter,
    checkpoint: PruneCheckpoint,
) -> ProviderResult<()> {
    provider.save_prune_checkpoint(PruneSegment::Receipts, checkpoint)?;

    // `PruneSegment::Receipts` overrides `PruneSegment::ContractLogs`, so we can preemptively
    // limit their pruning start point.
    provider.save_prune_checkpoint(PruneSegment::ContractLogs, checkpoint)?;

    Ok(())
}

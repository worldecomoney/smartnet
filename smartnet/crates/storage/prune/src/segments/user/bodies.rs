use crate::{
    segments::{PruneInput, Segment},
    PrunerError,
};
use smartnet_prune_types::{PruneMode, PrunePurpose, PruneSegment, SegmentOutput};
use smartnet_provider::{BlockReader, PruneCheckpointReader};
use tracing::{debug, instrument};

/// Segment for historical transaction bodies.
///
/// Historically this pruned transaction bodies from static files (the retired MDBX-era
/// store). Block bodies now live in the file tier, which has its own retention model, so
/// this segment is a no-op kept for config compatibility (`bodies_history`).
#[derive(Debug)]
pub struct Bodies {
    mode: PruneMode,
    /// Transaction lookup prune mode. Retained for config-shaped construction; the
    /// segment itself no longer prunes.
    #[allow(dead_code)]
    tx_lookup_mode: Option<PruneMode>,
}

impl Bodies {
    /// Creates a new [`Bodies`] segment with the given prune mode and optional transaction
    /// lookup prune mode (kept for config compatibility).
    pub const fn new(mode: PruneMode, tx_lookup_mode: Option<PruneMode>) -> Self {
        Self { mode, tx_lookup_mode }
    }
}

impl<Provider> Segment<Provider> for Bodies
where
    Provider: BlockReader + PruneCheckpointReader,
{
    fn segment(&self) -> PruneSegment {
        PruneSegment::Bodies
    }

    fn mode(&self) -> Option<PruneMode> {
        Some(self.mode)
    }

    fn purpose(&self) -> PrunePurpose {
        PrunePurpose::User
    }

    #[instrument(
        name = "Bodies::prune",
        target = "pruner",
        skip(self, _provider),
        ret(level = "trace")
    )]
    fn prune(&self, _provider: &Provider, input: PruneInput) -> Result<SegmentOutput, PrunerError> {
        debug!(
            to_block = input.to_block,
            "Bodies pruning is a no-op: block bodies live in the file tier"
        );
        Ok(SegmentOutput::done())
    }
}

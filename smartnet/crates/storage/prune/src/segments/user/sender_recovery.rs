use crate::{
    segments::{PruneInput, Segment},
    PrunerError,
};
use smartnet_prune_types::{PruneMode, PrunePurpose, PruneSegment, SegmentOutput};
use tracing::{debug, instrument};

#[derive(Debug)]
pub struct SenderRecovery {
    mode: PruneMode,
}

impl SenderRecovery {
    pub const fn new(mode: PruneMode) -> Self {
        Self { mode }
    }
}

impl<Provider> Segment<Provider> for SenderRecovery {
    fn segment(&self) -> PruneSegment {
        PruneSegment::SenderRecovery
    }

    fn mode(&self) -> Option<PruneMode> {
        Some(self.mode)
    }

    fn purpose(&self) -> PrunePurpose {
        PrunePurpose::User
    }

    #[instrument(
        name = "SenderRecovery::prune",
        target = "pruner",
        skip(self, provider),
        ret(level = "trace")
    )]
    fn prune(&self, provider: &Provider, input: PruneInput) -> Result<SegmentOutput, PrunerError> {
        let _ = (provider, input);
        debug!(target: "pruner", "Senders live in the file tier; nothing to prune in the database.");
        Ok(SegmentOutput::done())
    }
}

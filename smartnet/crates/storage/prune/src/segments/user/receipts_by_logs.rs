use crate::{
    segments::{PruneInput, Segment},
    PrunerError,
};
use smartnet_prune_types::{
    PruneMode, PrunePurpose, PruneSegment, ReceiptsLogPruneConfig, SegmentOutput,
};
use tracing::{instrument, trace};
#[derive(Debug)]
pub struct ReceiptsByLogs;

impl ReceiptsByLogs {
    pub fn new(_config: ReceiptsLogPruneConfig) -> Self {
        Self
    }
}

impl<Provider> Segment<Provider> for ReceiptsByLogs {
    fn segment(&self) -> PruneSegment {
        PruneSegment::ContractLogs
    }

    fn mode(&self) -> Option<PruneMode> {
        None
    }

    fn purpose(&self) -> PrunePurpose {
        PrunePurpose::User
    }

    #[instrument(
        name = "ReceiptsByLogs::prune",
        target = "pruner",
        skip(self, provider),
        ret(level = "trace")
    )]
    fn prune(&self, provider: &Provider, input: PruneInput) -> Result<SegmentOutput, PrunerError> {
        let _ = (provider, input);
        trace!(target: "pruner", "Receipts live in the file tier; nothing to prune in the database.");
        Ok(SegmentOutput::done())
    }
}

use alloy_primitives::BlockNumber;

/// Determines the control flow during pipeline execution.
///
/// See [`Pipeline::run_loop`](crate::Pipeline::run_loop) for more information.
///
/// `SmartNet`: no unwind — sync failures drop the run (recoverable stage errors retry;
/// everything else fails the pipeline). Only `restore_unwind(2)` may unwind, on
/// cold-shutdown recovery.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ControlFlow {
    /// The pipeline made progress.
    Continue {
        /// Block number reached by the stage.
        block_number: BlockNumber,
    },
    /// Pipeline made no progress
    NoProgress {
        /// Block number reached by the stage.
        block_number: Option<BlockNumber>,
    },
}

impl ControlFlow {
    /// Whether the pipeline should continue executing stages.
    pub const fn should_continue(&self) -> bool {
        matches!(self, Self::Continue { .. } | Self::NoProgress { .. })
    }

    /// Returns the pipeline block number the stage reached, if any.
    pub const fn block_number(&self) -> Option<BlockNumber> {
        match self {
            Self::Continue { block_number } => Some(*block_number),
            Self::NoProgress { block_number } => *block_number,
        }
    }
}

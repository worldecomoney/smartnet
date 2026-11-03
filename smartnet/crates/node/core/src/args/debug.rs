//! clap [Args](clap::Args) for debugging purposes

use alloy_primitives::B256;
use clap::Args;
use std::path::PathBuf;

/// Parameters for debugging purposes
#[derive(Debug, Clone, Default, Args, PartialEq, Eq)]
#[command(next_help_heading = "Debug")]
pub struct DebugArgs {
    /// Flag indicating whether the node should be terminated after the pipeline sync.
    #[arg(skip)]
    pub terminate: bool,

    /// Set the chain tip manually for testing purposes.
    ///
    /// NOTE: This is a temporary flag
    #[arg(skip)]
    pub tip: Option<B256>,

    /// Runs the sync only up to the specified block.
    #[arg(skip)]
    pub max_block: Option<u64>,

    /// If provided, the engine will skip `n` consecutive FCUs.
    #[arg(skip)]
    pub skip_fcu: Option<usize>,

    /// If provided, the engine will skip `n` consecutive new payloads.
    #[arg(skip)]
    pub skip_new_payload: Option<usize>,

    /// If provided, the chain will be reorged at specified frequency.
    #[arg(skip)]
    pub reorg_frequency: Option<usize>,

    /// The reorg depth for chain reorgs.
    #[arg(skip)]
    pub reorg_depth: Option<usize>,

    /// The path to store engine API messages at.
    /// If specified, all of the intercepted engine API messages
    /// will be written to specified location.
    #[arg(skip)]
    pub engine_api_store: Option<PathBuf>,

}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    /// The operator CLI is config-file driven: these per-flag options were removed
    /// (all struct fields are `#[arg(skip)]`). The legacy flags must stay rejected.
    #[test]
    fn removed_cli_flags_are_rejected() {
        const REMOVED: &[&str] = &[
            "--debug.invalid-block-hook",
        ];
        for flag in REMOVED {
            assert!(
                CommandParser::<DebugArgs>::try_parse_from(["smartnet", flag, "x"]).is_err(),
                "{flag} must stay rejected (config-file-driven CLI)"
            );
        }
    }

    #[test]
    fn test_parse_default_debug_args() {
        // Config-file-driven CLI: all fields are `#[arg(skip)]`, so defaults come from
        // the struct impl (config loader path), not from argv parsing.
        let args = DebugArgs::default();
        assert!(!args.terminate);
        assert!(args.tip.is_none());
        assert!(args.max_block.is_none());
        assert!(args.skip_fcu.is_none());
        assert!(args.skip_new_payload.is_none());
        assert!(args.reorg_frequency.is_none());
        assert!(args.reorg_depth.is_none());
        assert!(args.engine_api_store.is_none());
        CommandParser::<DebugArgs>::parse_from(["smartnet"]);
    }


}

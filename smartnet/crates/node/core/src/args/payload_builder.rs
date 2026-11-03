use crate::{cli::config::PayloadBuilderConfig, version::default_extra_data};
use alloy_primitives::Bytes;
use clap::Args;
use crate::parsers::{
    format_duration_as_secs_or_ms, parse_duration_from_secs_or_ms,
};
use std::{sync::OnceLock, time::Duration};

/// Global static payload builder defaults
static PAYLOAD_BUILDER_DEFAULTS: OnceLock<DefaultPayloadBuilderValues> = OnceLock::new();

/// Default values for payload builder that can be customized
///
/// Global defaults can be set via [`DefaultPayloadBuilderValues::try_init`].
#[derive(Debug, Clone)]
pub struct DefaultPayloadBuilderValues {
    /// Default extra data for blocks
    extra_data: String,
    /// Default interval between payload builds in seconds
    interval: String,
    /// Default deadline for payload builds in seconds
    deadline: String,
    /// Default maximum number of concurrent payload building tasks
    max_payload_tasks: usize,
}

impl DefaultPayloadBuilderValues {
    /// Initialize the global payload builder defaults with this configuration
    pub fn try_init(self) -> Result<(), Self> {
        PAYLOAD_BUILDER_DEFAULTS.set(self)
    }

    /// Get a reference to the global payload builder defaults
    pub fn get_global() -> &'static Self {
        PAYLOAD_BUILDER_DEFAULTS.get_or_init(Self::default)
    }

    /// Set the default extra data
    pub fn with_extra_data(mut self, v: impl Into<String>) -> Self {
        self.extra_data = v.into();
        self
    }

    /// Set the default interval in seconds
    pub fn with_interval(mut self, v: Duration) -> Self {
        self.interval = format_duration_as_secs_or_ms(v);
        self
    }

    /// Set the default deadline in seconds
    pub fn with_deadline(mut self, v: u64) -> Self {
        self.deadline = format!("{}", v);
        self
    }

    /// Set the default maximum payload tasks
    pub const fn with_max_payload_tasks(mut self, v: usize) -> Self {
        self.max_payload_tasks = v;
        self
    }
}

impl Default for DefaultPayloadBuilderValues {
    fn default() -> Self {
        Self {
            extra_data: default_extra_data(),
            interval: "1".to_string(),
            deadline: "12".to_string(),
            max_payload_tasks: 3,
        }
    }
}

/// Parameters for configuring the Payload Builder
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(next_help_heading = "Builder")]
pub struct PayloadBuilderArgs {
    /// Block extra data set by the payload builder.
    ///
    /// If the value is a `0x`-prefixed hex string, it is decoded into raw bytes. Otherwise, the
    /// raw UTF-8 bytes of the string are used.
    #[arg(skip)]
    pub extra_data: Bytes,

    /// Target gas limit for built blocks.
    #[arg(skip)]
    pub gas_limit: Option<u64>,

    /// The interval at which the job should build a new payload after the last.
    ///
    /// Interval is specified in seconds or in milliseconds if the value ends with `ms`:
    ///   * `50ms` -> 50 milliseconds
    ///   * `1` -> 1 second
    #[arg(skip)]
    pub interval: Duration,

    /// The deadline for when the payload builder job should resolve.
    #[arg(skip)]
    pub deadline: Duration,

    /// Maximum number of tasks to spawn for building a payload.
    #[arg(skip)]
    pub max_payload_tasks: usize,

    /// Maximum number of blobs to include per block.
    #[arg(skip)]
    pub max_blobs_per_block: Option<u64>,
}

impl Default for PayloadBuilderArgs {
    fn default() -> Self {
        let defaults = DefaultPayloadBuilderValues::get_global();
        Self {
            extra_data: Bytes::from(defaults.extra_data.as_bytes().to_vec()),
            interval: parse_duration_from_secs_or_ms(defaults.interval.as_str()).unwrap(),
            gas_limit: None,
            deadline: Duration::from_secs(defaults.deadline.parse().unwrap()),
            max_payload_tasks: defaults.max_payload_tasks,
            max_blobs_per_block: None,
        }
    }
}

impl PayloadBuilderConfig for PayloadBuilderArgs {
    fn extra_data(&self) -> Bytes {
        self.extra_data.clone()
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn deadline(&self) -> Duration {
        self.deadline
    }

    fn gas_limit(&self) -> Option<u64> {
        self.gas_limit
    }

    fn max_payload_tasks(&self) -> usize {
        self.max_payload_tasks
    }

    fn max_blobs_per_block(&self) -> Option<u64> {
        self.max_blobs_per_block
    }
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
            "--builder.max-tasks",
            "--builder.extradata",
            "--builder.interval",
        ];
        for flag in REMOVED {
            assert!(
                CommandParser::<PayloadBuilderArgs>::try_parse_from(["smartnet", flag, "x"]).is_err(),
                "{flag} must stay rejected (config-file-driven CLI)"
            );
        }
    }









    #[test]
    fn payload_builder_args_default_sanity_check() {
        // Defaults are config-driven (`DefaultPayloadBuilderValues::get_global()`).
        let args = PayloadBuilderArgs::default();
        assert!(!args.extra_data.is_empty());
        assert!(args.interval > Duration::ZERO);
        assert!(args.deadline > Duration::ZERO);
        assert!(args.max_payload_tasks > 0);
        assert!(args.gas_limit.is_none());
        assert!(args.max_blobs_per_block.is_none());
        CommandParser::<PayloadBuilderArgs>::parse_from(["smartnet"]);
    }


}

//! clap [Args](clap::Args) for logging configuration.

use clap::{ArgAction, Args, ValueEnum};
use smartnet_tracing::{
    tracing_subscriber::filter::Directive, LayerInfo, Layers, LogFormat, SmartnetTracer, Tracer,
};
use std::{fmt, fmt::Display, sync::OnceLock};
use tracing::{level_filters::LevelFilter, Level};

const PROFILER_TRACING_FILTER: &str = "debug";

/// Global static log defaults
static LOG_DEFAULTS: OnceLock<DefaultLogArgs> = OnceLock::new();

/// The log configuration.
#[derive(Debug, Args)]
#[command(next_help_heading = "Logging")]
pub struct LogArgs {
    /// The format to use for logs written to stdout.
    #[arg(long = "log.stdout.format", value_name = "FORMAT", global = true, default_value_t = DefaultLogArgs::get_global().log_stdout_format)]
    pub log_stdout_format: LogFormat,

    /// The filter to use for logs written to stdout.
    #[arg(long = "log.stdout.filter", value_name = "FILTER", global = true, default_value_t = DefaultLogArgs::get_global().log_stdout_filter.clone())]
    pub log_stdout_filter: String,

    /// Write logs to journald.
    #[arg(long = "log.journald", global = true, default_value_t = DefaultLogArgs::get_global().journald)]
    pub journald: bool,

    /// The filter to use for logs written to journald.
    #[arg(
        long = "log.journald.filter",
        value_name = "FILTER",
        global = true,
        default_value_t = DefaultLogArgs::get_global().journald_filter.clone()
    )]
    pub journald_filter: String,

    /// Emit traces to samply. Only useful when profiling.
    #[arg(long = "log.samply", global = true, hide = true, default_value_t = DefaultLogArgs::get_global().samply)]
    pub samply: bool,

    /// The filter to use for traces emitted to samply.
    #[arg(
        long = "log.samply.filter",
        value_name = "FILTER",
        global = true,
        default_value_t = DefaultLogArgs::get_global().samply_filter.clone(),
        hide = true
    )]
    pub samply_filter: String,

    /// Sets whether or not the formatter emits ANSI terminal escape codes for colors and other
    /// text formatting.
    #[arg(
        long,
        value_name = "COLOR",
        global = true,
        default_value_t = DefaultLogArgs::get_global().color
    )]
    pub color: ColorMode,

    /// The verbosity settings for the tracer.
    #[command(flatten)]
    pub verbosity: Verbosity,
}

impl LogArgs {
    /// Creates a [`LayerInfo`] instance.
    fn layer_info(&self, format: LogFormat, filter: String, use_color: bool) -> LayerInfo {
        LayerInfo::new(
            format,
            self.verbosity.directive().to_string(),
            filter,
            use_color.then(|| self.color.to_string()),
        )
    }

    /// Initializes tracing with the configured options from cli args.
    ///
    /// All logs go to the screen; file logging was removed by operator order.
    pub fn init_tracing(&self) -> eyre::Result<()> {
        self.init_tracing_with_layers(Layers::new(), false)
    }

    /// Initializes tracing with the configured options from cli args.
    ///
    /// When `enable_reload` is true, a global log handle is installed that allows changing
    /// log levels at runtime via RPC methods like `debug_verbosity` and `debug_vmodule`.
    ///
    /// # Arguments
    /// * `layers` - Pre-configured layers to include
    /// * `enable_reload` - If true, enables runtime log level changes
    pub fn init_tracing_with_layers(
        &self,
        layers: Layers,
        enable_reload: bool,
    ) -> eyre::Result<()> {
        let mut tracer = SmartnetTracer::new();

        let stdout = self.layer_info(self.log_stdout_format, self.log_stdout_filter.clone(), true);
        tracer = tracer.with_stdout(stdout);

        if self.journald {
            tracer = tracer.with_journald(self.journald_filter.clone());
        }

        if self.samply {
            let config = self.layer_info(LogFormat::Terminal, self.samply_filter.clone(), false);
            tracer = tracer.with_samply(config);
        }

        tracer.with_reload(enable_reload).init_with_layers(layers).map(|_| ())
    }
}

/// Default values for log configuration that can be customized.
///
/// Global defaults can be set via [`DefaultLogArgs::try_init`].
#[derive(Debug, Clone)]
pub struct DefaultLogArgs {
    log_stdout_format: LogFormat,
    log_stdout_filter: String,
    journald: bool,
    journald_filter: String,
    samply: bool,
    samply_filter: String,
    color: ColorMode,
}

impl DefaultLogArgs {
    /// Initialize the global log defaults with this configuration.
    pub fn try_init(self) -> Result<(), Self> {
        LOG_DEFAULTS.set(self)
    }

    /// Get a reference to the global log defaults.
    pub fn get_global() -> &'static Self {
        LOG_DEFAULTS.get_or_init(Self::default)
    }

    /// Set the default stdout log format.
    pub const fn with_log_stdout_format(mut self, v: LogFormat) -> Self {
        self.log_stdout_format = v;
        self
    }

    /// Set the default stdout log filter.
    pub fn with_log_stdout_filter(mut self, v: String) -> Self {
        self.log_stdout_filter = v;
        self
    }

    /// Set whether journald logging is enabled by default.
    pub const fn with_journald(mut self, v: bool) -> Self {
        self.journald = v;
        self
    }

    /// Set the default journald filter.
    pub fn with_journald_filter(mut self, v: String) -> Self {
        self.journald_filter = v;
        self
    }

    /// Set whether samply tracing is enabled by default.
    pub const fn with_samply(mut self, v: bool) -> Self {
        self.samply = v;
        self
    }

    /// Set the default samply filter.
    pub fn with_samply_filter(mut self, v: String) -> Self {
        self.samply_filter = v;
        self
    }

    /// Set the default color mode.
    pub const fn with_color(mut self, v: ColorMode) -> Self {
        self.color = v;
        self
    }
}

impl Default for DefaultLogArgs {
    fn default() -> Self {
        Self {
            log_stdout_format: LogFormat::Terminal,
            log_stdout_filter: String::new(),
            journald: false,
            journald_filter: "error".to_string(),
            samply: false,
            samply_filter: PROFILER_TRACING_FILTER.to_string(),
            color: ColorMode::Always,
        }
    }
}

/// The color mode for the cli.
#[derive(Debug, Copy, Clone, ValueEnum, Eq, PartialEq)]
pub enum ColorMode {
    /// Colors on
    Always,
    /// Auto-detect
    Auto,
    /// Colors off
    Never,
}

impl Display for ColorMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Always => write!(f, "always"),
            Self::Auto => write!(f, "auto"),
            Self::Never => write!(f, "never"),
        }
    }
}

/// The verbosity settings for the cli.
#[derive(Debug, Copy, Clone, Args)]
#[command(next_help_heading = "Display")]
pub struct Verbosity {
    /// Set the minimum log level.
    ///
    /// -v      Errors
    /// -vv     Warnings
    /// -vvv    Info
    /// -vvvv   Debug
    /// -vvvvv  Traces (warning: very verbose!)
    #[arg(short, long, action = ArgAction::Count, global = true, default_value_t = 3, verbatim_doc_comment, help_heading = "Display")]
    verbosity: u8,

    /// Silence all log output.
    #[arg(long, alias = "silent", short = 'q', global = true, help_heading = "Display")]
    quiet: bool,
}

impl Verbosity {
    /// Get the corresponding [Directive] for the given verbosity, or none if the verbosity
    /// corresponds to silent.
    pub fn directive(&self) -> Directive {
        if self.quiet {
            LevelFilter::OFF.into()
        } else {
            let level = match self.verbosity - 1 {
                0 => Level::ERROR,
                1 => Level::WARN,
                2 => Level::INFO,
                3 => Level::DEBUG,
                _ => Level::TRACE,
            };

            level.into()
        }
    }
}

//! `SmartNet` validator — merged into the node; the node heartbeat calls its seal beat.

// eyre::bail!/ensure! expansions end in a `;` we do not control (third-party macro).

/// Default seal-beat cadence (`HEARTBEAT_MS` absent → this), ms.
pub const DEFAULT_HEARTBEAT_MS: u64 = 2000;

pub mod app;
pub mod balances;
pub mod block_rlp_wire;
pub mod config;
pub mod import;
pub mod io;
pub mod keystore_util;
pub mod mempool_policy;
pub mod outcome;
pub mod payload_checkpoints;
pub mod run_once;
pub mod seed_tx;

use clap::{Parser, Subcommand};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{fmt, EnvFilter};

/// External `SmartNet` block producer.
#[derive(Parser, Debug)]
#[command(
    name = "smartnet-validator",
    about = "SmartNet validator: config-driven keystore, seal via smartnet_newPayload"
)]
pub struct Cli {
    #[command(subcommand)]
    /// The operator subcommand to run; `None` runs the default (daemon) behaviour.
    pub command: Option<Commands>,

    /// Operator config (`KEY_PATH`, `KEY_FILE`, `NODE_URL`, `COINBASE`, `ALLOW_EMPTY`, …).
    /// Defaults to `~/.smartnet/config.toml` if not specified.
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Write sealed block RLP to PATH (`smartnet-test-util` probes).
    #[arg(long = "dump-payload", value_name = "PATH")]
    pub dump_payload: Option<PathBuf>,
}

/// Validator operator subcommands.
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// The only way a key enters the validator: import a plaintext private-key file into an
    /// encrypted keystore, point the config at it, then delete the source. Also replaces an
    /// existing keystore (key-change flow).
    Keystore(KeystoreCli),
    /// Fund an account through the canonical seed-transaction flow.
    SeedTx,
    /// Probe node liveness with a forkchoice update (read-only diagnostic).
    Ping(PingCli),
}

/// Arguments for the `keystore` import subcommand.
#[derive(clap::Args, Debug)]
pub struct KeystoreCli {
    /// Path to a file holding the 64-hex-character private key, with or without a `0x`
    /// prefix. The file is deleted once the keystore and config are written.
    #[arg(long, value_name = "PATH")]
    pub key: PathBuf,
    /// Path to the password file for the encrypted keystore. The path is written to
    /// `PASSWORD_FILE` in the config; a missing/empty file prompts (and is created).
    #[arg(long, value_name = "PATH")]
    pub password: PathBuf,
}

/// Arguments for the `ping` liveness probe.
#[derive(clap::Args, Debug)]
pub struct PingCli {
    /// Explicit head block hash for the FCU. Requires `--finalize`; without that flag the
    /// probe always sends the node's *current* head and never steers fork choice (A6 · M6).
    #[arg(long, value_parser = parse_b256)]
    pub head: Option<alloy_primitives::B256>,
    /// Explicit safe block hash (only meaningful with `--finalize`; defaults to `--head`).
    #[arg(long, value_parser = parse_b256)]
    pub safe: Option<alloy_primitives::B256>,
    /// Explicit finalized block hash (only meaningful with `--finalize`; defaults to `--head`).
    #[arg(long, value_parser = parse_b256)]
    pub finalized: Option<alloy_primitives::B256>,
    /// Opt in to explicit fork-choice steering. Without this flag `ping` is a read-only
    /// health probe: `head = safe = finalized = current canonical head`.
    #[arg(long)]
    pub finalize: bool,
}

/// Default operator config path: `$HOME/.smartnet/validator/config.toml`.
///
/// `HOME` unset is an error — there is no fallback to the current working directory and no
/// built-in config location. Pass `--config <path>` explicitly instead.
pub(crate) fn default_validator_config_path() -> eyre::Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| {
        eyre::eyre!(
            "HOME is not set, so the default config path ~/.smartnet/config.toml cannot \
             be resolved — pass `--config /path/to/config.toml` (no built-in fallback path exists)"
        )
    })?;
    Ok(PathBuf::from(home).join(".smartnet").join("config.toml"))
}

impl Cli {
    /// Returns the resolved config path — explicit `--config` flag or `~/.smartnet/config.toml`.
    pub fn resolved_config(&self) -> eyre::Result<PathBuf> {
        match &self.config {
            Some(p) => Ok(p.clone()),
            None => default_validator_config_path(),
        }
    }
}

/// Parse a hex `B256` for clap, mapping the parse error to a plain message.
pub fn parse_b256(s: &str) -> Result<alloy_primitives::B256, String> {
    s.parse::<alloy_primitives::B256>().map_err(|e| e.to_string())
}

/// Maps a `VERB` rank (0-3) to a tracing level filter:
/// 0 = errors only, 1 = info (default), 2 = debug, 3 = everything (trace).
pub const fn verb_to_level(rank: Option<u8>) -> LevelFilter {
    match rank {
        Some(0) => LevelFilter::ERROR,
        Some(2) => LevelFilter::DEBUG,
        Some(3) => LevelFilter::TRACE,
        // Unset, `1` (info), or anything the parser let through keeps the default (info).
        _ => LevelFilter::INFO,
    }
}

/// Drive the validator's CLI dispatch from the merged node binary (`smartnet keystore`,
/// `smartnet seedTx`, `smartnet ping`). Installs the TLS provider + tracing exactly like the
/// old standalone `main()` did, then returns the process exit code.
pub async fn run_validator_cli(cli: Cli) -> i32 {
    // Install the pure-Rust TLS provider before any rustls client is built;
    // rustls panics if no process-level CryptoProvider is available. The merged node
    // binary already installs it at process start — a second install returns the
    // existing provider, which is fine (the process default is set either way).
    let _ = smartnet_tls_provider::install_default();

    // Resolve log verbosity from config before tracing (optional key `VERB`, defaults to 1).
    let verb =
        match crate::config::load_validator_config(&cli.resolved_config().unwrap_or_default()) {
            Ok(cfg) => cfg.verb,
            Err(_) => None,
        };
    let level = verb_to_level(verb);

    // Terminal only. There is no log file: everything goes to stderr so stdout stays clean
    // for command output (`ping` prints the node's JSON response there).
    let filter = EnvFilter::builder().with_default_directive(level.into()).from_env_lossy();
    let _ = fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();

    app::run(cli).await.finish()
}

/// Parse the shared config now and report whether the merged validator is switched on
/// (`SEAL = true`). A malformed validator section aborts the caller instead of silently
/// leaving the node without a validator (fail-closed boot).
pub fn validator_daemon_enabled(config_path: &Path) -> eyre::Result<bool> {
    Ok(config::load_validator_config(config_path)?.enabled)
}

/// The optional unified `VERB` rank (0-3) from the shared config, for the merged binary's
/// tracing setup. It drives both node and validator logs. `Ok(None)` when the key is absent
/// or blank (the documented default is info).
pub fn validator_verb(config_path: &Path) -> eyre::Result<Option<u8>> {
    Ok(config::load_validator_config(config_path)?.verb)
}

/// The merged validator's beat state. The node's single heartbeat owns the loop and calls
/// [`ValidatorDaemon::beat`]; this type prepares the one-time keystore/config state and runs
/// the seal path against the node it lives inside, through the in-process [`io::ValidatorIo`].
///
/// There is one process and one config file. Config parse errors are reported by the caller
/// (`validator_daemon_enabled`) before this state is built. A failed beat is logged and retried
/// on the next beat — the node heartbeat never exits on a bad/slow node.
pub struct ValidatorDaemon<I> {
    io: I,
    args: run_once::RunOnceArgs,
    last_block_nr: AtomicU64,
    last_block_print: Mutex<Instant>,
    /// The single-lane red light (`VAL_BUSY`): set while one validation sequence runs,
    /// cleared on every exit path. A trigger (heartbeat, mesh) that fires while it is
    /// set is coalesced away — never a second pass.
    busy: AtomicBool,
}

// Bound-free Debug (no `I: Debug` requirement) to satisfy `missing_debug_implementations`.
impl<I> core::fmt::Debug for ValidatorDaemon<I> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ValidatorDaemon").finish_non_exhaustive()
    }
}

impl<I> ValidatorDaemon<I>
where
    I: io::ValidatorIo + 'static,
{
    /// Cadence of the repeated `BLOCK NR = <n>` head status when the chain is not moving.
    const BLOCK_NR_STATUS: Duration = Duration::from_secs(10);
    /// Loads the validator section and keystore. Returns `Ok(None)` when `SEAL` is absent or
    /// false; a malformed validator section is an error (fail-closed at boot).
    pub async fn load(config_path: PathBuf, chain_id: u64, io: I) -> eyre::Result<Option<Self>> {
        let cfg = config::load_validator_config(&config_path)?;
        if !cfg.enabled {
            tracing::info!(
                target: "smartnet_validator",
                event = "VALIDATOR_DISABLED",
                config = %config_path.display(),
                "validator section is absent or SEAL = false — merged validator stays off"
            );
            return Ok(None);
        }

        let password = keystore_util::resolve_password_for_run(cfg.password_file.as_deref())?;
        let key_path = cfg.keystore_path()?;
        let secret_key =
            keystore_util::load_secret_from_keystore(&key_path, &password).map_err(|status| {
                match status {
                    outcome::ValidatorStatus::Failed { message } => eyre::eyre!(message),
                    other => eyre::eyre!("{other:?}"),
                }
            })?;

        let signer = run_once::signer_address(&secret_key);
        let heartbeat_ms = cfg.heartbeat_ms;
        // The daemon re-checks on the next beat anyway, so the post-import canonical probe is
        // bounded by one heartbeat: a slow promotion is benign (`IMPORT_CANONICAL_STALLED`) and
        // the next beat observes the advanced head. This folds the old INTERVAL /
        // CANONICAL_PROBE_* keys into the single heartbeat knob.
        let probe_interval_ms = (heartbeat_ms / 10).max(1);
        // Equivocation-guard state (XCURR D-2): shared write-through with the seal pass —
        // the beat reads it to refuse re-sealing while the head lags the last seal.
        let sealed_height = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let args = run_once::RunOnceArgs {
            coinbase: signer,
            fee_receiver: signer,
            secret_key,
            dump_payload: None,
            allow_empty: cfg.allow_empty,
            chain_id,
            canonical_probe_timeout_ms: heartbeat_ms,
            canonical_probe_interval_ms: probe_interval_ms,
            canonical_probe_max_interval_ms: heartbeat_ms.max(probe_interval_ms),
            selection_caps: cfg.selection_caps,
            txpool_val_max_txs: cfg.txpool_val_max_txs,
            sealed_height,
            in_flight: std::sync::Arc::new(AtomicBool::new(false)),
        };

        tracing::info!(
            target: "smartnet_validator",
            event = "VALIDATOR_DAEMON_STARTED",
            chain_id,
            signer = %signer,
            heartbeat_ms,
            txpool_val_max_txs = cfg.txpool_val_max_txs,
            config = %config_path.display(),
            "merged validator armed for the node heartbeat (one keystore = node ID + coinbase)"
        );

        Ok(Some(Self {
            io,
            args,
            last_block_nr: AtomicU64::new(u64::MAX),
            last_block_print: Mutex::new(Instant::now()),
            busy: AtomicBool::new(false),
        }))
    }

    /// Runs one seal pass. The caller owns the cadence (`HEARTBEAT_MS`) — the trigger
    /// may fire too fast or too slow; the `VAL_BUSY` lane decides, not the trigger. A
    /// trigger that arrives while a pass is in progress is coalesced (logged once at
    /// debug), and the lane clears on every exit path — the validator can never be
    /// left permanently busy by a failed pass.
    pub async fn beat(&self) {
        if self.busy.swap(true, Ordering::AcqRel) {
            tracing::debug!(
                target: "smartnet_validator",
                event = "VAL_BUSY",
                "validation pass still in progress — trigger coalesced"
            );
            return;
        }
        let lane = BusyLane(&self.busy); // clears on every exit, including unwind

        // Equivocation guard (XCURR D-2): while the canonical head hasn't caught up to
        // the last seal we put on the wire, sealing again on the stale head would produce
        // a *different* block at the same height from this key (self-equivocation under
        // slow promotion). Wait this beat instead — the network waits, nothing is lost.
        // Head unreadable: proceed — the pass re-reads it and reports its own error.
        if let Ok(Some(block)) = self
            .io
            .block_by_number(alloy_rpc_types_eth::BlockNumberOrTag::Latest)
            .await
        {
            let last = self.args.sealed_height.load(Ordering::Acquire);
            if last != 0 && block.header.number < last {
                tracing::debug!(
                    target: "smartnet_validator",
                    event = "VAL_WAIT_CANONICAL",
                    head = block.header.number,
                    last_sealed = last,
                    "head hasn't promoted the last sealed block — skipping this beat"
                );
                drop(lane);
                return;
            }
        }

        let _ = run_once::run_once(&self.io, &self.args).await.finish();
        drop(lane);
        self.log_block_nr().await;
    }

    /// The heartbeat's only line: `BLOCK NR = <n>` on every new canonical head, repeated at most
    /// once per [`Self::BLOCK_NR_STATUS`] while the head stands still — silent in between, so an
    /// idle `VERB=1` log carries block numbers and nothing else.
    async fn log_block_nr(&self) {
        match self.io.block_by_number(alloy_rpc_types_eth::BlockNumberOrTag::Latest).await {
            Ok(Some(block)) => {
                let nr = block.header.number;
                let changed = self.last_block_nr.swap(nr, Ordering::Relaxed) != nr;
                let mut last =
                    self.last_block_print.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let should_print = changed || last.elapsed() >= Self::BLOCK_NR_STATUS;
                if should_print {
                    *last = Instant::now();
                }
                drop(last);
                if should_print {
                    tracing::info!(target: "smartnet_validator", "BLOCK NR = {nr}");
                }
            }
            Ok(None) => {}
            Err(err) => {
                tracing::debug!(target: "smartnet_validator", %err, "block head probe failed")
            }
        }
    }
}

/// Drop guard for the `VAL_BUSY` lane: clears the flag when the pass ends — normal
/// return, early `?`, or panic unwind. The validator must never stay permanently busy.
struct BusyLane<'a>(&'a AtomicBool);

impl Drop for BusyLane<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    #[test]
    fn verb_to_level_maps_ranks_and_defaults() {
        assert_eq!(verb_to_level(Some(0)), LevelFilter::ERROR);
        assert_eq!(verb_to_level(Some(1)), LevelFilter::INFO);
        assert_eq!(verb_to_level(Some(2)), LevelFilter::DEBUG);
        assert_eq!(verb_to_level(Some(3)), LevelFilter::TRACE);
        // Unset or out-of-range keeps the default (info).
        assert_eq!(verb_to_level(None), LevelFilter::INFO);
        assert_eq!(verb_to_level(Some(9)), LevelFilter::INFO);
    }

    /// The lane is single: a trigger that fires while a pass is in progress is
    /// coalesced away (`VAL_BUSY`) — never a second concurrent pass — and the lane
    /// clears once the active pass ends.
    #[tokio::test]
    async fn busy_lane_coalesces_triggers_during_an_active_pass() {
        use crate::io::ValidatorIo;
        use alloy_primitives::Address;
        use std::sync::Arc;

        struct SlowIo;
        impl ValidatorIo for SlowIo {
            fn label(&self) -> &str {
                "slow-io"
            }
            async fn chain_id(&self) -> eyre::Result<u64> {
                tokio::time::sleep(Duration::from_millis(200)).await; // the active-pass window
                Ok(315)
            }
            async fn block_by_number(
                &self,
                _tag: alloy_rpc_types_eth::BlockNumberOrTag,
            ) -> eyre::Result<Option<alloy_rpc_types_eth::Block<alloy_rpc_types_eth::Transaction>>>
            {
                eyre::bail!("stop the pass");
            }
            async fn block_by_hash(
                &self,
                _hash: alloy_primitives::B256,
            ) -> eyre::Result<Option<alloy_rpc_types_eth::Block<alloy_rpc_types_eth::Transaction>>>
            {
                eyre::bail!("stop the pass");
            }
            async fn txpool_content(
                &self,
            ) -> eyre::Result<alloy_rpc_types_txpool::TxpoolContent<
                alloy_rpc_types_eth::Transaction,
            >> {
                eyre::bail!("stop the pass");
            }
            async fn txpool_arrival(
                &self,
            ) -> eyre::Result<std::collections::BTreeMap<alloy_primitives::TxHash, Instant>> {
                eyre::bail!("stop the pass");
            }
            async fn txpool_status(&self) -> eyre::Result<serde_json::Value> {
                eyre::bail!("stop the pass");
            }
            async fn balances(
                &self,
                _owners: &[alloy_primitives::Address],
            ) -> Vec<eyre::Result<alloy_primitives::U256>> {
                Vec::new()
            }
            async fn erc20_balances(
                &self,
                _pairs: &[(alloy_primitives::Address, alloy_primitives::Address)],
            ) -> Vec<eyre::Result<alloy_primitives::U256>> {
                Vec::new()
            }
            async fn nonces(
                &self,
                _owners: &[alloy_primitives::Address],
            ) -> Vec<eyre::Result<u64>> {
                Vec::new()
            }
            async fn simulate_v1(
                &self,
                _payload: alloy_rpc_types_eth::simulate::SimulatePayload,
            ) -> eyre::Result<
                Vec<
                    alloy_rpc_types_eth::simulate::SimulatedBlock<
                        alloy_rpc_types_eth::Block<alloy_rpc_types_eth::Transaction>,
                    >,
                >,
            > {
                eyre::bail!("stop the pass");
            }
            async fn submit_block_rlp(&self, _rlp: alloy_primitives::Bytes) -> eyre::Result<serde_json::Value> {
                eyre::bail!("stop the pass");
            }
        }

        let daemon = Arc::new(ValidatorDaemon {
            io: SlowIo,
            args: run_once::RunOnceArgs {
                coinbase: Address::ZERO,
                fee_receiver: Address::ZERO,
                secret_key: k256::ecdsa::SigningKey::from_slice(&[1u8; 32]).expect("valid scalar"),
                dump_payload: None,
                allow_empty: false,
                chain_id: 315,
                canonical_probe_timeout_ms: 50,
                canonical_probe_interval_ms: 1,
                canonical_probe_max_interval_ms: 50,
                selection_caps: Default::default(),
                txpool_val_max_txs: 16,
                sealed_height: std::sync::Arc::new(AtomicU64::new(0)),
                in_flight: std::sync::Arc::new(AtomicBool::new(false)),
            },
            last_block_nr: AtomicU64::new(u64::MAX),
            last_block_print: Mutex::new(Instant::now()),
            busy: AtomicBool::new(false),
        });

        let first = tokio::spawn({
            let daemon = daemon.clone();
            async move { daemon.beat().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await; // first pass inside chain_id's window
        let coalesced_at = Instant::now();
        daemon.beat().await; // second trigger while VAL_BUSY — must return at once
        assert!(
            coalesced_at.elapsed() < Duration::from_millis(150),
            "trigger during an active pass must coalesce, not queue or fork"
        );
        first.await.expect("first beat");
        assert!(!daemon.busy.load(Ordering::Acquire), "lane must clear after the pass");
    }

    /// Equivocation guard (XCURR D-2): while the canonical head lags the last sealed
    /// height, the beat returns before `run_once` — no second block can be sealed on the
    /// stale head. Once the head has caught up (or nothing was ever sealed), the pass runs.
    #[tokio::test]
    async fn beat_waits_for_promotion_before_sealing_again() {
        use crate::io::ValidatorIo;
        use alloy_primitives::Address;
        use std::sync::Arc;

        /// Reports a fixed head; any pass-level call after the guard flips `entered`.
        struct HeadIo {
            head: u64,
            entered: AtomicBool,
        }
        impl ValidatorIo for HeadIo {
            fn label(&self) -> &str {
                "head-io"
            }
            async fn chain_id(&self) -> eyre::Result<u64> {
                self.entered.store(true, Ordering::Release);
                Ok(315)
            }
            async fn block_by_number(
                &self,
                tag: alloy_rpc_types_eth::BlockNumberOrTag,
            ) -> eyre::Result<
                Option<alloy_rpc_types_eth::Block<alloy_rpc_types_eth::Transaction>>,
            > {
                if matches!(tag, alloy_rpc_types_eth::BlockNumberOrTag::Latest) {
                    let mut block =
                        <alloy_rpc_types_eth::Block<alloy_rpc_types_eth::Transaction> as Default>::default();
                    block.header.inner.number = self.head;
                    return Ok(Some(block));
                }
                Ok(None)
            }
            async fn block_by_hash(
                &self,
                _hash: alloy_primitives::B256,
            ) -> eyre::Result<
                Option<alloy_rpc_types_eth::Block<alloy_rpc_types_eth::Transaction>>,
            > {
                self.entered.store(true, Ordering::Release);
                eyre::bail!("pass entered");
            }
            async fn txpool_content(
                &self,
            ) -> eyre::Result<
                alloy_rpc_types_txpool::TxpoolContent<alloy_rpc_types_eth::Transaction>,
            > {
                self.entered.store(true, Ordering::Release);
                eyre::bail!("pass entered");
            }
            async fn txpool_arrival(
                &self,
            ) -> eyre::Result<std::collections::BTreeMap<alloy_primitives::TxHash, Instant>>
            {
                self.entered.store(true, Ordering::Release);
                eyre::bail!("pass entered");
            }
            async fn txpool_status(&self) -> eyre::Result<serde_json::Value> {
                self.entered.store(true, Ordering::Release);
                eyre::bail!("pass entered");
            }
            async fn balances(
                &self,
                _owners: &[alloy_primitives::Address],
            ) -> Vec<eyre::Result<alloy_primitives::U256>> {
                self.entered.store(true, Ordering::Release);
                Vec::new()
            }
            async fn erc20_balances(
                &self,
                _pairs: &[(alloy_primitives::Address, alloy_primitives::Address)],
            ) -> Vec<eyre::Result<alloy_primitives::U256>> {
                Vec::new()
            }
            async fn nonces(
                &self,
                _owners: &[alloy_primitives::Address],
            ) -> Vec<eyre::Result<u64>> {
                Vec::new()
            }
            async fn simulate_v1(
                &self,
                _payload: alloy_rpc_types_eth::simulate::SimulatePayload,
            ) -> eyre::Result<
                Vec<
                    alloy_rpc_types_eth::simulate::SimulatedBlock<
                        alloy_rpc_types_eth::Block<alloy_rpc_types_eth::Transaction>,
                    >,
                >,
            > {
                self.entered.store(true, Ordering::Release);
                eyre::bail!("pass entered");
            }
            async fn submit_block_rlp(
                &self,
                _rlp: alloy_primitives::Bytes,
            ) -> eyre::Result<serde_json::Value> {
                self.entered.store(true, Ordering::Release);
                eyre::bail!("pass entered");
            }
        }

        let make_daemon = |head: u64, last_sealed: u64| {
            Arc::new(ValidatorDaemon {
                io: HeadIo { head, entered: AtomicBool::new(false) },
                args: run_once::RunOnceArgs {
                    coinbase: Address::ZERO,
                    fee_receiver: Address::ZERO,
                    secret_key: k256::ecdsa::SigningKey::from_slice(&[1u8; 32])
                        .expect("valid scalar"),
                    dump_payload: None,
                    allow_empty: false,
                    chain_id: 315,
                    canonical_probe_timeout_ms: 50,
                    canonical_probe_interval_ms: 1,
                    canonical_probe_max_interval_ms: 50,
                    selection_caps: Default::default(),
                    txpool_val_max_txs: 16,
                    sealed_height: std::sync::Arc::new(AtomicU64::new(last_sealed)),
                    in_flight: std::sync::Arc::new(AtomicBool::new(false)),
                },
                last_block_nr: AtomicU64::new(u64::MAX),
                last_block_print: Mutex::new(Instant::now()),
                busy: AtomicBool::new(false),
            })
        };

        // Head (5) behind the last seal (7): the beat must skip before the pass runs.
        let waiting = make_daemon(5, 7);
        waiting.beat().await;
        assert!(
            !waiting.io.entered.load(Ordering::Acquire),
            "a head behind the last sealed block must skip the pass entirely (no re-seal)"
        );
        assert!(!waiting.busy.load(Ordering::Acquire), "lane must clear on the skip path");

        // Head caught up (7 == 7): the guard releases and the pass runs.
        let caught = make_daemon(7, 7);
        caught.beat().await;
        assert!(
            caught.io.entered.load(Ordering::Acquire),
            "head at the last sealed height must proceed to the pass"
        );

        // Nothing sealed yet (0): no guard, the pass runs.
        let fresh = make_daemon(3, 0);
        fresh.beat().await;
        assert!(
            fresh.io.entered.load(Ordering::Acquire),
            "with no prior seal the guard must not block"
        );
    }

    #[test]
    fn parse_b256_accepts_hex_and_rejects_garbage() {
        let full = B256::repeat_byte(0xab);
        assert_eq!(parse_b256(&format!("{full:#x}")).unwrap(), full);
        assert!(parse_b256("0x1234").is_err(), "short hash must be rejected");
        assert!(parse_b256("nothex").is_err(), "garbage must be rejected");
    }

    /// An explicit `--config` always wins over the default `~/.smartnet/...` path.
    #[test]
    fn resolved_config_prefers_the_explicit_flag() {
        let cli = Cli {
            command: None,
            config: Some(std::path::PathBuf::from("/explicit/config.toml")),
            dump_payload: None,
        };
        assert_eq!(
            cli.resolved_config().unwrap(),
            std::path::PathBuf::from("/explicit/config.toml")
        );
    }

    /// `keystore` carries both paths (`--key`, `--password`) — the only key-change interface.
    #[test]
    fn keystore_subcommand_parses_key_and_password() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "smartnet-validator",
            "keystore",
            "--key",
            "/tmp/smartnet/validator/key",
            "--password",
            "/home/u/.smartnet/validator/pw.txt",
        ])
        .expect("keystore with --key and --password");
        match cli.command {
            Some(Commands::Keystore(k)) => {
                assert_eq!(k.key, std::path::PathBuf::from("/tmp/smartnet/validator/key"));
                assert_eq!(
                    k.password,
                    std::path::PathBuf::from("/home/u/.smartnet/validator/pw.txt")
                );
            }
            other => panic!("expected keystore, got {other:?}"),
        }
    }

    /// Both flags are required: keys or a password path cannot be omitted.
    #[test]
    fn keystore_requires_key_and_password() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["smartnet-validator", "keystore"]).is_err());
        assert!(Cli::try_parse_from(["smartnet-validator", "keystore", "--key", "/tmp/k"]).is_err());
        assert!(Cli::try_parse_from(["smartnet-validator", "keystore", "--password", "/tmp/pw"])
            .is_err());
        // The old random-key `keystore` and positional `import` no longer exist.
        assert!(Cli::try_parse_from(["smartnet-validator", "keystore", "import", "/x"]).is_err());
    }
}

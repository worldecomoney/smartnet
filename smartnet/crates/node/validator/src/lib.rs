//! `SmartNet` validator — merged into the node; the node heartbeat calls its seal beat.

// eyre::bail!/ensure! expansions end in a `;` we do not control (third-party macro).

/// Default heartbeat when `NODE_HEARTBEAT` is absent — **500 ms**, advised alike for a
/// plain node and a validator (a bridge may run 2000; everything is adjustable).
pub const DEFAULT_HEARTBEAT_MS: u64 = 500;

pub mod app;
pub mod balances;
pub mod block_rlp_wire;
pub mod chair;
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

    /// Operator config (`KEY_PATH`, `PASSWORD_FILE`, node-level `COINBASE` keystore selector,
    /// `SEAL`, `ALLOW_EMPTY`, …). `COINBASE` is any node's selector and only picks the key file;
    /// the fee receiver is derived from the decrypted key. Explicit path is used verbatim;
    /// otherwise search `./config.toml`, then `~/.smartnet/config.toml`.
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Write sealed block RLP to PATH (`smartnet-test-util` probes).
    #[arg(long = "dump-payload", value_name = "PATH")]
    pub dump_payload: Option<PathBuf>,
}

/// Validator operator subcommands.
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Write an encrypted keystore into the config's `KEY_PATH`. The optional positional file
    /// is a plaintext private key (deleted after success); without it a fresh key is generated.
    Keystore(KeystoreCli),
    /// Fund an account through the canonical seed-transaction flow.
    SeedTx,
    /// Probe node liveness with a forkchoice update (read-only diagnostic).
    Ping(PingCli),
}

/// Arguments for the `keystore` subcommand.
///
/// Usage: `keystore [RAW_PRIVATE_KEY_FILE] --config CONFIG`. There are no `--key` or
/// `--password` flags: `KEY_PATH` and `PASSWORD_FILE` are read from the named config. The
/// raw-key file is optional; without it a random secp256k1 key is generated.
#[derive(clap::Args, Debug)]
pub struct KeystoreCli {
    /// Optional file holding the 64-hex-character private key (`0x` prefix optional, any
    /// case). Deleted only after the encrypted keystore is written. Omit to generate a key.
    #[arg(value_name = "RAW_PRIVATE_KEY_FILE")]
    pub private_key_file: Option<PathBuf>,
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

/// Resolve the operator config path: explicit `--config` verbatim, else `./config.toml`, then
/// `~/.smartnet/config.toml`; all three missing is an error.
///
/// Re-exported from `smartnet-fs-util` so the node CLI, extensions and this crate all apply one
/// rule. `_with` takes the environment as arguments so the order can be tested in isolation.
pub use smartnet_fs_util::runtime_paths::{resolve_config_path, resolve_config_path_with};

impl Cli {
    /// Returns the resolved config path: explicit `--config`, else the two-step scaffolding.
    pub fn resolved_config(&self) -> eyre::Result<PathBuf> {
        resolve_config_path(self.config.as_deref())
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

    // Resolve log verbosity with the same global config rule as every command (optional
    // `VERB`, defaults to 1).
    let verb = cli
        .resolved_config()
        .ok()
        .and_then(|path| crate::config::load_validator_config(&path).ok())
        .and_then(|cfg| cfg.verb);
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

        // Headless runtime: PASSWORD_FILE is mandatory; no prompt. Only the encrypted keystore
        // and password bytes stay in RAM — no decrypted signing key is retained.
        let password_file = cfg.password_file.as_deref().ok_or_else(|| {
            eyre::eyre!("PASSWORD_FILE is required in config — headless boot never prompts")
        })?;
        // COINBASE is node-level: read it from the same config for every role. It only selects
        // the key file; the signer address is derived inside the material, never written back.
        let coinbase = smartnet_fs_util::keystore_selector::coinbase_from_config(&config_path)?;
        let key = keystore_util::KeystoreMaterial::load(&cfg.key_dir, password_file, coinbase)?;

        let signer = key.address();

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
            key,
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
            fn enabled_validators(&self) -> Vec<Address> {
                Vec::new()
            }
            fn get_val_chair(
                &self,
                _block_hash: B256,
                _ts: u64,
                _purpose: &str,
            ) -> Option<Address> {
                None
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
                key: keystore_util::test_material(
                    &k256::ecdsa::SigningKey::from_slice(&[1u8; 32]).expect("valid scalar"),
                ),
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
            fn enabled_validators(&self) -> Vec<Address> {
                Vec::new()
            }
            fn get_val_chair(
                &self,
                _block_hash: B256,
                _ts: u64,
                _purpose: &str,
            ) -> Option<Address> {
                None
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
                    key: keystore_util::test_material(
                        &k256::ecdsa::SigningKey::from_slice(&[1u8; 32]).expect("valid scalar"),
                    ),
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

    /// An explicit `--config` is used verbatim — no scaffolding, even for a missing file.
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

    /// Without `--config`, the global rule searches launch directory first, then `~/.smartnet`.
    #[test]
    fn config_search_order_is_cwd_then_home() {
        let root = std::env::temp_dir().join(format!("smartnet-config-search-{}", std::process::id()));
        let launch_dir = root.join("launch");
        let home_dir = root.join("home");
        std::fs::create_dir_all(&launch_dir).expect("launch dir");
        std::fs::create_dir_all(home_dir.join(".smartnet")).expect("smartnet dir");
        let local = launch_dir.join("config.toml");
        let installed = home_dir.join(".smartnet/config.toml");
        std::fs::write(&local, "KEY_PATH = \"/tmp/local\"").expect("local config");
        std::fs::write(&installed, "KEY_PATH = \"/tmp/home\"").expect("installed config");

        let home = home_dir.to_string_lossy().to_string();
        assert_eq!(
            resolve_config_path_with(None, Some(&launch_dir), Some(&home)).unwrap(),
            local,
            "the launch directory wins"
        );

        std::fs::remove_file(&local).expect("remove local config");
        assert_eq!(
            resolve_config_path_with(None, Some(&launch_dir), Some(&home)).unwrap(),
            installed,
            "the installed file is the second stop"
        );

        let err = resolve_config_path_with(None, Some(&launch_dir), None).unwrap_err();
        assert!(err.to_string().contains("searched"), "got: {err}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Both accepted orders: `--config` may follow the optional raw-key positional or precede
    /// the subcommand. `--key`/`--password` are gone; the config owns both paths.
    #[test]
    fn keystore_subcommand_accepts_both_config_orders() {
        use clap::Parser;
        for argv in [
            vec![
                "smartnet-validator",
                "keystore",
                "/tmp/smartnet/validator/raw.key",
                "--config",
                "/home/u/smartnet/config.toml",
            ],
            vec![
                "smartnet-validator",
                "--config",
                "/home/u/smartnet/config.toml",
                "keystore",
                "/tmp/smartnet/validator/raw.key",
            ],
        ] {
            let cli = Cli::try_parse_from(argv).expect("keystore with an explicit config");
            assert_eq!(
                cli.config,
                Some(std::path::PathBuf::from("/home/u/smartnet/config.toml"))
            );
            match cli.command {
                Some(Commands::Keystore(k)) => assert_eq!(
                    k.private_key_file,
                    Some(std::path::PathBuf::from("/tmp/smartnet/validator/raw.key"))
                ),
                other => panic!("expected keystore, got {other:?}"),
            }
        }
    }

    /// The raw-key file is optional: omitting it generates a key. Old flags are hard errors.
    #[test]
    fn keystore_key_is_optional_and_old_flags_are_gone() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "smartnet-validator",
            "--config",
            "/home/u/smartnet/config.toml",
            "keystore",
        ])
        .expect("key generation needs no positional file");
        match cli.command {
            Some(Commands::Keystore(k)) => assert_eq!(k.private_key_file, None),
            other => panic!("expected keystore, got {other:?}"),
        }
        assert!(
            Cli::try_parse_from([
                "smartnet-validator",
                "keystore",
                "--key",
                "/tmp/k",
                "--config",
                "/home/u/smartnet/config.toml",
            ])
            .is_err(),
            "--key must be rejected"
        );
        assert!(
            Cli::try_parse_from([
                "smartnet-validator",
                "keystore",
                "--password",
                "/tmp/pw",
                "--config",
                "/home/u/smartnet/config.toml",
            ])
            .is_err(),
            "--password must be rejected"
        );
    }
}

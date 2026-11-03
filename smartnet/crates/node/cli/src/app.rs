use crate::{
    cli_core::chainspec::ChainSpecParser,
    commands::{
        common::{CliComponentsBuilder, CliNodeTypes, HeaderMut},
        launcher::{FnLauncher, Launcher},
        node::{operator_datadir_args, operator_default_node_command},
    },
    interface::{Commands, NoSubCmd},
    runner::CliRunner,
    Cli,
};
use alloy_primitives::{Address, U256};
use calibri_lite_consensus::pre_consensus::SMARTNET_FIXED_GAS_PRICE_WEI;
use clap::Subcommand;
use eyre::{eyre, Result};
use smartnet_chainspec::{Chain, ChainSpec, EthChainSpec, EthereumHardforks, Hardforks};
use smartnet_db::{open_db, DatabaseBackend, DatabaseEnv};
use smartnet_node_api::NodePrimitives;
use smartnet_node_builder::{NodeBuilder, WithLaunchContext};
use smartnet_node_core::args::DatabaseArgs;
use smartnet_node_ethereum::{consensus::EthBeaconConsensus, EthEvmConfig, EthereumNode};
use smartnet_rpc_server_types::RpcModuleValidator;
use smartnet_tracing::{FileWorkerGuard, Layers};
use std::{
    fmt,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing::{info, warn};

/// Maps a VERBOSITY rank (0-4) to a tracing log-level filter string.
///
/// 0=off, 1=info, 2=debug, 3=warning, 4=errors. Emits the given level and everything more severe.
fn verbosity_to_filter(rank: u8) -> String {
    match rank {
        0 => "off".to_string(),
        1 => {
            "info,lsm_tree=warn,fjall=warn,engine::tree=warn,engine::execution_cache=warn,smartnet::storage=warn"
                .to_string()
        }
        2 => "debug".to_string(),
        3 => "warn".to_string(),
        _ => "error".to_string(),
    }
}

/// Maps `VERB` (0-3) to the process-wide tracing level: 0=errors only, 1=info, 2=debug,
/// 3=trace. `VERB` drives both the node and the merged validator; it wins over `VERBOSITY`.
const fn verb_to_filter(rank: u8) -> &'static str {
    match rank {
        // `build_env_filter` prepends target-specific `=warn` noise suppressors; override them
        // explicitly so rank 0 really is errors only, with no dependency warnings leaking in.
        0 => {
            "error,opentelemetry-otlp=off,opentelemetry_sdk=off,opentelemetry-http=off,rustls=off,tungstenite=off"
        }
        1 => {
            "info,lsm_tree=warn,fjall=warn,engine::tree=warn,engine::execution_cache=warn,smartnet::storage=warn"
        }
        2 => "debug",
        _ => "trace",
    }
}

/// Resolve the tracing filter from the shared config (`--config` or the canonical default).
/// `VERB` wins over the legacy `VERBOSITY` rank; `None` keeps the CLI default.
fn tracing_filter_from_config(explicit: Option<&Path>) -> Option<String> {
    let cfg_path =
        explicit.map(Path::to_path_buf).or_else(|| crate::interface::default_config_path().ok())?;
    if let Ok(Some(rank)) = smartnet_validator::validator_verb(&cfg_path) {
        return Some(verb_to_filter(rank).to_string());
    }
    let overlay = load_operator_overlay(&cfg_path).ok()?;
    overlay.verbosity.map(verbosity_to_filter)
}

#[derive(Debug, Default, Clone)]
pub(crate) struct OperatorConfigOverlay {
    datadir_path: Option<PathBuf>,
    http_port: Option<u16>,
    http_addr: Option<IpAddr>,
    rpc_server: Option<bool>,
    /// Temporary `txpool_*` RPC namespace (`RPC_MEMPOOL`, optional key, default off): read-only,
    /// but leaks pending-tx data and is a `DoS` surface — disable before any public launch.
    rpc_mempool: Option<bool>,
    /// Expose `web3_nodeInfo` (mesh roster) on the public RPC (`RPC_NODEINFO`, optional key,
    /// default off).
    nodeinfo: Option<bool>,
    /// Tx lifetime caps (`TXPOOL_QUEUED_TTL_SECS` / `TXPOOL_PENDING_TTL_SECS`, optional
    /// keys): queued txs expire at the queued TTL (default 8 h; TTL-POOL.md); pending txs
    /// expire only when a pending TTL > 0 is set (0 = no cap, the "until included..." rule).
    txpool_queued_ttl_secs: Option<u64>,
    txpool_pending_ttl_secs: Option<u64>,
    /// RPC batch limits (`RPC_BATCH_TX_MAX` / `RPC_BATCH_HEAVY_TX_MAX` /
    /// `RPC_BATCH_HEAVY_MAX` / `RPC_BATCH_READ_MAX`, optional keys): the per-batch class
    /// budgets of the batch-policy middleware (default 128 / 16 / 1 / 1024). Rejected
    /// entries are answered per-id, never touching pool or trie.
    batch_tx_max: Option<usize>,
    batch_heavy_tx_max: Option<usize>,
    batch_heavy_max: Option<usize>,
    batch_read_max: Option<usize>,
    /// Mempool entry floor in wei (`MIN_TO_TXPOOL_ENTRY`, optional key): a tx-ENTRY check only —
    /// the sender must hold at least this balance at head, or the tx is rejected at the RPC
    /// edge before the pool. The validator separately compares the real balance against the tx
    /// value at inclusion; signed-block import performs no checks. `0` disables the gate.
    min_to_txpool_entry: Option<u128>,
    default_suggested_fee: Option<U256>,
    /// Master switch for the in-node RPC catch-up service (`RPC_SYNC`, optional key,
    /// **default off**): the service starts only when this is `true` AND `SYNC_PEERS` is
    /// non-empty AND `SYNC_BATCH >= 1` AND `SYNC_POLL_INTERVAL` is non-zero. Absent or
    /// `false` = the catch-up loop is never started — mesh, RLP import, and the rest of
    /// the RPC are completely unaffected either way.
    rpc_sync: Option<bool>,
    /// Raw endpoint strings; parsed into `Url`s when applied to the node command.
    sync_peers: Option<Vec<String>>,
    /// Number of blocks fetched per batch round trip (`SYNC_BATCH`, required key).
    sync_batch: Option<u64>,
    /// Seconds between sync poll cycles (`SYNC_POLL_INTERVAL`, required key).
    sync_poll_interval: Option<u64>,
    /// Mesh gossip listen port (`MESH_PORT`, optional key): `0`/unset = mesh off.
    mesh_port: Option<u16>,
    /// Mesh listen address (`MESH_ADDR`, optional key): absent = `0.0.0.0` (all interfaces).
    mesh_addr: Option<IpAddr>,
    /// Mesh peer file (`MESH_PEERS_FILE`, optional key; `~`-expanded to absolute).
    mesh_peers_file: Option<PathBuf>,
    /// Max mesh `WebSocket` message size in MB (`MESH_MAX_MSG_MB`, optional key).
    /// Unset = 32 MB; `0` is rejected.
    mesh_max_msg_mb: Option<usize>,
    /// Mesh keepalive ping cadence in ms (`MESH_HEARTBEAT`, optional key).
    /// Unset = 500 ms; `0` is rejected. Silence bound = this + 300 ms.
    mesh_heartbeat_ms: Option<u64>,
    /// Validator heartbeat (`HEARTBEAT_MS`, optional key): seal beats run at this cadence.
    /// Unset = 2000 ms; `0` is rejected.
    heartbeat_ms: Option<u64>,
    /// Cross-block execution cache size in MB (`CROSS_BLOCK_CACHE_SIZE`, required key).
    cross_block_cache_size: Option<usize>,
    /// Log verbosity rank (`VERBOSITY`, 0-4): 0=off, 1=info, 2=debug, 3=warning, 4=errors.
    /// Emits the given level and everything more severe. Optional; unset keeps the CLI default.
    /// Ignored when the unified `VERB` key is set.
    verbosity: Option<u8>,
    /// Storage engine (`DB_ENGINE`, optional key, default `fjall` — the only engine;
    /// the historical `sqlite`/`redb` names fail parsing with a dedicated error): the
    /// config file dictates the engine. Every datadir path is scoped as
    /// `DATADIR_PATH/DB_ENGINE` (the engine directory is a child of whatever
    /// `DATADIR_PATH` names).
    db_engine: Option<DatabaseBackend>,
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        &s[1..s.len().saturating_sub(1)]
    } else {
        s
    }
}

/// Expand a leading `~`. `HOME` unset is an error — never silently keep the literal `~/…`
/// and never fall back to the current working directory.
fn expand_home(p: &str) -> eyre::Result<PathBuf> {
    if let Some(rest) = p.strip_prefix('~') {
        let home = std::env::var("HOME").map_err(|_| {
            eyre::eyre!(
                "config path {p:?} starts with `~` but HOME is not set — use an absolute path \
                 instead (no built-in fallback path exists)"
            )
        })?;
        return Ok(PathBuf::from(home).join(rest.trim_start_matches('/')));
    }
    Ok(PathBuf::from(p))
}

/// Parse one config value, naming the key on failure so a bad value is never mistaken
/// for a missing one.
fn parse_key<T: std::str::FromStr>(key: &str, value: &str) -> eyre::Result<T>
where
    T::Err: std::fmt::Display,
{
    strip_quotes(value)
        .parse::<T>()
        .map_err(|e| eyre::eyre!("config {key} has an invalid value {value:?}: {e}"))
}

fn parse_csv_list(input: &str) -> Vec<String> {
    let trimmed = input.trim();
    let body = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(trimmed);
    body.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| strip_quotes(s).to_string())
        .collect()
}

/// Strip a trailing `#` comment from a raw config line.
///
/// A `#` starts a comment only at the start of the line or preceded by whitespace, and only
/// outside quotes — so `SYNC_PEERS = ["http://host/rpc#v1"]` and `KEY = "a#b"` survive while
/// `KEY = value # comment` and whole-line `# comment` are cut. (The previous
/// `line.split('#')` mangled any value containing `#`.)
fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    let (mut in_single, mut in_double) = (false, false);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' if in_double => {
                i += 2;
                continue;
            }
            b'\'' if !in_double => {
                // A single quote only opens a quoted value at a token boundary; an apostrophe
                // inside a bare word (`don't`) must not swallow the trailing `#` comment.
                if in_single || i == 0 || b[i - 1].is_ascii_whitespace() || b[i - 1] == b'=' {
                    in_single = !in_single;
                }
            }
            b'"' if !in_single => in_double = !in_double,
            b'#' if !in_single && !in_double
                && (i == 0 || b[i - 1].is_ascii_whitespace()) => {
                    return &line[..i];
                }
            _ => {}
        }
        i += 1;
    }
    line
}

pub(crate) fn load_operator_overlay(path: &Path) -> eyre::Result<OperatorConfigOverlay> {
    let raw = std::fs::read_to_string(path)?;
    let mut cfg = OperatorConfigOverlay::default();
    // Every problem found is collected so an unedited config reports all of them in one
    // error instead of making the operator play whack-a-mole.
    let mut seen: Vec<&'static str> = Vec::new();
    let mut problems: Vec<String> = Vec::new();

    // Join multi-line TOML arrays: `KEY = [\n  …,\n]` becomes one logical line.
    let mut lines: Vec<String> = Vec::new();
    for line in raw.lines() {
        let stripped = strip_comment(line).trim().to_string();
        if let Some(last) = lines.last() {
            let open = last.matches('[').count() > last.matches(']').count();
            if open {
                *lines.last_mut().expect("checked non-empty") =
                    format!("{last} {}", stripped.trim_end_matches(','));
                continue;
            }
        }
        lines.push(stripped);
    }

    for line in &lines {
        let line = line.trim();
        if line.is_empty() || !line.contains('=') {
            continue;
        }
        let mut parts = line.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        if key.is_empty() {
            continue;
        }

        // The validator section lives in this same file and is parsed by `smartnet-validator`
        // (`SEAL`, `ALLOW_EMPTY`, keystore paths, heartbeat, caps). The node overlay must
        // recognize these keys so they are not reported as unknown, but it owns none of them:
        // the validator's own parser validates them (fail-closed) and the merged daemon reads
        // them at boot.
        if matches!(
            key,
            "SEAL" |
                "ALLOW_EMPTY" |
                "KEY_PATH" |
                "KEY_FILE" |
                "PASSWORD_FILE" |
                "VERB" |
                "MAX_NATIVE_PER_BLOCK" |
                "MAX_ERC_20_PER_BLOCK" |
                "MAX_CONTRACTS_PER_BLOCK" |
                "MAX_NFT_PER_BLOCK" |
                "TXPOOL_VAL_MAX_TXS"
        ) {
            continue;
        }

        // Map to the canonical key name; unknown keys are ignored (they are not part of
        // the operator contract and never cause a silent default).
        let name = match key {
            "DATADIR_PATH" => "DATADIR_PATH",
            "HTTP_PORT" => "HTTP_PORT",
            "HTTP_ADDR" => "HTTP_ADDR",
            "RPC_SERVER" => "RPC_SERVER",
            "RPC_MEMPOOL" => "RPC_MEMPOOL",
            "RPC_NODEINFO" => "RPC_NODEINFO",
            "MIN_TO_TXPOOL_ENTRY" => "MIN_TO_TXPOOL_ENTRY",
            "RPC_BATCH_TX_MAX" => "RPC_BATCH_TX_MAX",
            "RPC_BATCH_HEAVY_TX_MAX" => "RPC_BATCH_HEAVY_TX_MAX",
            "RPC_BATCH_HEAVY_MAX" => "RPC_BATCH_HEAVY_MAX",
            "RPC_BATCH_READ_MAX" => "RPC_BATCH_READ_MAX",
            "TXPOOL_QUEUED_TTL_SECS" => "TXPOOL_QUEUED_TTL_SECS",
            "TXPOOL_PENDING_TTL_SECS" => "TXPOOL_PENDING_TTL_SECS",
            "GPO_DEFAULT_SUGGESTED_FEE" => "GPO_DEFAULT_SUGGESTED_FEE",
            "RPC_SYNC" => "RPC_SYNC",
            "SYNC_PEERS" => "SYNC_PEERS",
            "SYNC_BATCH" => "SYNC_BATCH",
            "SYNC_POLL_INTERVAL" => "SYNC_POLL_INTERVAL",
            "MESH_PORT" => "MESH_PORT",
            "MESH_ADDR" => "MESH_ADDR",
            "MESH_PEERS_FILE" => "MESH_PEERS_FILE",
            "MESH_MAX_MSG_MB" => "MESH_MAX_MSG_MB",
            "MESH_HEARTBEAT" => "MESH_HEARTBEAT",
            "HEARTBEAT_MS" => "HEARTBEAT_MS",
            "CROSS_BLOCK_CACHE_SIZE" => "CROSS_BLOCK_CACHE_SIZE",
            "VERBOSITY" => "VERBOSITY",
            "DB_ENGINE" => "DB_ENGINE",
            // Unknown keys are rejected, not ignored: a typo or a leftover key from an older
            // version must fail the boot loudly (the "no silent defaults" doctrine).
            _ => {
                problems.push(format!(
                    "config has an unknown key `{key}` — remove it or fix the spelling; SmartNet \
                     rejects unknown keys (fail-closed)"
                ));
                continue;
            }
        };
        seen.push(name);

        // A key that is present but blank is a problem in its own right — there is no
        // built-in default to fall back to. (`SYNC_PEERS = []` is not blank: `[]` is an
        // explicit "nobody" and is accepted as-is.)
        if strip_quotes(value).trim().is_empty() {
            problems.push(format!(
                "config `{name}` is empty — set a value (SmartNet ships no built-in default)"
            ));
            continue;
        }

        match name {
            "DATADIR_PATH" => cfg.datadir_path = Some(expand_home(strip_quotes(value))?),
            "HTTP_PORT" => cfg.http_port = Some(parse_key(name, value)?),
            "HTTP_ADDR" => cfg.http_addr = Some(parse_key(name, value)?),
            "RPC_SERVER" => cfg.rpc_server = Some(parse_key(name, value)?),
            // Optional (absent = off, keeping existing configs booting — same pattern as
            // VERBOSITY/MESH_*). The registry hard-blocks `txpool` unless this is true.
            "RPC_MEMPOOL" => cfg.rpc_mempool = Some(parse_key(name, value)?),
            // Optional (absent = off): `web3_nodeInfo` stays disabled unless true.
            "RPC_NODEINFO" => cfg.nodeinfo = Some(parse_key(name, value)?),
            // Optional (default 0.02 WEM in wei): mempool ENTRY floor in wei — knownWallet /
            // min-balance abuse gate at the RPC edge only (never on the signed-block import
            // path; the validator re-checks the real balance against the tx value).
            "MIN_TO_TXPOOL_ENTRY" => cfg.min_to_txpool_entry = Some(parse_key(name, value)?),
            // Optional batch budgets (the batch-policy middleware; defaults 128/16/1/1024).
            "RPC_BATCH_TX_MAX" => cfg.batch_tx_max = Some(parse_key(name, value)?),
            "RPC_BATCH_HEAVY_TX_MAX" => cfg.batch_heavy_tx_max = Some(parse_key(name, value)?),
            "RPC_BATCH_HEAVY_MAX" => cfg.batch_heavy_max = Some(parse_key(name, value)?),
            "RPC_BATCH_READ_MAX" => cfg.batch_read_max = Some(parse_key(name, value)?),
            // Optional (default 8 h): non-executable, external queued txs expire after this
            // many seconds (`TTL-POOL.md`). Same rule family: entry/maintenance policy, never
            // consensus.
            "TXPOOL_QUEUED_TTL_SECS" => cfg.txpool_queued_ttl_secs = Some(parse_key(name, value)?),
            // Optional (0/off): executable pending txs older than this many seconds are swept
            // by the pool maintenance task. 0 = no cap (until included/replaced/invalidated).
            "TXPOOL_PENDING_TTL_SECS" => {
                cfg.txpool_pending_ttl_secs = Some(parse_key(name, value)?)
            }
            "GPO_DEFAULT_SUGGESTED_FEE" => {
                cfg.default_suggested_fee = Some(parse_key(name, value)?)
            }
            // Optional (default off): master switch for the in-node RPC catch-up service.
            // Absent keeps every existing config booting with sync disabled — fail-closed,
            // same pattern as RPC_MEMPOOL/RPC_NODEINFO.
            "RPC_SYNC" => cfg.rpc_sync = Some(parse_key(name, value)?),
            // Fixed allow-list of JSON-RPC sync peers (`SYNC_PEERS = ["http://host:8545", …]`).
            // Admission by operator config only; block authenticity comes from seal
            // validation at import, not from the peer. An explicit `[]` means "sync from
            // nobody" — presence is what matters, there is no built-in peer list.
            "SYNC_PEERS" => cfg.sync_peers = Some(parse_csv_list(value)),
            "SYNC_BATCH" => cfg.sync_batch = Some(parse_key(name, value)?),
            "SYNC_POLL_INTERVAL" => cfg.sync_poll_interval = Some(parse_key(name, value)?),
            // Optional mesh keys (absent = mesh off / protocol default). `MESH_PORT = 0`
            // is the explicit "no mesh listener" value and is deliberately not rejected.
            "MESH_PORT" => cfg.mesh_port = Some(parse_key(name, value)?),
            "MESH_ADDR" => cfg.mesh_addr = Some(parse_key(name, value)?),
            "MESH_MAX_MSG_MB" => cfg.mesh_max_msg_mb = Some(parse_key(name, value)?),
            "MESH_HEARTBEAT" => cfg.mesh_heartbeat_ms = Some(parse_key(name, value)?),
            "HEARTBEAT_MS" => cfg.heartbeat_ms = Some(parse_key(name, value)?),
            "MESH_PEERS_FILE" => match expand_home(strip_quotes(value)) {
                Ok(path) => cfg.mesh_peers_file = Some(path),
                // Collected (not `?`): a `~` with HOME unset must join the full problem
                // report instead of aborting the load before the other keys are checked.
                Err(err) => problems.push(format!("config `{name}`: {err}")),
            },
            "CROSS_BLOCK_CACHE_SIZE" => cfg.cross_block_cache_size = Some(parse_key(name, value)?),
            "VERBOSITY" => {
                let rank = strip_quotes(value)
                    .parse::<u8>()
                    .map_err(|e| eyre::eyre!("config VERBOSITY is not a number ({value}): {e}"))?;
                if rank > 4 {
                    return Err(eyre::eyre!(
                        "config VERBOSITY must be 0-4 (got {rank}): 0=off,1=info,2=debug,3=warning,4=errors"
                    ));
                }
                cfg.verbosity = Some(rank);
            }
            // Optional engine key (absent = `fjall`, keeping existing configs booting —
            // same pattern as VERBOSITY/MESH_*). An invalid name is a hard config error:
            // config dictates the engine, and a typo must never fall through to a
            // different engine than the operator intended.
            "DB_ENGINE" => cfg.db_engine = Some(parse_key::<DatabaseBackend>(name, value)?),
            // WS_* are not valid keys at all: WS is gone entirely (never RPC, mesh will
            // use its own transport; resurrection recipe: WS.md at the repo root). A
            // leftover WS key is a hard config error, same as any typo.
            _ => {}
        }
    }

    // Config only, no built-in defaults: every key the node reads must be present. A typo
    // surfaces here as a missing key rather than silently keeping a Rust-side default
    // (`Ipv4Addr::LOCALHOST`, port 8545, …).
    //
    // The chain id and the validator set are not config keys at all: the database is the
    // single source of truth — `smartnet init <genesis>` writes both into it and every boot
    // reads them back from there (no config fallback; a boot on a database without those
    // values exits with a distinct code naming `smartnet init`).
    const REQUIRED: &[&str] = &[
        "DATADIR_PATH",
        "HTTP_PORT",
        "HTTP_ADDR",
        "GPO_DEFAULT_SUGGESTED_FEE",
        "SYNC_PEERS",
        "SYNC_BATCH",
        "SYNC_POLL_INTERVAL",
        "CROSS_BLOCK_CACHE_SIZE",
        "MESH_PORT",
        "MESH_ADDR",
    ];
    let missing: Vec<&str> = REQUIRED.iter().copied().filter(|k| !seen.contains(k)).collect();
    if !missing.is_empty() {
        problems.push(format!(
            "missing required key(s): {} (template: smartnet/example.config.toml)",
            missing.join(", ")
        ));
    }

    // Value sanity checks: an insane value aborts startup instead of silently falling
    // through to a default. A bad config must never produce a node running against the
    // wrong chain, an unreachable port, or an under-pinned gas price.
    if let Some(datadir) = cfg.datadir_path.as_ref()
        && !datadir.is_absolute() {
            problems.push(format!(
                "DATADIR_PATH is relative ({}) — an absolute path is required; resolving it \
                 against the current working directory would be a hidden default",
                datadir.display()
            ));
        }
    {
        let (name, port) = ("HTTP_PORT", cfg.http_port);
        if port == Some(0) {
            problems.push(format!("{name} is 0 — that port would not be reachable"));
        }
    }
    if let Some(fee) = cfg.default_suggested_fee
        && fee.is_zero() {
            problems.push(
                "GPO_DEFAULT_SUGGESTED_FEE is 0 — an under-pinned gas price produces \
                 unexecutable transactions"
                    .to_string(),
            );
        }
    // `RPC_SYNC` (default off) is the master switch: without it the catch-up service can
    // never start, so the busy-loop check below only applies when sync can actually run.
    // `SYNC_BATCH = 0` is the operator's explicit "do not sync" switch on top of that
    // (same idea as `SYNC_PEERS = []`): it is accepted, and `spawn_rpc_sync_service` never
    // starts the catch-up loop. The node then advances only from blocks validators submit
    // via `engine_newPayload`. `RPC_SYNC` itself is optional (absent = off); `SYNC_BATCH`
    // and `SYNC_POLL_INTERVAL` remain required — 0 is a value, not an absence.
    // The poll interval only matters while sync is enabled; a 0 there would busy-loop.
    if cfg.rpc_sync.unwrap_or(false) &&
        cfg.sync_batch != Some(0) &&
        cfg.sync_poll_interval == Some(0)
    {
        problems.push(
            "SYNC_POLL_INTERVAL is 0 — would busy-loop (set SYNC_BATCH = 0 or RPC_SYNC = false \
             to switch sync off)"
                .to_string(),
        );
    }
    // The mesh is always on: `MESH_PORT` and `MESH_ADDR` are required and the port must be
    // a reachable one (>= 1024). The RPC is the optional half of the pair — it can be off
    // while the mesh stays up (injection is in-process, no socket). The listener port must
    // not collide with any other listener, and the peer file must be absolute — a
    // cwd-relative path would silently change meaning with the launch directory.
    if cfg.heartbeat_ms == Some(0) {
        problems.push(
            "HEARTBEAT_MS is 0 — must be at least 1 (it drives the validator seal beats)"
                .to_string(),
        );
    }
    if cfg.mesh_max_msg_mb == Some(0) {
        problems.push(
            "MESH_MAX_MSG_MB is 0 — must be at least 1 (it is the per-frame size cap in MB)"
                .to_string(),
        );
    }
    if cfg.mesh_heartbeat_ms == Some(0) {
        problems.push(
            "MESH_HEARTBEAT is 0 — must be at least 1 (it is the mesh keepalive ping \
             cadence in ms; the silence bound derives as it + 300 ms)"
                .to_string(),
        );
    }
    if let Some(port) = cfg.mesh_port {
        if port < 1024 {
            problems.push(format!(
                "MESH_PORT ({port}) is below 1024 — unprivileged ports only (the mesh is \
                 always on; pick a port >= 1024)"
            ));
        }
        {
            let (name, other) = ("HTTP_PORT", cfg.http_port);
            if other == Some(port) {
                problems.push(format!(
                    "MESH_PORT ({port}) collides with {name} — the mesh listener and \
                     {name} cannot share a port"
                ));
            }
        }
    }
    if let Some(file) = cfg.mesh_peers_file.as_ref()
        && !file.is_absolute() {
            problems.push(format!(
                "MESH_PEERS_FILE is relative ({}) — an absolute path is required (use \
                 `~/.smartnet/mainnet.jsonl`; there is no cwd fallback)",
                file.display()
            ));
        }
    if cfg.cross_block_cache_size == Some(0) {
        problems.push(
            "CROSS_BLOCK_CACHE_SIZE is 0 — set the cross-block execution cache size in MB"
                .to_string(),
        );
    }

    if !problems.is_empty() {
        eyre::bail!(
            "operator config `{}` is not usable ({} problem(s)):\n  - {}\nSmartNet ships no \
             built-in defaults for operator config — fill in every key above (template: \
             smartnet/example.config.toml)",
            path.display(),
            problems.len(),
            problems.join("\n  - ")
        );
    }

    Ok(cfg)
}

/// The chain identity + genesis spec as stored in the database (the single source of truth).
#[derive(Debug, Default)]
pub(crate) struct DatabaseChainIdentity {
    /// `CHAIN_ID` row — written at `smartnet init`.
    pub(crate) chain_id: Option<u64>,
    /// `VALIDATORS` row — written at `smartnet init` when the genesis carries a signer section;
    /// a JSON array of 0x addresses, index 0 the default validator.
    pub(crate) validators: Option<Vec<Address>>,
    /// `GENESIS_SPEC` row — the exact `Genesis` the file declared at init; the boot rebuilds
    /// the runtime rulebook from it.
    pub(crate) genesis_spec: Option<alloy_genesis::Genesis>,
}

/// Reads the chain identity and the full genesis spec straight from the database the boot would
/// use.
///
/// The database is the single source of truth for the chain id, the validator set, and the
/// genesis spec (the exact `Genesis` the genesis file declared at `smartnet init`, from which the
/// boot rebuilds the runtime rulebook) — there is no config fallback and no built-in chain data.
/// Returns `None` when the database was never initialised (`smartnet init` was not run), cannot
/// be opened with the configured engine, or carries no `Settings` rows; callers turn `None` into
/// a fail-closed boot error naming `smartnet init`.
pub(crate) fn read_chain_identity_from_db(
    overlay: &OperatorConfigOverlay,
) -> eyre::Result<Option<DatabaseChainIdentity>> {
    let Some(datadir) = overlay.datadir_path.as_ref() else {
        // The required-key check reports a missing `DATADIR_PATH` on its own.
        return Ok(None);
    };
    let db_args = DatabaseArgs { engine: overlay.db_engine.unwrap_or_default(), ..DatabaseArgs::default() };
    let db_path = {
        let mut datadir_args = operator_datadir_args(datadir.clone());
        datadir_args.engine = db_args.engine;
        datadir_args.resolve_datadir(Chain::mainnet()).db()
    };
    // Fjall has no read-only open, so this opens read-write, reads the rows, and drops the
    // handle before the node opens the database for itself. It must NEVER create the database:
    // the init-absence check is the same one `init_db` uses — `is_database_empty` on the engine
    // directory.
    let db_dir = db_path.join(db_args.engine.subdir());
    if smartnet_db::is_database_empty(&db_dir) {
        return Ok(None);
    }
    match open_db(&db_path, db_args.database_args()) {
        Ok(db) => {
            let (chain_id, validators) =
                smartnet_provider::init::read_network_settings(&db).unwrap_or((None, None));
            Ok(Some(DatabaseChainIdentity {
                chain_id,
                validators,
                genesis_spec: smartnet_provider::init::read_genesis_spec(&db)?,
            }))
        }
        Err(err) => {
            warn!(
                target: "smartnet::cli", %err, path = ?db_path,
                "could not open the database to read the chain identity; \
                 the boot resolves no chain identity and fails"
            );
            Ok(None)
        }
    }
}

/// A wrapper around a parsed CLI that handles command execution.
#[derive(Debug)]
pub struct CliApp<
    Spec: ChainSpecParser,
    Ext: clap::Args + fmt::Debug + Default,
    Rpc: RpcModuleValidator,
    SubCmd: Subcommand + fmt::Debug = NoSubCmd,
> {
    cli: Cli<Spec, Ext, Rpc, SubCmd>,
    runner: Option<CliRunner>,
    layers: Option<Layers>,
    guard: Option<FileWorkerGuard>,
}

impl<C, Ext, Rpc, SubCmd> CliApp<C, Ext, Rpc, SubCmd>
where
    C: ChainSpecParser,
    C::ChainSpec: EthChainSpec,
    Ext: clap::Args + fmt::Debug + Default,
    Rpc: RpcModuleValidator,
    SubCmd: ExtendedCommand + Subcommand + fmt::Debug,
{
    pub(crate) fn new(cli: Cli<C, Ext, Rpc, SubCmd>) -> Self {
        Self { cli, runner: None, layers: Some(Layers::new()), guard: None }
    }

    /// Sets the runner for the CLI commander.
    ///
    /// This replaces any existing runner with the provided one.
    pub fn set_runner(&mut self, runner: CliRunner) {
        self.runner = Some(runner);
    }

    /// Access to tracing layers.
    ///
    /// Returns a mutable reference to the tracing layers, or error
    /// if tracing initialized and layers have detached already.
    pub fn access_tracing_layers(&mut self) -> Result<&mut Layers> {
        self.layers.as_mut().ok_or_else(|| eyre!("Tracing already initialized"))
    }

    /// Execute the configured CLI command.
    pub fn run(self, launcher: impl Launcher<C, Ext>) -> Result<()>
    where
        C: ChainSpecParser<ChainSpec = ChainSpec>,
        C::ChainSpec: EthChainSpec,
    {
        let components = |spec: Arc<ChainSpec>| {
            (EthEvmConfig::ethereum(spec.clone()), Arc::new(EthBeaconConsensus::new(spec)))
        };

        self.run_with_components::<EthereumNode>(components, async move |builder, ext| {
            launcher.entrypoint(builder, ext).await
        })
    }

    /// Execute the configured CLI command with the provided [`CliComponentsBuilder`].
    pub fn run_with_components<N>(
        mut self,
        components: impl CliComponentsBuilder<N>,
        launcher: impl AsyncFnOnce(
            WithLaunchContext<NodeBuilder<DatabaseEnv, C::ChainSpec>>,
            Ext,
        ) -> Result<()>,
    ) -> Result<()>
    where
        N: CliNodeTypes<Primitives: NodePrimitives<BlockHeader: HeaderMut>, ChainSpec: Hardforks>,
        C: ChainSpecParser<ChainSpec = N::ChainSpec>,
        C::ChainSpec: EthChainSpec,
    {
        let runner = match self.runner.take() {
            Some(runner) => runner,
            None => CliRunner::try_with_runtime_config(smartnet_tasks::RuntimeConfig::default())?,
        };

        // Resolve log verbosity from the shared config before tracing — including when
        // `--config` is absent (the canonical default path is the same one the commands use).
        // `VERB` is the single node+validator level and wins over the legacy `VERBOSITY` rank.
        if let Some(filter) = tracing_filter_from_config(self.cli.config_path.as_deref()) {
            self.cli.logs.log_stdout_filter = filter.clone();
            self.cli.logs.log_file_filter = filter;
        }

        self.init_tracing(&runner)?;

        // Deprioritize background threads spawned by tracing/OTel libraries.
        smartnet_tasks::utils::deprioritize_background_threads();

        run_commands_with::<C, Ext, Rpc, N, SubCmd>(self.cli, runner, components, launcher)
    }

    /// Initializes tracing with the configured options.
    ///
    /// See [`Cli::init_tracing`] for more information.
    pub fn init_tracing(&mut self, runner: &CliRunner) -> Result<()> {
        if let Some(layers) = self.layers.take() {
            self.guard = self.cli.init_tracing(runner, layers)?;
        }

        Ok(())
    }
}

/// Run CLI commands with the provided runner, components and launcher.
pub(crate) fn run_commands_with<C, Ext, Rpc, N, SubCmd>(
    cli: Cli<C, Ext, Rpc, SubCmd>,
    runner: CliRunner,
    _components: impl CliComponentsBuilder<N>,
    launcher: impl AsyncFnOnce(
        WithLaunchContext<NodeBuilder<DatabaseEnv, C::ChainSpec>>,
        Ext,
    ) -> Result<()>,
) -> Result<()>
where
    C: ChainSpecParser<ChainSpec = N::ChainSpec>,
    Ext: clap::Args + fmt::Debug + Default,
    Rpc: RpcModuleValidator,
    N: CliNodeTypes<Primitives: NodePrimitives<BlockHeader: HeaderMut>, ChainSpec: Hardforks>,
    SubCmd: ExtendedCommand + Subcommand + fmt::Debug,
    N::ChainSpec: EthChainSpec + EthereumHardforks,
{
    match cli.command {
        Some(Commands::Init(command)) => {
            let cfg_path = match cli.config_path {
                Some(p) => p,
                None => crate::interface::default_config_path()?,
            };
            let rt = runner.runtime();
            runner.block_on(async move { command.execute::<N>(&cfg_path, rt).await })
        }
        Some(Commands::Ext(command)) => {
            let cfg_path = match cli.config_path {
                Some(p) => p,
                None => crate::interface::default_config_path()?,
            };
            command.execute(runner, cfg_path)
        }
        Some(Commands::Import(command)) => {
            let cfg_path = match cli.config_path {
                Some(p) => p,
                None => crate::interface::default_config_path()?,
            };
            // Fail fast, before boot: the chain file is read in place (read-only), so a
            // missing path is a usage error, not a boot error.
            let file = command.file;
            if !file.is_file() {
                return Err(eyre::eyre!(
                    "chain file `{}` does not exist or is not a regular file",
                    file.display()
                ));
            }
            let overlay = load_operator_overlay(&cfg_path)?;
            // Exactly one datadir, from the config file — the same boot sequence as a node
            // run (`smartnet init` must have written the chain identity first).
            let datadir = overlay.datadir_path.clone().ok_or_else(|| {
                eyre::eyre!(
                    "config file `{}` must set `DATADIR_PATH` (the single source of truth for \
                     the data directory) to import into",
                    cfg_path.display()
                )
            })?;
            smartnet_file_tier::set_base(datadir.clone());
            let identity = read_chain_identity_from_db(&overlay)?.unwrap_or_default();
            smartnet_file_tier::bootstrap_network_settings(identity.chain_id, identity.validators)
                .map_err(|err| {
                    eprintln!("smartnet: fatal: {err} (database: run `smartnet init`).");
                    std::process::exit(err.exit_code());
                })?;
            let Some(db_genesis) = identity.genesis_spec else {
                eprintln!(
                    "smartnet: fatal: the database carries no genesis spec (old identity \
                     format) — wipe the datadir and run `smartnet init <genesis>`."
                );
                std::process::exit(6);
            };
            let chain = C::smartnet_chain_spec_from_db(db_genesis)?;
            let datadir_args = operator_datadir_args(datadir);
            let mut node_cmd = operator_default_node_command::<C, Ext>(cfg_path, chain);
            node_cmd.db.engine = overlay.db_engine.unwrap_or_default();
            // NO VALIDATOR in import: clear the launch flag — the heartbeat task is
            // never spawned and the config's validator section is not read at all.
            node_cmd.validator_daemon = false;
            // The mesh is always on: `mesh_gate` refuses to boot without `MESH_PORT` and
            // its identity reads the operator config file, so the mesh keys are stamped
            // exactly like a node run and `node_cmd.config` stays as the constructor set
            // it (`Some(cfg_path)`). The sync service stays off — `SYNC_PEERS` is never
            // stamped, so `sync_gate` logs "not started" and no peer traffic competes
            // with the file stream. HTTP and every RPC quoting/gating knob stay off too:
            // nothing submits transactions while importing; the launcher runs the import
            // loop after boot and the runner shuts the node down when it returns.
            if let Some(v) = overlay.mesh_port {
                node_cmd.rpc.mesh_port = Some(v);
            }
            if let Some(v) = overlay.mesh_addr {
                node_cmd.rpc.mesh_addr = Some(v);
            }
            if let Some(v) = overlay.mesh_peers_file {
                node_cmd.rpc.mesh_peers_file = Some(v);
            }
            if let Some(v) = overlay.mesh_max_msg_mb {
                node_cmd.rpc.mesh_max_msg_mb = Some(v);
            }
            if let Some(v) = overlay.mesh_heartbeat_ms {
                node_cmd.rpc.mesh_heartbeat_ms = Some(v);
            }
            if let Some(v) = overlay.cross_block_cache_size {
                node_cmd.engine.cross_block_cache_size = v;
            }
            // Batched persistence for import. The network default persists every
            // canonical block (threshold 0): one Fjall commit per block on the import
            // hot path (the fsync-dominated cost that made import ~10x slower than
            // geth's batched commits) and the in-memory copy of the parent is pruned
            // the moment it lands — the transient window behind the `Syncing` stop.
            // Import loses nothing by batching: the chain file is the source of truth
            // and a re-run resumes from the DB tip, so up to `persistence_threshold`
            // accepted blocks may safely ride in memory. `memory_block_buffer_target`
            // 0 persists up to the head when triggered, i.e. batch = threshold.
            node_cmd.engine.persistence_threshold = 4096;
            node_cmd.engine.persistence_backpressure_threshold = 5120; // validate(): > threshold
            node_cmd.engine.memory_block_buffer_target = 0;
            runner.run_command_until_exit(move |ctx| async move {
                let wrapped = FnLauncher::new::<C, Ext>(launcher);
                node_cmd.execute(ctx, datadir_args, wrapped).await
            })?;
            Ok(())
        }
        None => {
            let cfg_path = match cli.config_path {
                Some(p) => p,
                None => crate::interface::default_config_path()?,
            };
            let overlay = load_operator_overlay(&cfg_path)?;
            // Mempool entry floor (operator key `MIN_TO_TXPOOL_ENTRY`, default
            // `DEFAULT_MIN_TO_TXPOOL_ENTRY_WEI` = 0.02 WEM): the knownWallet/minBalance gate
            // the RPC tx entry applies. Entry-only by design — never on the signed-block
            // import path; the validator re-checks the real balance against the tx value.
            smartnet_rpc_eth_api::entry_gate::set_min_to_txpool_entry_wei(
                overlay
                    .min_to_txpool_entry
                    .unwrap_or(smartnet_rpc_eth_api::entry_gate::DEFAULT_MIN_TO_TXPOOL_ENTRY_WEI),
            );
            // RPC batch budgets (the batch-policy middleware; defaults 128/16/1/1024):
            // the per-batch class admission gate — better-than-deny, per-id retries.
            smartnet_rpc_builder::batch_policy::set_batch_limits(
                smartnet_rpc_builder::batch_policy::BatchGlobals {
                    tx_max: overlay
                        .batch_tx_max
                        .unwrap_or(smartnet_rpc_builder::batch_policy::DEFAULT_TX_MAX),
                    heavy_tx_max: overlay
                        .batch_heavy_tx_max
                        .unwrap_or(smartnet_rpc_builder::batch_policy::DEFAULT_HEAVY_TX_MAX),
                    heavy_max: overlay
                        .batch_heavy_max
                        .unwrap_or(smartnet_rpc_builder::batch_policy::DEFAULT_HEAVY_MAX),
                    read_max: overlay
                        .batch_read_max
                        .unwrap_or(smartnet_rpc_builder::batch_policy::DEFAULT_READ_MAX),
                },
            );
            // Tx lifetime knobs (TTL-POOL.md): the maintenance task prefers these over the
            // code defaults. Queued default 8 h (operator band 4–16 H); pending cap 0 = off.
            smartnet_transaction_pool::maintain::set_operator_queued_ttl_secs(
                overlay.txpool_queued_ttl_secs.unwrap_or(8 * 60 * 60),
            );
            smartnet_transaction_pool::maintain::set_operator_pending_ttl_secs(
                overlay.txpool_pending_ttl_secs.unwrap_or(0),
            );
            // Exactly one datadir, from the config file — no second source, no built-in
            // default, no cwd fallback. `overlay.datadir_path` is already `~`-expanded.
            let datadir = overlay.datadir_path.clone().ok_or_else(|| {
                eyre::eyre!(
                    "config file `{}` must set `DATADIR_PATH` (the single source of truth for \
                     the data directory) to run the node",
                    cfg_path.display()
                )
            })?;
            // Node bootstrap: open the database, read the chain identity and the full genesis
            // spec, and store the identity in memory. The database (the rows written at
            // `smartnet init`) is the single source of truth — no config fallback, no built-in
            // genesis, no hard-coded chain data; a value absent from the database is a soft
            // error and the node cannot start.
            smartnet_file_tier::set_base(datadir.clone());
            // One boot sequence: the database is the single source of truth for the chain
            // identity (written at `smartnet init`) — there is no config fallback — and a
            // value absent from the database is a soft boot error. No validation or
            // cross-check happens at this stage — the values are read from the database,
            // stored once, and read back through the accessors everywhere else.
            let identity = read_chain_identity_from_db(&overlay)?.unwrap_or_default();
            smartnet_file_tier::bootstrap_network_settings(identity.chain_id, identity.validators)
                .map_err(|err| {
                    // Boot failure: print the reason and exit with the source-specific code
                    // (2 = chain id, 3 = validators) so a supervisor can tell what to provide.
                    eprintln!("smartnet: fatal: {err} (database: run `smartnet init`).");
                    std::process::exit(err.exit_code());
                })?;
            // The runtime chain rulebook is the genesis itself: the exact `Genesis` the file
            // declared at init (config, header fields, and the alloc with every pre-minted
            // address and balance), rebuilt through the same `From<Genesis>` path. Any genesis
            // init must just work — a database without the spec row is an old-format identity
            // and cannot start (exit code 6).
            let Some(db_genesis) = identity.genesis_spec else {
                eprintln!(
                    "smartnet: fatal: the database carries no genesis spec (old identity \
                     format) — wipe the datadir and run `smartnet init <genesis>`."
                );
                std::process::exit(6);
            };
            let chain = C::smartnet_chain_spec_from_db(db_genesis)?;
            let datadir_args = operator_datadir_args(datadir);
            let mut node_cmd = operator_default_node_command::<C, Ext>(cfg_path, chain);
            // The engine is dictated by config (`DB_ENGINE`, optional; absent = fjall) —
            // stamped here so the datadir scoping in `NodeCommand::execute` and the
            // database open both resolve the exact same engine (`SMARTNET_DB_BACKEND`
            // still overrides on top for test tiers).
            node_cmd.db.engine = overlay.db_engine.unwrap_or_default();
            if let Some(v) = overlay.http_port {
                node_cmd.rpc.http = true;
                node_cmd.rpc.http_port = v;
            }
            if let Some(v) = overlay.http_addr {
                node_cmd.rpc.http = true;
                node_cmd.rpc.http_addr = v;
            }
            if let Some(v) = overlay.rpc_server {
                node_cmd.rpc.http = v;
            }
            // Optional: absent = off (`txpool` stays hard-blocked by the registry allowlist).
            node_cmd.rpc.rpc_mempool = overlay.rpc_mempool.unwrap_or(false);
            // Optional: absent = off (`web3_nodeInfo` stays disabled).
            node_cmd.rpc.rpc_nodeinfo = overlay.nodeinfo.unwrap_or(false);
            if let Some(v) = overlay.default_suggested_fee {
                node_cmd.rpc.gas_price_oracle.default_suggested_fee = Some(v);
                if v < U256::from(SMARTNET_FIXED_GAS_PRICE_WEI) {
                    warn!(
                        target: "smartnet::cli",
                        event = "GPO_DEFAULT_SUGGESTED_FEE_BELOW_PROTOCOL_PRICE",
                        configured_wei = %v,
                        fixed_gas_price_wei = SMARTNET_FIXED_GAS_PRICE_WEI,
                        "GPO_DEFAULT_SUGGESTED_FEE is below SMARTNET_FIXED_GAS_PRICE_WEI: wallet fee quotes \
                         would advertise a price the protocol will not accept — once headers carry the fixed \
                         base fee, txs priced below it are rejected (max fee < block base fee) and never \
                         executable. Pin GPO_DEFAULT_SUGGESTED_FEE to the fixed price."
                    );
                }
            } else {
                info!(
                    target: "smartnet::cli",
                    event = "GPO_DEFAULT_SUGGESTED_FEE_UNSET",
                    fixed_gas_price_wei = SMARTNET_FIXED_GAS_PRICE_WEI,
                    "GPO_DEFAULT_SUGGESTED_FEE is not set: fee quotes fall back to the protocol constant \
                     SMARTNET_FIXED_GAS_PRICE_WEI, so quoting stays correct; pin it explicitly \
                     (~/.smartnet/node/config.toml) so the node's configured price is auditable."
                );
            }
            // Master switch (absent = off): without it the catch-up service never starts,
            // so the SYNC_* keys below stay inert on an unedited/older config.
            node_cmd.rpc.rpc_sync = overlay.rpc_sync;
            if let Some(v) = overlay.sync_peers {
                // Parse here (not during overlay load) so the overlay holds plain strings.
                let parsed = v
                    .into_iter()
                    .map(|p| {
                        p.parse().map_err(|e| eyre::eyre!("invalid SYNC_PEERS entry {p:?}: {e}"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                node_cmd.rpc.sync_peers = Some(parsed);
            }
            if let Some(v) = overlay.sync_batch {
                node_cmd.rpc.sync_batch = Some(v);
            }
            if let Some(v) = overlay.sync_poll_interval {
                node_cmd.rpc.sync_poll_interval = Some(v);
            }
            // Optional: absent = mesh off (`spawn_mesh_service` is never called).
            if let Some(v) = overlay.mesh_port {
                node_cmd.rpc.mesh_port = Some(v);
            }
            if let Some(v) = overlay.mesh_addr {
                node_cmd.rpc.mesh_addr = Some(v);
            }
            if let Some(v) = overlay.mesh_peers_file {
                node_cmd.rpc.mesh_peers_file = Some(v);
            }
            if let Some(v) = overlay.mesh_max_msg_mb {
                node_cmd.rpc.mesh_max_msg_mb = Some(v);
            }
            if let Some(v) = overlay.mesh_heartbeat_ms {
                node_cmd.rpc.mesh_heartbeat_ms = Some(v);
            }
            if let Some(v) = overlay.heartbeat_ms {
                node_cmd.rpc.heartbeat_ms = Some(v);
            }
            if let Some(v) = overlay.cross_block_cache_size {
                node_cmd.engine.cross_block_cache_size = v;
            }
            runner.run_command_until_exit(move |ctx| async move {
                let wrapped = FnLauncher::new::<C, Ext>(launcher);
                node_cmd.execute(ctx, datadir_args, wrapped).await
            })?;
            Ok(())
        }
    }
}

/// A trait for extension subcommands that can be added to the CLI.
///
/// Consumers implement this trait for their custom subcommands to define
/// how they should be executed.
pub trait ExtendedCommand {
    /// Execute the extension command with the provided CLI runner and the resolved
    /// operator-config path (`--config` or the default `~/.smartnet/node/config.toml`).
    fn execute(self, runner: CliRunner, cfg_path: PathBuf) -> Result<()>;
}

/// Tests touch **only** the shipped template `smartnet/example.config.toml`, and only by
/// reading it. A config file is never written by a test: the template is the single config
/// source, `make install` copies it to `~/.smartnet/node/config.toml`, and that installed
/// copy is the only config the node ever reads.
#[cfg(test)]
mod tests {
    use super::*;

    /// Pure comment-stripping rules — no file involved.
    #[test]
    fn strip_comment_only_cuts_hash_outside_quotes_and_after_whitespace() {
        assert_eq!(strip_comment("# whole line comment"), "");
        assert_eq!(strip_comment("KEY = value # trailing"), "KEY = value ");
        assert_eq!(strip_comment(r#"KEY = "a#b""#), r#"KEY = "a#b""#);
        // URL fragment: `#` not preceded by whitespace survives
        assert_eq!(strip_comment("KEY = http://h/p#frag"), "KEY = http://h/p#frag");
        assert_eq!(strip_comment("KEY = value#x"), "KEY = value#x");
        assert_eq!(strip_comment("KEY = 1"), "KEY = 1");
        // A bare apostrophe must not be mistaken for an opening quote
        assert_eq!(strip_comment("KEY = 5 # we don't need this"), "KEY = 5 ");
        assert_eq!(strip_comment(r#"KEY = 'a # b'"#), r#"KEY = 'a # b'"#);
    }

    /// `VERB` is the process-wide level for both the node and the merged validator.
    #[test]
    fn verb_maps_to_the_process_wide_filter() {
        let errors = verb_to_filter(0);
        assert!(errors.starts_with("error"), "{errors}");
        for target in ["rustls", "tungstenite", "opentelemetry-otlp"] {
            assert!(errors.contains(&format!("{target}=off")), "{target} must be off: {errors}");
        }
        assert_eq!(
            verb_to_filter(1),
            "info,lsm_tree=warn,fjall=warn,engine::tree=warn,engine::execution_cache=warn,smartnet::storage=warn"
        );
        assert_eq!(verb_to_filter(2), "debug");
        assert_eq!(verb_to_filter(3), "trace");
    }

    /// `VERB` drives node and validator logs alike and wins over `VERBOSITY`.
    #[test]
    fn tracing_filter_prefers_verb_and_falls_back_to_verbosity() {
        let template =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example.config.toml");
        // Strip any shipped VERB/VERBOSITY line: the template carries working values
        // (VERB = 1), which would otherwise win over the appended body and pin every case
        // at "info".
        let base = std::fs::read_to_string(&template)
            .unwrap()
            .lines()
            .filter(|l| {
                let t = l.trim();
                !(t.starts_with("VERB ") || t.starts_with("VERBOSITY"))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for (body, expected) in [
            ("VERB = 0\n", Some("error")),
            (
                "VERB = 1\n",
                Some(
                    "info,lsm_tree=warn,fjall=warn,engine::tree=warn,engine::execution_cache=warn,smartnet::storage=warn",
                ),
            ),
            ("VERB = 2\n", Some("debug")),
            ("VERB = 3\n", Some("trace")),
            ("VERB = 0\nVERBOSITY = 2\n", Some("error")),
            ("VERBOSITY = 2\n", Some("debug")),
            ("VERBOSITY = 0\n", Some("off")),
        ] {
            std::fs::write(&path, format!("{base}\n{body}")).unwrap();
            let got = tracing_filter_from_config(Some(&path));
            match expected {
                Some("error") => {
                    let got = got.expect("VERB=0 must resolve a filter");
                    assert!(got.starts_with("error"), "{body:?}: {got}");
                }
                expected => assert_eq!(got.as_deref(), expected, "{body:?}"),
            }
        }
    }

    /// The shipped operator template must declare every required key the overlay reads (drift
    /// guard against `load_operator_overlay`) and — as its own header states — must ship working
    /// values so an unedited copy boots. The template carries no `CHAINID` / `VALIDATORS`
    /// keys: the database is the single source of truth for both — `smartnet init` writes them
    /// into it and every boot reads them back from there.
    #[test]
    fn shipped_node_template_declares_every_key_and_boots_with_shipped_values() {
        let template =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example.config.toml");
        let raw = std::fs::read_to_string(&template)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", template.display()));
        for key in [
            "DATADIR_PATH",
            "HTTP_PORT",
            "HTTP_ADDR",
            "RPC_SERVER",
                "GPO_DEFAULT_SUGGESTED_FEE",
            "RPC_SYNC",
            "SYNC_PEERS",
            "SYNC_BATCH",
            "SYNC_POLL_INTERVAL",
            "CROSS_BLOCK_CACHE_SIZE",
            "MESH_PORT",
            "MESH_ADDR",
            "MESH_PEERS_FILE",
            "MESH_MAX_MSG_MB",
            "MESH_HEARTBEAT",
            "HEARTBEAT_MS",
            "RPC_MEMPOOL",
            "RPC_NODEINFO",
            "MIN_TO_TXPOOL_ENTRY",
        ] {
            assert!(
                raw.lines().any(|l| {
                    let t = l.trim();
                    t.starts_with(key) ||
                        t.strip_prefix('#').is_some_and(|s| s.trim_start().starts_with(key))
                }),
                "template is missing {key}: {}",
                template.display()
            );
        }
        // The template ships working values for the current chain, so an unedited copy boots.
        // Drift guard: a new overlay key the template omits, or a shipped value that stops
        // parsing, fails here.
        let cfg = load_operator_overlay(&template)
            .expect("the shipped template must boot with its shipped values");
        assert!(cfg.datadir_path.is_some(), "DATADIR_PATH must be set");
        // The template carries no `CHAINID` / `VALIDATORS` — no key, no fallback: the
        // database is the single source of truth for both (written by `smartnet init`, read
        // back at every boot). `DB_ENGINE` ships commented out and defaults to `fjall` when
        // absent.
        assert!(cfg.db_engine.is_none(), "DB_ENGINE ships commented out (default fjall)");

        // Unknown keys are rejected, not ignored (fail-closed): a config that still names
        // removed or never-known keys must not boot — `CHAINID`/`VALIDATORS` are DB-only
        // identity (written by `smartnet init`), `WS_*` is removed entirely (WS.md rules:
        // mesh-only, never RPC; keys are invalid), and anything else is a typo. The
        // problems list must name each offender.
        let with_legacy = tempfile::tempdir().unwrap();
        let with_path = with_legacy.path().join("with_legacy.toml");
        std::fs::write(
            &with_path,
            format!(
              "{raw}\nCHAINID = 315\nVALIDATORS = \"0x24bf191a358c09e79b9ead2d8999d08590b5af31\"\nWS_PORT = 8546\nMESH_HEARTBEAT_MS = 2000\nVALIDATOR_HEARTBEAT_MS = 600\nNOT_A_KEY = 1\n"
            ),
        )
        .unwrap();
        let err = load_operator_overlay(&with_path)
            .expect_err("unknown/removed keys must be rejected, not silently ignored");
        let msg = format!("{err:?}");
        for key in [
            "CHAINID",
            "VALIDATORS",
            "WS_PORT",
            "MESH_HEARTBEAT_MS",
            "VALIDATOR_HEARTBEAT_MS",
            "NOT_A_KEY",
        ] {
            assert!(msg.contains(key), "problems must name unknown key {key}: {msg}");
        }
    }

    /// The heartbeat is mandatory once present: 0 would stop validator beats, so it is
    /// rejected rather than silently disabling them.
    #[test]
    fn heartbeat_ms_zero_is_rejected() {
        let template =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example.config.toml");
        let raw = std::fs::read_to_string(&template).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("heartbeat-zero.toml");
        std::fs::write(&path, raw.replace("HEARTBEAT_MS = 2000", "HEARTBEAT_MS = 0")).unwrap();
        let err = load_operator_overlay(&path).expect_err("HEARTBEAT_MS = 0 must be rejected");
        assert!(format!("{err:?}").contains("HEARTBEAT_MS"), "got: {err:?}");
    }

    /// `MESH_MAX_MSG_MB` is optional (unset = the 32 MB protocol default) but a present
    /// value must be sane: 0 would mean no frame can ever be carried, so it is rejected
    /// rather than silently disabling mesh transport.
    #[test]
    fn mesh_max_msg_mb_zero_is_rejected() {
        let template =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example.config.toml");
        let raw = std::fs::read_to_string(&template).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mesh-max-msg-zero.toml");
        std::fs::write(&path, format!("{raw}\nMESH_MAX_MSG_MB = 0\n")).unwrap();
        let err = load_operator_overlay(&path).expect_err("MESH_MAX_MSG_MB = 0 must be rejected");
        assert!(format!("{err:?}").contains("MESH_MAX_MSG_MB"), "got: {err:?}");

        // A sane value parses through as a `usize`.
        let path = dir.path().join("mesh-max-msg-ok.toml");
        std::fs::write(&path, format!("{raw}\nMESH_MAX_MSG_MB = 64\n")).unwrap();
        let cfg = load_operator_overlay(&path).expect("MESH_MAX_MSG_MB = 64 must boot");
        assert_eq!(cfg.mesh_max_msg_mb, Some(64));
    }

    /// `MESH_HEARTBEAT` is optional (unset = the 500 ms default) but a present value must
    /// be a positive millisecond count — `0` would collapse the keepalive/silence timers.
    #[test]
    fn mesh_heartbeat_zero_is_rejected() {
        let template =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example.config.toml");
        let raw = std::fs::read_to_string(&template).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mesh-heartbeat-zero.toml");
        std::fs::write(&path, format!("{raw}\nMESH_HEARTBEAT = 0\n")).unwrap();
        let err = load_operator_overlay(&path).expect_err("MESH_HEARTBEAT = 0 must be rejected");
        assert!(format!("{err:?}").contains("MESH_HEARTBEAT"), "got: {err:?}");

        // A sane value parses through as a millisecond count.
        let path = dir.path().join("mesh-heartbeat-ok.toml");
        std::fs::write(&path, format!("{raw}\nMESH_HEARTBEAT = 250\n")).unwrap();
        let cfg = load_operator_overlay(&path).expect("MESH_HEARTBEAT = 250 must boot");
        assert_eq!(cfg.mesh_heartbeat_ms, Some(250));
    }

    /// `RPC_MEMPOOL` and `RPC_NODEINFO` are optional: when absent the overlay stays `None` and
    /// the command path applies the `false` default (see `DefaultRpcServerArgs`).
    #[test]
    fn rpc_mempool_and_nodeinfo_are_optional_and_default_off() {
        let template =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example.config.toml");
        let raw = std::fs::read_to_string(&template).unwrap();
        let filtered: String = raw
            .lines()
            .filter(|line| {
                let key = strip_comment(line).trim();
                !key.starts_with("RPC_MEMPOOL") && !key.starts_with("RPC_NODEINFO")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-rpc-flags.toml");
        std::fs::write(&path, filtered).unwrap();

        let cfg = load_operator_overlay(&path).expect("removing the two optional keys must boot");
        assert!(cfg.rpc_mempool.is_none(), "RPC_MEMPOOL is optional");
        assert!(cfg.nodeinfo.is_none(), "RPC_NODEINFO is optional");
    }

    /// `RPC_SYNC` is the master switch for the in-node catch-up service: optional (absent =
    /// off, so older configs keep booting with sync disabled), `true`/`false` parse, and
    /// anything else is a hard config error naming the key.
    #[test]
    fn rpc_sync_is_optional_defaults_off_and_parses() {
        let template =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example.config.toml");
        let raw = std::fs::read_to_string(&template).unwrap();
        let dir = tempfile::tempdir().unwrap();

        // Absent → `None` (the command path reads that as false): sync stays off while the
        // SYNC_* keys keep parsing, so removing only the master switch still boots.
        let filtered: String = raw
            .lines()
            .filter(|line| !strip_comment(line).trim().starts_with("RPC_SYNC"))
            .collect::<Vec<_>>()
            .join("\n");
        let path = dir.path().join("no-rpc-sync.toml");
        std::fs::write(&path, filtered).unwrap();
        let cfg = load_operator_overlay(&path).expect("RPC_SYNC is optional and must boot");
        assert!(cfg.rpc_sync.is_none(), "RPC_SYNC absent must stay None (read as off)");
        assert!(cfg.sync_batch.is_some(), "the SYNC_* keys must still parse");

        // Explicit `true` flips the master switch on.
        let path = dir.path().join("rpc-sync-on.toml");
        std::fs::write(&path, raw.replace("RPC_SYNC = false", "RPC_SYNC = true")).unwrap();
        let cfg = load_operator_overlay(&path).expect("RPC_SYNC = true must boot");
        assert_eq!(cfg.rpc_sync, Some(true), "RPC_SYNC = true must parse");

        // Anything else is a hard error naming the key (fail-closed, no silent default).
        let path = dir.path().join("rpc-sync-bad.toml");
        std::fs::write(&path, raw.replace("RPC_SYNC = false", "RPC_SYNC = maybe")).unwrap();
        let err = load_operator_overlay(&path).expect_err("RPC_SYNC must be true or false");
        assert!(format!("{err:?}").contains("RPC_SYNC"), "error must name the key: {err:?}");
    }

    /// With `RPC_SYNC = true` the busy-loop guard is armed again: `SYNC_POLL_INTERVAL = 0`
    /// alongside `SYNC_BATCH >= 1` aborts startup. With the master switch off (the default)
    /// the same config boots — sync can never run, so the interval is irrelevant.
    #[test]
    fn zero_poll_interval_is_rejected_only_while_rpc_sync_is_on() {
        let template =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example.config.toml");
        let raw = std::fs::read_to_string(&template).unwrap();
        let dir = tempfile::tempdir().unwrap();

        // Template ships SYNC_BATCH = 0, so arm the switch with a batch that would loop.
        let armed = raw
            .replace("RPC_SYNC = false", "RPC_SYNC = true")
            .replace("SYNC_BATCH = 0", "SYNC_BATCH = 4")
            .replace("SYNC_POLL_INTERVAL = 3", "SYNC_POLL_INTERVAL = 0");
        let path = dir.path().join("sync-loop.toml");
        std::fs::write(&path, &armed).unwrap();
        let err = load_operator_overlay(&path)
            .expect_err("SYNC_POLL_INTERVAL = 0 with RPC_SYNC = true must be rejected");
        assert!(format!("{err:?}").contains("SYNC_POLL_INTERVAL"), "got: {err:?}");

        // Same values with the master switch off: booting is fine, sync stays dark.
        let path = dir.path().join("sync-loop-off.toml");
        std::fs::write(&path, armed.replace("RPC_SYNC = true", "RPC_SYNC = false")).unwrap();
        let cfg =
            load_operator_overlay(&path).expect("with RPC_SYNC = false the poll interval is inert");
        assert_eq!(cfg.rpc_sync, Some(false));
    }
}

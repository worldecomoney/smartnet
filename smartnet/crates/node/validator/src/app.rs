//! Command dispatch; the merged daemon beat calls the same seal paths.

use crate::{
    config::{load_validator_config, ValidatorConfig},
    keystore_util::KeystoreMaterial,
    outcome::{format_rpc_error, from_eyre, is_node_unreachable, ValidatorStatus},
    seed_tx::{seed_tx, SeedTxArgs},
    Cli,
};
use alloy_network::Ethereum;
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_engine::ForkchoiceState;
use alloy_rpc_types_eth::BlockNumberOrTag;
use eyre::{Context as _, Result};
use serde_json::json;
use smartnet_fs_util::runtime_paths::{home_dir_from_config, resolve_runtime_path};
use std::path::PathBuf;
use tracing::debug;
use url::Url;

pub(crate) async fn run(cli: Cli) -> ValidatorStatus {
    match cli.command {
        // The one-shot seal path is GONE: the merged validator is a daemon inside the node
        // (the shared HEARTBEAT_MS beat), started by the node's own boot when the
        // config's validator section carries SEAL = true. There is no CLI one-shot seal.
        None => ValidatorStatus::Failed {
            message: "the validator's seal path is merged into the node: boot the node with \
                      SEAL = true in the config's validator section; there is no one-shot CLI seal"
                .to_string(),
        },
        Some(crate::Commands::Keystore(ref keystore)) => {
            crate::import::keystore_import_cmd(&cli, keystore.private_key_file.as_deref())
        }
        Some(crate::Commands::SeedTx) => seed_tx_cmd().await,
        Some(crate::Commands::Ping(ref ping)) => ping_cmd(ping).await,
    }
}

/// The node's own RPC endpoint: the merged node listens on its template loopback port
/// (`HTTP_PORT` 8545 / `HTTP_ADDR` 127.0.0.1 — `smartnet/example.config.toml`). The old `NODE_URL`
/// config key is gone: the merged validator has no external RPC configuration, it addresses
/// its own node.
pub(crate) const OWN_NODE_URL: &str = "http://127.0.0.1:8545";
/// Per-request JSON-RPC timeout for the ops commands (the old `RPC_TIMEOUT_MS` key is gone;
/// this is the fixed ops budget, not something the validator section configures).
pub(crate) const OPS_RPC_TIMEOUT_MS: u64 = 30_000;

async fn seed_tx_cmd() -> ValidatorStatus {
    // Recipient = derived from the private key (SEED_TX_TO is gone from the config — the
    // merged flow derives it from the keystore identity).
    let key = match load_signing_key(None).await {
        Ok(k) => k,
        Err(s) => return s,
    };
    // The chain id is NOT a config key anymore (the DB is the single source). The ops
    // command reads it from the database beside the config's `DATADIR_PATH` datadir.
    let chain_id = match match db_chain_id() {
        Ok(v) => v,
        Err(e) => return from_eyre(e),
    } {
        Some(v) => v,
        None => {
            return ValidatorStatus::Failed {
                message: "no chain id in the database — run `smartnet init` first".to_string(),
            };
        }
    };
    let to = key.address();
    match seed_tx(&SeedTxArgs {
        node_url: OWN_NODE_URL.to_string(),
        key,
        to,
        // Payload values are the merged flow's defaults (the old SEED_TX_VALUE_WEI /
        // SEED_TX_GAS_LIMIT config keys are gone).
        value_wei: alloy_primitives::U256::from(1_000_000_000_000_000u64),
        gas_limit: 21_000,
        chain_id,
        rpc_timeout_ms: OPS_RPC_TIMEOUT_MS,
    })
    .await
    {
        Ok(()) => ValidatorStatus::Ok,
        Err(e) if is_node_unreachable(&e) => ValidatorStatus::Idle {
            event: "NODE_UNREACHABLE",
            message: format_rpc_error(OWN_NODE_URL, "eth_sendRawTransaction", &e),
        },
        Err(e) => from_eyre(e),
    }
}

async fn ping_cmd(ping: &crate::PingCli) -> ValidatorStatus {
    let url: Url = match OWN_NODE_URL.parse() {
        Ok(u) => u,
        Err(e) => {
            return ValidatorStatus::Failed {
                message: format!("invalid OWN_NODE_URL constant: {e}"),
            };
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(OPS_RPC_TIMEOUT_MS))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ValidatorStatus::Failed { message: format!("build HTTP client: {e}") };
        }
    };
    let provider = ProviderBuilder::new().network::<Ethereum>().connect_reqwest(client, url);

    // Read-only by default (A6 · M6): the probe never steers fork choice, so it only needs
    // the node's current head. `--finalize` is the explicit opt-in that skips this lookup.
    let current_head = if ping.finalize {
        alloy_primitives::B256::ZERO // unused: finalize mode builds the state from the flags
    } else {
        match provider.get_block_by_number(BlockNumberOrTag::Latest).await {
            Ok(Some(block)) => block.header.hash,
            Ok(None) => {
                return ValidatorStatus::Failed {
                    message: "no latest block from node — cannot build a read-only FCU state (use \
                          --finalize --head <hash> to steer explicitly)"
                        .to_string(),
                };
            }
            Err(e) if is_node_unreachable(&e) => {
                return ValidatorStatus::Idle {
                    event: "NODE_UNREACHABLE",
                    message: format_rpc_error(OWN_NODE_URL, "eth_getBlockByNumber(latest)", &e),
                };
            }
            Err(e) => {
                return ValidatorStatus::Failed {
                    message: format!("eth_getBlockByNumber(latest) failed: {e}"),
                };
            }
        }
    };

    let state = match forkchoice_state_for_ping(
        ping.head,
        ping.safe,
        ping.finalized,
        ping.finalize,
        current_head,
    ) {
        Ok(s) => s,
        Err(message) => return ValidatorStatus::Failed { message },
    };
    debug!(
        target: "smartnet_validator",
        event = "PING_FORKCHOICE_STATE",
        head = %state.head_block_hash,
        safe = %state.safe_block_hash,
        finalized = %state.finalized_block_hash,
        mode = if ping.finalize { "explicit-steering" } else { "read-only" },
        "ping forkchoice state (a bare ping never steers fork choice; --finalize opts in)"
    );

    match provider
        .client()
        .request("engine_forkchoiceUpdatedV3", json!([state, serde_json::Value::Null]))
        .await
    {
        Ok(updated) => {
            let updated: serde_json::Value = updated;
            println!("{updated:#?}");
            ValidatorStatus::Ok
        }
        Err(e) if is_node_unreachable(&e) => ValidatorStatus::Idle {
            event: "NODE_UNREACHABLE",
            message: format_rpc_error(OWN_NODE_URL, "engine_forkchoiceUpdatedV3", &e),
        },
        Err(e) => {
            ValidatorStatus::Failed { message: format!("engine_forkchoiceUpdatedV3 failed: {e}") }
        }
    }
}

/// A6 · M6 — build the `engine_forkchoiceUpdatedV3` state for `ping`.
///
/// A health probe must never steer fork choice, so without `--finalize` the state is pinned to
/// the node's **current** head (`head = safe = finalized = current`) and any explicit hash on
/// the command line is refused: a stale `--head` used to be able to move the canonical head
/// backwards and finalize an arbitrary hash. `--finalize` is the explicit opt-in, and it
/// requires `--head`.
fn forkchoice_state_for_ping(
    head_arg: Option<alloy_primitives::B256>,
    safe_arg: Option<alloy_primitives::B256>,
    finalized_arg: Option<alloy_primitives::B256>,
    finalize: bool,
    current_head: alloy_primitives::B256,
) -> Result<ForkchoiceState, String> {
    if !finalize {
        if head_arg.is_some() || safe_arg.is_some() || finalized_arg.is_some() {
            return Err(
                "refusing explicit head/safe/finalized without --finalize: a health probe must \
                 not steer fork choice (pass --finalize to opt in, or drop the hashes)"
                    .to_string(),
            );
        }
        return Ok(ForkchoiceState {
            head_block_hash: current_head,
            safe_block_hash: current_head,
            finalized_block_hash: current_head,
        });
    }
    let Some(head) = head_arg else {
        return Err("--finalize requires --head <hash> (there is nothing to steer to)".to_string());
    };
    Ok(ForkchoiceState {
        head_block_hash: head,
        safe_block_hash: safe_arg.unwrap_or(head),
        finalized_block_hash: finalized_arg.unwrap_or(head),
    })
}

/// Load the encrypted keystore + password buffers from config (`KEY_PATH`, one gathered file or
/// one picked by the node-level `COINBASE` selector, + mandatory `PASSWORD_FILE`). `cfg = None`
/// means: load the config from `--config`/default. No decrypted key is retained.
async fn load_signing_key(
    cfg: Option<&ValidatorConfig>,
) -> Result<KeystoreMaterial, ValidatorStatus> {
    let cfg = match cfg {
        Some(c) => c.clone(),
        None => {
            let cfg_path = match std::env::var("SMARTNET_CONFIG_PATH") {
                Ok(p) => PathBuf::from(p),
                Err(_) => crate::resolve_config_path(None).map_err(from_eyre)?,
            };
            match load_validator_config(&cfg_path) {
                Ok(c) => c,
                Err(e) => return Err(from_eyre(e)),
            }
        }
    };
    let password_file = cfg.password_file.as_deref().ok_or_else(|| {
        from_eyre(eyre::eyre!("PASSWORD_FILE is required in config — headless boot never prompts"))
    })?;
    let cfg_path = match std::env::var("SMARTNET_CONFIG_PATH") {
        Ok(p) => PathBuf::from(p),
        Err(_) => crate::resolve_config_path(None).map_err(from_eyre)?,
    };
    // Node-level selector, not a validator field: it only picks the encrypted key file.
    let coinbase =
        smartnet_fs_util::keystore_selector::coinbase_from_config(&cfg_path).map_err(from_eyre)?;
    KeystoreMaterial::load(&cfg.key_dir, password_file, coinbase).map_err(from_eyre)
}

/// Chain id straight from the database (the single source of truth): reads `DATADIR_PATH`
/// from the shared config (the value the node booted with), opens the database and reads
/// the persisted network settings. Returns `Ok(None)` when nothing is initialised.
fn db_chain_id() -> Result<Option<u64>> {
    let cfg_path = match std::env::var("SMARTNET_CONFIG_PATH") {
        Ok(p) => PathBuf::from(p),
        Err(_) => crate::resolve_config_path(None)?,
    };
    let raw = std::fs::read_to_string(&cfg_path)
        .with_context(|| format!("read config {}", cfg_path.display()))?;
    let home_dir = home_dir_from_config(&raw)?;
    let mut datadir: Option<PathBuf> = None;
    let mut engine: Option<String> = None;
    for line in raw.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() || !line.contains('=') {
            continue;
        }
        let mut parts = line.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim().trim_matches('"');
        match key {
            "DATADIR_PATH" => datadir = Some(resolve_runtime_path(&home_dir, value)?),
            "DB_ENGINE" => engine = Some(value.to_string()),
            _ => {}
        }
    }
    let Some(datadir) = datadir else {
        eyre::bail!(
            "config {} has no DATADIR_PATH — cannot locate the database",
            cfg_path.display()
        );
    };
    // The engine is the one `DB_ENGINE` named (fjall default); mirror the node's scoping.
    let db_dir = match engine.as_deref() {
        Some("fjall") | None => datadir.join("fjall").join("db"),
        Some("redb") => datadir.join("redb").join("db"),
        Some(other) => {
            eyre::bail!("unsupported DB_ENGINE {other:?}");
        }
    };
    if smartnet_db::is_database_empty(&db_dir) {
        return Ok(None);
    }
    // Read-only in spirit: open, read the rows, drop. Fjall opens RW; the handle dies here.
    let db = smartnet_db::open_db(&db_dir, Default::default())
        .with_context(|| format!("open database {}", db_dir.display()))?;
    let (chain_id, _) = smartnet_provider::init::read_network_settings(&db).unwrap_or((None, None));
    Ok(chain_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    fn h(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    /// A bare ping is read-only: head = safe = finalized = the node's current head.
    #[test]
    fn bare_ping_pins_state_to_current_head() {
        let s = forkchoice_state_for_ping(None, None, None, false, h(1)).unwrap();
        assert_eq!(s.head_block_hash, h(1));
        assert_eq!(s.safe_block_hash, h(1));
        assert_eq!(s.finalized_block_hash, h(1));
    }

    /// A6 · M6 regression: explicit hashes without `--finalize` must be refused — a stale
    /// `--head` used to move the canonical head backwards and finalize an arbitrary hash.
    #[test]
    fn explicit_hashes_without_finalize_are_refused() {
        for args in [
            (Some(h(2)), None, None),
            (None, Some(h(3)), None),
            (None, None, Some(h(4))),
            (Some(h(2)), Some(h(3)), Some(h(4))),
        ] {
            let err = forkchoice_state_for_ping(args.0, args.1, args.2, false, h(1)).unwrap_err();
            assert!(err.contains("--finalize"), "got: {err}");
        }
    }

    #[test]
    fn finalize_requires_head() {
        let err = forkchoice_state_for_ping(None, None, None, true, h(1)).unwrap_err();
        assert!(err.contains("--head"), "got: {err}");
    }

    #[test]
    fn finalize_defaults_safe_and_finalized_to_head() {
        let s = forkchoice_state_for_ping(Some(h(2)), None, None, true, h(1)).unwrap();
        assert_eq!(s.head_block_hash, h(2));
        assert_eq!(s.safe_block_hash, h(2));
        assert_eq!(s.finalized_block_hash, h(2));
    }

    #[test]
    fn finalize_honours_explicit_safe_and_finalized() {
        let s = forkchoice_state_for_ping(Some(h(2)), Some(h(3)), Some(h(4)), true, h(1)).unwrap();
        assert_eq!(s.head_block_hash, h(2));
        assert_eq!(s.safe_block_hash, h(3));
        assert_eq!(s.finalized_block_hash, h(4));
    }

    /// `--head` is no longer mandatory: a bare `ping` is the normal read-only probe.
    #[test]
    fn ping_without_head_flag_parses() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["smartnet", "ping"]).unwrap();
        match cli.command {
            Some(crate::Commands::Ping(p)) => {
                assert!(p.head.is_none());
                assert!(p.safe.is_none());
                assert!(p.finalized.is_none());
                assert!(!p.finalize);
            }
            other => panic!("expected ping, got {other:?}"),
        }
    }

    #[test]
    fn ping_with_head_and_finalize_parses() {
        use clap::Parser;
        let head = h(9);
        let cli = crate::Cli::try_parse_from([
            "smartnet",
            "ping",
            "--head",
            &format!("{head:#x}"),
            "--finalize",
        ])
        .unwrap();
        match cli.command {
            Some(crate::Commands::Ping(p)) => {
                assert_eq!(p.head, Some(head));
                assert!(p.finalize);
            }
            other => panic!("expected ping, got {other:?}"),
        }
    }
}

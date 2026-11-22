//! Operator config file (`KEY=value`, same style as the node operator config).

use eyre::{Context, Result};
use smartnet_fs_util::runtime_paths::{home_dir_from_config, resolve_runtime_path};
use std::path::{Path, PathBuf};

use crate::mempool_policy::SelectionCaps;

/// Built-in default for `TXPOOL_VAL_MAX_TXS` — the total txs one seal pass processes
/// (the pool snapshot is truncated to the oldest that many before state prefetch and
/// selection). Per-class narrowing (native / heavy / …) comes later.
const DEFAULT_TXPOOL_VAL_MAX_TXS: usize = 512;

/// Parsed validator operator config.
#[derive(Debug, Clone)]
pub struct ValidatorConfig {
    /// Directory holding the encrypted keystore (`KEY_PATH`).
    pub key_dir: PathBuf,
    /// Keystore password file (`PASSWORD_FILE`). Empty in config = prompt interactively;
    /// never a built-in path in the code.
    pub password_file: Option<PathBuf>,
    /// Log verbosity (`VERB`, 0-3): 0=errors only, 1=info, 2=debug, 3=everything.
    /// Optional; unset keeps the default (1 = info).
    pub verb: Option<u8>,
    /// Per-class block caps (`MAX_NATIVE_PER_BLOCK`, `MAX_ERC_20_PER_BLOCK`,
    /// `MAX_CONTRACTS_PER_BLOCK`, `MAX_NFT_PER_BLOCK`). Optional; each unset key keeps its
    /// default (64 / 32 / 1 / 2).
    pub selection_caps: SelectionCaps,
    /// Total txs one seal pass may process (`TXPOOL_VAL_MAX_TXS`). Optional; absent or
    /// blank = [`DEFAULT_TXPOOL_VAL_MAX_TXS`] (512), 0 is rejected. The pool snapshot is
    /// truncated to the oldest that many before the state prefetch and the selection read
    /// it; per-class narrowing comes later.
    pub txpool_val_max_txs: usize,
    /// Seal a block when the txpool is empty (`ALLOW_EMPTY`, true/false). Optional; absent =
    /// false (an empty txpool is an idle beat, not an empty block).
    pub allow_empty: bool,
    /// Validator seal-beat cadence (`NODE_HEARTBEAT`, ms between txpool/seal beats) —
    /// the SAME key and value the node reads; there is one cadence per node.
    /// Optional; absent = 2000, 0 is rejected.
    pub heartbeat_ms: u64,
    /// Is the validator section present in the config at all? THE SECTION IS OPTIONAL:
    /// a node config may carry no validator keys at all — then this is false and the
    /// merged flow simply does nothing (no keystore, no sealing). When true, every key
    /// of the section is required as before.
    pub enabled: bool,
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    // `len >= 2` is load-bearing: a lone `"` both starts *and* ends the string, and slicing it
    // as `&s[1..0]` panics (`start > end`). An operator's typo must be a reported problem, not
    // a crash of the validator. Both boundary bytes are ASCII quotes, so the slice is also at a
    // char boundary.
    if s.len() >= 2 &&
        ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Strip a trailing `#` comment from a raw config line.
///
/// A `#` starts a comment only when it is at the start of the line **or** preceded by
/// whitespace, and only when it sits outside quotes. That keeps
/// `NODE_URL = "http://host:8545/#v1"` and `PASSWORD = "a#b"` intact while still
/// honouring `KEY = value # comment`, bare `KEY = value#not-a-comment`, and whole-line
/// `# comment`.
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
            b'#' if !in_single && !in_double && (i == 0 || b[i - 1].is_ascii_whitespace()) => {
                return &line[..i];
            }
            _ => {}
        }
        i += 1;
    }
    line
}

/// Load the validator section of the shared node config: `HOME_DIR_PATH` (the single root),
/// `KEY_PATH`, `PASSWORD_FILE`, `SEAL`, `ALLOW_EMPTY`, `NODE_HEARTBEAT`, the per-class caps and
/// `TXPOOL_VAL_MAX_TXS`. The two keystore paths are relative to `HOME_DIR_PATH`.
///
/// `VAL_FILE` is node-owned and is ignored here. The validator reads no registry file: it
/// asks the node for the enabled set and the chair through `ValidatorIo`. The keystore
/// (`KEY_PATH`/`PASSWORD_FILE`) is the node's identity too — the validator only consumes
/// the key the node hands it.
pub fn load_validator_config(path: &Path) -> Result<ValidatorConfig> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;

    // Every problem found is collected so an unedited config reports all of them in one
    // error instead of making the operator play whack-a-mole.
    let mut problems: Vec<String> = Vec::new();

    // `HOME_DIR_PATH` is the only absolute path in the config; KEY_PATH and PASSWORD_FILE are
    // relative to it. A bad root is one collected problem, and path keys are skipped so the
    // report does not drown in derived "relative path" noise.
    let home_dir = match home_dir_from_config(&raw) {
        Ok(base) => Some(base),
        Err(err) => {
            problems.push(format!("{err:#}"));
            None
        }
    };

    let mut key_dir: Option<PathBuf> = None;
    let mut heartbeat_ms: Option<u64> = None;
    let mut allow_empty: Option<bool> = None;
    let mut seal: Option<bool> = None;
    let mut password_file: Option<PathBuf> = None;
    let mut verb: Option<u8> = None;
    let mut max_native_per_block: Option<u64> = None;
    let mut max_erc20_per_block: Option<u64> = None;
    let mut max_contracts_per_block: Option<u64> = None;
    let mut max_nft_per_block: Option<u64> = None;
    let mut txpool_val_max_txs: Option<usize> = None;

    let mut seen: Vec<&'static str> = Vec::new();

    for line in raw.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() || !line.contains('=') {
            continue;
        }
        let mut parts = line.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        if key.is_empty() {
            continue;
        }

        let name = match key {
            "KEY_PATH" => "KEY_PATH",
            "PASSWORD_FILE" => "PASSWORD_FILE",
            // Node-owned keystore selector: recognized here so it is not rejected as unknown,
            // but parsed and validated by `smartnet-fs-util` for every node role.
            "COINBASE" => "COINBASE",
            "VERB" => "VERB",
            "MAX_NATIVE_PER_BLOCK" => "MAX_NATIVE_PER_BLOCK",
            "MAX_ERC_20_PER_BLOCK" => "MAX_ERC_20_PER_BLOCK",
            "MAX_CONTRACTS_PER_BLOCK" => "MAX_CONTRACTS_PER_BLOCK",
            "MAX_NFT_PER_BLOCK" => "MAX_NFT_PER_BLOCK",
            "TXPOOL_VAL_MAX_TXS" => "TXPOOL_VAL_MAX_TXS",
            "SEAL" => "SEAL",
            "ALLOW_EMPTY" => "ALLOW_EMPTY",
            "NODE_HEARTBEAT" => "NODE_HEARTBEAT",
            // The config file is now the shared NODE config (the old validator config is
            // gone): other keys on it belong to the node CLI and are parsed (fail-closed for
            // their own keys) by the node's overlay — anonymous keys here belong to the node
            // section, not to the validator, and are ignored, not rejected.
            _ => continue,
        };
        seen.push(name);

        // A key that is present but blank is a problem in its own right — there is no
        // built-in default to fall back to. A few keys legitimately accept an empty value:
        // `PASSWORD_FILE` (prompt for the password instead of reading a file), `VERB`
        // (optional — blank means the default, 1), the
        // four class caps (optional — blank means the built-in default) and
        // `TXPOOL_VAL_MAX_TXS` (optional — blank means the built-in default, 512).
        if strip_quotes(value).trim().is_empty() &&
            name != "SEAL" &&
            name != "PASSWORD_FILE" &&
            name != "VERB" &&
            name != "ALLOW_EMPTY" &&
            name != "TXPOOL_VAL_MAX_TXS" &&
            !name.starts_with("MAX_")
        {
            problems.push(format!(
                "config `{name}` is empty — set a value (SmartNet ships no built-in default)"
            ));
            continue;
        }

        // Every parse failure below is pushed into `problems` instead of being returned with
        // `?`: the promise made by this function, its own comment and the README is that one
        // boot reports *every* problem, not the first one — otherwise an operator fixes the
        // blank NODE_URL, restarts, and only then discovers the typo in CHAINID. The closure
        // keeps `?` and `bail!` readable while turning each of them into one more collected
        // problem.
        let parsed: Result<()> = (|| {
            match name {
                "KEY_PATH" => {
                    if let Some(base) = home_dir.as_ref() {
                        key_dir = Some(resolve_runtime_path(base, strip_quotes(value))?);
                    }
                }
                "SEAL" => {
                    seal = Some(
                        parse_bool(strip_quotes(value))
                            .with_context(|| format!("parse SEAL {value}"))?,
                    );
                }
                "ALLOW_EMPTY" => {
                    let raw = strip_quotes(value);
                    if !raw.is_empty() {
                        allow_empty = Some(
                            parse_bool(raw)
                                .with_context(|| format!("parse ALLOW_EMPTY {value}"))?,
                        );
                    }
                }
                "NODE_HEARTBEAT" => {
                    let n = strip_quotes(value)
                        .parse::<u64>()
                        .with_context(|| format!("parse NODE_HEARTBEAT {value}"))?;
                    heartbeat_ms = Some(n);
                }
                "PASSWORD_FILE" => {
                    let raw_pw = strip_quotes(value);
                    if !raw_pw.is_empty() &&
                        let Some(base) = home_dir.as_ref()
                    {
                        password_file = Some(resolve_runtime_path(base, raw_pw)?);
                    }
                }
                "VERB" => {
                    let raw = strip_quotes(value);
                    if !raw.is_empty() {
                        let rank =
                            raw.parse::<u8>().with_context(|| format!("parse VERB {value}"))?;
                        if rank > 3 {
                            eyre::bail!(
                                "VERB must be 0-3 (got {rank}): 0=errors only, 1=info, 2=debug, 3=everything"
                            );
                        }
                        verb = Some(rank);
                    }
                }
                // Total-per-pass tx budget (`TXPOOL_VAL_MAX_TXS`). Optional, blank means
                // the built-in default; a non-blank value must parse and be non-zero —
                // a zero budget would never seal anything.
                "TXPOOL_VAL_MAX_TXS" => {
                    let raw = strip_quotes(value);
                    if raw.is_empty() {
                        return Ok(());
                    }
                    let n = raw
                        .parse::<usize>()
                        .with_context(|| format!("parse TXPOOL_VAL_MAX_TXS {value}"))?;
                    if n == 0 {
                        eyre::bail!(
                            "TXPOOL_VAL_MAX_TXS must be >= 1 (got 0) — a zero budget never seals"
                        );
                    }
                    txpool_val_max_txs = Some(n);
                }
                // Per-class caps. Optional, so a blank value means "use the default" rather than
                // being an error; a non-blank value must parse and be non-zero, because a zero
                // cap would silently select nothing for that class.
                "MAX_NATIVE_PER_BLOCK" |
                "MAX_ERC_20_PER_BLOCK" |
                "MAX_CONTRACTS_PER_BLOCK" |
                "MAX_NFT_PER_BLOCK" => {
                    let raw = strip_quotes(value);
                    if raw.is_empty() {
                        return Ok(());
                    }
                    let n = raw.parse::<u64>().with_context(|| format!("parse {name} {value}"))?;
                    if n == 0 {
                        eyre::bail!("{name} must be >= 1 (got 0) — a zero cap selects nothing");
                    }
                    match name {
                        "MAX_NATIVE_PER_BLOCK" => max_native_per_block = Some(n),
                        "MAX_ERC_20_PER_BLOCK" => max_erc20_per_block = Some(n),
                        "MAX_CONTRACTS_PER_BLOCK" => max_contracts_per_block = Some(n),
                        _ => max_nft_per_block = Some(n),
                    }
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(e) = parsed {
            // `{:#}` renders the whole eyre chain on one line so it joins the bullet list.
            problems.push(format!("{name}: {e:#}"));
        }
    }

    // Config only, no built-in defaults: the keystore's keys decide the validator part.
    // NODE_URL / CHAINID / SEED_TX_* / INTERVAL / CANONICAL_PROBE_* / RPC_TIMEOUT_MS are REMOVED:
    // the merged validator addresses its own node in-proc (no HTTP), the chain id comes from the
    // database (single source), the fee receiver is derived from the private key, and the old
    // RPC-era timings are folded into HEARTBEAT_MS. COINBASE is a node-level keystore selector,
    // parsed by `smartnet-fs-util` for every role — not part of this section.
    const REQUIRED: &[&str] = &["KEY_PATH", "PASSWORD_FILE"];
    // The validator section is OPTIONAL and the switch is SEAL: absent or false = the
    // merged validator is simply off (a disabled config with placeholder values — every
    // consumer must consult `enabled` before using any field). With SEAL = true the
    // keystore keys are required as before.
    let enabled = seal == Some(true);
    if enabled {
        let missing: Vec<&str> = REQUIRED.iter().copied().filter(|k| !seen.contains(k)).collect();
        if !missing.is_empty() {
            problems.push(format!(
                "missing required key(s): {} (template: smartnet/example.config.toml, \
                 validator section)",
                missing.join(", ")
            ));
        }
    }

    // Value sanity checks: an empty or insane value is reported rather than falling back
    // to a default. A bad config must never produce a validator that runs against the
    // wrong chain, the wrong node, or no keystore.
    if heartbeat_ms == Some(0) {
        problems.push("NODE_HEARTBEAT is 0 — must be at least 1".to_string());
    }

    if !problems.is_empty() {
        eyre::bail!(
            "config `{}` is not usable ({} problem(s)):\n  - {}\nSmartNet ships no built-in \
             defaults for operator config — fill in every key above (template: \
             smartnet/example.config.toml, validator section)",
            path.display(),
            problems.len(),
            problems.join("\n  - ")
        );
    }

    if !enabled {
        // Disabled = no sealing, but the keystore keys are still returned when present: the
        // mesh identity (same KEY_PATH/PASSWORD_FILE) loads even on a non-sealing node. The
        // node-level COINBASE selector is read by `smartnet-fs-util`, not by this section.
        return Ok(ValidatorConfig {
            key_dir: key_dir.unwrap_or_default(),
            password_file,
            verb,
            selection_caps: SelectionCaps {
                max_native_per_block: max_native_per_block
                    .unwrap_or_else(|| SelectionCaps::default().max_native_per_block),
                max_erc20_per_block: max_erc20_per_block
                    .unwrap_or_else(|| SelectionCaps::default().max_erc20_per_block),
                max_contracts_per_block: max_contracts_per_block
                    .unwrap_or_else(|| SelectionCaps::default().max_contracts_per_block),
                max_nft_per_block: max_nft_per_block
                    .unwrap_or_else(|| SelectionCaps::default().max_nft_per_block),
            },
            allow_empty: allow_empty.unwrap_or(false),
            heartbeat_ms: heartbeat_ms.unwrap_or(crate::DEFAULT_HEARTBEAT_MS),
            txpool_val_max_txs: txpool_val_max_txs.unwrap_or(DEFAULT_TXPOOL_VAL_MAX_TXS),
            enabled: false,
        });
    }

    // Every problem above implies the matching key was missing or blank, so these are all
    // `Some` by now.
    let key_dir = key_dir.expect("checked above");

    Ok(ValidatorConfig {
        key_dir,
        password_file,
        verb,
        selection_caps: SelectionCaps {
            max_native_per_block: max_native_per_block
                .unwrap_or_else(|| SelectionCaps::default().max_native_per_block),
            max_erc20_per_block: max_erc20_per_block
                .unwrap_or_else(|| SelectionCaps::default().max_erc20_per_block),
            max_contracts_per_block: max_contracts_per_block
                .unwrap_or_else(|| SelectionCaps::default().max_contracts_per_block),
            max_nft_per_block: max_nft_per_block
                .unwrap_or_else(|| SelectionCaps::default().max_nft_per_block),
        },
        // Empty txpool: absent/blank = false — an idle beat, never an empty block.
        allow_empty: allow_empty.unwrap_or(false),
        // Validator seal cadence: absent = 2000 ms. 0 = rejected above.
        heartbeat_ms: heartbeat_ms.unwrap_or(crate::DEFAULT_HEARTBEAT_MS),
        // Total txs one pass processes: absent/blank = 512; 0 is rejected above.
        txpool_val_max_txs: txpool_val_max_txs.unwrap_or(DEFAULT_TXPOOL_VAL_MAX_TXS),
        enabled: true,
    })
}

fn parse_bool(s: &str) -> Result<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => {
            eyre::bail!("expected true/false, got {other}");
        }
    }
}

/// The config keys the `keystore` command actually uses: `HOME_DIR_PATH`, `KEY_PATH` and
/// `PASSWORD_FILE`. The latter two are relative to the root.
///
/// It makes no RPC call and touches no chain, and deliberately does not run the full
/// validation pass. The password path comes from the named config, never from a CLI flag;
/// a blank `PASSWORD_FILE` means prompt without creating a file.
pub(crate) struct ImportConfig {
    pub key_dir: PathBuf,
    pub password_file: Option<PathBuf>,
}

pub(crate) fn load_import_config(path: &Path) -> Result<ImportConfig> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    let base = home_dir_from_config(&raw)?;

    let mut key_path: Option<String> = None;
    let mut password_file: Option<PathBuf> = None;

    for line in raw.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() || !line.contains('=') {
            continue;
        }
        let mut parts = line.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let value = strip_quotes(parts.next().unwrap_or("").trim()).to_string();
        match key {
            "KEY_PATH" => key_path = Some(value),
            "PASSWORD_FILE" if !value.is_empty() => {
                password_file = Some(resolve_runtime_path(&base, &value)?);
            }
            _ => {}
        }
    }

    let key_path = key_path.ok_or_else(|| {
        eyre::eyre!(
            "config {} has no KEY_PATH — keystore needs somewhere to write the keystore \
             (template: smartnet/example.config.toml)",
            path.display()
        )
    })?;
    let key_dir = resolve_runtime_path(&base, &key_path)?;

    Ok(ImportConfig { key_dir, password_file })
}

/// Tests touch **only** the shipped template `validator/example.config.toml`, and only by
/// reading it. A config file is never written by a test: the template is the single config
/// source, `make install` copies it to `~/.smartnet/validator/config.toml`, and that
/// installed copy is the only config the validator ever reads.
#[cfg(test)]
mod tests {
    use super::*;
    use smartnet_fs_util::runtime_paths::HOME_DIR_KEY;

    /// The template — the shared node template at the workspace root; the only file these
    /// tests are allowed to read.
    fn template() -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example.config.toml")
    }

    /// The template must declare every validator key (a drift guard against the required-key
    /// list in `load_validator_config`). Keys ship uncommented with working values (the
    /// section is active by default); a declaration counts commented too, in case a key is
    /// re-optionalized later.
    #[test]
    fn shipped_template_declares_every_validator_key() {
        let template = template();
        let raw = std::fs::read_to_string(&template)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", template.display()));
        let declares = |key: &str| {
            raw.lines().any(|l| {
                l.trim().starts_with(&format!("{key} ")) ||
                    l.trim().starts_with(&format!("# {key} "))
            })
        };
        for key in [
            HOME_DIR_KEY,
            "SEAL",
            "ALLOW_EMPTY",
            "KEY_PATH",
            "PASSWORD_FILE",
            "VERB",
            "MAX_NATIVE_PER_BLOCK",
            "MAX_ERC_20_PER_BLOCK",
            "MAX_CONTRACTS_PER_BLOCK",
            "MAX_NFT_PER_BLOCK",
            "TXPOOL_VAL_MAX_TXS",
        ] {
            assert!(declares(key), "template is missing {key}: {}", template.display());
        }
        // The heartbeat ships a working default value, uncommented.
        assert!(
            raw.lines().any(|l| l.starts_with("NODE_HEARTBEAT =")),
            "template must ship NODE_HEARTBEAT uncommented: {}",
            template.display()
        );
    }

    /// `HOME_DIR_PATH` expands a leading `~/` once against `HOME`; a *named* user
    /// (`~alice/…`) is rejected instead of folding into `$HOME/alice/…`.
    #[test]
    fn home_dir_expands_tilde_once_and_rejects_named_user() {
        use smartnet_fs_util::runtime_paths::home_dir_from_config_with;

        let home = std::env::temp_dir().join("smartnet-home-dir-test");
        let home_str = home.to_string_lossy().to_string();
        assert_eq!(
            home_dir_from_config_with("HOME_DIR_PATH = \"~/keys\"\n", Some(&home_str)).unwrap(),
            home.join("keys")
        );
        assert_eq!(
            home_dir_from_config_with("HOME_DIR_PATH = \"~/.smartnetX/\"\n", Some(&home_str))
                .unwrap(),
            home.join(".smartnetX")
        );
        let err = home_dir_from_config_with("HOME_DIR_PATH = \"~alice/keys\"\n", Some(&home_str))
            .expect_err("`~user` must be rejected");
        assert!(err.to_string().contains("~user"), "got: {err}");
    }

    /// `VAL_FILE` is node-owned: the validator config neither requires nor parses it. The
    /// validator asks the node for the registry through `ValidatorIo`.
    #[test]
    fn validator_config_does_not_own_val_file() {
        let base = std::fs::read_to_string(template()).expect("template readable");
        let without = base
            .lines()
            .filter(|line| !line.starts_with("VAL_FILE"))
            .collect::<Vec<_>>()
            .join("\n");
        load_from_str(&without, "val-file-absent").expect("validator needs no VAL_FILE");

        let blank = base.replace("VAL_FILE = ", "VAL_FILE = \"\" # ");
        load_from_str(&blank, "val-file-blank").expect("blank VAL_FILE is not validator data");
    }

    /// The shipped template carries working validator values (its own header states so): an
    /// unedited copy loads with the validator ON — keystore paths present, the 500 ms
    /// heartbeat, the 512-tx budget, and the default per-class caps.
    #[test]
    fn unedited_template_loads_with_validator_enabled_defaults() {
        let cfg = load_validator_config(&template()).expect("the shipped template must load as-is");
        assert!(cfg.enabled, "template ships SEAL = true");
        assert_eq!(cfg.heartbeat_ms, 500);
        assert_eq!(cfg.txpool_val_max_txs, 512);
        assert_eq!(cfg.selection_caps, SelectionCaps::default());
    }

    /// `NODE_HEARTBEAT` is honored, not decorative: a non-default value reaches the config and
    /// 0 is rejected (no silent fallback).
    #[test]
    fn heartbeat_ms_is_parsed_not_ignored() {
        let text = format!("{}\nNODE_HEARTBEAT = 900", filled_template("VERB = 1"));
        let cfg = load_from_str(&text, "hb-900").expect("heartbeat 900 must load");
        assert_eq!(cfg.heartbeat_ms, 900);

        let zero = format!("{}\nNODE_HEARTBEAT = 0", filled_template("VERB = 1"));
        let err = load_from_str(&zero, "hb-0").expect_err("heartbeat 0 must be rejected");
        assert!(err.to_string().contains("NODE_HEARTBEAT"), "got: {err}");
    }

    /// `TXPOOL_VAL_MAX_TXS` reaches the config: the template ships its 512 default, a
    /// numeric value parses, blank falls back to the default, and 0 is rejected (a zero
    /// budget would never seal).
    #[test]
    fn txpool_val_max_txs_is_parsed_and_defaults_to_512() {
        let base = std::fs::read_to_string(template()).expect("template readable");
        assert!(
            base.contains("TXPOOL_VAL_MAX_TXS = 512"),
            "template must ship TXPOOL_VAL_MAX_TXS = 512"
        );

        let cfg = load_from_str(&base, "val-max-default").expect("template budget must load");
        assert_eq!(cfg.txpool_val_max_txs, 512);

        let cfg = load_from_str(
            &base.replace("TXPOOL_VAL_MAX_TXS = 512", "TXPOOL_VAL_MAX_TXS = 128"),
            "val-max-128",
        )
        .expect("a numeric budget must load");
        assert_eq!(cfg.txpool_val_max_txs, 128);

        let cfg = load_from_str(
            &base.replace("TXPOOL_VAL_MAX_TXS = 512", "TXPOOL_VAL_MAX_TXS ="),
            "val-max-blank",
        )
        .expect("blank must fall back to the default");
        assert_eq!(cfg.txpool_val_max_txs, 512);

        let err = load_from_str(
            &base.replace("TXPOOL_VAL_MAX_TXS = 512", "TXPOOL_VAL_MAX_TXS = 0"),
            "val-max-zero",
        )
        .expect_err("a zero budget must be rejected");
        assert!(err.to_string().contains("TXPOOL_VAL_MAX_TXS"), "got: {err}");
    }

    /// `ALLOW_EMPTY` is optional and boolean: absent/blank = false, `true` = seal empty blocks.
    #[test]
    fn allow_empty_is_optional_and_boolean() {
        let base = filled_template("VERB = 1");
        let without = base
            .lines()
            .filter(|l| !l.trim_start().starts_with("ALLOW_EMPTY"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!load_from_str(&without, "ae-absent").unwrap().allow_empty, "absent = false");
        assert!(!load_from_str(&base, "ae-false").unwrap().allow_empty, "false stays false");

        let enabled = base.replace("ALLOW_EMPTY = false", "ALLOW_EMPTY = true");
        assert!(load_from_str(&enabled, "ae-true").unwrap().allow_empty, "true = seal empty");

        let bad = base.replace("ALLOW_EMPTY = false", "ALLOW_EMPTY = maybe");
        let err = load_from_str(&bad, "ae-bad").expect_err("only true/false is accepted");
        assert!(err.to_string().contains("ALLOW_EMPTY"), "got: {err}");
    }

    /// SEAL = true with NO keystore keys anywhere must fail, naming every missing key.
    #[test]
    fn enabled_section_requires_keystore_keys() {
        let enabled: String = template_text_uncommented()
            .lines()
            .filter(|l| !l.starts_with("KEY_PATH") && !l.starts_with("PASSWORD_FILE"))
            .collect::<Vec<_>>()
            .join("\n")
            .replace("SEAL = false", "SEAL = true");
        let err = load_from_str(&enabled, "enabled-no-keys")
            .expect_err("SEAL = true without the keystore keys must fail");
        let msg = err.to_string();
        for key in ["KEY_PATH", "PASSWORD_FILE"] {
            assert!(msg.contains(key), "expected {key} in: {msg}");
        }
    }

    /// `SEAL = false` (or absent) disables the validator even when the section is filled in:
    /// the flag is the explicit switch.
    #[test]
    fn seal_false_disables_even_a_filled_section() {
        let filled = filled_template("VERB = 1").replace("SEAL = true", "SEAL = false");
        let cfg = load_from_str(&filled, "seal-false").expect("SEAL = false must load");
        assert!(!cfg.enabled);
    }

    /// Pure comment-stripping rules — no file involved.
    #[test]
    fn strip_comment_only_cuts_hash_outside_quotes_and_after_whitespace() {
        assert_eq!(strip_comment("# just a comment"), "");
        assert_eq!(strip_comment("#KEY = 1"), "");
        assert_eq!(strip_comment("KEY = value # trailing"), "KEY = value ");
        assert_eq!(strip_comment("KEY = value#nospace"), "KEY = value#nospace");
        assert_eq!(strip_comment(r#"KEY = "a#b""#), r#"KEY = "a#b""#);
        assert_eq!(strip_comment(r#"KEY = "http://h/#frag""#), r#"KEY = "http://h/#frag""#);
        assert_eq!(strip_comment("KEY = 1"), "KEY = 1");
        assert_eq!(strip_comment("  # indented comment"), "  ");
        // An apostrophe inside a bare word must not swallow the trailing comment marker.
        assert_eq!(strip_comment("KEY = we don't # cut"), "KEY = we don't ");
    }

    /// Builds a complete ENABLED section: every validator key as the template ships it —
    /// uncommented, documented value. Comment lines are skipped entirely: a wrapped comment
    /// tail may begin with a key name (the heartbeat comment reads "… (only when /
    /// SEAL = true), then …") and must never be mistaken for a setting. Inline-comment tails
    /// on real key lines are cut.
    fn template_text_uncommented() -> String {
        let raw = std::fs::read_to_string(template()).expect("read template");
        raw.lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .filter(|l| {
                [
                    HOME_DIR_KEY,
                    "SEAL",
                    "ALLOW_EMPTY",
                    "KEY_PATH",
                    "PASSWORD_FILE",
                    "VERB",
                    "MAX_NATIVE_PER_BLOCK",
                    "MAX_ERC_20_PER_BLOCK",
                    "MAX_CONTRACTS_PER_BLOCK",
                    "MAX_NFT_PER_BLOCK",
                    "TXPOOL_VAL_MAX_TXS",
                ]
                .iter()
                .any(|k| l.starts_with(&format!("{k} ")))
            })
            .map(|l| {
                let cut = strip_comment(l.split('(').next().unwrap_or(l));
                cut.trim().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The enabled template with one substituted `VERB` line.
    fn filled_template(verb_line: &str) -> String {
        template_text_uncommented()
            .lines()
            .map(
                |line| {
                    if line.starts_with("VERB") {
                        verb_line.to_string()
                    } else {
                        line.to_string()
                    }
                },
            )
            .collect::<Vec<_>>()
            .join("\n")
            .replace("SEAL = false", "SEAL = true")
    }

    fn load_from_str(text: &str, tag: &str) -> Result<ValidatorConfig> {
        let path = std::env::temp_dir().join(format!("smartnet-val-config-test-{tag}.toml"));
        std::fs::write(&path, text).expect("write temp config");
        let out = load_validator_config(&path);
        let _ = std::fs::remove_file(&path);
        out
    }

    /// `VERB` is optional and clamped to 0-3; unset keeps the default (info).
    #[test]
    fn verb_is_optional_and_clamped_to_zero_through_three() {
        for rank in ["0", "1", "2", "3"] {
            let cfg = load_from_str(&filled_template(&format!("VERB = {rank}")), rank)
                .unwrap_or_else(|e| panic!("VERB = {rank} must load: {e}"));
            assert_eq!(cfg.verb, Some(rank.parse::<u8>().unwrap()), "VERB = {rank}");
        }
        // Blank means "use the default" rather than a parse error.
        let cfg = load_from_str(&filled_template("VERB ="), "blank").expect("blank VERB must load");
        assert_eq!(cfg.verb, None, "blank VERB must fall back to the default (info)");
        // Unset entirely is also allowed.
        let uninstalled = filled_template("VERB = 1")
            .lines()
            .filter(|l| !l.trim_start().starts_with("VERB"))
            .collect::<Vec<_>>()
            .join("\n");
        let cfg = load_from_str(&uninstalled, "unset").expect("absent VERB must load");
        assert_eq!(cfg.verb, None);
        // Anything above 3 is a config error, not a silent clamp.
        let err = load_from_str(&filled_template("VERB = 4"), "over")
            .expect_err("VERB = 4 must be rejected");
        assert!(err.to_string().contains("VERB must be 0-3"), "got: {err}");
    }

    /// `COINBASE` is node-level: this section recognizes it so it is not rejected as unknown,
    /// but does not own or parse it. `smartnet-fs-util` reads it for every node role.
    #[test]
    fn coinbase_is_node_owned_not_a_validator_field() {
        let base = filled_template("VERB = 1");
        let text = format!("{base}\nCOINBASE = \"0x70997970c51812dc3a010c7d01b50e0d17dc79c8\"\n");
        let cfg = load_from_str(&text, "coinbase-node-owned").expect("node key must be accepted");
        assert!(cfg.enabled, "COINBASE must not gate the validator section");
    }

    /// A lone quote must be reported, never a panic: `strip_quotes` used to slice `&s[1..0]`
    /// for `"` / `'` (start > end), taking the whole validator down on an operator's typo.
    #[test]
    fn strip_quotes_survives_a_lone_quote_without_panicking() {
        // A single quote both starts and ends its own string — the case that panicked.
        assert_eq!(strip_quotes("\""), "\"");
        assert_eq!(strip_quotes("'"), "'");
        assert_eq!(strip_quotes("\"  "), "\"");
        // Paired quotes still strip, including an empty pair.
        assert_eq!(strip_quotes("\"\""), "");
        assert_eq!(strip_quotes("''"), "");
        assert_eq!(strip_quotes("'abc'"), "abc");
        assert_eq!(strip_quotes("\"abc\""), "abc");
        // Unterminated or mixed quotes are left exactly as written, not guessed at.
        assert_eq!(strip_quotes("\"abc"), "\"abc");
        assert_eq!(strip_quotes("abc\""), "abc\"");
        assert_eq!(strip_quotes("'abc\""), "'abc\"");
        assert_eq!(strip_quotes(" plain "), "plain");
    }

    /// End to end: a config carrying a lone quote in a *required* value fails to boot with a
    /// message naming the key, instead of panicking inside the parser.
    #[test]
    fn a_lone_quote_is_a_reported_problem_not_a_crash() {
        let cfg = filled_template("VERB = 1").replace("KEY_PATH = ", "KEY_PATH = \"");
        let err = load_from_str(&cfg, "lone-quote").expect_err("KEY_PATH = \" must not boot");
        let msg = err.to_string();
        assert!(msg.contains("KEY_PATH"), "expected the key in: {msg}");
        assert!(msg.contains("no built-in defaults"), "got: {msg}");
    }

    /// The "every problem at once" promise covers parse failures too: a config with several
    /// bad values reports all of them in one shot, so an operator never has to fix-and-restart
    /// one key per boot.
    #[test]
    fn every_parse_failure_is_reported_in_one_boot() {
        let cfg = filled_template("VERB = 1")
            .replace("PASSWORD_FILE =", "PASSWORD_FILE = /etc/passwd")
            .replace("VERB = 1", "VERB = 9");
        let err = load_from_str(&cfg, "many-parse-errors").expect_err("bad values must fail");
        let msg = err.to_string();
        assert!(msg.contains("PASSWORD_FILE"), "expected PASSWORD_FILE in: {msg}");
        // The out-of-range VERB aborts nowhere — it is collected like every other problem.
        assert!(msg.contains("VERB must be 0-3"), "got: {msg}");
    }

    /// Substitutes one `MAX_*` line in the filled template (or blanks / removes it).
    fn filled_with_cap(key: &str, replacement: &str) -> String {
        filled_template("VERB = 1")
            .lines()
            .map(|line| {
                if line.trim_start().starts_with(key) {
                    replacement.to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The four per-class caps default to 64 / 32 / 1 / 2 when unset or blank, and an explicit
    /// value must be >= 1 — a zero cap would silently select nothing for that class.
    #[test]
    fn per_class_caps_default_when_unset_and_reject_zero() {
        let d = SelectionCaps::default();
        assert_eq!((d.max_native_per_block, d.max_erc20_per_block), (64, 32));
        assert_eq!((d.max_contracts_per_block, d.max_nft_per_block), (1, 2));

        // The shipped template carries the defaults verbatim, so a filled template loads them.
        let cfg = load_from_str(&filled_template("VERB = 1"), "template")
            .expect("template with caps must load");
        assert_eq!(cfg.selection_caps, d, "template must ship the documented defaults");

        // Blank keeps the default.
        let cfg = load_from_str(
            &filled_with_cap("MAX_NATIVE_PER_BLOCK", "MAX_NATIVE_PER_BLOCK ="),
            "blank",
        )
        .expect("blank cap must load");
        assert_eq!(cfg.selection_caps.max_native_per_block, 64);

        // Unset entirely also keeps the default.
        let cfg = load_from_str(&filled_with_cap("MAX_NFT_PER_BLOCK", ""), "unset")
            .expect("absent cap must load");
        assert_eq!(cfg.selection_caps.max_nft_per_block, 2);

        // An explicit value is honoured.
        let cfg = load_from_str(
            &filled_with_cap("MAX_ERC_20_PER_BLOCK", "MAX_ERC_20_PER_BLOCK = 7"),
            "explicit",
        )
        .expect("explicit cap must load");
        assert_eq!(cfg.selection_caps.max_erc20_per_block, 7);

        // Zero is a config error, not a silent "select nothing".
        let err = load_from_str(
            &filled_with_cap("MAX_CONTRACTS_PER_BLOCK", "MAX_CONTRACTS_PER_BLOCK = 0"),
            "zero",
        )
        .expect_err("zero cap must be rejected");
        assert!(err.to_string().contains("MAX_CONTRACTS_PER_BLOCK must be >= 1"), "got: {err}");

        // A non-numeric value is a parse error, not a silent default.
        let err = load_from_str(
            &filled_with_cap("MAX_NATIVE_PER_BLOCK", "MAX_NATIVE_PER_BLOCK = lots"),
            "junk",
        )
        .expect_err("non-numeric cap must be rejected");
        assert!(err.to_string().contains("parse MAX_NATIVE_PER_BLOCK"), "got: {err}");
    }
}

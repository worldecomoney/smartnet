//! Operator config file (`KEY=value`, same style as the node operator config).

use eyre::{Context, Result};
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
    /// Keystore filename under `key_dir` (`KEY_FILE`). Empty = not initialized.
    pub key_file: String,
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
    /// Validator seal-beat cadence (`HEARTBEAT_MS`, ms between txpool/seal beats).
    /// Optional; absent = 2000, 0 is rejected.
    pub heartbeat_ms: u64,
    /// Is the validator section present in the config at all? THE SECTION IS OPTIONAL:
    /// a node config may carry no validator keys at all — then this is false and the
    /// merged flow simply does nothing (no keystore, no sealing). When true, every key
    /// of the section is required as before.
    pub enabled: bool,
}

impl ValidatorConfig {
    pub(crate) fn is_key_initialized(&self) -> bool {
        !self.key_file.trim().is_empty()
    }

    pub(crate) fn keystore_path(&self) -> Result<PathBuf> {
        if !self.is_key_initialized() {
            eyre::bail!(
                "KEY_FILE is empty (keystore not created); run `smartnet-validator keystore` first"
            );
        }
        Ok(self.key_dir.join(self.key_file.trim()))
    }
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

/// Expand a leading `~`. `HOME` unset is an error — never silently keep the literal `~/…`
/// (it would resolve against the current working directory) and never fall back to a
/// built-in path.
///
/// Only `~` (bare) and `~/…` expand, both relative to `HOME`. A *named* user —
/// `~alice/…` — has no portable meaning here, so it is rejected rather than folded into
/// `$HOME/alice/…`, which would silently resolve to the wrong directory.
pub fn expand_home(p: &str) -> Result<PathBuf> {
    if let Some(rest) = p.strip_prefix('~') {
        let rel = match rest {
            "" => "",
            _ => rest.strip_prefix('/').ok_or_else(|| {
                eyre::eyre!(
                    "config path {p:?} starts with `~user` — only `~` and `~/…` are expanded \
                     (relative to HOME); use an absolute path instead"
                )
            })?,
        };
        let home = std::env::var("HOME").map_err(|_| {
            eyre::eyre!(
                "config path {p:?} starts with `~` but HOME is not set — use an absolute path \
                 instead (no built-in fallback path exists)"
            )
        })?;
        return Ok(if rel.is_empty() { PathBuf::from(home) } else { PathBuf::from(home).join(rel) });
    }
    Ok(PathBuf::from(p))
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

/// Load the operator config: `KEY_PATH`, `KEY_FILE`, `PASSWORD_FILE`, `NODE_URL`, `COINBASE`,
/// `CHAINID`,
/// `INTERVAL`, `ALLOW_EMPTY`, `CANONICAL_PROBE_TIMEOUT_MS`, `CANONICAL_PROBE_INTERVAL_MS`,
/// `CANONICAL_PROBE_MAX_INTERVAL_MS`, `RPC_TIMEOUT_MS`, `SEED_TX_TO`, `SEED_TX_VALUE_WEI`,
/// `SEED_TX_GAS_LIMIT`. `VERB` is optional (defaults to 1 = info), and so are the four
/// per-class caps `MAX_NATIVE_PER_BLOCK` (64), `MAX_ERC_20_PER_BLOCK` (32),
/// `MAX_CONTRACTS_PER_BLOCK` (1) and `MAX_NFT_PER_BLOCK` (2), and the total-per-pass
/// budget `TXPOOL_VAL_MAX_TXS` (512).
///
/// Config only — every required key has no built-in default and any of them missing aborts
/// startup. The key list ships in `validator/example.config.toml` (the `make install` source);
/// the template-drift test keeps the two in step.
///
/// Public so the node's mesh service can load the same keystore identity (same
/// `KEY_PATH`/`KEY_FILE`/`PASSWORD_FILE` triple) at boot — one identity, one config.
pub fn load_validator_config(path: &Path) -> Result<ValidatorConfig> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;

    let mut key_dir: Option<PathBuf> = None;
    let mut key_file: Option<String> = None;
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

    // Every problem found is collected so an unedited config reports all of them in one
    // error instead of making the operator play whack-a-mole.
    let mut seen: Vec<&'static str> = Vec::new();
    let mut problems: Vec<String> = Vec::new();

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
            "KEY_FILE" => "KEY_FILE",
            "PASSWORD_FILE" => "PASSWORD_FILE",
            "VERB" => "VERB",
            "MAX_NATIVE_PER_BLOCK" => "MAX_NATIVE_PER_BLOCK",
            "MAX_ERC_20_PER_BLOCK" => "MAX_ERC_20_PER_BLOCK",
            "MAX_CONTRACTS_PER_BLOCK" => "MAX_CONTRACTS_PER_BLOCK",
            "MAX_NFT_PER_BLOCK" => "MAX_NFT_PER_BLOCK",
            "TXPOOL_VAL_MAX_TXS" => "TXPOOL_VAL_MAX_TXS",
            "SEAL" => "SEAL",
            "ALLOW_EMPTY" => "ALLOW_EMPTY",
            "HEARTBEAT_MS" => "HEARTBEAT_MS",
            // The config file is now the shared NODE config (the old validator config is
            // gone): other keys on it belong to the node CLI and are parsed (fail-closed for
            // their own keys) by the node's overlay — anonymous keys here belong to the node
            // section, not to the validator, and are ignored, not rejected.
            _ => continue,
        };
        seen.push(name);

        // A key that is present but blank is a problem in its own right — there is no
        // built-in default to fall back to. Three keys legitimately accept an empty value:
        // `KEY_FILE` (keystore not created yet), `PASSWORD_FILE` (prompt for the password
        // instead of reading a file), `VERB` (optional — blank means the default, 1), the
        // four class caps (optional — blank means the built-in default) and
        // `TXPOOL_VAL_MAX_TXS` (optional — blank means the built-in default, 512).
        if strip_quotes(value).trim().is_empty() &&
            name != "SEAL" &&
            name != "KEY_FILE" &&
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
                "KEY_PATH" => key_dir = Some(expand_home(strip_quotes(value))?),
                "KEY_FILE" => key_file = Some(strip_quotes(value).to_string()),
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
                "HEARTBEAT_MS" => {
                    let n = strip_quotes(value)
                        .parse::<u64>()
                        .with_context(|| format!("parse HEARTBEAT_MS {value}"))?;
                    heartbeat_ms = Some(n);
                }
                "PASSWORD_FILE" => {
                    let raw_pw = strip_quotes(value);
                    if !raw_pw.is_empty() {
                        password_file = Some(expand_home(raw_pw)?);
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
    // NODE_URL / CHAINID / COINBASE / SEED_TX_* / INTERVAL / ALLOW_EMPTY / CANONICAL_PROBE_* /
    // RPC_TIMEOUT_MS are REMOVED: the merged validator addresses its own node in-proc (no
    // HTTP), the chain id comes from the database (single source), the coinbase is derived
    // from the private key, and the old RPC-era timings are folded into HEARTBEAT_MS.
    const REQUIRED: &[&str] = &["KEY_PATH", "KEY_FILE", "PASSWORD_FILE"];
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
    if let Some(dir) = key_dir.as_ref()
        && !dir.is_absolute() {
            problems.push(format!(
                "KEY_PATH is relative ({}) — an absolute path is required; resolving it \
                 against the current working directory would be a hidden default",
                dir.display()
            ));
        }
    if let Some(pw) = password_file.as_ref()
        && !pw.is_absolute() {
            problems.push(format!(
                "PASSWORD_FILE is relative ({}) — an absolute path is required (e.g. \
                 ~/.smartnet/validator/secure/password.txt); resolving it against the current \
                 working directory would be a hidden default",
                pw.display()
            ));
        }
    if heartbeat_ms == Some(0) {
        problems.push("HEARTBEAT_MS is 0 — must be at least 1".to_string());
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
        // Disabled = no sealing, but the keystore triple is still returned when present:
        // the mesh identity (same KEY_PATH/KEY_FILE/PASSWORD_FILE) loads even on a
        // non-sealing node. Seal-path consumers still gate on `enabled` before using it.
        return Ok(ValidatorConfig {
            key_dir: key_dir.unwrap_or_default(),
            key_file: key_file.unwrap_or_default(),
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
    let key_file = key_file.expect("checked above");

    Ok(ValidatorConfig {
        key_dir,
        key_file,
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

/// The config keys the `keystore` command actually uses.
///
/// It makes no RPC call and touches no chain, and it is the one command an operator runs
/// *before* `COINBASE`/`SEED_TX_TO` have any value — the command is what writes them. So it
/// reads only what it needs (`KEY_PATH`) and deliberately does not run the full validation
/// pass; the full pass runs normally at seal time. `KEY_FILE` and `PASSWORD_FILE` are never
/// read here: the command always names the keystore `UTC--…--<address>` itself and the
/// password path comes from `--password`.
pub(crate) struct ImportConfig {
    pub key_dir: PathBuf,
}

pub(crate) fn load_import_config(path: &Path) -> Result<ImportConfig> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;

    let mut key_path: Option<String> = None;

    for line in raw.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() || !line.contains('=') {
            continue;
        }
        let mut parts = line.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let value = strip_quotes(parts.next().unwrap_or("").trim()).to_string();
        if key == "KEY_PATH" {
            key_path = Some(value);
        }
    }

    let key_path = key_path.ok_or_else(|| {
        eyre::eyre!(
            "config {} has no KEY_PATH — keystore needs somewhere to write the keystore \
             (template: validator/example.config.toml)",
            path.display()
        )
    })?;
    let key_dir = expand_home(&key_path)?;
    if !key_dir.is_absolute() {
        eyre::bail!("KEY_PATH is relative ({}) — an absolute path is required", key_dir.display());
    }

    Ok(ImportConfig { key_dir })
}

/// Rewrite (or append) the given `key = "value"` pairs in the operator config, in place,
/// preserving every other line — comments, ordering and blank lines all survive.
pub(crate) fn set_config_values(config_path: &Path, updates: &[(&str, &str)]) -> Result<()> {
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("read config {}", config_path.display()))?;
    let out = apply_config_updates(&raw, updates);
    write_config_atomically(config_path, &out)
        .with_context(|| format!("write config {}", config_path.display()))
}

/// Write `contents` to `path` atomically: a sibling temp file is written (and fsynced) first,
/// then renamed over the target. A truncate-then-write crash would otherwise leave a
/// half-written config the validator cannot boot from.
fn write_config_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // Best-effort: fsync the parent so the rename itself survives a crash. A failure here
    // does not invalidate the already-renamed file, so it is not fatal.
    if let Some(dir) = path.parent() &&
        let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Pure core of [`set_config_values`], split out so it can be tested on a string without
/// writing a config file.
pub(crate) fn apply_config_updates(raw: &str, updates: &[(&str, &str)]) -> String {
    let mut written: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut out = String::new();

    for line in raw.lines() {
        // Only an assignment can be an update target; a comment mentioning KEY_FILE is not.
        let trimmed = strip_comment(line).trim();
        let replaced = if let Some((key, _)) = trimmed.split_once('=') {
            let key = key.trim();
            updates.iter().find(|(k, _)| *k == key).map(|(_, value)| (key, *value))
        } else {
            None
        };
        match replaced {
            Some((key, value)) => {
                out.push_str(&format!("{key} = \"{value}\"\n"));
                written.insert(key);
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }

    // A key that is absent from the file is appended, so an import can complete even if the
    // template never shipped `COINBASE`/`SEED_TX_TO` as assignments.
    for (key, value) in updates {
        if written.contains(key) {
            continue;
        }
        if !out.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("{key} = \"{value}\"\n"));
        written.insert(key);
    }
    out
}

/// Tests touch **only** the shipped template `validator/example.config.toml`, and only by
/// reading it. A config file is never written by a test: the template is the single config
/// source, `make install` copies it to `~/.smartnet/validator/config.toml`, and that
/// installed copy is the only config the validator ever reads.
#[cfg(test)]
mod tests {
    use super::*;

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
            "SEAL",
            "ALLOW_EMPTY",
            "KEY_PATH",
            "KEY_FILE",
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
            raw.lines().any(|l| l.starts_with("HEARTBEAT_MS =")),
            "template must ship HEARTBEAT_MS uncommented: {}",
            template.display()
        );
    }

    /// `~` and `~/…` expand against `HOME`; a *named* user (`~alice/…`) is rejected instead
    /// of silently folding into `$HOME/alice/…`, which would resolve to the wrong directory.
    #[test]
    fn expand_home_expands_tilde_and_rejects_named_user() {
        if let Ok(home) = std::env::var("HOME") {
            let home = PathBuf::from(home);
            assert_eq!(expand_home("~").unwrap(), home);
            assert_eq!(expand_home("~/keys").unwrap(), home.join("keys"));
        }
        let err = expand_home("~alice/keys").expect_err("`~user` must be rejected");
        assert!(err.to_string().contains("~user"), "got: {err}");
    }

    /// The shipped template carries working validator values (its own header states so): an
    /// unedited copy loads with the validator ON — keystore paths present, the 2000 ms
    /// heartbeat, the 512-tx budget, and the default per-class caps.
    #[test]
    fn unedited_template_loads_with_validator_enabled_defaults() {
        let cfg = load_validator_config(&template()).expect("the shipped template must load as-is");
        assert!(cfg.enabled, "template ships SEAL = true");
        assert_eq!(cfg.heartbeat_ms, 2000);
        assert_eq!(cfg.txpool_val_max_txs, 512);
        assert_eq!(cfg.selection_caps, SelectionCaps::default());
    }

    /// `HEARTBEAT_MS` is honored, not decorative: a non-default value reaches the config and
    /// 0 is rejected (no silent fallback).
    #[test]
    fn heartbeat_ms_is_parsed_not_ignored() {
        let text = format!("{}\nHEARTBEAT_MS = 900", filled_template("VERB = 1"));
        let cfg = load_from_str(&text, "hb-900").expect("heartbeat 900 must load");
        assert_eq!(cfg.heartbeat_ms, 900);

        let zero = format!("{}\nHEARTBEAT_MS = 0", filled_template("VERB = 1"));
        let err = load_from_str(&zero, "hb-0").expect_err("heartbeat 0 must be rejected");
        assert!(err.to_string().contains("HEARTBEAT_MS"), "got: {err}");
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
            .filter(|l| {
                !l.starts_with("KEY_PATH") &&
                    !l.starts_with("KEY_FILE") &&
                    !l.starts_with("PASSWORD_FILE")
            })
            .collect::<Vec<_>>()
            .join("\n")
            .replace("SEAL = false", "SEAL = true");
        let err = load_from_str(&enabled, "enabled-no-keys")
            .expect_err("SEAL = true without the keystore keys must fail");
        let msg = err.to_string();
        for key in ["KEY_PATH", "KEY_FILE", "PASSWORD_FILE"] {
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
                    "SEAL",
                    "ALLOW_EMPTY",
                    "KEY_PATH",
                    "KEY_FILE",
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
            .replace("PASSWORD_FILE =", "PASSWORD_FILE = nonthes")
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

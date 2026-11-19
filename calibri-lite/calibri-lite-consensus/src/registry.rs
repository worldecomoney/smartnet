//! `validators.jsonl` — the local validator registry (miner-plus.md §2).
//!
//! One JSON object per line, mirroring the `mainnet.jsonl`
//! peer-file shape: tolerant line parsing (a malformed line is skipped with a
//! reported error, one bad line never disables the rest), blank lines and `#`
//! comments ignored, file re-read on every call so membership changes need no
//! restart.
//!
//! ```text
//! {"address":"0x...","status":true}
//! ```
//!
//! Status semantics (miner-plus.md §9A):
//!
//! | status  | meaning                                              |
//! | ------- | ---------------------------------------------------- |
//! | `true`  | enabled — may seal and may be accepted as a validator |
//! | `false` | disabled/dismissed — may NOT seal, blocks it signed are rejected |
//! | `null`  | registered candidate, not yet promoted               |
//!
//! **Only `status: true` addresses are returned.** Genesis validators get no
//! exemption: a genesis address with `status: false` in this file is out.
//!
//! Fail closed: a missing or unreadable file is an `Err` — callers must treat
//! that as "no enabled validators" (node rejects sealed blocks, validator does
//! not seal) until the file exists.

use alloy_primitives::Address;
use std::{
    io,
    path::{Path, PathBuf},
    sync::RwLock,
};
use tracing::warn;

/// Default registry file: `$HOME/.smartnet/validators.jsonl` (same home as
/// `bridges.json`). `HOME` unset yields an empty path — callers treat that as
/// "file missing" (fail closed).
pub fn default_validators_file() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".smartnet").join("validators.jsonl"),
        None => PathBuf::new(),
    }
}

/// One registry record: the address plus its tri-state status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatorRecord {
    /// Validator address (parsed, case-insensitive, `0x` optional in the file).
    pub address: Address,
    /// `true` = enabled, `false` = disabled, `null`/absent = candidate.
    pub status: Option<bool>,
}

impl ValidatorRecord {
    /// Enabled validators are exactly `status == Some(true)` (§9A).
    pub const fn is_enabled(&self) -> bool {
        matches!(self.status, Some(true))
    }
}

/// The RAM array (user-in.md #3) — written once at bootstrap by [`load_validators`],
/// read for the life of the process. Never re-read from disk.
static VALIDATORS: RwLock<Vec<ValidatorRecord>> = RwLock::new(Vec::new());

/// Parse registry content into records. Blank/`#` lines are skipped; a
/// malformed line is reported and skipped — one bad line never disables the
/// rest (same rule as `mainnet.jsonl`).
///
/// A line with an unparseable address is a per-line error (that record is
/// dropped — fail closed for THAT validator, the rest still load).
pub fn parse_validators(content: &str) -> (Vec<ValidatorRecord>, Vec<String>) {
    let mut records = Vec::new();
    let mut errors = Vec::new();

    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parsed = parse_record(line);
        match parsed {
            Ok(Some(record)) => records.push(record),
            Ok(None) => {
                // Valid JSON object but not a validator record (no address).
                errors.push(format!("line {}: no address field", index + 1));
            }
            Err(err) => errors.push(format!("line {}: {err}", index + 1)),
        }
    }
    (records, errors)
}

/// Parse one non-empty line into a record.
///
/// `address` must be a hex string (with or without `0x`); `status` must be
/// `true` / `false` / `null` (or absent → `None`).
fn parse_record(line: &str) -> Result<Option<ValidatorRecord>, String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("invalid JSON: {e}"))?;

    let Some(obj) = value.as_object() else {
        return Err("not a JSON object".to_string());
    };

    let address = match obj.get("address").and_then(|v| v.as_str()) {
        Some(a) => parse_address(a).ok_or_else(|| format!("bad address {a:?}"))?,
        None => return Ok(None), // no address — not a validator record
    };

    let status = match obj.get("status") {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => {
            Some(v.as_bool().ok_or_else(|| format!("status must be true/false/null, got {v}"))?)
        }
    };

    Ok(Some(ValidatorRecord { address, status }))
}

/// Hex address parse: with or without `0x`, any case (the file format is not
/// case-locked; SELECT normalizes via `addr_key` before hashing).
fn parse_address(s: &str) -> Option<Address> {
    let s = s.trim();
    let bare = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if bare.len() != 40 {
        return None;
    }
    let bytes = alloy_primitives::hex::decode(bare).ok()?;
    Some(Address::from_slice(&bytes))
}

/// Load and parse the registry file. Returns all records plus per-line errors
/// (callers log errors; only `is_enabled()` records may act as validators).
///
/// Missing/unreadable file → `Err` — **fail closed**: callers must treat this
/// as "no enabled validators" (no seal, no accepting sealed blocks) until the
/// file exists.
pub fn load_validators(path: &Path) -> Result<(Vec<ValidatorRecord>, Vec<String>), io::Error> {
    let content = std::fs::read_to_string(path)?;
    let parsed = parse_validators(&content);
    // Hold it in RAM for the life of the process (user-in.md #3): bootstrap loads ONCE
    // and every later question is answered from memory — the file is never re-opened.
    // The first successful load wins: a later load can never silently replace a running
    // registry.
    *VALIDATORS.write().expect("validators registry lock poisoned") = parsed.0.clone();
    Ok(parsed)
}

/// The RAM array — validators and their statuses, exactly as [`load_validators`] read
/// them (user-in.md #3).
///
/// Empty until bootstrap loads it: an unloaded registry is a **fail-closed** registry —
/// no validator is ever enabled by accident.
pub fn get_val_all() -> Vec<ValidatorRecord> {
    VALIDATORS.read().expect("validators registry lock poisoned").clone()
}

/// Convenience: the enabled (`status: true`) addresses only.
///
/// File errors propagate (fail closed); per-line parse errors are logged and
/// skipped — a broken line can only ever REMOVE a validator, never add one.
pub fn load_enabled_validators(path: &Path) -> Result<Vec<Address>, io::Error> {
    let (records, errors) = load_validators(path)?;
    for err in &errors {
        warn!(
            target: "validators_jsonl",
            error = %err,
            "validators.jsonl line rejected"
        );
    }
    Ok(records.into_iter().filter(|r| r.is_enabled()).map(|r| r.address).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const GENESIS: &str = "24bf191a358c09e79b9ead2d8999d08590b5af31";

    #[test]
    fn only_status_true_is_enabled() {
        let content = format!(
            r#"{{"address":"{GENESIS}","status":true}}
{{"address":"0x1111111111111111111111111111111111111111","status":false}}
{{"address":"0x2222222222222222222222222222222222222222","status":null}}
{{"address":"0x3333333333333333333333333333333333333333","status":true}}
"#
        );
        let (records, errors) = parse_validators(&content);
        assert!(errors.is_empty(), "clean file has no errors: {errors:?}");
        assert_eq!(records.len(), 4);
        assert!(records[0].is_enabled());
        assert!(!records[1].is_enabled());
        assert!(!records[2].is_enabled());
        assert!(records[3].is_enabled());

        let enabled: Vec<Address> =
            records.iter().filter(|r| r.is_enabled()).map(|r| r.address).collect();
        assert_eq!(
            enabled,
            vec![
                address!("24bf191a358c09e79b9ead2d8999d08590b5af31"),
                address!("0x3333333333333333333333333333333333333333"),
            ]
        );
    }

    #[test]
    fn example_file_disables_genesis_then_enables_it() {
        // The operator's two-state workflow (miner-plus.md): flip the genesis
        // address to false → not enabled; back to true → enabled again.
        let off = format!("{{\"address\":\"{GENESIS}\",\"status\":false}}\n");
        let (records, _) = parse_validators(&off);
        assert!(!records[0].is_enabled());

        let on = format!("{{\"address\":\"{GENESIS}\",\"status\":true}}\n");
        let (records, _) = parse_validators(&on);
        assert!(records[0].is_enabled());
    }

    #[test]
    fn address_parsing_is_prefix_and_case_tolerant() {
        for raw in [GENESIS, &format!("0x{GENESIS}"), &format!("0x{}", GENESIS.to_uppercase())] {
            let content = format!("{{\"address\":\"{raw}\",\"status\":true}}");
            let (records, errors) = parse_validators(&content);
            assert!(errors.is_empty(), "raw {raw:?}: {errors:?}");
            assert_eq!(records[0].address, address!("24bf191a358c09e79b9ead2d8999d08590b5af31"));
        }
    }

    #[test]
    fn blank_and_comment_lines_are_ignored() {
        let content = "\n# registry\n\n  \n{\"address\":\"0x1111111111111111111111111111111111111111\",\"status\":true}\n";
        let (records, errors) = parse_validators(content);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn malformed_line_is_skipped_not_fatal() {
        let content = format!(
            "not json at all\n{{\"address\":\"{GENESIS}\",\"status\":true}}\n{{\"address\":\"0xzz\",\"status\":true}}\n"
        );
        let (records, errors) = parse_validators(&content);
        assert_eq!(errors.len(), 2, "two bad lines reported: {errors:?}");
        assert_eq!(records.len(), 1, "the good line still loads");
        assert!(records[0].is_enabled());
    }

    #[test]
    fn missing_address_field_is_reported() {
        let (records, errors) = parse_validators("{\"status\":true}");
        assert!(records.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("address"), "{errors:?}");
    }

    #[test]
    fn bad_status_type_is_reported() {
        let (records, errors) = parse_validators(
            "{\"address\":\"0x1111111111111111111111111111111111111111\",\"status\":\"yes\"}",
        );
        assert!(records.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("status"), "{errors:?}");
    }

    #[test]
    fn missing_file_fails_closed() {
        let err = load_validators(Path::new("/nonexistent/validators.jsonl"))
            .expect_err("missing file must be an error (fail closed)");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(load_enabled_validators(Path::new("/nonexistent/validators.jsonl")).is_err());
    }

    #[test]
    fn default_path_lives_in_smartnet_home() {
        let p = default_validators_file();
        if std::env::var_os("HOME").is_some() {
            assert!(p.ends_with(".smartnet/validators.jsonl"), "{p:?}");
        }
    }
}

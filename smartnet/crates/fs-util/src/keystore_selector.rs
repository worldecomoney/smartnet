//! Node-level `COINBASE` keystore selector.
//!
//! `COINBASE` is only a helper for picking one encrypted key file when `KEY_PATH` holds several.
//! It is not a validator setting, a node identity, a fee receiver, or a consensus input: the
//! seal/mesh identity is always derived from the decrypted key. Every node may set it.

use alloy_primitives::Address;
use eyre::{Context, Result};
use std::path::{Path, PathBuf};

/// The one config key that selects a keystore file by the address in its UTC filename.
pub const COINBASE_KEY: &str = "COINBASE";

/// Read `COINBASE` from config text. Absent or blank means no selector.
pub fn coinbase_from_config_text(raw: &str) -> Result<Option<Address>> {
    match super::runtime_paths::config_value(raw, COINBASE_KEY) {
        Some(value) => parse_coinbase_value(&value),
        None => Ok(None),
    }
}

/// Read `COINBASE` from a config file. Absent or blank means no selector.
pub fn coinbase_from_config(path: &Path) -> Result<Option<Address>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config {}", path.display()))?;
    coinbase_from_config_text(&raw)
}

/// Parse one `COINBASE` value. `0x`-prefixed or bare 40-hex addresses are accepted; blank means
/// absent. An invalid address is a config error, never a silent fallback.
pub fn parse_coinbase_value(value: &str) -> Result<Option<Address>> {
    let raw = value.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let prefixed = if raw.starts_with("0x") || raw.starts_with("0X") {
        raw.to_string()
    } else {
        format!("0x{raw}")
    };
    let address = prefixed
        .parse::<Address>()
        .with_context(|| format!("invalid {COINBASE_KEY} address {raw:?}"))?;
    Ok(Some(address))
}

/// Pick the encrypted keystore inside `key_dir`.
///
/// Without `COINBASE` the key is *gathered*: `key_dir` must hold exactly one file, because more
/// than one is ambiguous and must never be picked by order or luck.
///
/// With `COINBASE`, that address selects the encrypted key whose UTC filename ends in it; every
/// other file in `key_dir` is ignored, so a directory holding several keys is normal. Zero or
/// multiple files for that address are fatal: there is no unique key to select. This only picks
/// a file — the key itself remains the identity and the fee receiver.
pub fn select_keystore(key_dir: &Path, coinbase: Option<Address>) -> Result<PathBuf> {
    let entries = std::fs::read_dir(key_dir).with_context(|| {
        format!("KEY_PATH {} cannot be read — run `smartnet keystore` first", key_dir.display())
    })?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read entry in {}", key_dir.display()))?;
        if entry.path().is_file() {
            files.push(entry.path());
        }
    }

    if let Some(address) = coinbase {
        let mut matches = files.into_iter().filter(|path| filename_has_address(path, address));
        let Some(found) = matches.next() else {
            eyre::bail!(
                "no keystore in {} for COINBASE {address:#x} — run `smartnet keystore` with the \
                 matching raw key first",
                key_dir.display()
            );
        };
        if matches.next().is_some() {
            eyre::bail!(
                "more than one keystore in {} matches COINBASE {address:#x} — remove the \
                 duplicates",
                key_dir.display()
            );
        }
        return Ok(found);
    }

    if files.len() == 1 {
        return Ok(files.pop().expect("exactly one file"));
    }
    if files.is_empty() {
        eyre::bail!("no keystore in {} — run `smartnet keystore` first", key_dir.display());
    }
    eyre::bail!(
        "{} files in {} — expected exactly 1 keystore: remove the others or set COINBASE \
         (the key is gathered, never named)",
        files.len(),
        key_dir.display()
    );
}

/// True when the filename ends in `address` after `UTC--…--`, with or without `0x`, in any case.
fn filename_has_address(path: &Path, address: Address) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some((_, suffix)) = name.rsplit_once("--") else {
        return false;
    };
    let suffix = suffix.strip_prefix("0x").or_else(|| suffix.strip_prefix("0X")).unwrap_or(suffix);
    let expected = format!("{address:#x}")[2..].to_string();
    suffix.eq_ignore_ascii_case(&expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: &str = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";
    const OTHER: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("smartnet-coinbase-select-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn coinbase_is_blank_absent_and_address_parseable() {
        let text = format!("HOME_DIR_PATH = \"/tmp\"\n{COINBASE_KEY} = \"{ADDR}\"\n");
        assert_eq!(
            coinbase_from_config_text(&text).expect("prefixed address"),
            Some(ADDR.parse().expect("valid address"))
        );
        let bare = ADDR.trim_start_matches("0x");
        let text = format!("{COINBASE_KEY} = \"{bare}\"\n");
        assert_eq!(
            coinbase_from_config_text(&text).expect("bare address"),
            Some(ADDR.parse().expect("valid address"))
        );
        assert_eq!(
            coinbase_from_config_text(&format!("{COINBASE_KEY} = \"\"\n")).expect("blank"),
            None
        );
        assert_eq!(coinbase_from_config_text("HOME_DIR_PATH = \"/tmp\"\n").expect("absent"), None);
        let err = coinbase_from_config_text(&format!("{COINBASE_KEY} = \"nope\"\n"))
            .expect_err("invalid address");
        assert!(err.to_string().contains(COINBASE_KEY), "got: {err}");
    }

    #[test]
    fn coinbase_selects_one_file_among_many_and_ignores_the_rest() {
        let dir = temp_dir("many");
        let wanted = ADDR.trim_start_matches("0x");
        let other = OTHER.trim_start_matches("0x");
        std::fs::write(dir.join(format!("UTC--0000--{other}")), b"other").expect("other key");
        std::fs::write(dir.join(format!("UTC--0001--{wanted}")), b"wanted").expect("wanted key");

        let picked = select_keystore(&dir, Some(ADDR.parse().expect("valid address")))
            .expect("COINBASE selects one file");
        let name = picked.file_name().and_then(|n| n.to_str()).expect("utf8 name");
        assert!(name.ends_with(wanted), "got: {name}");

        let err = select_keystore(&dir, None).unwrap_err().to_string();
        assert!(err.contains("expected exactly 1"), "got: {err}");
        let err = select_keystore(
            &dir,
            Some("0x1111111111111111111111111111111111111111".parse().expect("valid address")),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("COINBASE"), "got: {err}");

        std::fs::write(dir.join(format!("UTC--0002--{wanted}")), b"duplicate").expect("duplicate");
        let err = select_keystore(&dir, Some(ADDR.parse().expect("valid address")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than one keystore"), "got: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_coinbase_the_single_gathered_file_still_works() {
        let dir = temp_dir("single");
        std::fs::write(dir.join("anything.json"), b"key").expect("key");
        let picked = select_keystore(&dir, None).expect("gathered");
        assert_eq!(picked, dir.join("anything.json"));

        // A single file does not excuse a contradiction: a mismatched COINBASE is fatal.
        let err = select_keystore(
            &dir,
            Some("0x1111111111111111111111111111111111111111".parse().expect("valid address")),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("COINBASE"), "got: {err}");

        let matched = temp_dir("single-match");
        let name = format!("UTC--0000--{}", ADDR.trim_start_matches("0x"));
        std::fs::write(matched.join(&name), b"key").expect("named key");
        let picked = select_keystore(&matched, Some(ADDR.parse().expect("valid address")))
            .expect("single matching file");
        assert!(picked.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(&name)));

        let empty = temp_dir("empty");
        let err = select_keystore(&empty, None).unwrap_err().to_string();
        assert!(err.contains("no keystore"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&matched);
        let _ = std::fs::remove_dir_all(&empty);
    }
}

//! Node-owned validator registry.
//!
//! `validators.jsonl` is node data: the node's block gate and chair queries read it. The
//! merged validator never opens this file — it asks the node for the enabled set through
//! `ValidatorIo`. `VAL_FILE` is relative to `HOME_DIR_PATH` from the shared `config.toml`;
//! when the key is absent or blank, the shared scaffold searches `./validators.jsonl` then
//! `HOME_DIR_PATH/validators.jsonl`. It is never a `smartnet` CLI argument.

use alloy_primitives::Address;
use eyre::{Context, Result};
use smartnet_fs_util::runtime_paths::{data_file_from_config, data_file_from_config_with};
use std::path::{Path, PathBuf};
use tracing::warn;

pub use calibri_lite_consensus::{
    get_val_chair,
    registry::{get_val_all, ValidatorRecord},
};

/// Resolve `VAL_FILE` from the shared operator config text.
///
/// A present value is relative to `HOME_DIR_PATH` (absolute values and `~` are errors). Missing
/// or blank means "not keyed", so the shared local → `HOME_DIR_PATH` scaffold applies; if
/// neither holds the file, it is an error.
pub fn parse_val_file(raw: &str) -> Result<PathBuf> {
    data_file_from_config(raw, "VAL_FILE", "validators.jsonl")
        .with_context(|| "config `VAL_FILE` / validators.jsonl")
}

/// [`parse_val_file`] with the environment injected for deterministic tests.
pub fn parse_val_file_with(raw: &str, cwd: Option<&Path>, home: Option<&str>) -> Result<PathBuf> {
    data_file_from_config_with(raw, "VAL_FILE", "validators.jsonl", cwd, home)
        .with_context(|| "config `VAL_FILE` / validators.jsonl")
}

/// Read the shared config file and return the registry path its `VAL_FILE` key names.
pub fn load_val_file(path: &Path) -> Result<PathBuf> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    parse_val_file(&raw).with_context(|| format!("config {}", path.display()))
}

/// Load the node's registry file into the RAM array read by [`get_val_all`] and
/// [`get_val_chair`]. Bad lines are logged and skipped; an unreadable file is a hard error.
pub fn load_validators(path: &Path) -> Result<()> {
    let (_, errors) = calibri_lite_consensus::registry::load_validators(path)
        .with_context(|| format!("read validator registry {}", path.display()))?;
    for err in errors {
        warn!(target: "validators_jsonl", error = %err, "validators.jsonl line rejected");
    }
    Ok(())
}

/// The enabled (`status: true`) addresses of the node's RAM registry.
pub fn enabled_set() -> Vec<Address> {
    get_val_all()
        .into_iter()
        .filter(|record| record.is_enabled())
        .map(|record| record.address)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example.config.toml")
    }

    #[test]
    fn val_file_is_relative_to_home_dir() {
        let raw = std::fs::read_to_string(template()).expect("read shipped config template");
        let home_dir =
            std::env::temp_dir().join(format!("smartnet-val-template-{}", std::process::id()));
        let home = home_dir.to_string_lossy().to_string();
        let path = parse_val_file_with(&raw, None, Some(&home)).expect("template sets VAL_FILE");
        assert_eq!(path, home_dir.join(".smartnetX/validators.jsonl"));

        let relative = raw
            .lines()
            .map(
                |l| if l.starts_with("VAL_FILE =") { "VAL_FILE = \"validators.jsonl\"" } else { l },
            )
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            parse_val_file_with(&relative, None, Some(&home)).unwrap(),
            home_dir.join(".smartnetX/validators.jsonl")
        );

        let absolute = raw
            .lines()
            .map(|l| if l.starts_with("VAL_FILE =") { "VAL_FILE = \"/etc/passwd\"" } else { l })
            .collect::<Vec<_>>()
            .join("\n");
        let err = parse_val_file_with(&absolute, None, Some(&home)).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("relative"), "got: {msg}");
        assert!(msg.contains("VAL_FILE"), "got: {msg}");
    }

    #[test]
    fn absent_or_blank_val_file_uses_the_local_then_home_dir_scaffold() {
        let raw = std::fs::read_to_string(template()).expect("read shipped config template");
        let without =
            raw.lines().filter(|l| !l.starts_with("VAL_FILE")).collect::<Vec<_>>().join("\n");
        let blank = raw
            .lines()
            .map(|l| if l.starts_with("VAL_FILE =") { "VAL_FILE = \"\"" } else { l })
            .collect::<Vec<_>>()
            .join("\n");

        let root =
            std::env::temp_dir().join(format!("smartnet-val-scaffold-{}", std::process::id()));
        let launch = root.join("launch");
        let home_dir = root.join("home");
        let base = home_dir.join(".smartnetX");
        std::fs::create_dir_all(&launch).expect("launch dir");
        std::fs::create_dir_all(&base).expect("runtime root");
        let local = launch.join("validators.jsonl");
        let installed = base.join("validators.jsonl");
        std::fs::write(&local, "{}").expect("local registry");
        std::fs::write(&installed, "{}").expect("runtime registry");
        let home = home_dir.to_string_lossy().to_string();

        for config in [&without, &blank] {
            assert_eq!(
                parse_val_file_with(config, Some(&launch), Some(&home)).unwrap(),
                local,
                "launch directory wins"
            );
        }

        std::fs::remove_file(&local).expect("remove local registry");
        for config in [&without, &blank] {
            assert_eq!(
                parse_val_file_with(config, Some(&launch), Some(&home)).unwrap(),
                installed,
                "HOME_DIR_PATH file is second"
            );
        }

        std::fs::remove_file(&installed).expect("remove runtime registry");
        let err =
            parse_val_file_with(&without, Some(&launch), Some(&home)).unwrap_err().to_string();
        assert!(err.contains("validators.jsonl"), "got: {err}");

        let _ = std::fs::remove_dir_all(&root);
    }
}

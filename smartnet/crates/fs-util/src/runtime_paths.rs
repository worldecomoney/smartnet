//! One global path rule, shared by the node and every merged command.
//!
//! 1. `--config` is used verbatim; without it, search `./config.toml` then
//!    `~/.smartnet/config.toml`. Neither holds the file is an error.
//! 2. `config.toml` has exactly one absolute root, `HOME_DIR_PATH`. A leading `~/` is substituted
//!    once against `$HOME`.
//! 3. Every other config path is relative to `HOME_DIR_PATH`. No other `~` expansion or CWD
//!    fallback is applied to those values.
//! 4. Config-owned data files are allowlisted in [`DATA_FILES`] so `bridges.json` can be added
//!    later with one entry. When their key is absent/blank, search `./<name>` then
//!    `HOME_DIR_PATH/<name>`; neither holding the file is an error.

use eyre::{bail, eyre};
use std::{
    env,
    path::{Path, PathBuf},
};

/// The single absolute root in `config.toml`; all other path values are relative to it.
pub const HOME_DIR_KEY: &str = "HOME_DIR_PATH";

/// Config-owned data files that get the local → `HOME_DIR_PATH` scaffold when their config key
/// is absent or blank. Add new ones here (e.g. `bridges.json`).
pub const DATA_FILES: &[&str] = &["mainnet.jsonl", "validators.jsonl"];

/// Resolve the operator config file. Explicit `--config` is used as-is; otherwise try
/// `./config.toml`, then `~/.smartnet/config.toml`, else fail.
pub fn resolve_config_path(explicit: Option<&Path>) -> eyre::Result<PathBuf> {
    let cwd = env::current_dir().ok();
    let home = env::var("HOME").ok();
    resolve_config_path_with(explicit, cwd.as_deref(), home.as_deref())
}

/// The config search with injected environment, for tests.
pub fn resolve_config_path_with(
    explicit: Option<&Path>,
    cwd: Option<&Path>,
    home: Option<&str>,
) -> eyre::Result<PathBuf> {
    resolve_named_file("config.toml", explicit.map(Path::to_path_buf), cwd, home)
}

/// Read `HOME_DIR_PATH` from config text and expand a leading `~/` once against `$HOME`.
pub fn home_dir_from_config(raw: &str) -> eyre::Result<PathBuf> {
    let home = env::var("HOME").ok();
    home_dir_from_config_with(raw, home.as_deref())
}

/// [`home_dir_from_config`] with `HOME` injected, for tests.
pub fn home_dir_from_config_with(raw: &str, home: Option<&str>) -> eyre::Result<PathBuf> {
    let value = config_value(raw, HOME_DIR_KEY).ok_or_else(|| {
        eyre!(
            "missing required key `{HOME_DIR_KEY}` — it is the only absolute path in config.toml; \
             all other paths are relative to it"
        )
    })?;
    if value.trim().is_empty() {
        bail!("config `{HOME_DIR_KEY}` is empty — set the runtime root (e.g. ~/.smartnetX/)");
    }
    let path = expand_home_once(Path::new(&value), home)?;
    if !path.is_absolute() {
        bail!("{HOME_DIR_KEY} must be absolute after `~` expansion (got {value:?})");
    }
    Ok(path)
}

/// Resolve one relative config path against `HOME_DIR_PATH`. Absolute values and `~` are
/// rejected: only `HOME_DIR_PATH` may be absolute or use `~`, and only once.
pub fn resolve_runtime_path(base: &Path, value: &str) -> eyre::Result<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        bail!("path value is empty — set a path relative to {HOME_DIR_KEY}");
    }
    if value.starts_with('"') || value.starts_with('\'') {
        bail!("path value {value:?} has an unbalanced quote");
    }
    let relative = Path::new(value);
    let raw = relative.to_string_lossy();
    if raw.starts_with('~') {
        bail!("path value {value:?} must be relative to {HOME_DIR_KEY}; `~` belongs only in {HOME_DIR_KEY}");
    }
    if relative.is_absolute() {
        bail!("path value {value:?} must be relative to {HOME_DIR_KEY}; absolute paths are not config-owned");
    }
    let resolved = base.join(relative);
    if !resolved.is_absolute() {
        bail!("{HOME_DIR_KEY} must be absolute before joining relative paths");
    }
    Ok(resolved)
}

/// Resolve one of [`DATA_FILES`]. A present config value is relative to `HOME_DIR_PATH`; when it
/// is absent/blank, search `./<name>` then `HOME_DIR_PATH/<name>`. Neither holding the file is
/// an error.
pub fn resolve_data_file(
    name: &str,
    base: &Path,
    explicit: Option<&Path>,
) -> eyre::Result<PathBuf> {
    let cwd = env::current_dir().ok();
    resolve_data_file_with(name, base, explicit, cwd.as_deref())
}

/// [`resolve_data_file`] with the launch directory injected, for tests.
pub fn resolve_data_file_with(
    name: &str,
    base: &Path,
    explicit: Option<&Path>,
    cwd: Option<&Path>,
) -> eyre::Result<PathBuf> {
    if !DATA_FILES.contains(&name) {
        bail!(
            "{name:?} is not a registered SmartNet data file; add it to DATA_FILES to get the \
             local → {HOME_DIR_KEY} scaffold"
        );
    }
    if !base.is_absolute() {
        bail!("{HOME_DIR_KEY} must be absolute before resolving data files");
    }

    if let Some(path) = explicit {
        return resolve_runtime_path(base, &path.to_string_lossy());
    }

    let mut searched = Vec::new();
    if let Some(dir) = cwd {
        let local = dir.join(name);
        searched.push(local.display().to_string());
        if local.is_file() {
            return Ok(local);
        }
    }
    let installed = base.join(name);
    searched.push(installed.display().to_string());
    if installed.is_file() {
        return Ok(installed);
    }

    Err(eyre!(
        "{name} was found in none of the scaffold locations; searched: {}. Put the file in the \
         launch directory or under {HOME_DIR_KEY}, or set the config key to a path relative to \
         {HOME_DIR_KEY}",
        searched.join(", ")
    ))
}

/// Resolve a config-owned data file directly from config text: `HOME_DIR_PATH` supplies the root,
/// `key` supplies the relative value (absent/blank uses the scaffold).
pub fn data_file_from_config(raw: &str, key: &str, name: &str) -> eyre::Result<PathBuf> {
    let home = env::var("HOME").ok();
    let cwd = env::current_dir().ok();
    data_file_from_config_with(raw, key, name, cwd.as_deref(), home.as_deref())
}

/// [`data_file_from_config`] with the environment injected, for tests.
pub fn data_file_from_config_with(
    raw: &str,
    key: &str,
    name: &str,
    cwd: Option<&Path>,
    home: Option<&str>,
) -> eyre::Result<PathBuf> {
    let base = home_dir_from_config_with(raw, home)?;
    let explicit =
        config_value(raw, key).filter(|value| !value.trim().is_empty()).map(PathBuf::from);
    resolve_data_file_with(name, &base, explicit.as_deref(), cwd)
}

fn resolve_named_file(
    name: &str,
    explicit: Option<PathBuf>,
    cwd: Option<&Path>,
    home: Option<&str>,
) -> eyre::Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }

    let mut searched = Vec::new();
    if let Some(dir) = cwd {
        let local = dir.join(name);
        searched.push(local.display().to_string());
        if local.is_file() {
            return Ok(local);
        }
    }
    if let Some(home) = home {
        let installed = PathBuf::from(home).join(".smartnet").join(name);
        searched.push(installed.display().to_string());
        if installed.is_file() {
            return Ok(installed);
        }
    }

    Err(eyre!(
        "no --config was given and {name} was found in none of the scaffold locations; searched: \
         {}. Put the file in the launch directory or ~/.smartnet/, or pass --config \
         /path/to/config.toml",
        if searched.is_empty() { "nowhere".to_string() } else { searched.join(", ") }
    ))
}

/// Expand the first leading `~` against the supplied `HOME`, exactly once. Named users
/// (`~alice/…`) are rejected; the rest of the value is never expanded again.
fn expand_home_once(path: &Path, home: Option<&str>) -> eyre::Result<PathBuf> {
    let raw = path.to_string_lossy();
    let Some(rest) = raw.strip_prefix('~') else {
        return Ok(path.to_path_buf());
    };
    let rel = match rest {
        "" => "",
        _ => rest.strip_prefix('/').ok_or_else(|| {
            eyre!("path {raw:?} starts with `~user` — only `~` and `~/…` are expanded against HOME")
        })?,
    };
    let home = home.ok_or_else(|| eyre!("path {raw:?} starts with `~` but HOME is not set"))?;
    Ok(if rel.is_empty() { PathBuf::from(home) } else { PathBuf::from(home).join(rel) })
}

/// Read one `KEY = value` pair from raw config text, stripping comments and matching quotes.
pub(super) fn config_value(raw: &str, key: &str) -> Option<String> {
    for line in raw.lines() {
        let line = strip_comment(line).trim();
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() == key {
            return Some(strip_quotes(value).trim().to_string());
        }
    }
    None
}

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

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2 &&
        ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_rule_is_explicit_then_local_then_home() {
        let root = env::temp_dir().join(format!("smartnet-fs-config-{}", std::process::id()));
        let launch = root.join("launch");
        let home_dir = root.join("home");
        std::fs::create_dir_all(&launch).expect("launch dir");
        std::fs::create_dir_all(home_dir.join(".smartnet")).expect("smartnet dir");
        let local = launch.join("config.toml");
        let installed = home_dir.join(".smartnet/config.toml");
        std::fs::write(&local, "local").expect("local config");
        std::fs::write(&installed, "installed").expect("installed config");

        let home = home_dir.to_string_lossy().to_string();
        let explicit = launch.join("elsewhere.toml");
        assert_eq!(
            resolve_config_path_with(Some(&explicit), Some(&launch), Some(&home)).unwrap(),
            explicit,
            "an explicit path is used verbatim"
        );
        assert_eq!(
            resolve_config_path_with(None, Some(&launch), Some(&home)).unwrap(),
            local,
            "launch directory wins"
        );
        std::fs::remove_file(&local).expect("remove local");
        assert_eq!(
            resolve_config_path_with(None, Some(&launch), Some(&home)).unwrap(),
            installed,
            "installed file is next"
        );
        std::fs::remove_file(&installed).expect("remove installed");
        assert!(resolve_config_path_with(None, Some(&launch), Some(&home)).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn home_dir_expands_tilde_once_and_roots_every_path() {
        let home_dir = env::temp_dir().join(format!("smartnet-fs-home-{}", std::process::id()));
        let home = home_dir.to_string_lossy().to_string();
        let raw = "HOME_DIR_PATH = \"~/.smartnetX/\"\n";
        let base = home_dir_from_config_with(raw, Some(&home)).unwrap();
        assert_eq!(base, home_dir.join(".smartnetX"));

        assert_eq!(resolve_runtime_path(&base, "data").unwrap(), base.join("data"));
        assert_eq!(resolve_runtime_path(&base, "keystore").unwrap(), base.join("keystore"));
        assert!(resolve_runtime_path(&base, "/etc/passwd").is_err());
        assert!(resolve_runtime_path(&base, "~/keys").is_err());

        let missing = home_dir_from_config_with("HOME_DIR_PATH = \"\"\n", Some(&home));
        assert!(missing.is_err());
        let named = home_dir_from_config_with("HOME_DIR_PATH = \"~alice/x\"\n", Some(&home));
        assert!(named.is_err());
        let _ = std::fs::remove_dir_all(&home_dir);
    }

    #[test]
    fn data_files_are_allowlisted_and_scaffolded_from_home_dir() {
        let root = env::temp_dir().join(format!("smartnet-fs-data-{}", std::process::id()));
        let launch = root.join("launch");
        let base = root.join("home").join(".smartnetX");
        std::fs::create_dir_all(&launch).expect("launch dir");
        std::fs::create_dir_all(&base).expect("runtime root");
        let local = launch.join("validators.jsonl");
        let installed = base.join("validators.jsonl");
        std::fs::write(&local, "{}").expect("local registry");
        std::fs::write(&installed, "{}").expect("runtime registry");

        assert_eq!(
            resolve_data_file_with("validators.jsonl", &base, None, Some(&launch)).unwrap(),
            local
        );
        std::fs::remove_file(&local).expect("remove local");
        assert_eq!(
            resolve_data_file_with("validators.jsonl", &base, None, Some(&launch)).unwrap(),
            installed
        );
        assert!(
            resolve_data_file_with("bridges.json", &base, None, Some(&launch)).is_err(),
            "new files need an explicit DATA_FILES entry"
        );

        std::fs::remove_file(&installed).expect("remove runtime registry");
        let err = resolve_data_file_with("validators.jsonl", &base, None, Some(&launch))
            .unwrap_err()
            .to_string();
        assert!(err.contains("validators.jsonl"), "got: {err}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn config_data_file_values_are_relative_to_home_dir() {
        let home_dir = env::temp_dir().join(format!("smartnet-fs-key-{}", std::process::id()));
        let home = home_dir.to_string_lossy().to_string();
        let raw = "HOME_DIR_PATH = \"~/.smartnetX/\"\nVAL_FILE = \"validators.jsonl\"\n";
        let resolved =
            data_file_from_config_with(raw, "VAL_FILE", "validators.jsonl", None, Some(&home))
                .unwrap();
        assert_eq!(resolved, home_dir.join(".smartnetX/validators.jsonl"));

        let absolute = raw.replace("validators.jsonl", "/etc/passwd");
        assert!(
            data_file_from_config_with(
                &absolute,
                "VAL_FILE",
                "validators.jsonl",
                None,
                Some(&home),
            )
            .is_err(),
            "absolute data-file values are not config-owned"
        );
        let _ = std::fs::remove_dir_all(&home_dir);
    }
}

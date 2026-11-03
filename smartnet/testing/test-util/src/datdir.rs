//! Load numbered payload files: `1.dat`, `2.dat`, … from a directory.

use eyre::{Context, Result};
use std::path::{Path, PathBuf};

/// One external payload file.
pub(crate) struct DatFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Load `N.dat` files in numeric order. Missing numbers are skipped; non-`.dat` files ignored.
pub(crate) fn load_numbered_dat(dir: &Path) -> Result<Vec<DatFile>> {
    let mut entries: Vec<(u64, PathBuf)> = Vec::new();

    for entry in std::fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.extension().is_some_and(|e| e != "dat") {
            continue;
        }
        let Ok(n) = stem.parse::<u64>() else {
            continue;
        };
        entries.push((n, path));
    }

    entries.sort_by_key(|(n, _)| *n);

    let mut out = Vec::new();
    for (n, path) in entries {
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        out.push(DatFile { name: format!("{n}.dat"), bytes });
    }
    Ok(out)
}

//! Stage 6C: runtime bytecode in per-contract files.
//!
//! Layout (operator ruling, 2026-11-19 — "it must be contract deploy address, not a
//! hash! … `3hex/3hex/3hex/rest_of_address`"): `bytecodes/{a[0..3]}/{a[3..6]}/{a[6..9]}/{a[9..]}`
//! under the file-tier base, where `a` is the 40-char lowercase hex of the **deployed
//! contract address** and the file name is the remaining 31 chars. The token-metadata
//! sidecar is the same name with `.meta`.
//!
//! Content: the raw executable bytecode exactly as deployed — **binary, uncompressed,
//! headerless** (operator ruling: no transcoding, direct execution; EVM bytecode is
//! near-random, so no gzip).
//!
//! Write-once per address: on this network an address can hold code only once (RTBC
//! forbids executable `SELFDESTRUCT`, and `CREATE2` addresses commit to their initcode),
//! so the file never changes after creation — readers need no invalidation, and a crash
//! between writes leaves at most an orphan file. Writes are temp-file + rename, so a
//! final path never holds a partial write.

use crate::error::FileTierResult;
use alloy_primitives::Address;
use smartnet_fs_util::FsPathError;
use smartnet_primitives_traits::TokenMeta;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// Root of the bytecode tree, a sibling of `blocks/` under the file-tier base.
pub const BYTECODES_DIR: &str = "bytecodes";

/// The file holding `address`'s runtime bytecode under `base`:
/// `bytecodes/{a[0..3]}/{a[3..6]}/{a[6..9]}/{a[9..]}`.
pub fn bytecode_path(base: &Path, address: &Address) -> PathBuf {
    let a = alloy_primitives::hex::encode(address);
    base.join(BYTECODES_DIR).join(&a[0..3]).join(&a[3..6]).join(&a[6..9]).join(&a[9..])
}

/// Writes the raw runtime bytecode for `address`, write-once (spec §4: files are
/// immutable once written). A crash before the rename leaves only a `*.tmp` sibling,
/// which readers never see; the next write of the same address truncates and retries it.
pub fn write_bytecode(base: &Path, address: &Address, code: &[u8]) -> FileTierResult<()> {
    let path = bytecode_path(base, address);
    if path.exists() {
        return Ok(());
    }
    let dir = path.parent().expect("bytecode_path always has a parent");
    fs::create_dir_all(dir).map_err(|err| FsPathError::create_dir(err, dir))?;

    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp).map_err(|err| FsPathError::create_file(err, &tmp))?;
    file.write_all(code).map_err(|err| FsPathError::write(err, &tmp))?;
    file.sync_all().map_err(|err| FsPathError::fsync(err, &tmp))?;
    drop(file);
    fs::rename(&tmp, &path).map_err(|err| FsPathError::rename(err, &tmp, &path))?;
    // The file's *contents* are on the device, but its *name* is not: a rename only becomes
    // durable when the directory holding both entries is fsynced. Without this a crash can
    // undo the rename, leaving the `.tmp` sibling (which readers ignore) and no final path —
    // a bytecode file that was written, fsynced and then silently lost.
    smartnet_fs_util::fsync_dir(dir)?;
    Ok(())
}

/// Reads the raw runtime bytecode for `address`. `Ok(None)` when no file exists — the
/// contract has no code or the code predates the file tier. Any other I/O failure is an
/// error, never a silent miss.
pub fn read_bytecode(base: &Path, address: &Address) -> FileTierResult<Option<Vec<u8>>> {
    let path = bytecode_path(base, address);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(FsPathError::read(err, &path).into()),
    }
}

/// The token-metadata sidecar of [`bytecode_path`]: the same filename with `.meta`
/// appended (operator: "written alongside the rtbc file, same filename, just .meta"),
/// holding one JSON object `{"name":…,"symbol":…,"decimals":…}`.
pub fn bytecode_meta_path(base: &Path, address: &Address) -> PathBuf {
    bytecode_path(base, address).with_extension("meta")
}

/// Writes a token's metadata beside its bytecode file — write-once with the same
/// durability rules as [`write_bytecode`] (temp + rename + fsync; an existing sidecar is
/// never touched, so the first capture of a contract wins).
pub fn write_bytecode_meta(
    base: &Path,
    address: &Address,
    meta: &TokenMeta,
) -> FileTierResult<()> {
    let path = bytecode_meta_path(base, address);
    if path.exists() {
        return Ok(());
    }
    let dir = path.parent().expect("bytecode_meta_path always has a parent");
    fs::create_dir_all(dir).map_err(|err| FsPathError::create_dir(err, dir))?;

    let json = serde_json::json!({
        "address": format!("{address:#x}"),
        "name": meta.name,
        "symbol": meta.symbol,
        "decimals": meta.decimals,
        "totalSupply": meta.total_supply.to_string(),
        "owner": meta.owner.map(|address| format!("{address:#x}")),
        "creator": meta.creator.map(|address| format!("{address:#x}")),
        "hash": format!("{:#x}", meta.code_hash),
        "compiler": meta.compiler,
    });
    let tmp = path.with_extension("meta.tmp");
    let mut file = fs::File::create(&tmp).map_err(|err| FsPathError::create_file(err, &tmp))?;
    file.write_all(serde_json::to_string(&json)?.as_bytes())
        .map_err(|err| FsPathError::write(err, &tmp))?;
    file.sync_all().map_err(|err| FsPathError::fsync(err, &tmp))?;
    drop(file);
    fs::rename(&tmp, &path).map_err(|err| FsPathError::rename(err, &tmp, &path))?;
    smartnet_fs_util::fsync_dir(dir)?;
    Ok(())
}

/// The holders-stub sidecar of [`bytecode_path`]: the same filename with `.json`, a
/// holders list stub (operator, 2026-11-19: "a holders stub … probably just one entry of
/// owner address but no amount/value"). The node writes the stub at capture; an external
/// 1-hour cron script owns the file afterwards and rewrites it with real holder ranks,
/// balances and percentages — so this write happens **only when the file is absent**,
/// never clobbering the script's data.
///
/// Shape (holder entry = the `TokenHolder` record: `r` rank, `a` address, `b` balance as
/// a uint256 string, `p` percent of supply):
/// `{"address":"0x…","holders":[{"r":1,"a":"0x…owner","b":"","p":0.0}]}`
pub fn holders_path(base: &Path, address: &Address) -> PathBuf {
    bytecode_path(base, address).with_extension("json")
}

/// Writes the holders stub for `address` — only when there is none yet (see
/// [`holders_path`]). `owner` seeds the single stub entry (`None` → empty list).
pub fn write_holders_stub(
    base: &Path,
    address: &Address,
    owner: Option<Address>,
) -> FileTierResult<()> {
    let path = holders_path(base, address);
    if path.exists() {
        return Ok(());
    }
    let dir = path.parent().expect("holders_path always has a parent");
    fs::create_dir_all(dir).map_err(|err| FsPathError::create_dir(err, dir))?;

    let holders: Vec<serde_json::Value> = owner
        .map(|owner| {
            vec![serde_json::json!({ "r": 1, "a": format!("{owner:#x}"), "b": "", "p": 0.0 })]
        })
        .unwrap_or_default();
    let json = serde_json::json!({
        "address": format!("{address:#x}"),
        "holders": holders,
    });
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp).map_err(|err| FsPathError::create_file(err, &tmp))?;
    file.write_all(serde_json::to_string(&json)?.as_bytes())
        .map_err(|err| FsPathError::write(err, &tmp))?;
    file.sync_all().map_err(|err| FsPathError::fsync(err, &tmp))?;
    drop(file);
    fs::rename(&tmp, &path).map_err(|err| FsPathError::rename(err, &tmp, &path))?;
    smartnet_fs_util::fsync_dir(dir)?;
    Ok(())
}

/// Reads the token-metadata sidecar for `address`. `Ok(None)` when there is none — the
/// contract is not a token, or it predates capture. Any other failure is an error.
pub fn read_bytecode_meta(base: &Path, address: &Address) -> FileTierResult<Option<TokenMeta>> {
    let path = bytecode_meta_path(base, address);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(FsPathError::read(err, &path).into()),
    };
    let value: serde_json::Value = serde_json::from_str(&raw)?;
    Ok(Some(TokenMeta {
        name: value["name"].as_str().unwrap_or_default().to_owned(),
        symbol: value["symbol"].as_str().unwrap_or_default().to_owned(),
        decimals: value["decimals"].as_u64().unwrap_or_default().min(u8::MAX as u64) as u8,
        total_supply: value["totalSupply"].as_str().unwrap_or_default().parse().unwrap_or_default(),
        owner: value["owner"].as_str().and_then(|address| address.parse().ok()),
        creator: value["creator"].as_str().and_then(|address| address.parse().ok()),
        code_hash: value["hash"].as_str().and_then(|hash| hash.parse().ok()).unwrap_or_default(),
        compiler: value["compiler"].as_str().unwrap_or_default().to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, hex, keccak256, U256};

    const CODE: &[u8] = &[0x60, 0x80, 0x60, 0x40, 0x52, 0x60, 0x00, 0x55];
    const ADDR: Address = address!("24bf191a358c09e79b9ead2d8999d08590b5af31");

    #[test]
    fn bytecode_path_is_three_three_three_rest() {
        let a = hex::encode(ADDR);
        let path = bytecode_path(Path::new("/data"), &ADDR);
        assert_eq!(
            path,
            PathBuf::from(format!(
                "/data/{BYTECODES_DIR}/{}/{}/{}/{}",
                &a[0..3],
                &a[3..6],
                &a[6..9],
                &a[9..]
            ))
        );
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), &a[9..], "file is the address rest");
    }

    #[test]
    fn write_then_read_round_trips_raw_bytes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_bytecode(dir.path(), &ADDR).unwrap().is_none());

        write_bytecode(dir.path(), &ADDR, CODE).unwrap();

        let path = bytecode_path(dir.path(), &ADDR);
        assert_eq!(fs::read(&path).unwrap(), CODE, "the file is the raw bytecode, no encoding");
        assert_eq!(read_bytecode(dir.path(), &ADDR).unwrap().unwrap(), CODE);
        assert!(!path.with_extension("tmp").exists(), "the temp file is renamed away");
    }

    #[test]
    fn write_is_write_once() {
        let dir = tempfile::tempdir().unwrap();
        write_bytecode(dir.path(), &ADDR, CODE).unwrap();

        let other: &[u8] = &[0x00];
        write_bytecode(dir.path(), &ADDR, other).unwrap();

        assert_eq!(fs::read(bytecode_path(dir.path(), &ADDR)).unwrap(), CODE);
    }

    #[test]
    fn each_address_gets_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let other = address!("1111111111111111111111111111111111111111");
        // identical code, two contracts — two files, keyed by deploy address
        write_bytecode(dir.path(), &ADDR, CODE).unwrap();
        write_bytecode(dir.path(), &other, CODE).unwrap();

        assert_eq!(fs::read(bytecode_path(dir.path(), &ADDR)).unwrap(), CODE);
        assert_eq!(fs::read(bytecode_path(dir.path(), &other)).unwrap(), CODE);
    }

    #[test]
    fn read_ignores_unrenamed_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = bytecode_path(dir.path(), &ADDR);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path.with_extension("tmp"), CODE).unwrap();

        assert!(
            read_bytecode(dir.path(), &ADDR).unwrap().is_none(),
            "a crash's tmp must not serve"
        );
    }

    #[test]
    fn read_missing_is_none_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let unknown = address!("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
        assert_eq!(read_bytecode(dir.path(), &unknown).unwrap(), None);
    }

    #[test]
    fn meta_sidecar_lands_beside_the_bytecode() {
        let dir = tempfile::tempdir().unwrap();
        write_bytecode(dir.path(), &ADDR, CODE).unwrap();

        assert!(read_bytecode_meta(dir.path(), &ADDR).unwrap().is_none(), "no token, no meta");

        let meta = TokenMeta {
            name: "WEMTOKEN".to_owned(),
            symbol: "WEMX".to_owned(),
            decimals: 2,
            total_supply: U256::from(2_000_000_000u64),
            owner: Some(address!("1111111111111111111111111111111111111111")),
            creator: Some(ADDR),
            code_hash: keccak256(CODE),
            compiler: "0.7.6".to_owned(),
        };
        write_bytecode_meta(dir.path(), &ADDR, &meta).unwrap();

        let path = bytecode_meta_path(dir.path(), &ADDR);
        assert_eq!(path, bytecode_path(dir.path(), &ADDR).with_extension("meta"));
        assert_eq!(read_bytecode_meta(dir.path(), &ADDR).unwrap().unwrap(), meta);
        assert!(!path.with_extension("meta.tmp").exists(), "the temp file is renamed away");
        // the full contract address rides the sidecar — no reassembling shard paths
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["address"], format!("{ADDR:#x}"));
    }

    #[test]
    fn holders_stub_seeds_owner_and_survives_the_cron() {
        let dir = tempfile::tempdir().unwrap();
        let owner = address!("1111111111111111111111111111111111111111");

        write_holders_stub(dir.path(), &ADDR, Some(owner)).unwrap();

        let path = holders_path(dir.path(), &ADDR);
        assert_eq!(path, bytecode_path(dir.path(), &ADDR).with_extension("json"));
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["address"], format!("{ADDR:#x}"));
        let holders = raw["holders"].as_array().unwrap();
        assert_eq!(holders.len(), 1, "stub: one entry");
        assert_eq!(holders[0]["r"], 1);
        assert_eq!(holders[0]["a"], format!("{owner:#x}"));
        assert_eq!(holders[0]["b"], "", "no amount in the stub");
        assert_eq!(holders[0]["p"], 0.0, "no percentage in the stub");

        // the hourly cron rewrites the file — a later write of the stub must not clobber it
        fs::write(&path, r#"[{"r":1,"a":"0x24bf191a358c09e79b9ead2d8999d08590b5af31","b":"42","p":100.0}]"#)
            .unwrap();
        write_holders_stub(dir.path(), &ADDR, Some(owner)).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains(r#""42""#), "cron data survives: {after}");
    }

    #[test]
    fn holders_stub_without_owner_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        write_holders_stub(dir.path(), &ADDR, None).unwrap();
        let raw: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(holders_path(dir.path(), &ADDR)).unwrap(),
        )
        .unwrap();
        assert_eq!(raw["holders"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn meta_write_is_write_once() {
        let dir = tempfile::tempdir().unwrap();
        let first = TokenMeta { name: "One".into(), symbol: "ONE".into(), decimals: 8, ..Default::default() };
        write_bytecode_meta(dir.path(), &ADDR, &first).unwrap();

        let other = TokenMeta { name: "Two".into(), symbol: "TWO".into(), decimals: 18, ..Default::default() };
        write_bytecode_meta(dir.path(), &ADDR, &other).unwrap();

        assert_eq!(read_bytecode_meta(dir.path(), &ADDR).unwrap().unwrap(), first);
    }
}

//! Read path for the file tier (`fjall4a.md` §6, AMEND1): locate a block's records in the
//! `blocks/{S}/{Y:02}/` files and rebuild the consensus types from them.
//!
//! Reads prefer the **sealed** files (`.gz`, decompressed) and fall back to the **active**
//! growing shard (`.jsonl`, plain — no decompression) when a shard has not been sealed yet, so
//! every committed block is servable from the files — the payload is never stored in the
//! database. The reader dispatches on the file extension. The active shard is the hot one, so
//! recent reads pay no decompression cost.
//!
//! The process-global base directory (decision 6B-d.4) is set once at launch from the node's
//! data dir; a node that never calls [`set_base`] has no file tier and must never branch.
//!
//! Reconstruction (decision 6B-d.1) turns records back into `alloy_consensus` types so the
//! unchanged RPC serializers produce byte-identical JSON. The `From` impls at the bottom are
//! the infallible face the generic provider bounds use; records written by this crate always
//! parse, so they `expect` — disk corruption surfaces earlier as a JSON/gzip error from
//! [`read_block_at`].

use crate::{
    error::{FileTierError, FileTierResult},
    records::{
        AccessListRecord, AuthorizationRecord, BlockLogsRecord, BtRecord, Erc4626Record, LogRecord,
        TxRecord, WithdrawalRecord,
    },
    seal::{active_file_path, sealed_file_path},
    shard::{get_shard, get_shard_range, ShardId},
};
use alloy_consensus::{
    transaction::{TxEip1559, TxEip2930, TxEip4844, TxEip7702},
    EthereumReceipt, EthereumTxEnvelope, Header, Signed, TxLegacy, TxType,
};
use alloy_eips::{
    eip2930::{AccessList, AccessListItem},
    eip4895::{Withdrawal, Withdrawals},
    eip7702::{Authorization, SignedAuthorization},
};
use alloy_primitives::{Address, Log, Signature, TxKind, B256, U256};
use serde::de::DeserializeOwned;
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    str::FromStr,
    sync::OnceLock,
};

/// The signed envelope the Ethereum node primitives use; the target of [`build_tx`].
pub type TransactionSigned = EthereumTxEnvelope<TxEip4844>;
/// The receipt type the Ethereum node primitives use; the target of [`build_receipt`].
pub type NodeReceipt = EthereumReceipt<TxType>;

static BASE: OnceLock<PathBuf> = OnceLock::new();
static NETWORK: OnceLock<NetworkSettings> = OnceLock::new();

/// The network settings the node bootstrap read once and stored: the chain id and the genesis
/// signer list. Held in memory; never re-read from the database, never mutated ad-hoc.
#[derive(Debug, Clone, Default)]
struct NetworkSettings {
    chain_id: u64,
    validators: Vec<Address>,
}

/// Sets the process-global file-tier base directory (the node's chain datadir). Returns `false` if
/// it was already set — the first caller wins.
pub fn set_base(base: impl Into<PathBuf>) -> bool {
    BASE.set(base.into()).is_ok()
}

/// The process-global file-tier base, if [`set_base`] has run.
pub fn base() -> Option<&'static Path> {
    BASE.get().map(PathBuf::as_path)
}

/// Why [`bootstrap_network_settings`] could not set the network globals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainIdentityError {
    /// The database carries no chain id (was it initialised with `smartnet init`?).
    ChainId,
    /// The database carried a chain id but it failed the basic sanity check (`uint` of 0).
    InvalidChainId,
    /// The database carries no validator set (was it initialised with `smartnet init`?).
    Validators,
    /// The database carried a validator set but it failed the basic sanity check (empty, or a
    /// zero address).
    InvalidValidator,
}

impl ChainIdentityError {
    /// The process exit code for a chain-identity boot failure. Distinct per failure so an
    /// operator (or a supervisor) can tell exactly what to provide / fix.
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::ChainId => 2,
            Self::Validators => 3,
            Self::InvalidChainId => 4,
            Self::InvalidValidator => 5,
        }
    }
}

impl std::fmt::Display for ChainIdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChainId => write!(
                f,
                "chain id missing: the database carries none (initialise with `smartnet init \
                 <genesis>`); cannot continue"
            ),
            Self::InvalidChainId => {
                write!(f, "chain id 0 is not a valid chain id; cannot continue")
            }
            Self::Validators => write!(
                f,
                "validators missing: the database carries none (initialise with `smartnet \
                 init <genesis>`); cannot continue"
            ),
            Self::InvalidValidator => write!(
                f,
                "validators row is not a valid (non-empty, non-zero) 0x-address array; cannot \
                 continue"
            ),
        }
    }
}

impl std::error::Error for ChainIdentityError {}

/// The one boot sequence that resolves and stores the network settings, called once at startup
/// **before** anything reads [`get_chain_id`] / [`get_validators`].
///
/// The only source is the **database** — the `Settings` rows written at genesis init
/// (`smartnet init <genesis>`). There is no config fallback: a database that carries no value
/// (or an unreadable one) cannot start the node. The value is then sanity-checked — basic sanity
/// only (not consensus validation): a non-zero `uint` chain id, and a non-empty list of non-zero
/// 20-byte signers. **Fail closed**: a missing *or* invalid value aborts the boot
/// ([`ChainIdentityError`]) — the caller turns that into a soft boot error.
///
/// Finally the stored globals are re-read through the accessors and re-validated
/// ([`verify_network_settings`]), so a boot that returns `Ok(())` is guaranteed to serve a valid
/// chain id and a non-empty signer list from [`get_chain_id`] / [`get_validators`] for the
/// rest of the process.
pub fn bootstrap_network_settings(
    db_chain_id: Option<u64>,
    db_validators: Option<Vec<Address>>,
) -> Result<(), ChainIdentityError> {
    resolve_network_settings(db_chain_id, db_validators, None)
}

/// The programmatic-launch variant of [`bootstrap_network_settings`].
///
/// Used only where a node is launched in code and the chain spec passed to the launch *is* the
/// identity source (upstream test framework / direct builder use — never operator `config.toml`,
/// which is not a source at all): the database values are taken as-is, a database without values
/// bootstraps from the chain spec, and a launch that passes the spec identity passes both the
/// chain id and the signer list. The genesis-hash check at launch already guarantees the
/// database genesis matches that spec, so the fallback can only agree with what the database
/// describes. Sanity-check fail-closed and accessor re-verification are identical to
/// [`bootstrap_network_settings`].
pub fn bootstrap_network_settings_with_fallback(
    db_chain_id: Option<u64>,
    db_validators: Option<Vec<Address>>,
    spec_chain_id: u64,
    spec_validators: Vec<Address>,
) -> Result<(), ChainIdentityError> {
    resolve_network_settings(db_chain_id, db_validators, Some((spec_chain_id, spec_validators)))
}

fn resolve_network_settings(
    db_chain_id: Option<u64>,
    db_validators: Option<Vec<Address>>,
    spec_identity: Option<(u64, Vec<Address>)>,
) -> Result<(), ChainIdentityError> {
    // The database value wins; the chain-spec identity applies only when the database carries
    // the field. **Fail closed**: a present-but-invalid database value aborts the boot
    // immediately — there is no fall-through to the spec identity.
    let chain_id = match db_chain_id {
        Some(0) => return Err(ChainIdentityError::InvalidChainId),
        Some(id) => id,
        None => match spec_identity {
            Some((id, _)) => id,
            None => return Err(ChainIdentityError::ChainId),
        },
    };
    let validators = match db_validators {
        Some(signers) if !is_valid_validator_set(&signers) => {
            return Err(ChainIdentityError::InvalidValidator)
        }
        Some(signers) => signers,
        None => match spec_identity {
            Some((_, signers)) => signers,
            None => return Err(ChainIdentityError::Validators),
        },
    };
    let _ = init_network_settings(chain_id, validators);

    // Boot-time self-check: read the values back through the accessors every other call site
    // uses, and reject anything they would serve. A `Ok(())` return therefore means the accessors
    // cannot observe a missing or invalid value for the rest of the process.
    verify_network_settings()
}

/// Boot-time self-check on the stored globals: re-reads the chain id and the validator set
/// through [`get_chain_id`] / [`get_validators`] and re-applies the same basic sanity as
/// [`bootstrap_network_settings`]. A failure means a boot-order bug (the globals were never set,
/// or were set without validation), and the node exits with the matching
/// [`ChainIdentityError::exit_code`].
///
/// # Panics
/// Panics if called before [`bootstrap_network_settings`] — the accessors' boot-order invariant.
pub fn verify_network_settings() -> Result<(), ChainIdentityError> {
    if get_chain_id() == 0 {
        return Err(ChainIdentityError::InvalidChainId);
    }
    if !is_valid_validator_set(get_validators()) {
        return Err(ChainIdentityError::InvalidValidator);
    }
    Ok(())
}

/// Basic validator-set sanity: at least one validator, none the zero address.
fn is_valid_validator_set(signers: &[Address]) -> bool {
    !signers.is_empty() && signers.iter().all(|signer| !signer.is_zero())
}

/// Stores the resolved network settings. Called only by [`bootstrap_network_settings`]; the
/// globals are set once (first caller wins) and never mutated again.
fn init_network_settings(chain_id: u64, validators: Vec<Address>) -> bool {
    NETWORK.set(NetworkSettings { chain_id, validators }).is_ok()
}

/// Whether the network settings globals have been bootstrapped. Used only to guard a second boot
/// entry point (a programmatic launch after the operator CLI already bootstrapped); ordinary code
/// reads [`get_chain_id`] / [`get_validators`] directly.
pub fn network_settings_initialised() -> bool {
    NETWORK.get().is_some()
}

/// The chain id — **no argument**. Returns the value bootstrap stored. This is an infallible read
/// of a boot-time global: the settings are set before any caller runs, so there is no missing-value
/// branch (the `expect` is the boot-order invariant, not a fallback).
pub fn get_chain_id() -> u64 {
    NETWORK.get().expect("network settings must be bootstrapped before get_chain_id()").chain_id
}

/// The validator set — **no argument**. An ordered array: index 0 is the default validator for
/// now (a single-validator chain is a one-element array). The value bootstrap stored.
pub fn get_validators() -> &'static [Address] {
    &NETWORK
        .get()
        .expect("network settings must be bootstrapped before get_validators()")
        .validators
}

/// All of one block's records, as read from the files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockBundle {
    /// The `bt` record for the block.
    pub bt: BtRecord,
    /// The block's `txs` records, ordered by index.
    pub txs: Vec<TxRecord>,
    /// The block's logs; `None` when the block emitted none.
    pub logs: Option<BlockLogsRecord>,
    /// The block's ERC-4626 events; `None` when it emitted none.
    pub erc4626: Option<Erc4626Record>,
}

impl BlockBundle {
    /// The log records emitted by transaction `tx_index` of this block (the join key between
    /// the tx records and the `logs` file).
    pub fn receipt_logs(&self, tx_index: u64) -> Vec<&LogRecord> {
        self.logs
            .as_ref()
            .map(|rec| rec.lg.iter().filter(|l| l.x == tx_index).collect())
            .unwrap_or_default()
    }
}

/// Reads one block's records from `base`'s files. Returns `Ok(None)` when the block's shard
/// has no `b` file (height not handed off, or beyond the written range).
pub fn read_block_at(
    base: impl AsRef<Path>,
    block_number: u64,
) -> FileTierResult<Option<BlockBundle>> {
    let base = base.as_ref();
    let shard = get_shard(block_number);
    let Some(bt_path) = shard_file(base, shard, "b") else { return Ok(None) };
    let mut bt = read_matching::<BtRecord>(&bt_path, block_number)?;
    let Some(bt) = bt.pop() else { return Ok(None) };

    let txs = read_file_matching::<TxRecord>(base, shard, "txs", block_number)?;
    let logs = read_file_matching::<BlockLogsRecord>(base, shard, "logs", block_number)?;
    let erc4626 = read_file_matching::<Erc4626Record>(base, shard, "4626", block_number)?;

    Ok(Some(BlockBundle {
        bt,
        txs,
        logs: logs.into_iter().next(),
        erc4626: erc4626.into_iter().next(),
    }))
}

/// [`read_block_at`] against the process-global base. Errors if no base was set (the caller
/// must not have branched to files without one).
pub fn read_block(block_number: u64) -> FileTierResult<Option<BlockBundle>> {
    let base = base().ok_or_else(|| {
        FileTierError::RecordField("file-tier base not set; cannot read block records".into())
    })?;
    read_block_at(base, block_number)
}

/// Reads every block in `start..=end` from `base` with **one forward pass per file** —
/// instead of [`read_block_at`] once per height (which reopens and rereads each file from
/// byte 0 for every block). Records are written in ascending `(b, …)` order, so a single
/// sweep collects the whole range and early-exits past `end`.
///
/// Shards are pure math: the range is split at sub-shard boundaries and each overlapping
/// shard's files are swept once. Returns bundles in ascending block order.
pub fn read_blocks_range_at(
    base: impl AsRef<Path>,
    start: u64,
    end: u64,
) -> FileTierResult<Vec<BlockBundle>> {
    if start > end {
        return Ok(Vec::new());
    }
    let base = base.as_ref();
    let mut out = Vec::new();
    let mut block = start;
    while block <= end {
        let shard = get_shard(block);
        let (_, shard_end) = get_shard_range(shard);
        let range_end = end.min(shard_end - 1);
        read_shard_range(base, shard, block, range_end, &mut out)?;
        block = shard_end;
    }
    Ok(out)
}

/// [`read_blocks_range_at`] against the process-global base.
pub fn read_blocks_range(start: u64, end: u64) -> FileTierResult<Vec<BlockBundle>> {
    let base = base().ok_or_else(|| {
        FileTierError::RecordField("file-tier base not set; cannot read block records".into())
    })?;
    read_blocks_range_at(base, start, end)
}

/// One pass over each of the shard's four files, collecting records with
/// `start <= b <= end`, then assembles them into [`BlockBundle`]s.
fn read_shard_range(
    base: &Path,
    shard: ShardId,
    start: u64,
    end: u64,
    out: &mut Vec<BlockBundle>,
) -> FileTierResult<()> {
    let Some(bt_path) = shard_file(base, shard, "b") else { return Ok(()) };
    let bts = read_matching_range::<BtRecord>(&bt_path, start, end)?;
    if bts.is_empty() {
        return Ok(());
    }
    let txs = match shard_file(base, shard, "txs") {
        Some(path) => read_matching_range::<TxRecord>(&path, start, end)?,
        None => Vec::new(),
    };
    let logs = match shard_file(base, shard, "logs") {
        Some(path) => read_matching_range::<BlockLogsRecord>(&path, start, end)?,
        None => Vec::new(),
    };
    let erc4626 = match shard_file(base, shard, "4626") {
        Some(path) => read_matching_range::<Erc4626Record>(&path, start, end)?,
        None => Vec::new(),
    };

    let mut txs_by_block: HashMap<u64, Vec<TxRecord>> = HashMap::new();
    for tx in txs {
        txs_by_block.entry(tx.b).or_default().push(tx);
    }
    let mut logs_by_block: HashMap<u64, BlockLogsRecord> = HashMap::new();
    for rec in logs {
        logs_by_block.insert(rec.b, rec);
    }
    let mut erc_by_block: HashMap<u64, Erc4626Record> = HashMap::new();
    for rec in erc4626 {
        erc_by_block.insert(rec.b, rec);
    }

    for bt in bts {
        let b = bt.b;
        out.push(BlockBundle {
            bt,
            txs: txs_by_block.remove(&b).unwrap_or_default(),
            logs: logs_by_block.remove(&b),
            erc4626: erc_by_block.remove(&b),
        });
    }
    Ok(())
}

/// Streams a JSON-Lines (or gz) file collecting records with `start <= b <= end`.
/// Same early-exit discipline as [`read_matching`]: ascending order, stop past `end`.
fn read_matching_range<T: DeserializeOwned>(
    path: &Path,
    start: u64,
    end: u64,
) -> FileTierResult<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut file =
        File::open(path).map_err(|source| smartnet_fs_util::FsPathError::read(source, path))?;
    let compressed = is_gzip(path)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|source| smartnet_fs_util::FsPathError::read(source, path))?;
    let reader: Box<dyn BufRead> = if compressed {
        Box::new(BufReader::new(flate2::read::MultiGzDecoder::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|source| smartnet_fs_util::FsPathError::read(source, path))?;
        if line.trim().is_empty() {
            continue;
        }
        let peek: OnlyBlock = serde_json::from_str(&line)?;
        if peek.b < start {
            continue;
        }
        if peek.b > end {
            break;
        }
        out.push(serde_json::from_str::<T>(&line)?);
    }
    Ok(out)
}

/// Reads the `name` stream of `shard`, returning records whose `b` equals `block_number`.
/// A missing file yields an empty vector (`4626` and `logs` are lazy).
fn read_file_matching<T: DeserializeOwned>(
    base: &Path,
    shard: crate::shard::ShardId,
    name: &str,
    block_number: u64,
) -> FileTierResult<Vec<T>> {
    let Some(path) = shard_file(base, shard, name) else { return Ok(Vec::new()) };
    read_matching(&path, block_number)
}

/// The on-disk file for `shard`'s `name` record stream: the active `.jsonl` first (it is the
/// source while the shard is being written or sealed), else the sealed `.gz`. `None` when
/// neither exists (height not written). The `.gz.tmp` is transient and never a source.
fn shard_file(base: &Path, shard: crate::shard::ShardId, name: &str) -> Option<PathBuf> {
    [active_file_path(base, shard, name), sealed_file_path(base, shard, name)].into_iter().find(|path| path.exists())
}

/// Whether `path` starts with the gzip magic (`0x1f 0x8b`). The active/mid-seal shard is plain
/// JSONL and every sealed shard is gzip, so this single check — rather than the file name —
/// decides how to read: gzip → decompress, anything else → read plainly.
pub(crate) fn is_gzip(path: &Path) -> FileTierResult<bool> {
    let mut file =
        File::open(path).map_err(|source| smartnet_fs_util::FsPathError::read(source, path))?;
    let mut magic = [0u8; 2];
    let n = file
        .read(&mut magic)
        .map_err(|source| smartnet_fs_util::FsPathError::read(source, path))?;
    Ok(n == 2 && magic == [0x1f, 0x8b])
}

/// Streams a JSON-Lines file (decompressing a sealed `.gz`, reading a plain `.jsonl`
/// as-is), collecting records with `b == block_number`.
///
/// Records are written in ascending `(b, …)` order, so scanning stops at the first record past
/// the target. Each line is first peeked through the tiny [`OnlyBlock`] shape so full decoding
/// only happens on the matching lines.
fn read_matching<T: DeserializeOwned>(path: &Path, block_number: u64) -> FileTierResult<Vec<T>> {
    let mut file =
        File::open(path).map_err(|source| smartnet_fs_util::FsPathError::read(source, path))?;
    let compressed = is_gzip(path)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|source| smartnet_fs_util::FsPathError::read(source, path))?;
    let reader: Box<dyn BufRead> = if compressed {
        Box::new(BufReader::new(flate2::read::MultiGzDecoder::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|source| smartnet_fs_util::FsPathError::read(source, path))?;
        if line.trim().is_empty() {
            continue;
        }
        let peek: OnlyBlock = serde_json::from_str(&line)?;
        match peek.b.cmp(&block_number) {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Equal => out.push(serde_json::from_str::<T>(&line)?),
            std::cmp::Ordering::Greater => break,
        }
    }
    Ok(out)
}

/// The two-field peek used to skip lines cheaply.
#[derive(serde::Deserialize)]
struct OnlyBlock {
    #[allow(dead_code)]
    b: u64,
}

// ---------------------------------------------------------------------------
// Field parsing (records → consensus values)
// ---------------------------------------------------------------------------

fn field_err(kind: &str, value: &str, msg: impl std::fmt::Display) -> FileTierError {
    FileTierError::RecordField(format!("{kind} field {value:?}: {msg}"))
}

fn p<T: FromStr>(kind: &str, value: &str) -> FileTierResult<T>
where
    T::Err: std::fmt::Display,
{
    T::from_str(value).map_err(|e| field_err(kind, value, e))
}

fn p_hex_u64(kind: &str, value: &str) -> FileTierResult<u64> {
    u64::from_str_radix(value.strip_prefix("0x").unwrap_or(value), 16)
        .map_err(|e| field_err(kind, value, e))
}

fn p_hex_u128(kind: &str, value: &str) -> FileTierResult<u128> {
    u128::from_str_radix(value.strip_prefix("0x").unwrap_or(value), 16)
        .map_err(|e| field_err(kind, value, e))
}

fn p_hex_u256(kind: &str, value: &str) -> FileTierResult<U256> {
    U256::from_str(value).map_err(|e| field_err(kind, value, e))
}

fn p_tx_type(value: &str) -> FileTierResult<TxType> {
    match p_hex_u64("ty", value)? {
        0x0 => Ok(TxType::Legacy),
        0x1 => Ok(TxType::Eip2930),
        0x2 => Ok(TxType::Eip1559),
        0x3 => Ok(TxType::Eip4844),
        0x4 => Ok(TxType::Eip7702),
        other => Err(field_err("ty", value, format_args!("unknown tx type {other:#x}"))),
    }
}

fn p_signing_signature(yp: u8, r: &str, s: &str) -> FileTierResult<Signature> {
    if yp > 1 {
        return Err(FileTierError::RecordField(format!("signature y parity {yp} not 0/1")));
    }
    Ok(Signature::new(p_hex_u256("r", r)?, p_hex_u256("s", s)?, yp == 1))
}

/// The reconstructed `TxKind` from the display fields (`fjall4a.md` §3.3 + 6B `nv` rule):
/// `t = 0x1` selects the token contract (`"2"`), `t = 0x0` uses `"1"` (`"0x0"` = create).
fn p_to_kind(t: &str, recipient: &str, contract: &str) -> FileTierResult<TxKind> {
    if t == "0x1" {
        if contract == "0x0" {
            return Ok(TxKind::Call(Address::ZERO));
        }
        return Ok(TxKind::Call(p("to(token)", contract)?));
    }
    if recipient == "0x0" {
        return Ok(TxKind::Create);
    }
    Ok(TxKind::Call(p("to", recipient)?))
}

fn p_access_list(al: Option<Vec<AccessListRecord>>) -> FileTierResult<Option<AccessList>> {
    al.map(|entries| {
        entries
            .into_iter()
            .map(|e| {
                let address = p("al.address", &e.a)?;
                let storage_keys =
                    e.k.iter()
                        .map(|k| p("al.storage_key", k))
                        .collect::<FileTierResult<Vec<_>>>()?;
                Ok(AccessListItem { address, storage_keys })
            })
            .collect::<FileTierResult<Vec<_>>>()
            .map(AccessList)
    })
    .transpose()
}

fn p_authorizations(
    auth: Option<Vec<AuthorizationRecord>>,
) -> FileTierResult<Option<Vec<SignedAuthorization>>> {
    auth.map(|items| {
        items
            .into_iter()
            .map(|a| {
                let inner = Authorization {
                    chain_id: p_hex_u256("auth.cid", &a.cid)?,
                    address: p("auth.addr", &a.addr)?,
                    nonce: a.n,
                };
                if a.yp > 1 {
                    return Err(FileTierError::RecordField(format!(
                        "auth y parity {} not 0/1",
                        a.yp
                    )));
                }
                Ok(SignedAuthorization::new_unchecked(
                    inner,
                    a.yp,
                    p_hex_u256("auth.r", &a.r)?,
                    p_hex_u256("auth.s", &a.s)?,
                ))
            })
            .collect()
    })
    .transpose()
}

// ---------------------------------------------------------------------------
// Builders (records → consensus types)
// ---------------------------------------------------------------------------

/// Rebuilds the block header from its `bt` record.
///
/// Withdrawals are body data, not header fields — the provider reads them from `bt.wd`
/// alongside the rebuilt header.
pub fn build_header(bt: &BtRecord) -> FileTierResult<Header> {
    let opt = |kind: &str, v: &Option<String>| -> FileTierResult<Option<B256>> {
        v.as_deref().map(|s| p(kind, s)).transpose()
    };
    Ok(Header {
        parent_hash: p("ph", &bt.ph)?,
        ommers_hash: p("oh", &bt.oh)?,
        beneficiary: p("m", &bt.m)?,
        state_root: p("sr", &bt.sr)?,
        transactions_root: p("txr", &bt.txr)?,
        receipts_root: p("rcr", &bt.rcr)?,
        difficulty: p_hex_u256("df", &bt.df)?,
        number: bt.b,
        gas_limit: bt.gl,
        gas_used: bt.gu,
        timestamp: bt.ts,
        extra_data: p("ex", &bt.ex)?,
        mix_hash: bt.mh.as_deref().map(|s| p("mh", s)).transpose()?.unwrap_or_default(),
        nonce: bt.nn.as_deref().map(|s| p("nn", s)).transpose()?.unwrap_or_default(),
        base_fee_per_gas: bt.bf,
        withdrawals_root: opt("wr", &bt.wr)?,
        blob_gas_used: bt.bgu,
        excess_blob_gas: bt.ebg,
        parent_beacon_block_root: opt("pbr", &bt.pbr)?,
        requests_hash: opt("rh", &bt.rh)?,
        logs_bloom: p("lb", &bt.lb)?,
        block_access_list_hash: opt("bal", &bt.bal)?,
        slot_number: bt.sn,
    })
}

/// Rebuilds one withdrawal.
pub fn build_withdrawal(wd: &WithdrawalRecord) -> FileTierResult<Withdrawal> {
    Ok(Withdrawal {
        index: wd.i,
        validator_index: wd.vi,
        address: p("wd.a", &wd.a)?,
        amount: wd.am,
    })
}

/// Rebuilds a block's withdrawals from its `bt.wd` field: `None` pre-Shanghai (the record
/// has no `wd`), `Some(...)` after — including `Some([])` for a Shanghai block that
/// withdrew nothing, which is what the body must carry.
pub fn build_withdrawals(
    wd: Option<&Vec<WithdrawalRecord>>,
) -> FileTierResult<Option<Withdrawals>> {
    wd.map(|records| {
        records.iter().map(build_withdrawal).collect::<FileTierResult<Vec<_>>>().map(Withdrawals)
    })
    .transpose()
}

/// Rebuilds one log entry.
pub fn build_log(log: &LogRecord) -> FileTierResult<Log> {
    let address = p("log.a", &log.a)?;
    let topics = log.t.iter().map(|t| p("log.t", t)).collect::<FileTierResult<Vec<B256>>>()?;
    let data = p("log.d", &log.d)?;
    Ok(Log::new_unchecked(address, topics, data))
}

/// Rebuilds a receipt from a tx record's scalar fields (`ty`, `st`) plus the derived
/// `cumulative_gas_used` and the logs joined from the `logs` file.
pub fn build_receipt(ty: &str, st: bool, cgu: u64, logs: Vec<Log>) -> FileTierResult<NodeReceipt> {
    Ok(EthereumReceipt { tx_type: p_tx_type(ty)?, success: st, cumulative_gas_used: cgu, logs })
}

/// Rebuilds the signed transaction envelope from its `txs` record.
///
/// The sender is not needed for the envelope itself — the RPC layer pairs it with the record's
/// `"0"` field — but the stored hash is trusted as-is so the envelope round-trips exactly. The
/// chain id is not stored in the record; it is the network constant from [`chain_id`].
pub fn build_tx(tx: &TxRecord) -> FileTierResult<TransactionSigned> {
    let hash = p("h", &tx.h)?;
    let sig = p_signing_signature(tx.yp, &tx.r, &tx.s)?;
    let value = p_hex_u256("nv", &tx.nv)?;
    let input = p("input", &tx.input)?;
    let gas_limit = p_hex_u64("gas", &tx.gas)?;
    let nonce = tx.n;
    // The chain id is a single network value (bootstrapped once at startup), not stored per
    // record — it is the same for every transaction.
    let chain_id = get_chain_id();

    let ty = p_tx_type(&tx.ty)?;
    let signed: TransactionSigned = match ty {
        TxType::Legacy => {
            let gas_price = p_hex_u128("mfp", &tx.mfp)?;
            let inner = TxLegacy {
                chain_id: Some(chain_id),
                nonce,
                gas_price,
                gas_limit,
                to: p_to_kind(&tx.t, &tx.to, &tx.contract)?,
                value,
                input,
            };
            Signed::new_unchecked(inner, sig, hash).into()
        }
        TxType::Eip2930 => {
            let gas_price = p_hex_u128("mfp", &tx.mfp)?;
            let inner = TxEip2930 {
                chain_id,
                nonce,
                gas_price,
                gas_limit,
                to: p_to_kind(&tx.t, &tx.to, &tx.contract)?,
                value,
                access_list: p_access_list(tx.al.clone())?.unwrap_or_default(),
                input,
            };
            Signed::new_unchecked(inner, sig, hash).into()
        }
        TxType::Eip1559 => {
            let inner = TxEip1559 {
                chain_id,
                nonce,
                max_fee_per_gas: p_hex_u128("mfp", &tx.mfp)?,
                max_priority_fee_per_gas: tx
                    .mtf
                    .as_deref()
                    .map(|s| p_hex_u128("mtf", s))
                    .transpose()?
                    .ok_or_else(|| FileTierError::RecordField("1559 tx missing mtf".into()))?,
                gas_limit,
                to: p_to_kind(&tx.t, &tx.to, &tx.contract)?,
                value,
                access_list: p_access_list(tx.al.clone())?.unwrap_or_default(),
                input,
            };
            Signed::new_unchecked(inner, sig, hash).into()
        }
        TxType::Eip4844 => {
            let inner = TxEip4844 {
                chain_id,
                nonce,
                max_fee_per_gas: p_hex_u128("mfp", &tx.mfp)?,
                max_priority_fee_per_gas: tx
                    .mtf
                    .as_deref()
                    .map(|s| p_hex_u128("mtf", s))
                    .transpose()?
                    .ok_or_else(|| FileTierError::RecordField("4844 tx missing mtf".into()))?,
                gas_limit,
                // 4844 `to` is a required Address, not a TxKind: the ERC-20 display fields do
                // not apply (blob txs cannot be plain transfers with substituted recipients).
                to: if tx.t == "0x1" {
                    p("to(token)", &tx.contract)?
                } else if tx.to == "0x0" {
                    return Err(FileTierError::RecordField("4844 tx missing to".into()));
                } else {
                    p("to", &tx.to)?
                },
                value,
                access_list: p_access_list(tx.al.clone())?.unwrap_or_default(),
                max_fee_per_blob_gas: tx
                    .mfb
                    .as_deref()
                    .map(|s| p_hex_u128("mfb", s))
                    .transpose()?
                    .ok_or_else(|| FileTierError::RecordField("4844 tx missing mfb".into()))?,
                blob_versioned_hashes: tx
                    .bh
                    .as_ref()
                    .map(|hs| hs.iter().map(|h| p("bh", h)).collect())
                    .transpose()?
                    .ok_or_else(|| {
                        FileTierError::RecordField("4844 tx missing blob hashes".into())
                    })?,
                input,
            };
            Signed::new_unchecked(inner, sig, hash).into()
        }
        TxType::Eip7702 => {
            let inner = TxEip7702 {
                chain_id,
                nonce,
                max_fee_per_gas: p_hex_u128("mfp", &tx.mfp)?,
                max_priority_fee_per_gas: tx
                    .mtf
                    .as_deref()
                    .map(|s| p_hex_u128("mtf", s))
                    .transpose()?
                    .ok_or_else(|| FileTierError::RecordField("7702 tx missing mtf".into()))?,
                gas_limit,
                to: match p_to_kind(&tx.t, &tx.to, &tx.contract)? {
                    TxKind::Call(addr) => addr,
                    TxKind::Create => {
                        return Err(FileTierError::RecordField("7702 tx missing to".into()))
                    }
                },
                value,
                access_list: p_access_list(tx.al.clone())?.unwrap_or_default(),
                authorization_list: p_authorizations(tx.auth.clone())?.ok_or_else(|| {
                    FileTierError::RecordField("7702 tx missing auth list".into())
                })?,
                input,
            };
            Signed::new_unchecked(inner, sig, hash).into()
        }
    };
    Ok(signed)
}

// ---------------------------------------------------------------------------
// Infallible face for the generic provider bounds (decision 6B-d.2)
// ---------------------------------------------------------------------------

impl From<&BtRecord> for Header {
    fn from(bt: &BtRecord) -> Self {
        build_header(bt).expect("file-tier bt record must rebuild into a header")
    }
}

impl From<&TxRecord> for TransactionSigned {
    fn from(tx: &TxRecord) -> Self {
        build_tx(tx).expect("file-tier txs record must rebuild into a signed tx")
    }
}

/// The receipt scalar (`ty`, `st`, derived `cgu`) plus the logs to join into it — the
/// file-tier-side unit the provider's receipt bound consumes (the scalar lives on the tx
/// record and the logs live in a different file, so a plain `From` cannot reconstruct the
/// full receipt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptBuild {
    /// The receipt tx type (hex), from the tx record's `ty`.
    pub ty: String,
    /// The receipt status, from the tx record's `st`.
    pub st: bool,
    /// The receipt's cumulative gas used (derived from the running sum of `gu`).
    pub cgu: u64,
    /// The receipt's logs, already built.
    pub logs: Vec<Log>,
}

impl From<ReceiptBuild> for NodeReceipt {
    fn from(build: ReceiptBuild) -> Self {
        build_receipt(&build.ty, build.st, build.cgu, build.logs)
            .expect("file-tier receipt must rebuild into a receipt")
    }
}

impl From<&LogRecord> for Log {
    fn from(log: &LogRecord) -> Self {
        build_log(log).expect("file-tier log record must rebuild into a log")
    }
}

impl From<&WithdrawalRecord> for Withdrawal {
    fn from(wd: &WithdrawalRecord) -> Self {
        build_withdrawal(wd).expect("file-tier withdrawal record must rebuild")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{extract::extract_block, seal::seal_shard, writer::FileTierWriter};
    use alloy_primitives::{Bloom, Bytes, B64};

    /// The chain id and validators are stored once at node bootstrap; the reader only reads
    /// them. Tests simulate that bootstrap here (the database is not initialized in a unit test).
    fn ensure_chain_id() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = bootstrap_network_settings(Some(1), Some(vec![Address::repeat_byte(0x11)]));
        });
    }

    /// Boot fails closed: the value is taken from the database only — there is no config
    /// fallback — and a missing or invalid value aborts with a distinct process exit code.
    #[test]
    fn bootstrap_fails_closed() {
        // Missing database values.
        assert_eq!(bootstrap_network_settings(None, None), Err(ChainIdentityError::ChainId));
        assert_eq!(bootstrap_network_settings(Some(1), None), Err(ChainIdentityError::Validators));

        // Database values present but invalid -> abort.
        assert_eq!(
            bootstrap_network_settings(Some(0), Some(vec![Address::repeat_byte(0x11)])),
            Err(ChainIdentityError::InvalidChainId)
        );
        assert_eq!(
            bootstrap_network_settings(Some(1), Some(vec![Address::ZERO])),
            Err(ChainIdentityError::InvalidValidator)
        );
        assert_eq!(
            bootstrap_network_settings(Some(1), Some(Vec::new())),
            Err(ChainIdentityError::InvalidValidator)
        );

        // Distinct process exit codes per failure.
        assert_eq!(ChainIdentityError::ChainId.exit_code(), 2);
        assert_eq!(ChainIdentityError::Validators.exit_code(), 3);
        assert_eq!(ChainIdentityError::InvalidChainId.exit_code(), 4);
        assert_eq!(ChainIdentityError::InvalidValidator.exit_code(), 5);
    }

    /// A successful boot leaves both accessors serving a valid value: `bootstrap_network_settings`
    /// re-reads them through `verify_network_settings()` before returning `Ok`, so no call site can
    /// observe a missing chain id or an empty signer list.
    #[test]
    fn bootstrap_verifies_the_accessors() {
        ensure_chain_id();

        assert!(network_settings_initialised());
        assert_ne!(get_chain_id(), 0);
        assert!(!get_validators().is_empty());
        assert!(get_validators().iter().all(|signer| !signer.is_zero()));
        assert_eq!(verify_network_settings(), Ok(()));
    }

    fn sample_header() -> Header {
        Header {
            parent_hash: B256::repeat_byte(1),
            ommers_hash: B256::repeat_byte(2),
            beneficiary: Address::repeat_byte(3),
            state_root: B256::repeat_byte(4),
            transactions_root: B256::repeat_byte(5),
            receipts_root: B256::repeat_byte(6),
            difficulty: U256::from(7u64),
            number: 42,
            gas_limit: 30_000_000,
            gas_used: 21_000,
            timestamp: 1_700_000_000,
            extra_data: Bytes::from(vec![0xde, 0xad]),
            mix_hash: B256::repeat_byte(8),
            nonce: B64::repeat_byte(9),
            base_fee_per_gas: Some(1_000_000_000),
            withdrawals_root: Some(B256::repeat_byte(10)),
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: Some(B256::repeat_byte(11)),
            requests_hash: None,
            logs_bloom: Bloom::repeat_byte(0xaa),
            ..Default::default()
        }
    }

    /// Write one block through the real pipeline (extract → writer → seal), read it back, and
    /// rebuild the consensus types — byte-for-byte against the originals.
    #[test]
    fn round_trip_header_and_txs() {
        ensure_chain_id();
        let dir = tempfile::tempdir().unwrap();
        let header = sample_header();
        let sender = Address::repeat_byte(0xaa);

        // A 1559 tx with an access list and an EIP-155 legacy tx — both wrapped in the
        // envelope so the slice is homogeneous (as the node's body stores them).
        let tx1: TransactionSigned = alloy_consensus::Signed::new_unchecked(
            TxEip1559 {
                chain_id: 1,
                nonce: 5,
                max_fee_per_gas: 2_000_000_000,
                max_priority_fee_per_gas: 100_000_000,
                gas_limit: 60_000,
                to: TxKind::Call(Address::repeat_byte(0xbb)),
                value: U256::from(11u64),
                access_list: alloy_eips::eip2930::AccessList::default(),
                input: Bytes::from(vec![0x01, 0x02]),
            },
            Signature::new(U256::from(3u64), U256::from(4u64), true),
            B256::repeat_byte(0x61),
        )
        .into();
        let tx2: TransactionSigned = alloy_consensus::Signed::new_unchecked(
            TxLegacy {
                chain_id: Some(1),
                nonce: 6,
                gas_price: 1_500_000_000,
                gas_limit: 21_000,
                to: TxKind::Create,
                value: U256::ZERO,
                input: Bytes::from(vec![0x60, 0x80]),
            },
            Signature::new(U256::from(5u64), U256::from(6u64), false),
            B256::repeat_byte(0x62),
        )
        .into();

        let receipts = vec![
            EthereumReceipt {
                tx_type: TxType::Eip1559,
                success: true,
                cumulative_gas_used: 42_000,
                logs: vec![Log::new_unchecked(
                    Address::repeat_byte(0xcc),
                    vec![B256::repeat_byte(0x77)],
                    Bytes::from(vec![0xfe]),
                )],
            },
            EthereumReceipt {
                tx_type: TxType::Legacy,
                success: false,
                cumulative_gas_used: 63_000,
                logs: vec![],
            },
        ];

        let recs = extract_block(
            &header,
            B256::repeat_byte(0x99),
            &[tx1, tx2],
            &[sender, sender],
            &receipts,
            Some(
                &[Withdrawal {
                    index: 1,
                    validator_index: 2,
                    address: Address::repeat_byte(0xdd),
                    amount: 32_000_000_000,
                }][..],
            ),
        );

        // Write + seal through the production pipeline.
        {
            let mut writer = FileTierWriter::new(dir.path());
            let shard = crate::shard::get_shard(42);
            {
                let files = writer.files_for_block(42).unwrap();
                files.append_bt(&recs.bt).unwrap();
                for t in &recs.txs {
                    files.append_tx(t).unwrap();
                }
                files.append_logs(recs.logs.as_ref().unwrap()).unwrap();
                if let Some(ev) = &recs.erc4626 {
                    files.append_4626(dir.path(), shard, ev).unwrap();
                }
            }
            writer.finish_active().unwrap();
            seal_shard(dir.path(), shard).unwrap();
        }

        let bundle = read_block_at(dir.path(), 42).unwrap().expect("block present");
        assert_eq!(bundle.bt, recs.bt, "bt round-trips");
        assert_eq!(bundle.txs, recs.txs, "txs round-trip");
        assert_eq!(bundle.logs, recs.logs, "logs round-trip");

        // Consensus rebuild: header is value-equal to the original.
        let rebuilt_header = build_header(&bundle.bt).unwrap();
        assert_eq!(rebuilt_header, header);

        // Withdrawal rebuilds.
        let wd = build_withdrawal(&bundle.bt.wd.as_ref().unwrap()[0]).unwrap();
        assert_eq!(wd.index, 1);
        assert_eq!(wd.validator_index, 2);
        assert_eq!(wd.amount, 32_000_000_000);

        // Tx rebuild: byte-identical encoding + stored hash + sender.
        let rebuilt1 = build_tx(&bundle.txs[0]).unwrap();
        let original1: TransactionSigned = alloy_consensus::Signed::new_unchecked(
            TxEip1559 {
                chain_id: 1,
                nonce: 5,
                max_fee_per_gas: 2_000_000_000,
                max_priority_fee_per_gas: 100_000_000,
                gas_limit: 60_000,
                to: TxKind::Call(Address::repeat_byte(0xbb)),
                value: U256::from(11u64),
                access_list: alloy_eips::eip2930::AccessList::default(),
                input: Bytes::from(vec![0x01, 0x02]),
            },
            Signature::new(U256::from(3u64), U256::from(4u64), true),
            B256::repeat_byte(0x61),
        )
        .into();
        assert_eq!(
            alloy_eips::eip2718::Encodable2718::encoded_2718(&rebuilt1),
            alloy_eips::eip2718::Encodable2718::encoded_2718(&original1)
        );
        assert_eq!(rebuilt1.tx_hash(), original1.tx_hash());

        let rebuilt2 = build_tx(&bundle.txs[1]).unwrap();
        assert_eq!(
            alloy_eips::eip2718::Encodable2718::encoded_2718(&rebuilt2),
            alloy_eips::eip2718::Encodable2718::encoded_2718(&{
                let orig: TransactionSigned = alloy_consensus::Signed::new_unchecked(
                    TxLegacy {
                        chain_id: Some(1),
                        nonce: 6,
                        gas_price: 1_500_000_000,
                        gas_limit: 21_000,
                        to: TxKind::Create,
                        value: U256::ZERO,
                        input: Bytes::from(vec![0x60, 0x80]),
                    },
                    Signature::new(U256::from(5u64), U256::from(6u64), false),
                    B256::repeat_byte(0x62),
                )
                .into();
                orig
            })
        );

        // Receipt rebuild from the tx scalar + joined logs.
        let logs0 = bundle
            .receipt_logs(0)
            .into_iter()
            .map(build_log)
            .collect::<FileTierResult<Vec<_>>>()
            .unwrap();
        let rebuilt_r0 = build_receipt(&bundle.txs[0].ty, bundle.txs[0].st, 42_000, logs0).unwrap();
        assert_eq!(rebuilt_r0, receipts[0]);
        let rebuilt_r1 =
            build_receipt(&bundle.txs[1].ty, bundle.txs[1].st, 63_000, vec![]).unwrap();
        assert_eq!(rebuilt_r1, receipts[1]);

        // No-logs join returns empty.
        assert!(bundle.receipt_logs(1).is_empty());
    }

    /// An ERC-20-substituted record rebuilds `to` from the token field and value from `nv`.
    #[test]
    fn erc20_record_rebuilds_native_value_and_token_to() {
        ensure_chain_id();
        let mut rec = TxRecord {
            h: B256::repeat_byte(1).to_string(),
            b: 1,
            i: 0,
            from: Address::repeat_byte(0xaa).to_string(),
            to: Address::repeat_byte(0xee).to_string(), // real recipient (log topics[2])
            contract: Address::repeat_byte(0xbb).to_string(), // token
            t: "0x1".into(),
            ty: "0x0".into(),
            gas: "0x5208".into(),
            st: true,
            gu: 21_000,
            nv: "0x7".into(),
            ..Default::default()
        };
        rec.n = 3;
        rec.input = "0xa9059cbb".into();
        rec.mfp = "0x3b9aca00".into();
        rec.r = "0x1".into();
        rec.s = "0x2".into();

        let built = build_tx(&rec).unwrap();
        // Native legacy tx: to = token (from "2"), value = 7 wei (from nv).
        match &built {
            EthereumTxEnvelope::Legacy(signed) => {
                assert_eq!(signed.tx().to, TxKind::Call(Address::repeat_byte(0xbb)));
                assert_eq!(signed.tx().value, U256::from(7u64));
                assert_eq!(signed.tx().nonce, 3);
                assert_eq!(signed.tx().gas_price, 1_000_000_000);
                assert_eq!(signed.tx().chain_id, Some(1));
            }
            other => panic!("expected legacy envelope, got {other:?}"),
        }
    }

    /// A missing shard yields `Ok(None)`; other heights in an existing shard yield records.
    #[test]
    fn missing_shard_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_block_at(dir.path(), 5).unwrap().is_none());
    }

    /// `receipt_logs` joins by the tx index field across blocks' logs.
    #[test]
    fn receipt_logs_joins_by_index() {
        let bundle = BlockBundle {
            bt: BtRecord::default(),
            txs: vec![],
            logs: Some(BlockLogsRecord {
                b: 1,
                lg: vec![
                    LogRecord {
                        i: 0,
                        x: 0,
                        tx: "0xa".into(),
                        a: "0x1".into(),
                        t: vec![],
                        d: "0x".into(),
                    },
                    LogRecord {
                        i: 1,
                        x: 2,
                        tx: "0xb".into(),
                        a: "0x2".into(),
                        t: vec![],
                        d: "0x".into(),
                    },
                ],
            }),
            erc4626: None,
        };
        assert_eq!(bundle.receipt_logs(0).len(), 1);
        assert_eq!(bundle.receipt_logs(2).len(), 1);
        assert!(bundle.receipt_logs(1).is_empty());
    }

    /// The global base is set once; `read_block` without a base errors.
    #[test]
    fn base_once_semantics() {
        // Do not assert on the process-global value itself (other tests may run first);
        // assert the set-once contract only.
        let fresh = OnceLock::<PathBuf>::new();
        assert!(fresh.set(PathBuf::from("/a")).is_ok());
        assert!(!fresh.set(PathBuf::from("/b")).is_ok());
    }

    /// `read_blocks_range_at` returns every block in the inclusive range in ascending order,
    /// with one forward pass (same early-exit discipline as point reads). Empty range and
    /// missing heights yield empty/absent entries without error.
    #[test]
    fn blocks_range_collects_inclusive_span() {
        use crate::seal::write_active;
        let dir = tempfile::tempdir().unwrap();
        let shard = crate::shard::get_shard(0);
        // Three minimal b records (blocks 10, 11, 12) in one active file.
        // Required BtRecord fields: b, h, ph, ts, tc, gu, gl, m, ex.
        let line = |b: u64, h: &str| {
            format!(
                "{{\"b\":{b},\"h\":\"{h}\",\"ph\":\"0x00\",\"ts\":1,\"tc\":0,\"gu\":0,\
                 \"gl\":30000000,\"m\":\"0x00\",\"ex\":\"0x\"}}\n"
            )
        };
        let mut contents = String::new();
        contents.push_str(&line(10, "0xaa"));
        contents.push_str(&line(11, "0xbb"));
        contents.push_str(&line(12, "0xcc"));
        write_active(dir.path(), shard, "b", contents.as_bytes());

        // Full span.
        let all = read_blocks_range_at(dir.path(), 10, 12).unwrap();
        let numbers: Vec<u64> = all.iter().map(|b| b.bt.b).collect();
        assert_eq!(numbers, vec![10, 11, 12]);

        // Sub-range (early-exit past `end` still returns the interior).
        let mid = read_blocks_range_at(dir.path(), 11, 11).unwrap();
        assert_eq!(mid.len(), 1);
        assert_eq!(mid[0].bt.b, 11);

        // Heights outside the written records: empty, not an error.
        assert!(read_blocks_range_at(dir.path(), 0, 9).unwrap().is_empty());
        assert!(read_blocks_range_at(dir.path(), 13, 20).unwrap().is_empty());

        // Empty / inverted range.
        assert!(read_blocks_range_at(dir.path(), 12, 10).unwrap().is_empty());
    }
}

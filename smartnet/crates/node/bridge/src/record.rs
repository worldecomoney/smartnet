//! THE record store (§4): one JSONL buffer per network, a RAM working set, and the
//! 48 h hash-reply memory (§5).
//!
//! The chain is the authority; these files are a **cache**. Every change rewrites the
//! whole file for that network atomically (`{chainId}-bridge-buffer.jsonl`, temp +
//! rename), so a crash can never leave a half-written line behind.

use std::{
    collections::HashMap,
    io::{Error, ErrorKind, Result},
    path::{Path, PathBuf},
};

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// The filename suffix of a network's buffer: `{chainId}-bridge-buffer.jsonl` (§4.1).
const FILE_SUFFIX: &str = "-bridge-buffer.jsonl";

/// A record leaves the store only at stage [`Stage::Done`] **and** 48 h old (§5).
///
/// Age is measured from the record's own `blockTs` — deterministic across reboots, no
/// hidden state. 48 h covers ≥ 2× the ≈12 h admission window (§6.1), so the hash-reply
/// memory can never forget a transaction that could still be admitted.
pub const RETENTION_SECS: u64 = 48 * 60 * 60;

/// The side a transfer came from (§4.2) — written once, at detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Foreign chain → our chain (mint on our side, §7).
    In,
    /// Our chain → foreign chain (burn on our side, release there, §8).
    Out,
}

/// The five stages of a record (§5). Serialized as the bare number `0`…`4`, so a
/// hand-edited line is just a number; an out-of-range number fails to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Stage {
    /// `0` — detected; nothing done yet (the record is written first).
    Detected = 0,
    /// `1` — checks passed, the `0.91`/`0.92` request is built.
    Picked = 1,
    /// `2` — IN: funds arrived · OUT: the burn is confirmed.
    Confirmed = 2,
    /// `3` — the delivery/release transaction is built.
    Built = 3,
    /// `4` — done (or a terminal verdict at detection, with `lastErr`).
    Done = 4,
}

impl TryFrom<u8> for Stage {
    type Error = u8;

    fn try_from(value: u8) -> std::result::Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Detected),
            1 => Ok(Self::Picked),
            2 => Ok(Self::Confirmed),
            3 => Ok(Self::Built),
            4 => Ok(Self::Done),
            other => Err(other),
        }
    }
}

impl Serialize for Stage {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for Stage {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let raw = u8::deserialize(deserializer)?;
        Self::try_from(raw).map_err(|_| {
            serde::de::Error::custom(format!("stage {raw} is out of range — must be 0..=4"))
        })
    }
}

/// One line of a network's buffer — exactly the field names and order of §4.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    /// Source chain of the transfer (IN: foreign → own; OUT: own → foreign).
    pub chain_id: u64,
    /// Destination chain of the transfer.
    pub out_chain_id: u64,
    /// `"in"` or `"out"`, fixed at detection so a config edit cannot reinterpret it.
    pub direction: Direction,
    /// The watched pair address the transfer hit.
    pub address: Address,
    /// Transaction hash — with `chainId` and `logIndex`, the dedup identity.
    pub hash: B256,
    /// `0` for a coin transfer; a real position for a multi-transfer ERC-20 tx.
    pub log_index: u64,
    /// Block number, for depth/age maths and canonicality re-checks.
    pub block_nr: u64,
    /// Block hash, for canonicality re-checks on retry.
    pub block_hash: B256,
    /// Block timestamp — the retention clock and the admission window's age source.
    pub block_ts: u64,
    /// The sender — and the recipient: delivery and release both pay this address.
    pub from: Address,
    /// The pair's instrument name on the source side.
    pub instrument: String,
    /// Contract on the source side; `null` for a coin side.
    #[serde(default)]
    pub contract_in: Option<Address>,
    /// Contract on the destination side; `null` for a coin side.
    #[serde(default)]
    pub contract_out: Option<Address>,
    /// What was sent, in base units, as a decimal wei string.
    pub amount_in: String,
    /// Decimals of `amountIn`.
    pub dec_in: u8,
    /// What comes out (floored to the destination's decimals), decimal wei string.
    pub amount_out: String,
    /// Decimals of `amountOut`.
    pub dec_out: u8,
    /// `0`…`4` (§5).
    pub stage: Stage,
    /// The `0.91`/`0.92` request tx — written **before** it is broadcast.
    #[serde(default)]
    pub tx_req: Option<B256>,
    /// The delivery/release tx — written **before** it is broadcast.
    #[serde(default)]
    pub tx_rel: Option<B256>,
    /// The OUT burn on our chain — written **before** it is broadcast; `null` for IN.
    #[serde(default)]
    pub tx_burn: Option<B256>,
    /// Reason of the last failed attempt; cleared by the next successful one.
    #[serde(default)]
    pub last_err: Option<String>,
}

/// Dedup identity: `(chainId, hash, logIndex)` (§4.2). One transaction can leave
/// several records when it moves several ERC-20 transfers at once.
pub type RecordKey = (u64, B256, u64);

impl Record {
    /// This record's identity in the dedup check (3.2 — **after** the admission window).
    pub const fn key(&self) -> RecordKey {
        (self.chain_id, self.hash, self.log_index)
    }

    /// A finished record is terminal; only those may be pruned, and only at 48 h (§5).
    pub fn is_done(&self) -> bool {
        self.stage == Stage::Done
    }

    /// Guards the two hand-editable decimal fields: amounts are decimal wei strings.
    fn is_well_formed(&self) -> bool {
        let decimal = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        decimal(&self.amount_in) && decimal(&self.amount_out)
    }
}

/// The per-network buffers plus their RAM working set.
#[derive(Debug)]
pub struct RecordStore {
    /// `~/.smartnet/bridge` — the directory holding the per-network buffers.
    dir: PathBuf,
    /// All open records, keyed by [`RecordKey`].
    records: HashMap<RecordKey, Record>,
}

impl RecordStore {
    /// An empty store rooted at `dir`. Use [`Self::open`] to read existing buffers back.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), records: HashMap::new() }
    }

    /// Opens `dir` (creating it if absent) and reads back every `{chainId}` buffer.
    ///
    /// Unparseable lines, bad stages, malformed amounts and lines that claim a chain
    /// other than their file's are skipped with a warning — one bad hand-edit never
    /// bricks the rest of the cache. Foreign files in the folder are ignored (§4.1).
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let mut store = Self::new(dir.clone());

        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if !path.is_file() {
                continue;
            }
            let Some(chain_id) = chain_id_from_path(&path) else {
                continue;
            };
            let text = std::fs::read_to_string(&path)?;
            for (line_no, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let record: Record = match serde_json::from_str(line) {
                    Ok(record) => record,
                    Err(err) => {
                        warn!(
                            "{} line {}: skipped, does not parse: {err}",
                            path.display(),
                            line_no + 1
                        );
                        continue;
                    }
                };
                if record.chain_id != chain_id {
                    warn!(
                        "{} line {}: skipped, chainId {} does not match the filename",
                        path.display(),
                        line_no + 1,
                        record.chain_id
                    );
                    continue;
                }
                if !record.is_well_formed() {
                    warn!(
                        "{} line {}: skipped, amounts must be decimal wei strings",
                        path.display(),
                        line_no + 1
                    );
                    continue;
                }
                store.records.insert(record.key(), record);
            }
        }
        Ok(store)
    }

    /// Inserts or replaces a record, then rewrites its whole network file atomically.
    ///
    /// Callers write the tx reference **before** broadcasting (§4.2), so a tx that
    /// mines between heartbeats is observed, never rebuilt.
    pub fn put(&mut self, record: Record) -> Result<()> {
        if !record.is_well_formed() {
            return Err(Error::new(ErrorKind::InvalidData, "amounts must be decimal wei strings"));
        }
        let chain_id = record.chain_id;
        self.records.insert(record.key(), record);
        self.flush_chain(chain_id)
    }

    /// The hash-reply (dedup) check — called only **after** the admission window (3.2).
    pub fn contains(&self, chain_id: u64, hash: B256, log_index: u64) -> bool {
        self.records.contains_key(&(chain_id, hash, log_index))
    }

    /// A record for this exact identity, if one is open.
    pub fn get(&self, chain_id: u64, hash: B256, log_index: u64) -> Option<&Record> {
        self.records.get(&(chain_id, hash, log_index))
    }

    /// Every open record — the heartbeat's working set.
    pub fn all(&self) -> impl Iterator<Item = &Record> {
        self.records.values()
    }

    /// Number of open records.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True when no records are open.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Drops records that are **both** at stage `4` and 48 h old (§5).
    ///
    /// Unfinished records are never dropped by age, however old they get. Returns the
    /// number dropped and rewrites each touched network file once.
    pub fn prune(&mut self, now_ts: u64) -> Result<usize> {
        let doomed: Vec<RecordKey> = self
            .records
            .values()
            .filter(|record| {
                record.is_done() && now_ts.saturating_sub(record.block_ts) >= RETENTION_SECS
            })
            .map(Record::key)
            .collect();
        let dropped = doomed.len();

        let mut touched = Vec::new();
        for key in doomed {
            if let Some(record) = self.records.remove(&key) {
                touched.push(record.chain_id);
            }
        }
        touched.sort_unstable();
        touched.dedup();
        for chain_id in touched {
            self.flush_chain(chain_id)?;
        }
        Ok(dropped)
    }

    /// Wholesale rewrite of one network's file: serialize → temp → rename (§4.1).
    fn flush_chain(&self, chain_id: u64) -> Result<()> {
        let mut lines: Vec<&Record> =
            self.records.values().filter(|record| record.chain_id == chain_id).collect();
        // Stable file contents, independent of hash-map iteration order.
        lines.sort_unstable_by_key(|record| (record.block_nr, record.hash, record.log_index));

        let mut text = String::new();
        for record in lines {
            let line = serde_json::to_string(record)
                .map_err(|err| Error::new(ErrorKind::InvalidData, err))?;
            text.push_str(&line);
            text.push('\n');
        }

        let file = self.file_for(chain_id);
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &file)
    }

    /// Path of one network's buffer.
    fn file_for(&self, chain_id: u64) -> PathBuf {
        self.dir.join(format!("{chain_id}{FILE_SUFFIX}"))
    }
}

/// Recognize `{chainId}-bridge-buffer.jsonl`; everything else is foreign (§4.1).
fn chain_id_from_path(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    name.strip_suffix(FILE_SUFFIX)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("smartnet-bridge-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The §4.2 example line, with the ellipses replaced by fully spelled-out hex.
    const DOC_LINE: &str = r#"{"chainId":315,"outChainId":56,"direction":"out","address":"0x1111111111111111111111111111111111111111","hash":"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","logIndex":0,"blockNr":2457,"blockHash":"0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","blockTs":1794700000,"from":"0x2222222222222222222222222222222222222222","instrument":"USDT","contractIn":"0x5555555555555555555555555555555555555555","contractOut":null,"amountIn":"1000000","decIn":6,"amountOut":"1000000000000000000","decOut":18,"stage":1,"txReq":"0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","txRel":null,"txBurn":null,"lastErr":null}"#;

    fn sample() -> Record {
        serde_json::from_str(DOC_LINE).expect("the documented line parses")
    }

    fn other_hash() -> B256 {
        "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".parse().expect("hash")
    }

    #[test]
    fn the_documented_line_shape_parses_and_round_trips() {
        let record = sample();
        assert_eq!(record.chain_id, 315);
        assert_eq!(record.out_chain_id, 56);
        assert_eq!(record.direction, Direction::Out);
        assert_eq!(record.log_index, 0);
        assert_eq!(record.stage, Stage::Picked);
        assert_eq!(record.amount_in, "1000000");
        assert_eq!(record.amount_out, "1000000000000000000");
        assert!(record.tx_rel.is_none() && record.tx_burn.is_none() && record.last_err.is_none());
        // Field names AND order stay exactly as §4.2 documents them.
        assert_eq!(serde_json::to_string(&record).expect("serialize"), DOC_LINE);
    }

    #[test]
    fn bad_lines_are_skipped_without_bricking_the_rest_of_the_file() {
        let dir = temp_dir("bad-lines");
        std::fs::create_dir_all(&dir).expect("dir");

        let good = sample();
        let second = Record { hash: other_hash(), ..sample() };
        let bad_amount = DOC_LINE.replace("\"amountIn\":\"1000000\"", "\"amountIn\":\"abc\"");
        let bad_stage = DOC_LINE.replace("\"stage\":1", "\"stage\":9");
        let wrong_chain = DOC_LINE.replace("\"chainId\":315", "\"chainId\":56");
        let good_line = serde_json::to_string(&good).expect("serialize");
        let second_line = serde_json::to_string(&second).expect("serialize");
        std::fs::write(
            dir.join("315-bridge-buffer.jsonl"),
            ["not json at all", &bad_stage, &bad_amount, &wrong_chain, &good_line, &second_line]
                .join("\n"),
        )
        .expect("write buffer");
        // A foreign file in the same folder is ignored.
        std::fs::write(dir.join("notes.txt"), "garbage").expect("foreign file");

        let store = RecordStore::open(&dir).expect("open");
        assert_eq!(store.len(), 2);
        assert!(store.contains(315, good.hash, 0));
        assert!(store.contains(315, second.hash, 0));

        // The next write rewrites wholesale: the skipped junk never comes back.
        let mut store = store;
        store.put(sample()).expect("put");
        let text = std::fs::read_to_string(dir.join("315-bridge-buffer.jsonl")).expect("read");
        assert_eq!(text.lines().count(), 2);
        assert!(!text.contains("not json at all"));
    }

    #[test]
    fn dedup_identity_is_chain_hash_log_index() {
        let dir = temp_dir("dedup");
        let mut store = RecordStore::open(&dir).expect("open");
        let base = sample();
        store.put(base.clone()).expect("put");

        // One transaction, several ERC-20 transfers: the log index separates them.
        let transfer_2 = Record { log_index: 1, ..base.clone() };
        store.put(transfer_2.clone()).expect("put");
        // The same hash on another chain is a different transfer.
        let other_chain = Record { chain_id: 56, ..base.clone() };
        store.put(other_chain).expect("put");

        assert_eq!(store.len(), 3);
        assert!(store.contains(315, base.hash, 0));
        assert!(store.contains(315, base.hash, 1));
        assert!(store.contains(56, base.hash, 0));
        assert!(!store.contains(315, base.hash, 2));
        assert!(!store.contains(56, base.hash, 1));
        assert_eq!(store.get(315, base.hash, 1), Some(&transfer_2));
    }

    #[test]
    fn a_record_leaves_only_at_48_hours_and_stage_four() {
        const HOUR: u64 = 60 * 60;
        let now = 1_800_000_000;
        let dir = temp_dir("prune");
        let mut store = RecordStore::open(&dir).expect("open");

        let mut old_done = sample();
        old_done.hash = other_hash();
        old_done.block_ts = now - 49 * HOUR;
        old_done.stage = Stage::Done;

        // Exactly 48 h is already 48 h old — it goes.
        let mut exact_done = sample();
        exact_done.hash = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
            .parse()
            .expect("hash");
        exact_done.block_ts = now - RETENTION_SECS;
        exact_done.stage = Stage::Done;

        let mut young_done = sample();
        young_done.hash = "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
            .parse()
            .expect("hash");
        young_done.block_ts = now - 47 * HOUR;
        young_done.stage = Stage::Done;

        // Unfinished records are never dropped by age — however old they get.
        let mut old_unfinished = sample();
        old_unfinished.hash = "0x1212121212121212121212121212121212121212121212121212121212121212"
            .parse()
            .expect("hash");
        old_unfinished.block_ts = now - 99 * HOUR;
        old_unfinished.stage = Stage::Built;

        for record in [&old_done, &exact_done, &young_done, &old_unfinished] {
            store.put(record.clone()).expect("put");
        }
        assert_eq!(store.prune(now).expect("prune"), 2);
        assert_eq!(store.len(), 2);
        assert!(!store.contains(315, old_done.hash, 0));
        assert!(!store.contains(315, exact_done.hash, 0));
        assert!(store.contains(315, young_done.hash, 0));
        assert!(store.contains(315, old_unfinished.hash, 0));
        assert_eq!(
            std::fs::read_to_string(dir.join("315-bridge-buffer.jsonl"))
                .expect("read")
                .lines()
                .count(),
            2
        );
    }

    #[test]
    fn every_change_rewrites_that_networks_file_wholesale_and_atomically() {
        let dir = temp_dir("atomic");
        let mut store = RecordStore::open(&dir).expect("open creates the directory");
        assert!(dir.is_dir());

        let mut first = sample();
        first.block_nr = 2457;
        let mut second = sample();
        second.hash = other_hash();
        second.block_nr = 2450;
        let mut third = sample();
        third.chain_id = 56;
        third.hash = "0x3434343434343434343434343434343434343434343434343434343434343434"
            .parse()
            .expect("hash");

        store.put(first).expect("put");
        store.put(second).expect("put");
        store.put(third).expect("put");

        let own = std::fs::read_to_string(dir.join("315-bridge-buffer.jsonl")).expect("read");
        let lines: Vec<&str> = own.lines().collect();
        assert_eq!(lines.len(), 2);
        // Sorted by block, independent of hash-map iteration order.
        assert!(lines[0].contains("\"blockNr\":2450"));
        assert!(lines[1].contains("\"blockNr\":2457"));
        assert_eq!(
            std::fs::read_to_string(dir.join("56-bridge-buffer.jsonl"))
                .expect("read")
                .lines()
                .count(),
            1
        );
        let leftovers: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp file may survive a rewrite");

        let reopened = RecordStore::open(&dir).expect("reopen");
        assert_eq!(reopened.len(), 3);
    }
}

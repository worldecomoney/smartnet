//! The three record types, one per sub-shard file, transcribed from `fjall4a.md` §3.2–3.4.
//!
//! Field names are the short lowercase keys the spec fixes (`b`, `h`, `ph`, …). Hex strings keep
//! their `0x` prefix *inside* the record (the "no 0x" rule applies to file paths only) and must
//! be lowercase when produced by the caller.
//!
//! **6B extension (operator-authorized divergence from `fjall4a.md` §3).** The spec's fixed
//! 9-field `b` and 15-field `txs` records are lossy projections: they cannot reconstruct the
//! consensus types the RPC layer serializes, so aged-out blocks would serve degraded JSON.
//! Per the operator's ruling, records store *the fields that produce the RPC JSON* (not RLP
//! bytes): `b` and `txs` are extended with every missing header/transaction field. There is no
//! separate `receipts.gz` — a transaction's receipt scalar (`st`, `gu`) is folded into its `txs`
//! record, `cumulative_gas_used` is derived from the running sum of `gu`, and logs live in
//! `logs`. The spec-fixed keys above are kept verbatim; extension keys are additionally
//! required.

use serde::{Deserialize, Serialize};

/// One withdrawal of a block's body (`b` extension), for reconstructing `eth_getBlockByNumber`
/// withdrawals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithdrawalRecord {
    /// Withdrawal index within the block.
    pub i: u64,
    /// Validator index.
    pub vi: u64,
    /// Recipient address.
    pub a: String,
    /// Amount in gwei.
    pub am: u64,
}

/// A `b` record: one line per block (`fjall4a.md` §3.2).
///
/// The first nine fields are the spec's fixed shape; everything after is the 6B full-fidelity
/// extension needed to rebuild `alloy_consensus::Header` (and the block body's withdrawals)
/// byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BtRecord {
    /// Block number.
    pub b: u64,
    /// Block hash.
    pub h: String,
    /// Parent hash.
    pub ph: String,
    /// Timestamp.
    pub ts: u64,
    /// Transaction count (0 for empty blocks).
    pub tc: u64,
    /// First transaction number (`TxNumber`) of this block's first transaction.
    ///
    /// 4.1 extension: with the payload in the file tier, the `b` record is the only
    /// per-block index, so it carries the block's first tx number — without it
    /// `block_body_indices` cannot be reconstructed for file-tier heights (the sibling
    /// `Blocks` row does not exist). For an empty block it is the number the next
    /// transaction would take, mirroring `StoredBlockBodyIndices`. `None` for records
    /// written before this field existed; the provider derives it from the first tx hash
    /// then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ft: Option<u64>,
    /// Gas used.
    pub gu: u64,
    /// Gas limit.
    pub gl: u64,
    /// Miner / proposer.
    pub m: String,
    /// Extra data.
    pub ex: String,
    /// Base fee per gas (EIP-1559), absent pre-London.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bf: Option<u64>,
    /// Difficulty (hex quantity; 0 post-merge).
    #[serde(default)]
    pub df: String,
    /// Ommers/uncles hash.
    #[serde(default)]
    pub oh: String,
    /// State root.
    #[serde(default)]
    pub sr: String,
    /// Transactions root.
    #[serde(default)]
    pub txr: String,
    /// Receipts root.
    #[serde(default)]
    pub rcr: String,
    /// Logs bloom (hex, 256 bytes).
    #[serde(default)]
    pub lb: String,
    /// Mix hash / prevRandao.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mh: Option<String>,
    /// Header nonce (B64 hex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nn: Option<String>,
    /// Withdrawals root (EIP-4895).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wr: Option<String>,
    /// Blob gas used (EIP-4844).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bgu: Option<u64>,
    /// Excess blob gas (EIP-4844).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ebg: Option<u64>,
    /// Parent beacon block root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pbr: Option<String>,
    /// Execution requests hash (EIP-7685).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rh: Option<String>,
    /// Block access list hash (EIP-7928).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bal: Option<String>,
    /// Slot number (EIP-7843).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sn: Option<u64>,
    /// Block withdrawals (body payload; `None` pre-Shanghai). Ommers are not stored: the node
    /// is engine-driven, so uncles are always empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wd: Option<Vec<WithdrawalRecord>>,
}

/// One access-list entry of a `txs` record (EIP-2930/1559/7702).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessListRecord {
    /// Touched address.
    pub a: String,
    /// Storage keys (hex) touched at that address.
    pub k: Vec<String>,
}

/// One signed authorization of a `txs` record (EIP-7702).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationRecord {
    /// Chain id the authorization is valid for (hex quantity — `Authorization.chain_id` is
    /// `U256`, so hex preserves full fidelity).
    pub cid: String,
    /// Delegated-to address.
    pub addr: String,
    /// Account nonce at signing time.
    pub n: u64,
    /// Signature y parity (0/1).
    pub yp: u8,
    /// Signature r (hex).
    pub r: String,
    /// Signature s (hex).
    pub s: String,
}

/// A `txs` record: one line per transaction (`fjall4a.md` §3.3).
///
/// The receipt's scalar facts (`st`, `gu`) are folded in — there is no separate `receipts.gz`;
/// the provider derives `cumulative_gas_used` from the running sum of `gu` and joins logs from
/// `logs`. The fields after `gu` rebuild the signed transaction (variant fields + signature)
/// byte-for-byte. The chain id is not stored: it is the network constant from config, and the
/// reader substitutes it on reconstruction.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TxRecord {
    /// Transaction hash.
    pub h: String,
    /// Block number.
    pub b: u64,
    /// Transaction index within the block.
    pub i: u64,
    /// From address (serialized as `"0"`).
    #[serde(rename = "0")]
    pub from: String,
    /// To address (recipient, substituted for ERC-20; serialized as `"1"`).
    #[serde(rename = "1")]
    pub to: String,
    /// Contract address (ERC-20 only, else `"0x0"`; serialized as `"2"`).
    #[serde(rename = "2")]
    pub contract: String,
    /// Tx type: `0x0` native, `0x1` ERC-20.
    pub t: String,
    /// RLP tx type (`0x0`, `0x2`, …).
    pub ty: String,
    /// Gas limit.
    pub gas: String,
    /// Status (receipt success).
    pub st: bool,
    /// Gas used by this transaction alone.
    pub gu: u64,
    /// Nonce of the sender account.
    pub n: u64,
    /// Calldata (`input`), hex.
    pub input: String,
    /// `max_fee_per_gas` (hex): the variant's fee cap — legacy/2930 return their gas price,
    /// 1559/4844/7702 the max fee. Always present (the trait accessor is not optional).
    pub mfp: String,
    /// `max_priority_fee_per_gas` (hex); `None` when the variant has no priority fee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtf: Option<String>,
    /// `max_fee_per_blob_gas` (hex, EIP-4844); `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mfb: Option<String>,
    /// Access list; `None` when the variant has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub al: Option<Vec<AccessListRecord>>,
    /// Blob versioned hashes (EIP-4844/7702); `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bh: Option<Vec<String>>,
    /// Signed authorization list (EIP-7702); `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Vec<AuthorizationRecord>>,
    /// Signature y parity (0/1).
    pub yp: u8,
    /// Signature r (hex).
    pub r: String,
    /// Signature s (hex).
    pub s: String,
    /// Native `tx.value` — always the real transferred wei, independent of the ERC-20
    /// substitution applied to `1`/`2`.
    pub nv: String,
}

/// The stub form of a `txs` record for a transaction that is not indexed but still included
/// (`fjall4a.md` §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxStubRecord {
    /// Transaction hash.
    pub h: String,
    /// Block number.
    pub b: u64,
    /// Transaction index within the block.
    pub i: u64,
    /// Always `"unsupported"`.
    pub st: String,
}

/// One log entry inside a `logs` block record (6B new file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    /// Block-global log index (order of emission within the block).
    pub i: u64,
    /// Transaction index within the block that emitted the log.
    pub x: u64,
    /// Emitting transaction hash.
    pub tx: String,
    /// Emitting contract address.
    pub a: String,
    /// Topics (hex); `t[0]` is the event signature.
    pub t: Vec<String>,
    /// ABI-encoded data (hex).
    pub d: String,
}

/// A `logs` record: one line per block that emitted logs (6B new file). Blocks without
/// logs get no line (same variable-count rule as `4626`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockLogsRecord {
    /// Block number.
    pub b: u64,
    /// Logs emitted by the block, in emission order.
    pub lg: Vec<LogRecord>,
}

/// One ERC-4626 event inside a `4626` record (`fjall4a.md` §3.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Erc4626Event {
    /// Vault contract address.
    pub addr: String,
    /// Topics array (`t[0]` = event signature).
    pub t: Vec<String>,
    /// Raw ABI-encoded data (undecoded; decode on read).
    pub d: String,
    /// Transaction hash.
    pub tx: String,
    /// Log index within the block.
    pub i: u64,
}

/// A `4626` record: one line per block that has ERC-4626 events (`fjall4a.md` §3.4).
/// Blocks without events get no line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Erc4626Record {
    /// Block number.
    pub b: u64,
    /// Events in this block.
    pub ev: Vec<Erc4626Event>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialized keys match the spec's short field names exactly.
    #[test]
    fn bt_record_uses_spec_keys() {
        let rec = BtRecord {
            b: 12345,
            h: "0xabc".into(),
            ph: "0xdef".into(),
            ts: 1749356528,
            tc: 5,
            gu: 21000,
            gl: 30000000,
            m: "0xminer".into(),
            ex: "0x".into(),
            ..Default::default()
        };
        let v: serde_json::Value = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["b"], 12345);
        assert_eq!(v["h"], "0xabc");
        assert_eq!(v["ph"], "0xdef");
        assert_eq!(v["ts"], 1749356528);
        assert_eq!(v["tc"], 5);
        assert_eq!(v["gu"], 21000);
        assert_eq!(v["gl"], 30000000);
        assert_eq!(v["m"], "0xminer");
        assert_eq!(v["ex"], "0x");
        assert!(v.get("bf").is_none(), "absent optional fields are skipped");
        assert!(v.get("wd").is_none());
        // 9 spec fields + the 6 mandatory extension hex strings (df/oh/sr/txr/rcr/lb);
        // Option-typed extensions are skipped when unset.
        assert_eq!(v.as_object().unwrap().len(), 15);
    }

    /// The from/to/contract addresses serialize under the numeric string keys "0"/"1"/"2".
    #[test]
    fn tx_record_uses_numeric_address_keys() {
        let rec = TxRecord {
            h: "0xh".into(),
            b: 1,
            i: 0,
            from: "0xa".into(),
            to: "0xb".into(),
            contract: "0x0".into(),
            t: "0x0".into(),
            ty: "0x2".into(),
            gas: "0x5208".into(),
            st: true,
            gu: 21_000,
            input: "0x".into(),
            mfp: "0x1".into(),
            r: "0x1".into(),
            s: "0x2".into(),
            nv: "0x0".into(),
            ..Default::default()
        };
        let v: serde_json::Value = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["0"], "0xa");
        assert_eq!(v["1"], "0xb");
        assert_eq!(v["2"], "0x0");
        assert_eq!(v["st"], true, "status always present");
        assert_eq!(v["gu"], 21_000);
        assert_eq!(v["mfp"], "0x1");
        assert_eq!(v["nv"], "0x0");
        assert!(v.get("al").is_none() && v.get("auth").is_none() && v.get("cid").is_none());
        // 15 spec fields + 5 mandatory extension keys (n/input/mfp/yp/r/s/nv minus the
        // removed v/cd/gp/fee/cid); Option extensions (mtf/mfb/al/bh/auth) skip when unset.
        assert_eq!(v.as_object().unwrap().len(), 18);
    }

    /// Logs records use the short keys and omit empty logs blocks correctly.
    #[test]
    fn logs_record_shape() {
        let logs = BlockLogsRecord {
            b: 7,
            lg: vec![LogRecord {
                i: 0,
                x: 3,
                tx: "0xtx".into(),
                a: "0xaa".into(),
                t: vec!["0xsig".into()],
                d: "0x".into(),
            }],
        };
        let v: serde_json::Value = serde_json::to_value(&logs).unwrap();
        assert_eq!(v["b"], 7);
        assert_eq!(v["lg"][0]["i"], 0);
        assert_eq!(v["lg"][0]["x"], 3);
        assert_eq!(v["lg"][0]["tx"], "0xtx");
        assert_eq!(v["lg"][0]["a"], "0xaa");
    }

    /// The stub record serializes to the four-field shape the spec shows.
    #[test]
    fn tx_stub_matches_spec_shape() {
        let stub = TxStubRecord { h: "0xh".into(), b: 2, i: 3, st: "unsupported".into() };
        let s = serde_json::to_string(&stub).unwrap();
        assert_eq!(s, "{\"h\":\"0xh\",\"b\":2,\"i\":3,\"st\":\"unsupported\"}");
    }

    /// The 4626 record nests events under "ev" with addr/t/d/tx/i.
    #[test]
    fn erc4626_record_nests_events() {
        let rec = Erc4626Record {
            b: 12345,
            ev: vec![Erc4626Event {
                addr: "0xvault".into(),
                t: vec!["0xsig".into(), "0xa".into(), "0xb".into()],
                d: "0xdead".into(),
                tx: "0xtx".into(),
                i: 0,
            }],
        };
        let v: serde_json::Value = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["b"], 12345);
        assert_eq!(v["ev"][0]["addr"], "0xvault");
        assert_eq!(v["ev"][0]["t"][0], "0xsig");
        assert_eq!(v["ev"][0]["d"], "0xdead");
        assert_eq!(v["ev"][0]["tx"], "0xtx");
        assert_eq!(v["ev"][0]["i"], 0);
    }
}

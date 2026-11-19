//! Extraction of file-tier records from block header, transaction, and receipt data.
//!
//! The extractor is generic over the alloy consensus traits so `file-tier` needs only
//! `alloy-consensus` + `alloy-primitives` — the engine unpacks its `ExecutedBlock` into the
//! header/txs/senders/receipts pieces and calls [`extract_block`].
//!
//! Classification follows the `archid.go` mechanism, with the spec (`fjall4a.md`) winning on
//! field shape: every tx gets a full record — native by default, ERC-20-enhanced when the input
//! starts with the `transfer(address,uint256)` selector and a matching `Transfer` log is found.
//! ERC-4626 events are filtered on the `Deposit`/`Withdraw` topic0 hashes and grouped one record
//! per block-with-events.
//!
//! **6B:** records additionally carry every field needed to rebuild the consensus types
//! (see [`crate::records`]); logs are extracted into their own record list.

use alloy_consensus::{
    transaction::{RlpEcdsaEncodableTx, TxHashRef},
    BlockHeader, EthereumReceipt, Signed, Transaction, TxLegacy, TxReceipt, TxType, Typed2718,
};
use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{b256, Address, Log, Signature, B256};
use smartnet_primitives_traits::hexfmt::prefixed;

use crate::records::{
    AccessListRecord, AuthorizationRecord, BlockLogsRecord, BtRecord, Erc4626Event, Erc4626Record,
    LogRecord, TxRecord, WithdrawalRecord,
};

/// ERC-20 `transfer(address,uint256)` selector.
const ERC20_TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

/// `keccak256("Transfer(address,address,uint256)")`.
const ERC20_TRANSFER_TOPIC: B256 =
    b256!("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

/// `keccak256("Deposit(address,address,uint256,uint256)")`.
const ERC4626_DEPOSIT_TOPIC: B256 =
    b256!("0xdcbc1c05240f31ff3ad067ef1ee35ce4997762752e3a095284754544f4c709d7");

/// `keccak256("Withdraw(address,address,address,uint256,uint256)")`.
const ERC4626_WITHDRAW_TOPIC: B256 =
    b256!("0xfbde797d201c681b91056529119e0b02407c7bb96a4a2c75c01fc9667232c8db");

/// Signature access for the two signed-transaction shapes the node uses (`Signed<T>` and the
/// typed envelope). alloy 2.x exposes `signature()` only as an inherent method, so this local
/// trait bridges it for the generic extractor.
pub trait SignatureAccess {
    /// The transaction's signature.
    fn tx_signature(&self) -> &Signature;
}

impl<T> SignatureAccess for alloy_consensus::Signed<T, Signature> {
    fn tx_signature(&self) -> &Signature {
        self.signature()
    }
}

impl<T: Typed2718 + RlpEcdsaEncodableTx> SignatureAccess
    for alloy_consensus::EthereumTxEnvelope<T>
{
    fn tx_signature(&self) -> &Signature {
        self.signature()
    }
}

/// The record types extracted from one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRecords {
    /// Block-header record (`b`).
    pub bt: BtRecord,
    /// Transaction records (`txs`); one per tx, in block order. The receipt scalar (`st`,
    /// `gu`) is folded into each record.
    pub txs: Vec<TxRecord>,
    /// The block's logs (`logs`); `None` when the block emitted no logs.
    pub logs: Option<BlockLogsRecord>,
    /// ERC-4626 event record (`4626`); `None` when the block has no matching events.
    pub erc4626: Option<Erc4626Record>,
}

/// Extracts the file-tier records for one block from its header, transactions, senders,
/// receipts, and withdrawals.
///
/// `txs` and `senders` are indexed together; `receipts` is indexed by tx position and may be
/// shorter (a missing receipt skips that tx's record). Per-tx gas used is derived from the
/// cumulative-gas delta between consecutive receipts. `withdrawals` is the block body's
/// withdrawals (`None` pre-Shanghai).
pub fn extract_block<H, Tx, R>(
    header: &H,
    block_hash: B256,
    txs: &[Tx],
    senders: &[Address],
    receipts: &[R],
    withdrawals: Option<&[Withdrawal]>,
) -> BlockRecords
where
    H: BlockHeader,
    Tx: Transaction + TxHashRef + Typed2718 + SignatureAccess,
    R: TxReceipt<Log = Log> + Typed2718,
{
    let block_number = header.number();

    let wd = withdrawals.map(|ws| {
        ws.iter()
            .map(|w| WithdrawalRecord {
                i: w.index,
                vi: w.validator_index,
                a: w.address.to_string(),
                am: w.amount,
            })
            .collect()
    });

    let bt = BtRecord {
        b: block_number,
        h: block_hash.to_string(),
        ph: header.parent_hash().to_string(),
        ts: header.timestamp(),
        tc: txs.len() as u64,
        // Stamped by the persistence layer, which owns the tx counter (`next_tx_number`).
        ft: None,
        gu: header.gas_used(),
        gl: header.gas_limit(),
        m: header.beneficiary().to_string(),
        ex: header.extra_data().to_string(),
        bf: header.base_fee_per_gas(),
        df: format!("0x{:x}", header.difficulty()),
        oh: header.ommers_hash().to_string(),
        sr: header.state_root().to_string(),
        txr: header.transactions_root().to_string(),
        rcr: header.receipts_root().to_string(),
        lb: header.logs_bloom().to_string(),
        mh: header.mix_hash().map(|b| b.to_string()),
        nn: header.nonce().map(|b| b.to_string()),
        wr: header.withdrawals_root().map(|b| b.to_string()),
        bgu: header.blob_gas_used(),
        ebg: header.excess_blob_gas(),
        pbr: header.parent_beacon_block_root().map(|b| b.to_string()),
        rh: header.requests_hash().map(|b| b.to_string()),
        bal: header.block_access_list_hash().map(|b| b.to_string()),
        sn: header.slot_number(),
        wd,
    };

    let mut tx_records = Vec::with_capacity(txs.len());
    let mut block_logs: Vec<LogRecord> = Vec::new();
    let mut prev_cumulative = 0u64;
    let mut log_index = 0u64;
    let mut erc4626_events = Vec::new();

    for (i, (tx, sender)) in txs.iter().zip(senders.iter()).enumerate() {
        let Some(receipt) = receipts.get(i) else { continue };

        let cumulative = receipt.cumulative_gas_used();
        let gas_used = cumulative - prev_cumulative;
        prev_cumulative = cumulative;

        tx_records.push(extract_tx(tx, *sender, block_number, i as u64, receipt, gas_used));

        for log in receipt.logs() {
            let topics = log.topics();
            block_logs.push(LogRecord {
                i: log_index,
                x: i as u64,
                tx: tx.tx_hash().to_string(),
                a: log.address.to_string(),
                t: topics.iter().map(|t| t.to_string()).collect(),
                d: log.data.data.to_string(),
            });
            if let Some(&topic0) = topics.first() &&
                (topic0 == ERC4626_DEPOSIT_TOPIC || topic0 == ERC4626_WITHDRAW_TOPIC)
            {
                erc4626_events.push(Erc4626Event {
                    addr: log.address.to_string(),
                    t: topics.iter().map(|t| t.to_string()).collect(),
                    d: log.data.data.to_string(),
                    tx: tx.tx_hash().to_string(),
                    i: log_index,
                });
            }
            log_index += 1;
        }
    }

    let logs = if block_logs.is_empty() {
        None
    } else {
        Some(BlockLogsRecord { b: block_number, lg: block_logs })
    };

    let erc4626 = if erc4626_events.is_empty() {
        None
    } else {
        Some(Erc4626Record { b: block_number, ev: erc4626_events })
    };

    BlockRecords { bt, txs: tx_records, logs, erc4626 }
}

/// Extracts the file-tier records for a header-only block (the genesis block): one `b` record
/// and no `txs`/`logs`/`4626`. The header is used verbatim, so its parent hash stays whatever
/// the genesis declared (zero for a normal genesis), and `block_hash` is stored as-is.
pub fn extract_empty_block<H: BlockHeader>(header: &H, block_hash: B256) -> BlockRecords {
    extract_block::<H, Signed<TxLegacy>, EthereumReceipt<TxType, Log>>(
        header,
        block_hash,
        &[],
        &[],
        &[],
        None,
    )
}

/// Extracts the file-tier records for a **calibri\* genesis block that carries the
/// pre-fund records as real transactions** (GENESIS.md): the alloc is the single source
/// of truth for opening balances — the tx records are LISTINGS (`from = 0x0`, zero
/// signature, never executed, never metered: `gasUsed` stays 0, no receipts to speak
/// of beyond the success stubs). The txs are the chainspec-derived deterministic list;
/// `from` is recorded as the zero address and `st` is always `true`.
pub fn genesis_block_records<H: BlockHeader>(
    header: &H,
    block_hash: B256,
    txs: &[crate::reader::TransactionSigned],
) -> BlockRecords {
    let records = extract_empty_block(header, block_hash);
    let mut bt = records.bt;
    bt.tc = txs.len() as u64;
    // The persistence layer owns the tx counter: genesis starts it at zero.
    bt.ft = Some(0);
    let txs_records = txs
        .iter()
        .enumerate()
        .map(|(idx, tx)| crate::records::TxRecord {
            h: prefixed(&tx.tx_hash()),
            b: header.number(),
            i: idx as u64,
            from: prefixed(&Address::ZERO),
            to: tx.to().map(|to| prefixed(&to)).unwrap_or_else(|| prefixed(&Address::ZERO)),
            contract: "0x0".to_string(),
            t: "0x0".to_string(),
            ty: "0x0".to_string(),
            gas: prefixed(&tx.gas_limit()),
            st: true,
            gu: 0,
            n: tx.nonce(),
            input: "0x".to_string(),
            mfp: prefixed(&tx.max_fee_per_gas()),
            mtf: None,
            mfb: None,
            al: None,
            bh: None,
            auth: None,
            yp: 0,
            r: "0x0".to_string(),
            s: "0x0".to_string(),
            nv: prefixed(&tx.value()),
        })
        .collect();
    BlockRecords { bt, txs: txs_records, logs: None, erc4626: None }
}

/// Builds one full [`TxRecord`] for a single transaction.
///
/// ERC-20 enhancement: when the input starts with `0xa9059cbb` and the receipt contains a
/// `Transfer` log with at least three topics, the record's fields are rewritten — `t` becomes
/// `0x1`, `2` holds the token contract (`to`), and `1` holds the real recipient (`topics[2]`).
/// Otherwise the record is native: `t` = `0x0`, `2` = `0x0`, `1` = `to`. Reconstruction always
/// uses `nv` (the true native value) and recovers `to` from `2`/`1` per `t`.
fn extract_tx<Tx, R>(
    tx: &Tx,
    sender: Address,
    block_number: u64,
    index: u64,
    receipt: &R,
    gas_used: u64,
) -> TxRecord
where
    Tx: Transaction + TxHashRef + Typed2718 + SignatureAccess,
    R: TxReceipt<Log = Log>,
{
    let to = tx.to();
    let input = tx.input();
    let is_erc20 = input.starts_with(&ERC20_TRANSFER_SELECTOR);

    let (t, recipient, contract) = if is_erc20 {
        let mut real_recipient = None;
        for log in receipt.logs() {
            let topics = log.topics();
            if topics.len() >= 3 && topics[0] == ERC20_TRANSFER_TOPIC {
                real_recipient = Some(Address::from_word(topics[2]));
                break;
            }
        }
        let recipient = real_recipient.unwrap_or(Address::ZERO);
        let token = to.unwrap_or(Address::ZERO);
        ("0x1".to_string(), recipient.to_string(), token.to_string())
    } else {
        let recipient = to.map(|a| a.to_string()).unwrap_or_else(|| "0x0".to_string());
        ("0x0".to_string(), recipient, "0x0".to_string())
    };

    let sig = tx.tx_signature();

    let access_list = tx.access_list().map(|al| {
        al.iter()
            .map(|item| AccessListRecord {
                a: item.address.to_string(),
                k: item.storage_keys.iter().map(|k| k.to_string()).collect(),
            })
            .collect()
    });
    let blob_hashes =
        tx.blob_versioned_hashes().map(|hs| hs.iter().map(|h| h.to_string()).collect());
    let auth = tx.authorization_list().map(|auths| {
        auths
            .iter()
            .map(|a| AuthorizationRecord {
                cid: format!("0x{:x}", a.chain_id),
                addr: a.address.to_string(),
                n: a.nonce,
                yp: a.y_parity(),
                r: format!("0x{:x}", a.r()),
                s: format!("0x{:x}", a.s()),
            })
            .collect()
    });

    TxRecord {
        h: tx.tx_hash().to_string(),
        b: block_number,
        i: index,
        from: sender.to_string(),
        to: recipient,
        contract,
        t,
        ty: format!("0x{:x}", tx.ty()),
        gas: format!("0x{:x}", tx.gas_limit()),
        st: receipt.status(),
        gu: gas_used,
        n: tx.nonce(),
        input: input.to_string(),
        mfp: format!("0x{:x}", tx.max_fee_per_gas()),
        mtf: tx.max_priority_fee_per_gas().map(|p| format!("0x{p:x}")),
        mfb: tx.max_fee_per_blob_gas().map(|p| format!("0x{p:x}")),
        al: access_list,
        bh: blob_hashes,
        auth,
        yp: sig.v() as u8,
        r: format!("0x{:x}", sig.r()),
        s: format!("0x{:x}", sig.s()),
        nv: format!("0x{:x}", tx.value()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{EthereumReceipt, Header, Signed, TxLegacy, TxType};
    use alloy_primitives::{Bytes, TxKind, U256};

    const BASE_FEE: u64 = 1_000_000_000;

    fn test_header(number: u64, gas_used: u64) -> Header {
        Header {
            number,
            gas_limit: 30_000_000,
            gas_used,
            timestamp: 1_700_000_000,
            base_fee_per_gas: Some(BASE_FEE),
            ..Default::default()
        }
    }

    fn dummy_signed<T>(tx: T, hash: B256) -> Signed<T> {
        let sig = Signature::new(U256::from(1), U256::from(2), false);
        Signed::new_unchecked(tx, sig, hash)
    }

    fn legacy_tx(to: Address, value: U256, gas_price: u128, gas_limit: u64) -> TxLegacy {
        TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price,
            gas_limit,
            to: TxKind::Call(to),
            value,
            input: Bytes::new(),
        }
    }

    fn ok_receipt(cumulative_gas: u64, logs: Vec<Log>) -> EthereumReceipt<TxType, Log> {
        EthereumReceipt {
            tx_type: TxType::Legacy,
            success: true,
            cumulative_gas_used: cumulative_gas,
            logs,
        }
    }

    /// A simple native transfer: all spec fields populated with expected values; fee is decimal.
    #[test]
    fn extracts_native_transfer() {
        let header = test_header(100, 21_000);
        let sender = Address::repeat_byte(0xaa);
        let to = Address::repeat_byte(0xbb);
        let tx = dummy_signed(
            legacy_tx(to, U256::from(10u64), 2_000_000_000, 21_000),
            B256::repeat_byte(0x11),
        );
        let receipts = vec![ok_receipt(21_000, vec![])];

        let out = extract_block(
            &header,
            B256::repeat_byte(0xcc),
            std::slice::from_ref(&tx),
            &[sender],
            &receipts,
            None,
        );

        assert_eq!(out.bt.b, 100);
        assert_eq!(out.bt.tc, 1);
        assert_eq!(out.bt.gu, 21_000);
        assert_eq!(out.bt.gl, 30_000_000);
        assert!(out.erc4626.is_none());

        let r = &out.txs[0];
        assert_eq!(r.b, 100);
        assert_eq!(r.i, 0);
        assert_eq!(r.from, sender.to_string());
        assert_eq!(r.to, to.to_string());
        assert_eq!(r.contract, "0x0");
        assert_eq!(r.nv, "0xa", "native value");
        assert_eq!(r.t, "0x0");
        assert_eq!(r.ty, "0x0");
        assert_eq!(r.gas, "0x5208");
        assert!(r.st);
        assert_eq!(r.gu, 21_000);
        // 6B extensions.
        assert_eq!(r.n, 0);
        assert_eq!(r.input, "0x");
        assert_eq!(r.mfp, "0x77359400", "2 gwei");
        assert_eq!(r.mtf, None);
        assert_eq!(r.yp, 0);
        assert_eq!(r.r, "0x1");
        assert_eq!(r.s, "0x2");
        assert!(r.al.is_none() && r.bh.is_none() && r.auth.is_none());
    }

    /// An ERC-20 transfer: t=0x1, contract=to, recipient from Transfer log topics[2], amount
    /// from log data; nv keeps the true native value.
    #[test]
    fn extracts_erc20_transfer() {
        let header = test_header(200, 65_000);
        let sender = Address::repeat_byte(0xaa);
        let token = Address::repeat_byte(0xbb);
        let real_recipient = Address::repeat_byte(0xdd);

        let mut input = vec![0xa9, 0x05, 0x9c, 0xbb];
        input.extend_from_slice(&[0u8; 64]);
        let tx = dummy_signed(
            TxLegacy {
                chain_id: Some(1),
                nonce: 0,
                gas_price: 1_000_000_000,
                gas_limit: 65_000,
                to: TxKind::Call(token),
                value: U256::from(7u64),
                input: Bytes::from(input),
            },
            B256::repeat_byte(0x22),
        );

        let transfer_log = Log::new_unchecked(
            token,
            vec![
                ERC20_TRANSFER_TOPIC,
                B256::repeat_byte(0xaa),
                B256::from(real_recipient.into_word()),
            ],
            alloy_primitives::Bytes::copy_from_slice(&{
                let mut d = [0u8; 32];
                d[31] = 0x2a; // 42
                d
            }),
        );
        let receipts = vec![ok_receipt(65_000, vec![transfer_log])];

        let out = extract_block(
            &header,
            B256::repeat_byte(0xcc),
            std::slice::from_ref(&tx),
            &[sender],
            &receipts,
            None,
        );
        let r = &out.txs[0];

        assert_eq!(r.t, "0x1");
        assert_eq!(r.contract, token.to_string());
        assert_eq!(r.to, real_recipient.to_string());
        assert_eq!(r.nv, "0x7", "nv preserves the true native value under substitution");
        assert!(r.st);
        assert_eq!(r.gu, 65_000);
    }

    /// A contract deployment (to=None): 1 and 2 are "0x0", t is 0x0.
    #[test]
    fn deployment_tx_uses_zero_addresses() {
        let header = test_header(1, 50_000);
        let sender = Address::repeat_byte(0xaa);
        let tx = dummy_signed(
            TxLegacy {
                chain_id: Some(1),
                nonce: 0,
                gas_price: 1_000_000_000,
                gas_limit: 50_000,
                to: TxKind::Create,
                value: U256::ZERO,
                input: Bytes::from(vec![0x60, 0x80]),
            },
            B256::repeat_byte(0x33),
        );
        let receipts = vec![ok_receipt(50_000, vec![])];

        let out = extract_block(
            &header,
            B256::repeat_byte(0xcc),
            std::slice::from_ref(&tx),
            &[sender],
            &receipts,
            None,
        );
        let r = &out.txs[0];

        assert_eq!(r.to, "0x0");
        assert_eq!(r.contract, "0x0");
        assert_eq!(r.t, "0x0");
        assert_eq!(r.input, "0x6080");
    }

    /// Logs records are extracted alongside the tx records (receipt scalar is folded into txs).
    #[test]
    fn extracts_logs_records() {
        let header = test_header(10, 42_000);
        let sender = Address::repeat_byte(0xaa);
        let to = Address::repeat_byte(0xbb);
        let tx0 =
            dummy_signed(legacy_tx(to, U256::ZERO, 1_000_000_000, 30_000), B256::repeat_byte(0x11));
        let tx1 =
            dummy_signed(legacy_tx(to, U256::ZERO, 1_000_000_000, 30_000), B256::repeat_byte(0x22));

        let log0 = Log::new_unchecked(
            to,
            vec![b256!("0x1111111111111111111111111111111111111111111111111111111111111111")],
            Bytes::from(vec![0xde, 0xad]),
        );
        let log1 = Log::new_unchecked(
            to,
            vec![b256!("0x2222222222222222222222222222222222222222222222222222222222222222")],
            Bytes::from(vec![0xbe, 0xef]),
        );
        let receipts = vec![ok_receipt(30_000, vec![log0]), ok_receipt(42_000, vec![log1])];

        let out = extract_block(
            &header,
            B256::repeat_byte(0xcc),
            &[tx0, tx1],
            &[sender, sender],
            &receipts,
            None,
        );

        // Receipt scalar folded into the tx records: per-tx gas from the cumulative delta.
        assert_eq!(out.txs.len(), 2);
        assert_eq!(out.txs[0].gu, 30_000);
        assert_eq!(out.txs[1].gu, 12_000);
        assert!(out.txs[0].st && out.txs[1].st);

        // Logs: block record with block-global index, tx index and tx hash.
        let logs = out.logs.expect("block emitted logs");
        assert_eq!(logs.b, 10);
        assert_eq!(logs.lg.len(), 2);
        assert_eq!(logs.lg[0].i, 0);
        assert_eq!(logs.lg[0].x, 0);
        assert_eq!(logs.lg[0].tx, out.txs[0].h);
        assert_eq!(logs.lg[1].i, 1);
        assert_eq!(logs.lg[1].x, 1);
        assert_eq!(logs.lg[1].tx, out.txs[1].h);
        assert_eq!(logs.lg[1].d, "0xbeef");
    }

    /// A block without logs has no logs record; a failed receipt yields st=false.
    #[test]
    fn no_logs_record_and_failed_status() {
        let header = test_header(1, 21_000);
        let sender = Address::repeat_byte(0xaa);
        let tx = dummy_signed(
            legacy_tx(Address::repeat_byte(0xbb), U256::ZERO, 1_000_000_000, 21_000),
            B256::repeat_byte(0x88),
        );
        let receipts = vec![EthereumReceipt::<TxType, Log> {
            tx_type: TxType::Legacy,
            success: false,
            cumulative_gas_used: 21_000,
            logs: vec![],
        }];

        let out = extract_block(
            &header,
            B256::repeat_byte(0xcc),
            std::slice::from_ref(&tx),
            &[sender],
            &receipts,
            None,
        );
        assert!(!out.txs[0].st);
        assert!(out.logs.is_none());
    }

    /// Withdrawals are carried on the bt record.
    #[test]
    fn withdrawals_go_to_bt() {
        let header = test_header(5, 0);
        let wd = vec![Withdrawal {
            index: 7,
            validator_index: 42,
            address: Address::repeat_byte(0xee),
            amount: 1_000_000,
        }];
        let out = extract_block(
            &header,
            B256::repeat_byte(0xcc),
            &[] as &[Signed<TxLegacy>],
            &[],
            &[] as &[EthereumReceipt<TxType, Log>],
            Some(&wd),
        );
        let rec = out.bt.wd.expect("withdrawals present");
        assert_eq!(rec.len(), 1);
        assert_eq!(rec[0].i, 7);
        assert_eq!(rec[0].vi, 42);
        assert_eq!(rec[0].a, Address::repeat_byte(0xee).to_string());
        assert_eq!(rec[0].am, 1_000_000);
    }

    /// ERC-4626 Deposit and Withdraw logs are collected into one block-level record with a
    /// block-global log index; non-matching logs are skipped but still advance the index.
    #[test]
    fn extracts_erc4626_events() {
        let header = test_header(300, 100_000);
        let sender = Address::repeat_byte(0xaa);
        let vault = Address::repeat_byte(0xee);

        let tx0 = dummy_signed(
            legacy_tx(vault, U256::ZERO, 1_000_000_000, 50_000),
            B256::repeat_byte(0x44),
        );
        let tx1 = dummy_signed(
            legacy_tx(Address::repeat_byte(0xbb), U256::ZERO, 1_000_000_000, 50_000),
            B256::repeat_byte(0x55),
        );
        let tx0_hash = tx0.tx_hash().to_string();
        let tx1_hash = tx1.tx_hash().to_string();

        let unrelated = Log::new_unchecked(
            vault,
            vec![b256!("0x1111111111111111111111111111111111111111111111111111111111111111")],
            Bytes::new(),
        );
        let deposit = Log::new_unchecked(
            vault,
            vec![ERC4626_DEPOSIT_TOPIC, B256::repeat_byte(0xaa), B256::repeat_byte(0xbb)],
            Bytes::from(vec![0u8; 64]),
        );
        let withdraw = Log::new_unchecked(
            vault,
            vec![
                ERC4626_WITHDRAW_TOPIC,
                B256::repeat_byte(0xaa),
                B256::repeat_byte(0xbb),
                B256::repeat_byte(0xcc),
            ],
            Bytes::from(vec![1u8; 64]),
        );

        // tx0 has [unrelated, deposit]; tx1 has [withdraw].
        let receipts =
            vec![ok_receipt(50_000, vec![unrelated, deposit]), ok_receipt(100_000, vec![withdraw])];

        let out = extract_block(
            &header,
            B256::repeat_byte(0xcc),
            &[tx0, tx1],
            &[sender, sender],
            &receipts,
            None,
        );

        let rec = out.erc4626.expect("events present");
        assert_eq!(rec.b, 300);
        assert_eq!(rec.ev.len(), 2);
        // unrelated at block-global index 0 is skipped; deposit at 1, withdraw at 2.
        assert_eq!(rec.ev[0].i, 1);
        assert_eq!(rec.ev[0].addr, vault.to_string());
        assert_eq!(rec.ev[0].t.len(), 3);
        assert_eq!(rec.ev[0].tx, tx0_hash);
        assert_eq!(rec.ev[1].i, 2);
        assert_eq!(rec.ev[1].t.len(), 4);
        assert_eq!(rec.ev[1].tx, tx1_hash);
        // The same logs are also in the block logs record with matching global indices.
        let logs = out.logs.expect("logs present");
        assert_eq!(logs.lg.len(), 3);
        assert_eq!(logs.lg[1].i, 1);
        assert_eq!(logs.lg[2].i, 2);
    }

    /// A block with no txs: bt has tc=0, no tx/receipt records, no logs/4626 record.
    #[test]
    fn empty_block() {
        let header = test_header(0, 0);
        let out = extract_block(
            &header,
            B256::repeat_byte(0xcc),
            &[] as &[Signed<TxLegacy>],
            &[],
            &[] as &[EthereumReceipt<TxType, Log>],
            None,
        );
        assert_eq!(out.bt.tc, 0);
        assert!(out.txs.is_empty());
        assert!(out.logs.is_none());
        assert!(out.erc4626.is_none());
    }

    /// Per-tx gas used is the cumulative delta, not the cumulative value itself.
    #[test]
    fn cumulative_gas_delta() {
        let header = test_header(10, 100_000);
        let sender = Address::repeat_byte(0xaa);
        let to = Address::repeat_byte(0xbb);

        let tx0 =
            dummy_signed(legacy_tx(to, U256::ZERO, 1_000_000_000, 60_000), B256::repeat_byte(0x66));
        let tx1 =
            dummy_signed(legacy_tx(to, U256::ZERO, 1_000_000_000, 60_000), B256::repeat_byte(0x77));

        let receipts = vec![ok_receipt(60_000, vec![]), ok_receipt(100_000, vec![])];

        let out = extract_block(
            &header,
            B256::repeat_byte(0xcc),
            &[tx0, tx1],
            &[sender, sender],
            &receipts,
            None,
        );

        assert_eq!(out.txs[0].gu, 60_000);
        assert_eq!(out.txs[1].gu, 40_000);
    }

    /// A tx with the ERC-20 selector but no valid Transfer log: classification is by selector
    /// alone (archid mechanism), so t=0x1 and contract=to; recipient/amount stay zeroed because
    /// the log scan found nothing.
    #[test]
    fn erc20_selector_without_transfer_log_zeroes_fields() {
        let header = test_header(1, 21_000);
        let sender = Address::repeat_byte(0xaa);
        let token = Address::repeat_byte(0xbb);

        let mut input = vec![0xa9, 0x05, 0x9c, 0xbb];
        input.extend_from_slice(&[0u8; 64]);
        let tx = dummy_signed(
            TxLegacy {
                chain_id: Some(1),
                nonce: 0,
                gas_price: 1_000_000_000,
                gas_limit: 21_000,
                to: TxKind::Call(token),
                value: U256::from(7u64),
                input: Bytes::from(input),
            },
            B256::repeat_byte(0x99),
        );

        // Transfer topic but only two topics — not enough to extract a recipient.
        let short_log = Log::new_unchecked(
            token,
            vec![ERC20_TRANSFER_TOPIC, B256::repeat_byte(0xaa)],
            Bytes::new(),
        );
        let receipts = vec![ok_receipt(21_000, vec![short_log])];

        let out = extract_block(
            &header,
            B256::repeat_byte(0xcc),
            std::slice::from_ref(&tx),
            &[sender],
            &receipts,
            None,
        );
        let r = &out.txs[0];

        // Selector match alone classifies as ERC-20 (archid); missing log zeroes 1.
        assert_eq!(r.t, "0x1");
        assert_eq!(r.contract, token.to_string());
        assert_eq!(r.to, Address::ZERO.to_string());
        assert_eq!(r.nv, "0x7", "nv stays the true native value");
    }
}

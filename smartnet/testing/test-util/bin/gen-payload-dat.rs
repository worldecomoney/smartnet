//! Maintainer-only: write numbered probe payloads to a directory.
//! Not used at test time — run once, commit the `.dat` files, iterate by adding more.
//!
//!   cargo run -p smartnet-test-util --bin gen-payload-dat -- payload-data

use alloy_consensus::{Block, BlockBody, Header, TxEnvelope, EMPTY_OMMER_ROOT_HASH};
use alloy_primitives::{b256, Address, B256, U256};
use alloy_rlp::Encodable;
use std::path::Path;

const EMPTY_TRIE: B256 =
    b256!("0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421");

/// `SmartNet` wemblockchain.json genesis block hash (chain 315).
const DEFAULT_PARENT: B256 =
    b256!("0x550d835eac84f4106f46972727cd8e117d23d352684d44a673a638091cf9ad00");

struct Sample {
    n: u32,
    note: &'static str,
    build: fn(B256) -> Vec<u8>,
}

fn encode_block(block: &Block<TxEnvelope>) -> Vec<u8> {
    let mut out = Vec::new();
    block.encode(&mut out);
    out
}

fn child(
    parent: B256,
    extra_len: usize,
    extra_fill: u8,
    seal_fill: u8,
    mut header_patch: impl FnMut(&mut Header),
) -> Vec<u8> {
    let mut extra = vec![extra_fill; extra_len];
    if extra_len >= 65 {
        extra[extra_len - 65..].fill(seal_fill);
    }
    let mut header = Header {
        parent_hash: parent,
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        beneficiary: Address::repeat_byte(0x42),
        state_root: EMPTY_TRIE,
        transactions_root: EMPTY_TRIE,
        receipts_root: EMPTY_TRIE,
        logs_bloom: Default::default(),
        difficulty: U256::from(2u64),
        number: 1,
        gas_limit: 30_000_000,
        gas_used: 0,
        timestamp: 1,
        extra_data: extra.into(),
        mix_hash: B256::ZERO,
        nonce: Default::default(),
        base_fee_per_gas: None,
        withdrawals_root: None,
        blob_gas_used: None,
        excess_blob_gas: None,
        parent_beacon_block_root: None,
        requests_hash: None,
        block_access_list_hash: None,
        slot_number: None,
    };
    header_patch(&mut header);
    encode_block(&Block::new(
        header,
        BlockBody { transactions: vec![], ommers: vec![], withdrawals: None },
    ))
}

fn main() -> eyre::Result<()> {
    let out_dir = std::env::args().nth(1).unwrap_or_else(|| "payload-data".into());
    let parent =
        std::env::var("PARENT_HASH").ok().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_PARENT);

    std::fs::create_dir_all(&out_dir)?;

    let samples: &[Sample] = &[
        Sample {
            n: 1,
            note: "extra_data 32 (post-merge cap probe)",
            build: |p| child(p, 32, 0, 0, |_| {}),
        },
        Sample {
            n: 2,
            note: "extra_data 97 fake seal (runtime shape)",
            build: |p| child(p, 97, 0, 0xbb, |_| {}),
        },
        Sample { n: 3, note: "extra_data 0", build: |p| child(p, 0, 0, 0, |_| {}) },
        Sample { n: 4, note: "extra_data 33", build: |p| child(p, 33, 0, 0, |_| {}) },
        Sample { n: 5, note: "extra_data 65 seal-only", build: |p| child(p, 65, 0, 0xbb, |_| {}) },
        Sample {
            n: 6,
            note: "extra_data 117 (32 vanity + 20 signer-ish + 65 seal)",
            build: |p| {
                let mut extra = vec![0u8; 32];
                extra.extend_from_slice(&[
                    0x24, 0xbf, 0x19, 0x1a, 0x35, 0x8c, 0x09, 0xe7, 0x9b, 0x9e, 0xad, 0x2d, 0x89,
                    0x99, 0xd0, 0x85, 0x90, 0xb5, 0xaf, 0x31,
                ]);
                extra.extend_from_slice(&[0xbb; 65]);
                let header = Header {
                    parent_hash: p,
                    ommers_hash: EMPTY_OMMER_ROOT_HASH,
                    beneficiary: Address::repeat_byte(0x42),
                    state_root: EMPTY_TRIE,
                    transactions_root: EMPTY_TRIE,
                    receipts_root: EMPTY_TRIE,
                    logs_bloom: Default::default(),
                    difficulty: U256::from(2u64),
                    number: 1,
                    gas_limit: 30_000_000,
                    gas_used: 0,
                    timestamp: 1,
                    extra_data: extra.into(),
                    mix_hash: B256::ZERO,
                    nonce: Default::default(),
                    base_fee_per_gas: None,
                    withdrawals_root: None,
                    blob_gas_used: None,
                    excess_blob_gas: None,
                    parent_beacon_block_root: None,
                    requests_hash: None,
                    block_access_list_hash: None,
                    slot_number: None,
                };
                encode_block(&Block::new(
                    header,
                    BlockBody { transactions: vec![], ommers: vec![], withdrawals: None },
                ))
            },
        },
        Sample { n: 7, note: "extra_data 128", build: |p| child(p, 128, 0xaa, 0xbb, |_| {}) },
        Sample {
            n: 8,
            note: "with base_fee_per_gas=1",
            build: |p| child(p, 97, 0, 0xbb, |h| h.base_fee_per_gas = Some(1)),
        },
        Sample {
            n: 9,
            note: "withdrawals_root set",
            build: |p| child(p, 97, 0, 0xbb, |h| h.withdrawals_root = Some(EMPTY_TRIE)),
        },
        Sample {
            n: 10,
            note: "difficulty 99",
            build: |p| child(p, 97, 0, 0xbb, |h| h.difficulty = U256::from(99)),
        },
        Sample {
            n: 11,
            note: "wrong parent_hash",
            build: |p| {
                child(B256::repeat_byte(0x11), 97, 0, 0xbb, |_| {
                    let _ = p;
                })
            },
        },
        Sample {
            n: 12,
            note: "blob gas fields",
            build: |p| {
                child(p, 97, 0, 0xbb, |h| {
                    h.blob_gas_used = Some(0);
                    h.excess_blob_gas = Some(0);
                })
            },
        },
        Sample { n: 13, note: "garbage 64 bytes (not RLP block)", build: |_| vec![0xde; 64] },
        Sample { n: 14, note: "rlp empty list 0xc0", build: |_| vec![0xc0] },
        Sample {
            n: 15,
            note: "timestamp 0",
            build: |p| child(p, 97, 0, 0xbb, |h| h.timestamp = 0),
        },
    ];

    let mut manifest =
        String::from("# generated probe payloads — edit PARENT via PARENT_HASH env\n\n");
    for s in samples {
        let path = Path::new(&out_dir).join(format!("{}.dat", s.n));
        let bytes = (s.build)(parent);
        std::fs::write(&path, &bytes)?;
        manifest.push_str(&format!("{}.dat — {} ({} bytes)\n", s.n, s.note, bytes.len()));
        eprintln!("wrote {} ({} bytes) — {}", path.display(), bytes.len(), s.note);
    }

    std::fs::write(Path::new(&out_dir).join("MANIFEST.txt"), manifest)?;
    eprintln!("done → {out_dir}/");
    Ok(())
}

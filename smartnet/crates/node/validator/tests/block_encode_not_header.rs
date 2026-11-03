//! Regression: `alloy_consensus::Block` must RLP-encode as a full block, not as a bare header.
//! If `Block::encode` ever resolved to `Header::encode` (e.g. via `Deref` to `header`), the node
//! sees a flattened "block" and `unexpected string` on decode.

use alloy_consensus::{Block, BlockBody, Header, TxEnvelope};
use alloy_rlp::{Encodable, Header as RlpHeader, PayloadView};
use alloy_rpc_types_eth::Transaction;

#[test]
fn consensus_block_rlp_has_short_top_level_list_first_child_is_header_list() {
    let header = Header::default();
    let body: BlockBody<TxEnvelope> = BlockBody::default();
    let block = Block::new(header, body);

    let mut buf = Vec::new();
    block.encode(&mut buf);

    let mut slice = buf.as_slice();
    let top = RlpHeader::decode_raw(&mut slice).expect("top RLP");
    let PayloadView::List(children) = top else {
        panic!("expected top-level list for block RLP");
    };
    assert!(
        (2..=4).contains(&children.len()),
        "expected 2..=4 top-level block children, got {}",
        children.len()
    );

    let mut first = *children.first().expect("first child");
    let first_head = RlpHeader::decode(&mut first).expect("first child RLP header");
    assert!(
        first_head.list,
        "first top-level child must be the nested header **list** (got non-list first byte {:?})",
        children[0].first()
    );
}

/// Same tx carrier as `run_once` after `map_transactions(Transaction::into_inner)` (required for
/// canonical execution-layer block RLP).
#[test]
fn consensus_block_with_rpc_transaction_encodes_full_envelope() {
    let header = Header::default();
    let body: BlockBody<Transaction> = BlockBody::default();
    let block = Block::new(header, body).map_transactions(Transaction::into_inner);

    let mut buf = Vec::new();
    block.encode(&mut buf);

    let mut slice = buf.as_slice();
    let top = RlpHeader::decode_raw(&mut slice).expect("top RLP");
    let PayloadView::List(children) = top else {
        panic!("expected top-level list for block RLP");
    };
    assert!((2..=4).contains(&children.len()));

    let mut first = *children.first().expect("first child");
    let first_head = RlpHeader::decode(&mut first).expect("first child RLP header");
    assert!(first_head.list);
}

#[test]
fn consensus_block_with_rpc_transaction_wrapper_does_not_encode_block_envelope() {
    let header = Header::default();
    let body: BlockBody<Transaction> = BlockBody::default();
    let block = Block::new(header, body);

    let mut buf = Vec::new();
    block.encode(&mut buf);

    let mut slice = buf.as_slice();
    let top = RlpHeader::decode_raw(&mut slice).expect("top RLP");
    let PayloadView::List(children) = top else {
        panic!("expected top-level list");
    };
    assert!(
        children.len() > 4,
        "regression guard: `Block<alloy_rpc_types_eth::Transaction>` must not be submitted as BlockRlp; \
         use `map_transactions(Transaction::into_inner)` first (got {} top-level children)",
        children.len()
    );
}

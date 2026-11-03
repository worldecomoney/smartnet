//! Helper functions for `smartnet_rpc_eth_api::EthFilterApiServer` implementation.
//!
//! Log parsing for building filter.

use alloy_consensus::TxReceipt;
use alloy_eips::{eip2718::Encodable2718, BlockNumHash};
use alloy_primitives::TxHash;
use alloy_rpc_types_eth::{Filter, Log};
use smartnet_primitives_traits::{BlockBody, RecoveredBlock, SignedTransaction};
use smartnet_chainspec::ChainInfo;
use smartnet_errors::ProviderError;
use smartnet_storage_api::{BlockReader, ProviderBlock};
use std::sync::Arc;
use thiserror::Error;

/// Returns all matching of a block's receipts when the transaction hashes are known.
pub fn matching_block_logs_with_tx_hashes<'a, I, R>(
    filter: &Filter,
    block_num_hash: BlockNumHash,
    block_timestamp: u64,
    tx_hashes_and_receipts: I,
    removed: bool,
) -> Vec<Log>
where
    I: IntoIterator<Item = (TxHash, &'a R)>,
    R: TxReceipt<Log = alloy_primitives::Log> + 'a,
{
    if !filter.matches_block(&block_num_hash) {
        return vec![];
    }

    let mut all_logs = Vec::new();
    // Tracks the index of a log in the entire block.
    let mut log_index: u64 = 0;

    // Iterate over transaction hashes and receipts and append matching logs.
    for (receipt_idx, (tx_hash, receipt)) in tx_hashes_and_receipts.into_iter().enumerate() {
        for log in receipt.logs() {
            if filter.matches(log) {
                let log = Log {
                    inner: log.clone(),
                    block_hash: Some(block_num_hash.hash),
                    block_number: Some(block_num_hash.number),
                    transaction_hash: Some(tx_hash),
                    // The transaction and receipt index is always the same.
                    transaction_index: Some(receipt_idx as u64),
                    log_index: Some(log_index),
                    removed,
                    block_timestamp: Some(block_timestamp),
                };
                all_logs.push(log);
            }
            log_index += 1;
        }
    }
    all_logs
}

/// Helper enum to fetch a transaction either from a block or from the provider.
#[derive(Debug)]
pub enum ProviderOrBlock<'a, P: BlockReader> {
    /// Provider
    Provider(&'a P),
    /// [`RecoveredBlock`]
    Block(Arc<RecoveredBlock<ProviderBlock<P>>>),
}

/// Appends all matching logs of a block's receipts.
/// If the log matches, look up the corresponding transaction hash.
pub fn append_matching_block_logs<P>(
    all_logs: &mut Vec<Log>,
    provider_or_block: ProviderOrBlock<'_, P>,
    filter: &Filter,
    block_num_hash: BlockNumHash,
    receipts: &[P::Receipt],
    removed: bool,
    block_timestamp: u64,
) -> Result<(), ProviderError>
where
    P: BlockReader<Transaction: SignedTransaction>,
{
    if !filter.matches_block(&block_num_hash) {
        return Ok(());
    }

    // Tracks the index of a log in the entire block.
    let mut log_index: u64 = 0;

    // Lazy loaded number of the first transaction in the block.
    // This is useful for blocks with multiple matching logs because it
    // prevents re-querying the block body indices.
    let mut loaded_first_tx_num = None;

    // Iterate over receipts and append matching logs.
    for (receipt_idx, receipt) in receipts.iter().enumerate() {
        // The transaction hash of the current receipt.
        let mut transaction_hash = None;

        for log in receipt.logs() {
            if filter.matches(log) {
                // if this is the first match in the receipt's logs, look up the transaction hash
                if transaction_hash.is_none() {
                    transaction_hash = match &provider_or_block {
                        ProviderOrBlock::Block(block) => {
                            block.body().transactions().get(receipt_idx).map(|t| t.trie_hash())
                        }
                        ProviderOrBlock::Provider(provider) => {
                            let first_tx_num = match loaded_first_tx_num {
                                Some(num) => num,
                                None => {
                                    let block_body_indices = provider
                                        .block_body_indices(block_num_hash.number)?
                                        .ok_or(ProviderError::BlockBodyIndicesNotFound(
                                            block_num_hash.number,
                                        ))?;
                                    loaded_first_tx_num = Some(block_body_indices.first_tx_num);
                                    block_body_indices.first_tx_num
                                }
                            };

                            // This is safe because Transactions and Receipts have the same
                            // keys.
                            let transaction_id = first_tx_num + receipt_idx as u64;
                            let transaction =
                                provider.transaction_by_id(transaction_id)?.ok_or_else(|| {
                                    ProviderError::TransactionNotFound(transaction_id.into())
                                })?;

                            Some(transaction.trie_hash())
                        }
                    };
                }

                let log = Log {
                    inner: log.clone(),
                    block_hash: Some(block_num_hash.hash),
                    block_number: Some(block_num_hash.number),
                    transaction_hash,
                    // The transaction and receipt index is always the same.
                    transaction_index: Some(receipt_idx as u64),
                    log_index: Some(log_index),
                    removed,
                    block_timestamp: Some(block_timestamp),
                };
                all_logs.push(log);
            }
            log_index += 1;
        }
    }
    Ok(())
}

/// Computes the block range based on the filter range and current block numbers.
///
/// Returns an error for invalid ranges rather than silently clamping values.
pub fn get_filter_block_range(
    from_block: Option<u64>,
    to_block: Option<u64>,
    start_block: u64,
    info: ChainInfo,
) -> Result<(u64, u64), FilterBlockRangeError> {
    let from_block_number = from_block.unwrap_or(start_block);
    let to_block_number = to_block.unwrap_or(info.best_number);

    // from > to is an invalid range
    if from_block_number > to_block_number {
        return Err(FilterBlockRangeError::InvalidBlockRange);
    }

    // we cannot query blocks that don't exist yet
    if to_block_number > info.best_number {
        return Err(FilterBlockRangeError::BlockRangeExceedsHead);
    }

    Ok((from_block_number, to_block_number))
}

/// Errors for filter block range validation.
///
/// See also <https://github.com/ethereum/go-ethereum/blob/master/eth/filters/filter.go#L224-L230>.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FilterBlockRangeError {
    /// `from_block > to_block`
    #[error("invalid block range params")]
    InvalidBlockRange,
    /// Block range extends beyond current head
    #[error("block range extends beyond current head block")]
    BlockRangeExceedsHead,
}

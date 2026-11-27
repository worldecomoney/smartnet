//! Loads fee history from database. Helper trait for `eth_` fee and transaction RPC methods.

use super::LoadBlock;
use crate::FromEthApiError;
use alloy_consensus::{constants::GWEI_TO_WEI, BlockHeader};
use alloy_eips::eip7840::BlobParams;
use alloy_primitives::U256;
use alloy_rpc_types_eth::{BlockNumberOrTag, FeeHistory};
use futures::{Future, StreamExt};
use smartnet_primitives_traits::BlockBody;
use smartnet_chainspec::{fixed_gas_price_wei, ChainSpecProvider, EthChainSpec};
use smartnet_rpc_eth_types::{
    fee_history::calculate_reward_percentiles_for_block, utils::checked_blob_gas_used_ratio,
    EthApiError, FeeHistoryCache, FeeHistoryEntry, GasPriceOracle, RpcInvalidTransactionError,
};
use smartnet_storage_api::{
    BlockIdReader, BlockNumReader, BlockReaderIdExt, HeaderProvider, ProviderHeader,
};
use tracing::debug;

/// Fee related functions for the [`EthApiServer`](crate::EthApiServer) trait in the
/// `eth_` namespace.
pub trait EthFees:
    LoadFee<
    Provider: ChainSpecProvider<ChainSpec: EthChainSpec<Header = ProviderHeader<Self::Provider>>>,
>
{
    /// Returns a suggestion for a gas price for legacy transactions.
    ///
    /// See also: <https://github.com/ethereum/pm/issues/328#issuecomment-853234014>
    fn gas_price(&self) -> impl Future<Output = Result<U256, Self::Error>> + Send
    where
        Self: LoadBlock,
    {
        LoadFee::gas_price(self)
    }

    /// Returns a suggestion for a base fee for blob transactions.
    fn blob_base_fee(&self) -> impl Future<Output = Result<U256, Self::Error>> + Send
    where
        Self: LoadBlock,
    {
        LoadFee::blob_base_fee(self)
    }

    /// Returns a suggestion for the priority fee (the tip)
    fn suggested_priority_fee(&self) -> impl Future<Output = Result<U256, Self::Error>> + Send
    where
        Self: 'static,
    {
        LoadFee::suggested_priority_fee(self)
    }

    /// Reports the fee history, for the given amount of blocks, up until the given newest block.
    ///
    /// If `reward_percentiles` are provided the [`FeeHistory`] will include the _approximated_
    /// rewards for the requested range.
    fn fee_history(
        &self,
        mut block_count: u64,
        mut newest_block: BlockNumberOrTag,
        reward_percentiles: Option<Vec<f64>>,
    ) -> impl Future<Output = Result<FeeHistory, Self::Error>> + Send {
        async move {
            if block_count == 0 {
                return Ok(FeeHistory::default());
            }

            // ensure the given reward percentiles aren't excessive
            if reward_percentiles.as_ref().map(|perc| perc.len() as u64) >
                Some(self.gas_oracle().config().max_reward_percentile_count)
            {
                return Err(EthApiError::InvalidRewardPercentiles.into());
            }

            // If reward percentiles were specified, we
            // need to validate that they are monotonically
            // increasing and 0 <= p <= 100
            // Note: The types used ensure that the percentiles are never < 0
            if let Some(percentiles) = &reward_percentiles &&
                (percentiles.iter().any(|p| *p < 0.0 || *p > 100.0) ||
                    percentiles.windows(2).any(|w| w[0] > w[1]))
            {
                return Err(EthApiError::InvalidRewardPercentiles.into());
            }

            // See https://github.com/ethereum/go-ethereum/blob/2754b197c935ee63101cbbca2752338246384fec/eth/gasprice/feehistory.go#L218C8-L225
            let max_fee_history = if reward_percentiles.is_none() {
                self.gas_oracle().config().max_header_history
            } else {
                self.gas_oracle().config().max_block_history
            };

            if block_count > max_fee_history {
                debug!(
                    requested = block_count,
                    truncated = max_fee_history,
                    "Sanitizing fee history block count"
                );
                block_count = max_fee_history
            }

            if newest_block.is_pending() {
                // cap the target block since we don't have fee history for the pending block
                newest_block = BlockNumberOrTag::Latest;
            }

            // For explicit block numbers, validate against chain head before resolution
            if let BlockNumberOrTag::Number(requested) = newest_block {
                let latest_block =
                    self.provider().best_block_number().map_err(Self::Error::from_eth_err)?;
                if requested > latest_block {
                    return Err(
                        EthApiError::RequestBeyondHead { requested, head: latest_block }.into()
                    );
                }
            }

            let end_block = self
                .provider()
                .block_number_for_id(newest_block.into())
                .map_err(Self::Error::from_eth_err)?
                .ok_or(EthApiError::HeaderNotFound(newest_block.into()))?;

            // need to add 1 to the end block to get the correct (inclusive) range
            let end_block_plus = end_block + 1;
            // Ensure that we would not be querying outside of genesis
            if end_block_plus < block_count {
                block_count = end_block_plus;
            }

            // Fetch the headers and ensure we got all of them
            //
            // Treat a request for 1 block as a request for `newest_block..=newest_block`,
            // otherwise `newest_block - 2`
            // NOTE: We ensured that block count is capped
            let start_block = end_block_plus - block_count;

            // Collect base fees, gas usage ratios and (optionally) reward percentile data
            // Pre-allocate capacity: base_fee and blob_fee need +1 for the next block's fee
            let mut base_fee_per_gas: Vec<u128> = Vec::with_capacity(block_count as usize + 1);
            let mut gas_used_ratio: Vec<f64> = Vec::with_capacity(block_count as usize);

            let mut base_fee_per_blob_gas: Vec<u128> = Vec::with_capacity(block_count as usize + 1);
            let mut blob_gas_used_ratio: Vec<f64> = Vec::with_capacity(block_count as usize);

            let mut rewards: Vec<Vec<u128>> = if reward_percentiles.is_some() {
                Vec::with_capacity(block_count as usize)
            } else {
                Vec::new()
            };

            // Check if the requested range is within the cache bounds
            let fee_entries = self.fee_history_cache().get_history(start_block, end_block).await;

            if let Some(fee_entries) = fee_entries {
                if fee_entries.len() != block_count as usize {
                    return Err(EthApiError::InvalidBlockRange.into());
                }

                for entry in &fee_entries {
                    base_fee_per_gas
                        .push(entry.header.base_fee_per_gas().unwrap_or_default() as u128);
                    gas_used_ratio.push(entry.gas_used_ratio);
                    base_fee_per_blob_gas.push(entry.base_fee_per_blob_gas.unwrap_or_default());
                    blob_gas_used_ratio.push(entry.blob_gas_used_ratio);

                    if let Some(percentiles) = &reward_percentiles {
                        let mut block_rewards = Vec::with_capacity(percentiles.len());
                        for &percentile in percentiles {
                            block_rewards.push(self.approximate_percentile(entry, percentile));
                        }
                        rewards.push(block_rewards);
                    }
                }
                let last_entry = fee_entries.last().expect("is not empty");

                // Also need to include the `base_fee_per_gas` and `base_fee_per_blob_gas` for the
                // next block
                base_fee_per_gas.push(
                    self.provider()
                        .chain_spec()
                        .next_block_base_fee(&last_entry.header, last_entry.header.timestamp())
                        .unwrap_or_default() as u128,
                );

                base_fee_per_blob_gas.push(last_entry.next_block_blob_fee().unwrap_or_default());
            } else {
                // read the requested header range
                let headers = self
                    .provider()
                    .sealed_headers_range(start_block..=end_block)
                    .map_err(Self::Error::from_eth_err)?;
                if headers.len() != block_count as usize {
                    return Err(EthApiError::InvalidBlockRange.into());
                }

                let chain_spec = self.provider().chain_spec();
                for header in &headers {
                    base_fee_per_gas.push(header.base_fee_per_gas().unwrap_or_default() as u128);
                    gas_used_ratio.push(header.gas_used() as f64 / header.gas_limit() as f64);

                    let blob_params = chain_spec
                        .blob_params_at_timestamp(header.timestamp())
                        .unwrap_or_else(BlobParams::cancun);

                    base_fee_per_blob_gas.push(header.blob_fee(blob_params).unwrap_or_default());
                    blob_gas_used_ratio.push(checked_blob_gas_used_ratio(
                        header.blob_gas_used().unwrap_or_default(),
                        blob_params.max_blob_gas_per_block(),
                    ));
                }

                if let Some(percentiles) = reward_percentiles.as_ref().filter(|p| !p.is_empty()) {
                    let hashes: Vec<_> = headers.iter().map(|h| h.hash()).collect();
                    let mut stream = futures::stream::iter(hashes)
                        .map(|hash| self.cache().get_block_and_receipts(hash))
                        .buffered(4);
                    let mut header_idx = 0;
                    while let Some(result) = stream.next().await {
                        let header = &headers[header_idx];
                        header_idx += 1;
                        let (block, receipts) = result
                            .map_err(Self::Error::from_eth_err)?
                            .ok_or(EthApiError::InvalidBlockRange)?;
                        rewards.push(
                            calculate_reward_percentiles_for_block(
                                percentiles,
                                header.gas_used(),
                                header.base_fee_per_gas().unwrap_or_default(),
                                block.body().transactions(),
                                &receipts,
                            )
                            .unwrap_or_default(),
                        );
                    }
                }

                // The spec states that `base_fee_per_gas` "[..] includes the next block after the
                // newest of the returned range, because this value can be derived from the
                // newest block"
                //
                // The unwrap is safe since we checked earlier that we got at least 1 header.
                let last_header = headers.last().expect("is present");
                base_fee_per_gas.push(
                    chain_spec
                        .next_block_base_fee(last_header.header(), last_header.timestamp())
                        .unwrap_or_default() as u128,
                );
                // Same goes for the `base_fee_per_blob_gas`:
                // > "[..] includes the next block after the newest of the returned range, because this value can be derived from the newest block.
                base_fee_per_blob_gas.push(
                    last_header
                        .maybe_next_block_blob_fee(
                            chain_spec.blob_params_at_timestamp(last_header.timestamp()),
                        )
                        .unwrap_or_default(),
                );
            };

            // SmartNet policy emulation: cap reported reward percentiles at the fixed-fee
            // policy tip (configured total minus the block's base fee). On a fixed-price
            // chain every transaction includes at the base fee, so the wallet-visible tip
            // market is the remainder — zero once headers carry the full protocol price.
            if let Some(total) = self.smartnet_total_fee_price() &&
                !rewards.is_empty()
            {
                clamp_rewards_to_smartnet_policy(&mut rewards, &base_fee_per_gas, total);
            }

            Ok(FeeHistory {
                base_fee_per_gas,
                gas_used_ratio,
                base_fee_per_blob_gas,
                blob_gas_used_ratio,
                oldest_block: start_block,
                reward: reward_percentiles.map(|_| rewards),
            })
        }
    }

    /// Approximates reward at a given percentile for a specific block
    /// Based on the configured resolution
    fn approximate_percentile(
        &self,
        entry: &FeeHistoryEntry<ProviderHeader<Self::Provider>>,
        requested_percentile: f64,
    ) -> u128 {
        let resolution = self.fee_history_cache().resolution();
        let rounded_percentile =
            (requested_percentile * resolution as f64).round() / resolution as f64;
        let clamped_percentile = rounded_percentile.clamp(0.0, 100.0);

        // Calculate the index in the precomputed rewards array
        let index = (clamped_percentile / (1.0 / resolution as f64)).round() as usize;
        // Fetch the reward from the FeeHistoryEntry
        entry.rewards.get(index).copied().unwrap_or_default()
    }
}

/// Loads fee from database.
///
/// Behaviour shared by several `eth_` RPC methods, not exclusive to `eth_` fees RPC methods.
pub trait LoadFee: LoadBlock
where
    Self::Provider: BlockReaderIdExt,
{
    /// Returns a handle for reading gas price.
    ///
    /// Data access in default (L1) trait method implementations.
    fn gas_oracle(&self) -> &GasPriceOracle<Self::Provider>;

    /// Returns a handle for reading fee history data from memory.
    ///
    /// Data access in default (L1) trait method implementations.
    fn fee_history_cache(&self) -> &FeeHistoryCache<ProviderHeader<Self::Provider>>;

    /// `SmartNet` fixed-fee quote: the total wallet-facing price per gas unit.
    ///
    /// On `SmartNet` chains (validator section in genesis `extraData`) the whole fee is policy,
    /// not a market, so every wallet-facing fee quote derives from the one configured total
    /// (`FIXED_GAS_PRICE_WEI`, installed at boot). The legacy `config.clique` object is inert.
    /// Returns `None` on other chains, which keep the gas-price oracle untouched.
    fn smartnet_total_fee_price(&self) -> Option<U256> {
        let chain_spec = self.provider().chain_spec();
        if !smartnet_chainspec::is_smartnet_genesis(chain_spec.genesis()) {
            return None;
        }
        Some(U256::from(fixed_gas_price_wei()))
    }

    /// Returns the gas price if it is set, otherwise fetches a suggested gas price for legacy
    /// transactions.
    fn legacy_gas_price(
        &self,
        gas_price: Option<U256>,
    ) -> impl Future<Output = Result<U256, Self::Error>> + Send {
        async move {
            match gas_price {
                Some(gas_price) => Ok(gas_price),
                None => {
                    // fetch a suggested gas price
                    self.gas_price().await
                }
            }
        }
    }

    /// Returns the EIP-1559 fees if they are set, otherwise fetches a suggested gas price for
    /// EIP-1559 transactions.
    ///
    /// Returns (`base_fee`, `priority_fee`)
    fn eip1559_fees(
        &self,
        base_fee: Option<U256>,
        max_priority_fee_per_gas: Option<U256>,
    ) -> impl Future<Output = Result<(U256, U256), Self::Error>> + Send {
        async move {
            let base_fee = match base_fee {
                Some(base_fee) => base_fee,
                None => {
                    // Derive the pending base fee from the latest header
                    let latest = self
                        .provider()
                        .latest_header()
                        .map_err(Self::Error::from_eth_err)?
                        .ok_or(EthApiError::HeaderNotFound(BlockNumberOrTag::Latest.into()))?;
                    let pending_base_fee = self
                        .provider()
                        .chain_spec()
                        .next_block_base_fee(&latest, latest.timestamp())
                        .ok_or(EthApiError::InvalidTransaction(
                            RpcInvalidTransactionError::TxTypeNotSupported,
                        ))?;
                    U256::from(pending_base_fee)
                }
            };

            let max_priority_fee_per_gas = match max_priority_fee_per_gas {
                Some(max_priority_fee_per_gas) => max_priority_fee_per_gas,
                None => self.suggested_priority_fee().await?,
            };
            Ok((base_fee, max_priority_fee_per_gas))
        }
    }

    /// Returns the EIP-4844 blob fee if it is set, otherwise fetches a blob fee.
    fn eip4844_blob_fee(
        &self,
        blob_fee: Option<U256>,
    ) -> impl Future<Output = Result<U256, Self::Error>> + Send {
        async move {
            match blob_fee {
                Some(blob_fee) => Ok(blob_fee),
                None => self.blob_base_fee().await,
            }
        }
    }

    /// Returns a suggestion for a gas price for legacy transactions.
    ///
    /// See also: <https://github.com/ethereum/pm/issues/328#issuecomment-853234014>
    fn gas_price(&self) -> impl Future<Output = Result<U256, Self::Error>> + Send {
        async move {
            // SmartNet: pin the announced legacy price to the configured native fee. A
            // historical header may carry any base fee (or none) — it is execution input only
            // and must never push the wallet quote away from what the pool/sealer expect.
            if let Some(total) = self.smartnet_total_fee_price() {
                tracing::debug!(
                    target: "smartnet::cli",
                    fixed_gas_price = %total,
                    "eth_gasPrice pinned to configured fixed native price"
                );
                return Ok(total);
            }
            let header = self.provider().latest_header().map_err(Self::Error::from_eth_err)?;
            let suggested_tip = self.suggested_priority_fee().await?;
            let base_fee = header.and_then(|h| h.base_fee_per_gas()).unwrap_or_default();
            let mut price = suggested_tip + U256::from(base_fee);
            if price.is_zero() {
                price = U256::from(GWEI_TO_WEI);
                tracing::warn!(
                    target: "smartnet::cli",
                    suggested_tip = %suggested_tip,
                    base_fee,
                    final_gas_price = %price,
                    eth_gas_emergency_floor = true,
                    "eth_gasPrice emergency floor: suggested_tip + base_fee was zero after oracle sampling"
                );
            } else {
                tracing::debug!(
                    target: "smartnet::cli",
                    suggested_tip = %suggested_tip,
                    base_fee,
                    gas_price = %price,
                    eth_gas_emergency_floor = false,
                    "eth_gasPrice returned (legacy: tip + base_fee; see GAS_ORACLE_* logs for sampling)"
                );
            }
            Ok(price)
        }
    }

    /// Returns a suggestion for a base fee for blob transactions.
    fn blob_base_fee(&self) -> impl Future<Output = Result<U256, Self::Error>> + Send {
        async move {
            self.provider()
                .latest_header()
                .map_err(Self::Error::from_eth_err)?
                .and_then(|h| {
                    h.maybe_next_block_blob_fee(
                        self.provider().chain_spec().blob_params_at_timestamp(h.timestamp()),
                    )
                })
                .ok_or(EthApiError::ExcessBlobGasNotSet.into())
                .map(U256::from)
        }
    }

    /// Returns a suggestion for the priority fee (the tip)
    fn suggested_priority_fee(&self) -> impl Future<Output = Result<U256, Self::Error>> + Send
    where
        Self: 'static,
    {
        async move {
            if let Some(total) = self.smartnet_total_fee_price() {
                // Emulate `total = pending base + tip` on a fixed-price chain. Use the same
                // pending base fee the new header, txpool and EVM env use — not the latest
                // historical header's own fee — so type-2 quotes also land on the fixed total.
                let latest = self
                    .provider()
                    .latest_header()
                    .map_err(Self::Error::from_eth_err)?
                    .ok_or(EthApiError::HeaderNotFound(BlockNumberOrTag::Latest.into()))?;
                let pending_base_fee = self
                    .provider()
                    .chain_spec()
                    .next_block_base_fee(&latest, latest.timestamp())
                    .unwrap_or_else(fixed_gas_price_wei);
                return Ok(smartnet_priority_tip(total, Some(pending_base_fee)));
            }
            self.gas_oracle().suggest_tip_cap().await.map_err(Self::Error::from_eth_err)
        }
    }
}

/// `SmartNet` tip emulation: configured total price minus the block base fee (saturating).
/// A header without a base fee (pre fee-era) leaves the tip equal to the whole price, so
/// `eth_gasPrice = tip + base` still lands on the configured total.
fn smartnet_priority_tip(total: U256, base_fee: Option<u64>) -> U256 {
    total.saturating_sub(U256::from(base_fee.unwrap_or_default()))
}

/// Caps fee-history reward percentiles at the fixed-fee policy tip: each block's reported
/// rewards never exceed `total − block_base_fee`. Fee-era blocks (base fee = total) report
/// a zero-tip market; pre fee-era blocks keep their honest values as long as they stay
/// within the configured total.
fn clamp_rewards_to_smartnet_policy(rewards: &mut [Vec<u128>], base_fees: &[u128], total: U256) {
    let total_wei = u128::try_from(total).unwrap_or(u128::MAX);
    for (index, block_rewards) in rewards.iter_mut().enumerate() {
        let Some(base_fee) = base_fees.get(index) else { break };
        let cap = total_wei.saturating_sub(*base_fee);
        for reward in block_rewards {
            *reward = (*reward).min(cap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smartnet_chainspec::SMARTNET_FIXED_GAS_PRICE_WEI;

    #[test]
    fn smartnet_tip_lands_on_total_price() {
        let total = U256::from(SMARTNET_FIXED_GAS_PRICE_WEI);

        // Fee-era header carries the full protocol price as base fee → zero-tip quote, so
        // `eth_gasPrice = tip + base` equals the configured total exactly.
        assert_eq!(smartnet_priority_tip(total, Some(SMARTNET_FIXED_GAS_PRICE_WEI)), U256::ZERO);

        // Pre fee-era header has no base fee → the tip is the whole configured price.
        assert_eq!(smartnet_priority_tip(total, None), total);
        assert_eq!(smartnet_priority_tip(total, Some(0)), total);

        // Never overshoot a hypothetical higher base fee (saturates at zero).
        assert_eq!(
            smartnet_priority_tip(total, Some(SMARTNET_FIXED_GAS_PRICE_WEI * 2)),
            U256::ZERO
        );
    }

    #[test]
    fn smartnet_rewards_capped_to_policy_tip() {
        let total = U256::from(SMARTNET_FIXED_GAS_PRICE_WEI);
        let fixed = u128::from(SMARTNET_FIXED_GAS_PRICE_WEI);

        // Block 0: pre fee-era (base 0) — honest in-policy values stay.
        // Blocks 1-2: fee-era — even the historically overpaid samples report a zero tip.
        let mut rewards = vec![vec![1_000_000_000u128; 3], vec![fixed; 3], vec![42u128; 3]];
        let base_fees = vec![0u128, fixed, fixed];

        clamp_rewards_to_smartnet_policy(&mut rewards, &base_fees, total);

        assert_eq!(rewards[0], vec![1_000_000_000u128; 3]);
        assert_eq!(rewards[1], vec![0u128; 3]);
        assert_eq!(rewards[2], vec![0u128; 3]);
    }
}

//! SmartNet fixed-fee policy — single source of truth for the protocol gas price.
//!
//! The fixed fee is a **native-coin price policy**: every ordinary native transfer
//! (21,000 gas) costs a flat fee of 0.005 coins. The gas price is therefore a protocol
//! constant — not a market.
//!
//! **ERC-20 / DeFi: 100% Ethereum compatibility.** Token contracts, approvals, permits,
//! multicalls and routers are never special-cased; their transactions pay the standard
//! `gasUsed × gasPrice` computed by ordinary EVM gas metering — same math as Ethereum,
//! just at a stable price. No contract-visible behavior changes.
//!
//! `0.005 coins = 5_000_000_000_000_000 wei` is not exactly divisible by 21,000, so the
//! canonical constant is the gas price (integer division) and the effective transfer fee
//! is derived from it: 21,000 × 238,095,238,095 = 4,999,999,999,995,000 wei
//! (0.005 coins minus 5,000,000 wei = 5×10⁻¹² coins — invisible at coin precision).
//!
//! EVM gas *metering* is unchanged (units of account kept for wallets, receipts, DeFi);
//! only the price per gas unit is fixed. Contract transactions therefore cost
//! `gasUsed × SMARTNET_FIXED_GAS_PRICE_WEI`, consistent with the compat-layer design in
//! `doc/fees.md`.
//!
//! The node pins the same value for wallet quotes via `GPO_DEFAULT_SUGGESTED_FEE` in
//! `~/.smartnet/node/config.toml` (see `doc/FEES-CURR.md`).

/// `SmartNet` fixed gas price: `5_000_000_000_000_000 / 21_000 = 238_095_238_095` wei/gas
/// (≈ 238.1 gwei). Sealed into every SmartNet-era header as `baseFeePerGas` and returned
/// by the wallet-facing fee quotes (`eth_gasPrice`, `eth_feeHistory`, priority-fee tips).
pub const SMARTNET_FIXED_GAS_PRICE_WEI: u64 = 5_000_000_000_000_000 / 21_000; // 238_095_238_095 ≈
                                                                              // 238.1 gwei

/// Effective flat fee for a standard 21,000-gas native transfer: ≈ 0.005 coins.
pub const SMARTNET_FIXED_TRANSFER_FEE_WEI: u128 = SMARTNET_FIXED_GAS_PRICE_WEI as u128 * 21_000;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChainSpecBuilder, EthChainSpec};
    use alloy_chains::Chain;
    use alloy_consensus::Header;
    use alloy_eips::{calc_next_block_base_fee, eip1559::INITIAL_BASE_FEE};
    use alloy_genesis::{ChainConfig, CliqueConfig, Genesis};

    fn fixed_price() -> u64 {
        // Same arithmetic the old per-crate copies used — one source now.
        5_000_000_000_000_000 / 21_000
    }

    fn smartnet_spec() -> crate::ChainSpec {
        ChainSpecBuilder::mainnet()
            .chain(Chain::from(315u64))
            .genesis(Genesis {
                config: ChainConfig {
                    chain_id: 315,
                    clique: Some(CliqueConfig { period: Some(5), epoch: Some(30_000) }),
                    ..Default::default()
                },
                ..Default::default()
            })
            .build()
    }

    #[test]
    fn constants_match_derivation() {
        assert_eq!(SMARTNET_FIXED_GAS_PRICE_WEI, fixed_price());
        assert_eq!(SMARTNET_FIXED_GAS_PRICE_WEI, 238_095_238_095);
        assert_eq!(SMARTNET_FIXED_TRANSFER_FEE_WEI, 4_999_999_999_995_000);
    }

    #[test]
    fn smartnet_next_base_fee_is_fixed_for_any_parent() {
        let spec = smartnet_spec();

        // Pre fee-era parent (no base fee) — the fee-era transition still yields the fixed price.
        let pre_era = Header { timestamp: 1_000, ..Default::default() };
        assert_eq!(spec.next_block_base_fee(&pre_era, 1_012), Some(SMARTNET_FIXED_GAS_PRICE_WEI));

        // SmartNet-era parent — identical on every block (no EIP-1559 market math).
        let era = Header {
            timestamp: 1_000,
            base_fee_per_gas: Some(SMARTNET_FIXED_GAS_PRICE_WEI),
            gas_used: 21_000,
            gas_limit: 5_917_125,
            ..Default::default()
        };
        assert_eq!(spec.next_block_base_fee(&era, 1_012), Some(SMARTNET_FIXED_GAS_PRICE_WEI));
    }

    #[test]
    fn non_smartnet_spec_keeps_eip1559_formula() {
        let spec = ChainSpecBuilder::mainnet().build();
        let parent = Header {
            gas_used: 15_000_000,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(INITIAL_BASE_FEE),
            timestamp: 1_000,
            ..Default::default()
        };

        let got = spec.next_block_base_fee(&parent, 1_012).unwrap();
        let expected = calc_next_block_base_fee(
            parent.gas_used,
            parent.gas_limit,
            parent.base_fee_per_gas.unwrap(),
            spec.base_fee_params_at_timestamp(1_012),
        );
        assert_eq!(got, expected);
        assert_ne!(got, SMARTNET_FIXED_GAS_PRICE_WEI);
    }
}

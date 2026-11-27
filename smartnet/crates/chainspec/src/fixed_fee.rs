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
//! The operator pins the live value with `FIXED_GAS_PRICE_WEI` in `~/.smartnet/config.toml`;
//! the accessor above is the one runtime source for header construction, EVM execution, the
//! sealer and wallet quotes (see `doc/FEES-CURR.md`). The constants remain only the pre-boot
//! fallback. This is distinct from `TXPOOL_MARGIN`, which is only an optional txpool-entry
//! filter and never changes the fee a transaction pays.

/// `SmartNet` fixed gas price: `5_000_000_000_000_000 / 21_000 = 238_095_238_095` wei/gas
/// (≈ 238.1 gwei). Sealed into every SmartNet-era header as `baseFeePerGas` and returned
/// by the wallet-facing fee quotes (`eth_gasPrice`, `eth_feeHistory`, priority-fee tips).
pub const SMARTNET_FIXED_GAS_PRICE_WEI: u64 = 5_000_000_000_000_000 / 21_000; // 238_095_238_095 ≈
                                                                              // 238.1 gwei

/// Effective flat fee for a standard 21,000-gas native transfer: ≈ 0.005 coins.
pub const SMARTNET_FIXED_TRANSFER_FEE_WEI: u128 = SMARTNET_FIXED_GAS_PRICE_WEI as u128 * 21_000;

/// The operator-configured price (`FIXED_GAS_PRICE_WEI` in `config.toml`) becomes the single
/// runtime source at boot. The compiled constant above is only the fallback for code that runs
/// before the config is read (genesis helpers, tests); it is never a second policy source.
static FIXED_GAS_PRICE_WEI: smartnet_primitives_traits::sync::OnceLock<u64> =
    smartnet_primitives_traits::sync::OnceLock::new();

/// Installs the operator's fixed gas price. First caller wins; returns `false` if already set.
pub fn set_fixed_gas_price_wei(wei: u64) -> bool {
    FIXED_GAS_PRICE_WEI.set(wei).is_ok()
}

/// The authoritative fixed gas price (wei/gas) for this process.
pub fn fixed_gas_price_wei() -> u64 {
    *FIXED_GAS_PRICE_WEI.get().unwrap_or(&SMARTNET_FIXED_GAS_PRICE_WEI)
}

/// Flat native-transfer fee at the authoritative price.
pub fn fixed_transfer_fee_wei() -> u128 {
    fixed_gas_price_wei() as u128 * 21_000
}

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
                // Live shape: validator signers in extraData; `clique` is inert legacy config.
                extra_data: signer_extra_data().into(),
                config: ChainConfig {
                    chain_id: 315,
                    clique: Some(CliqueConfig { period: Some(5), epoch: Some(30_000) }),
                    ..Default::default()
                },
                ..Default::default()
            })
            .build()
    }

    /// 32-byte vanity + one 20-byte validator + 65-byte seal: the live genesis identity.
    fn signer_extra_data() -> Vec<u8> {
        let mut extra = vec![0u8; 32];
        extra.extend_from_slice(&[0x11; 20]);
        extra.extend_from_slice(&[0u8; 65]);
        extra
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
    fn smartnet_identity_is_extra_data_not_inert_clique_config() {
        // Live genesis: validator section present, legacy `config.clique` absent. This is the
        // shape that used to fall through to parent-derived fees and park fixed-fee wallet txs.
        let spec = ChainSpecBuilder::mainnet()
            .chain(Chain::from(315u64))
            .genesis(Genesis {
                extra_data: signer_extra_data().into(),
                config: ChainConfig { chain_id: 315, ..Default::default() },
                ..Default::default()
            })
            .build();

        assert!(crate::is_smartnet_genesis(spec.genesis()));
        let legacy_parent = Header { timestamp: 1_000, ..Default::default() };
        assert_eq!(
            spec.next_block_base_fee(&legacy_parent, 1_012),
            Some(SMARTNET_FIXED_GAS_PRICE_WEI),
            "a legacy parent without baseFeePerGas must not drop the announced fixed price"
        );
        let fee_era_parent = Header {
            base_fee_per_gas: Some(SMARTNET_FIXED_GAS_PRICE_WEI * 10),
            ..legacy_parent
        };
        assert_eq!(
            spec.next_block_base_fee(&fee_era_parent, 1_012),
            Some(SMARTNET_FIXED_GAS_PRICE_WEI),
            "a historical parent fee must not override the fixed native price"
        );
    }

    #[test]
    fn non_smartnet_legacy_parent_falls_back_instead_of_returning_none() {
        let spec = ChainSpecBuilder::mainnet().build();
        let legacy_parent = Header { timestamp: 1_000, ..Default::default() };
        // Missing header fee: try the configured price rather than `None` → `0` at callers.
        assert_eq!(
            spec.next_block_base_fee(&legacy_parent, 1_012),
            Some(SMARTNET_FIXED_GAS_PRICE_WEI)
        );
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

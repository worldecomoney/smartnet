use alloy_primitives::U256;
use clap::Args;
use smartnet_rpc_eth_types::GasPriceOracleConfig;
use smartnet_rpc_server_types::constants::gas_oracle::{
    DEFAULT_GAS_PRICE_BLOCKS, DEFAULT_GAS_PRICE_PERCENTILE, DEFAULT_IGNORE_GAS_PRICE,
    DEFAULT_MAX_GAS_PRICE,
};

/// Parameters to configure Gas Price Oracle
#[derive(Debug, Clone, Copy, Args, PartialEq, Eq)]
#[command(next_help_heading = "Gas Price Oracle")]
pub struct GasPriceOracleArgs {
    /// Number of recent blocks to check for gas price
    #[arg(skip)]
    pub blocks: u32,

    /// Gas Price below which gpo will ignore transactions
    #[arg(skip)]
    pub ignore_price: u64,

    /// Maximum transaction priority fee(or gasprice before London Fork) to be recommended by gpo
    #[arg(skip)]
    pub max_price: u64,

    /// The percentile of gas prices to use for the estimate
    #[arg(skip)]
    pub percentile: u32,

    /// The default gas price to use if there are no blocks to use
    #[arg(skip)]
    pub default_suggested_fee: Option<U256>,
}

impl GasPriceOracleArgs {
    /// Returns a [`GasPriceOracleConfig`] from the arguments.
    pub fn gas_price_oracle_config(&self) -> GasPriceOracleConfig {
        let Self { blocks, ignore_price, max_price, percentile, default_suggested_fee } = self;
        GasPriceOracleConfig {
            max_price: Some(U256::from(*max_price)),
            ignore_price: Some(U256::from(*ignore_price)),
            percentile: *percentile,
            blocks: *blocks,
            default_suggested_fee: *default_suggested_fee,
            ..Default::default()
        }
    }
}

impl Default for GasPriceOracleArgs {
    fn default() -> Self {
        Self {
            blocks: DEFAULT_GAS_PRICE_BLOCKS,
            ignore_price: DEFAULT_IGNORE_GAS_PRICE.to(),
            max_price: DEFAULT_MAX_GAS_PRICE.to(),
            percentile: DEFAULT_GAS_PRICE_PERCENTILE,
            default_suggested_fee: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    #[test]
    fn test_parse_gpo_args() {
        assert_eq!(
            GasPriceOracleArgs::default(),
            GasPriceOracleArgs {
                blocks: 20,
                ignore_price: 0,
                max_price: 500_000_000_000,
                percentile: 60,
                default_suggested_fee: None,
            }
        );
        CommandParser::<GasPriceOracleArgs>::parse_from(["smartnet"]);
    }

    #[test]
    fn gpo_args_default_sanity_test() {
        // Defaults stay wired to the shared gas-oracle constants.
        assert_eq!(GasPriceOracleArgs::default(), GasPriceOracleArgs {
            blocks: DEFAULT_GAS_PRICE_BLOCKS,
            ignore_price: DEFAULT_IGNORE_GAS_PRICE.to(),
            max_price: DEFAULT_MAX_GAS_PRICE.to(),
            percentile: DEFAULT_GAS_PRICE_PERCENTILE,
            default_suggested_fee: None,
        });
    }
}

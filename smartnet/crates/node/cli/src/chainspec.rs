use crate::cli_core::chainspec::{parse_genesis, ChainSpecParser};
use smartnet_chainspec::ChainSpec;
use std::sync::Arc;

/// Clap value parser for [`ChainSpec`]s — parses a user-supplied path (init) or inline JSON.
pub fn chain_value_parser(s: &str) -> eyre::Result<Arc<ChainSpec>, eyre::Error> {
    Ok(Arc::new(parse_genesis(s)?.into()))
}

/// Ethereum chain specification parser — parses only a user-supplied genesis (init).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct EthereumChainSpecParser;

impl ChainSpecParser for EthereumChainSpecParser {
    type ChainSpec = ChainSpec;

    fn parse(s: &str) -> eyre::Result<Arc<ChainSpec>> {
        chain_value_parser(s)
    }

    fn set_chain_id(spec: &mut ChainSpec, chain_id: u64) {
        spec.chain = chain_id.into();
    }

    /// The run-time rulebook derives from the database: the exact `Genesis` the genesis file
    /// declared at `smartnet init`, stored there and rebuilt through the same `From<Genesis>`
    /// path at every boot. Any well-formed genesis works.
    fn smartnet_chain_spec_from_db(
        genesis: alloy_genesis::Genesis,
    ) -> eyre::Result<Arc<ChainSpec>> {
        Ok(Arc::new(genesis.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_raw_chainspec_hardforks() {
        let raw = r#"{
  "config": {
    "chainId": 315,
    "istanbulBlock": 0,
    "clique": { "period": 1, "epoch": 30000 }
  },
  "difficulty": "0x1",
  "gasLimit": "0x16345785d8a0000",
  "timestamp": "0x0"
}"#;
        let spec = <EthereumChainSpecParser as ChainSpecParser>::parse(raw).unwrap();
        assert_eq!(spec.chain.id(), 315);
    }
}

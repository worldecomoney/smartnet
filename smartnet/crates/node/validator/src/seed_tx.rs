//! Submit a signed native transfer into the node txpool (`eth_sendRawTransaction`).
//!
//! Production `SmartNet` nodes have **no** local `eth_accounts` / dev signers — never use
//! `eth_sendTransaction` (returns `-32602: unknown account`).

use alloy_eips::eip2718::Encodable2718;
use alloy_network::{Ethereum, EthereumWallet, NetworkTransactionBuilder, TransactionBuilder};
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use eyre::{Context, Result};
use tracing::info;
use url::Url;

use crate::keystore_util::KeystoreMaterial;

pub(crate) struct SeedTxArgs {
    pub node_url: String,
    /// Encrypted keystore + password in RAM; no decrypted key is retained between calls.
    pub key: KeystoreMaterial,
    /// `SEED_TX_TO` from config — never a hard-coded recipient.
    pub to: Address,
    /// `SEED_TX_VALUE_WEI` from config — never a hard-coded amount.
    pub value_wei: U256,
    /// `SEED_TX_GAS_LIMIT` from config — never a hard-coded gas limit.
    pub gas_limit: u64,
    pub chain_id: u64,
    /// `RPC_TIMEOUT_MS` from config — per-request HTTP timeout.
    pub rpc_timeout_ms: u64,
}

pub(crate) async fn seed_tx(args: &SeedTxArgs) -> Result<()> {
    let url: Url = args.node_url.parse().wrap_err("parse NODE_URL")?;
    let from = args.key.address();
    // The decrypted key exists only for this one local transaction signing. It is dropped as
    // soon as the wallet is built; the wallet is dropped once `tx.build` finishes.
    let signing_key = args.key.decrypt().wrap_err("decrypt keystore for seed tx")?;
    let signer =
        PrivateKeySigner::from_bytes(&alloy_primitives::B256::from_slice(&signing_key.to_bytes()))
            .wrap_err("build signer from the keystore key")?;
    drop(signing_key);
    let wallet = EthereumWallet::from(signer);
    // HTTP provider only — sign locally, submit via `eth_sendRawTransaction`.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(args.rpc_timeout_ms))
        .build()
        .wrap_err("build reqwest client")?;
    let provider = ProviderBuilder::new().network::<Ethereum>().connect_reqwest(client, url);

    let chain_id = args.chain_id;
    let nonce = provider
        .get_transaction_count(from)
        .pending()
        .await
        .wrap_err("eth_getTransactionCount(pending)")?;

    // Legacy type-0 tx shape with an explicit gas price. SmartNet runs London-compatible
    // execution (baseFeePerGas present from genesis/London@1), so legacy txs are accepted
    // as long as gas_price >= base fee. Use the protocol fixed gas price to ensure the
    // seed tx is valid and will be mined.
    let tx = TransactionRequest::default()
        .with_from(from)
        .with_to(args.to)
        .with_value(args.value_wei)
        .with_nonce(nonce)
        .with_gas_limit(args.gas_limit)
        .with_chain_id(chain_id)
        .with_gas_price(
            calibri_lite_consensus::pre_consensus::SMARTNET_FIXED_GAS_PRICE_WEI as u128,
        );

    let envelope = tx.build(&wallet).await.wrap_err("sign transaction locally")?;
    drop(wallet);
    let raw = envelope.encoded_2718();
    let pending = provider.send_raw_transaction(&raw).await.wrap_err("eth_sendRawTransaction")?;
    let tx_hash = *pending.tx_hash();

    info!(
        target: "smartnet_validator",
        event = "SEED_TX_SENT",
        %from,
        to = %args.to,
        value_wei = %args.value_wei,
        %nonce,
        %tx_hash,
        "submitted native transfer to txpool"
    );

    let status: serde_json::Value =
        provider.client().request_noparams("txpool_status").await.wrap_err("txpool_status")?;
    info!(
        target: "smartnet_validator",
        event = "SEED_TX_POOL_STATUS",
        ?status,
        "txpool after seed"
    );

    println!("tx_hash={tx_hash:#x}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use k256::ecdsa::SigningKey;

    fn args_with_url(node_url: &str) -> SeedTxArgs {
        let b: B256 = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
            .parse()
            .expect("key hex");
        let secret_key = SigningKey::from_slice(b.as_slice()).expect("valid scalar");
        SeedTxArgs {
            node_url: node_url.to_string(),
            key: crate::keystore_util::test_material(&secret_key),
            to: Address::ZERO,
            value_wei: U256::ZERO,
            gas_limit: 21_000,
            chain_id: 1,
            rpc_timeout_ms: 1_000,
        }
    }

    /// `NODE_URL` is parsed before any network I/O, so an unparseable URL fails offline
    /// (no node required). The `{:#}` form surfaces the `parse NODE_URL` context.
    #[tokio::test]
    async fn seed_tx_rejects_unparseable_node_url() {
        let err = seed_tx(&args_with_url("not a url")).await.unwrap_err();
        assert!(format!("{err:#}").contains("NODE_URL"), "got: {err:#}");
    }
}

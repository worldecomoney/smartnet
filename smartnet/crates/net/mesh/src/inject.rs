//! Local injection of a received rumor into this node — JSON-RPC semantics, in-process.
//!
//! The mesh never talks to the engine directly: a received rumor is delivered through the
//! node's own RPC *surface semantics*, exactly like an external client would —
//! `eth_sendRawTransaction` for transactions and `smartnet_newPayload` (BlockRlp,
//! `[hex_rlp, true, true]`) for blocks, the same call the validator makes. Import
//! validation therefore vets mesh traffic with no mesh-specific trust.
//!
//! [`Injector`] is the seam: the node implements it as a direct in-process call on the
//! live `EthApi` / consensus engine (no socket, no serialization), while [`LocalRpc`]
//! keeps the original loopback HTTP client for tests and as the transport reference.

use alloy_primitives::Bytes;
use async_trait::async_trait;
use std::time::Duration;

/// Default timeout for one local RPC round trip.
pub const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(1);

/// Delivers a received rumor into this node through the node's own RPC semantics.
#[async_trait]
pub trait Injector: std::fmt::Debug + Send + Sync + 'static {
    /// Injects a raw signed transaction (`eth_sendRawTransaction`).
    ///
    /// An "already known"-class rejection is treated as success: the goal (transaction
    /// present locally) is already met, e.g. when our own pool event raced a duplicate.
    async fn send_raw_transaction(&self, raw: &Bytes) -> Result<(), String>;

    /// Injects a full block RLP (`smartnet_newPayload`, `[hex_rlp, true, true]`).
    ///
    /// A payload status of `INVALID` is still "processed": v1 has no retries, and the
    /// engine/import path has already made its decision.
    async fn new_payload(&self, block_rlp: &Bytes) -> Result<(), String>;

    /// This node's canonical tip (`eth_blockNumber`) — the status/mode-gap input of the
    /// sync dialogs.
    async fn last_block_nr(&self) -> Result<u64, String>;

    /// Full seal-path block RLP by number (`None` when the block is not local). Serves
    /// `SYNC_BLOCK_RLP_PING`.
    async fn block_rlp(&self, block_nr: u64) -> Result<Option<Bytes>, String>;
}

/// A client for this node's own JSON-RPC HTTP endpoint (tests / transport reference).
#[derive(Debug, Clone)]
pub struct LocalRpc {
    client: reqwest::Client,
    url: String,
    timeout: Duration,
}

impl LocalRpc {
    /// Creates a client for `url` (e.g. `http://127.0.0.1:8545`).
    pub fn new(url: impl Into<String>) -> Self {
        Self::with_timeout(url, DEFAULT_RPC_TIMEOUT)
    }

    /// Creates a client with an explicit per-request timeout.
    pub fn with_timeout(url: impl Into<String>, timeout: Duration) -> Self {
        Self { client: reqwest::Client::new(), url: url.into(), timeout }
    }

    /// The endpoint this client posts to.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// One JSON-RPC round trip; returns the `result` value.
    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let body = serde_json::to_vec(&body).map_err(|err| format!("rpc encode: {err}"))?;

        let response = self
            .client
            .post(&self.url)
            .timeout(self.timeout)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|err| format!("rpc transport: {err}"))?;

        let status = response.status();
        if !status.is_success() {
            return Err(format!("rpc http status {status}"));
        }
        let bytes = response.bytes().await.map_err(|err| format!("rpc read: {err}"))?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|err| format!("rpc decode: {err}"))?;

        if let Some(error) = value.get("error") {
            let code = error.get("code").and_then(|code| code.as_i64()).unwrap_or(0);
            let message =
                error.get("message").and_then(|message| message.as_str()).unwrap_or("unknown");
            return Err(format!("[{code}] {message}"));
        }
        match value.get("result") {
            Some(result) => Ok(result.clone()),
            None => Err("rpc response has neither result nor error".to_string()),
        }
    }
}

#[async_trait]
impl Injector for LocalRpc {
    async fn send_raw_transaction(&self, raw: &Bytes) -> Result<(), String> {
        match self.call("eth_sendRawTransaction", serde_json::json!([raw])).await {
            Ok(_) => Ok(()),
            Err(message) if is_benign_duplicate(&message) => {
                tracing::debug!(
                    target: "smartnet::mesh",
                    %message,
                    "mesh tx already present locally"
                );
                Ok(())
            }
            Err(message) => Err(message),
        }
    }

    async fn new_payload(&self, block_rlp: &Bytes) -> Result<(), String> {
        self.call("smartnet_newPayload", serde_json::json!([block_rlp, true, true]))
            .await
            .map(|_| ())
    }

    async fn last_block_nr(&self) -> Result<u64, String> {
        let value = self.call("eth_blockNumber", serde_json::json!([])).await?;
        let text =
            value.as_str().ok_or_else(|| "eth_blockNumber returned a non-string".to_string())?;
        let hex = text
            .strip_prefix("0x")
            .ok_or_else(|| "eth_blockNumber missing 0x prefix".to_string())?;
        u64::from_str_radix(hex, 16).map_err(|err| format!("eth_blockNumber parse: {err}"))
    }

    async fn block_rlp(&self, _block_nr: u64) -> Result<Option<Bytes>, String> {
        // JSON-RPC has no raw-RLP block endpoint this client could re-encode losslessly;
        // the node's in-process injector (`NodeInjector`) serves block RLP directly.
        Err("LocalRpc cannot serve block RLP — use the in-process injector".to_string())
    }
}

/// Whether a local-RPC error merely means "this is already here" (benign for gossip).
pub fn is_benign_duplicate(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("already known") ||
        message.contains("known transaction") ||
        message.contains("already imported")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_messages_are_benign() {
        assert!(is_benign_duplicate("[123]: already imported"));
        assert!(is_benign_duplicate("already known"));
        assert!(is_benign_duplicate("Known transaction"));
    }

    #[test]
    fn real_errors_are_not_benign() {
        assert!(!is_benign_duplicate("insufficient funds for gas * price + value"));
        assert!(!is_benign_duplicate("nonce too low"));
        assert!(!is_benign_duplicate("invalid transaction"));
    }
}

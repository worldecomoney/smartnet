//! Raw HTTP JSON-RPC to `wem_newPayload` — no provider stack.

use eyre::{Context, Result};
use serde_json::{json, Value};
use smartnet_timing::fmt_wall;
use std::time::Instant;

pub(crate) struct SubmitResult {
    pub status: String,
    pub validation_error: Option<String>,
    #[allow(dead_code)]
    raw_response: Option<String>,
}

pub(crate) async fn submit_rlp(node_url: &str, rlp: Vec<u8>) -> Result<SubmitResult> {
    // Matches smartnet-validator: `SmartNetNewPayloadInput::BlockRlp` (untagged → hex string) +
    // wait flags.
    let hex_rlp = format!("0x{}", hex::encode(&rlp));
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "wem_newPayload",
        "params": [hex_rlp, true, true]
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(node_url)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .context("HTTP POST to node")?;

    let status_code = resp.status();
    let text = resp.text().await.context("read response body")?;

    if !status_code.is_success() {
        return Ok(SubmitResult {
            status: "HTTP_ERROR".into(),
            validation_error: Some(format!("HTTP {status_code}: {text}")),
            raw_response: Some(text),
        });
    }

    let raw: Value =
        serde_json::from_str(&text).unwrap_or_else(|_| json!({ "parse_error": text.clone() }));

    if let Some(err) = raw.get("error") {
        return Ok(SubmitResult {
            status: "JSONRPC_ERROR".into(),
            validation_error: Some(err.to_string()),
            raw_response: Some(text),
        });
    }

    let result = raw.get("result").cloned().unwrap_or(raw);
    Ok(SubmitResult {
        status: result
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("(no status field)")
            .to_string(),
        validation_error: result
            .get("validationError")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        raw_response: Some(text),
    })
}

pub(crate) async fn submit_rlp_verbose(node_url: &str, case: &str, rlp: Vec<u8>) -> SubmitResult {
    eprintln!("[{case}] POST {node_url} wem_newPayload rlp_bytes={}", rlp.len());
    // Full client wall (fresh reqwest client → connect → request → parse) so a slow
    // submission localizes to transport vs the node's own `latency_us` in the response.
    let wall = Instant::now();
    match submit_rlp(node_url, rlp).await {
        Ok(r) => {
            eprintln!(
                "[{case}] -> status={} validationError={} wall={}",
                r.status,
                r.validation_error.as_deref().unwrap_or(""),
                fmt_wall(wall.elapsed())
            );
            r
        }
        Err(e) => {
            eprintln!("[{case}] -> ERROR: {e:#} wall={}", fmt_wall(wall.elapsed()));
            SubmitResult {
                status: "RPC_ERROR".into(),
                validation_error: Some(e.to_string()),
                raw_response: None,
            }
        }
    }
}

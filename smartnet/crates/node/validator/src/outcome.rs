//! Run outcome — logged and mapped to the process exit code (daemon loop will reuse this).

use tracing::{debug, error};

#[derive(Debug)]
pub(crate) enum ValidatorStatus {
    Ok,
    /// Benign end-of-cycle: nothing to seal, node down, already initialized, …
    Idle {
        event: &'static str,
        message: String,
    },
    /// Operation failed; logged, and the one-shot process exits non-zero
    /// (a future daemon loop would retry in-process instead of exiting).
    Failed {
        message: String,
    },
}

impl ValidatorStatus {
    /// Log the outcome and return the process exit code (`0` success/idle, `1` failure).
    pub(crate) fn finish(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::Idle { event, message } => {
                debug!(target: "smartnet_validator", event, "{message}");
                0
            }
            Self::Failed { message } => {
                error!(target: "smartnet_validator", event = "VALIDATOR_FAILED", "{message}");
                1
            }
        }
    }
}

pub(crate) fn failed(message: impl Into<String>) -> ValidatorStatus {
    ValidatorStatus::Failed { message: message.into() }
}

pub(crate) fn from_eyre(e: eyre::Report) -> ValidatorStatus {
    failed(e.to_string())
}

pub(crate) fn idle(event: &'static str, message: impl Into<String>) -> ValidatorStatus {
    ValidatorStatus::Idle { event, message: message.into() }
}

/// True only for a **connection-level** failure: the node could not be reached at all.
///
/// Deliberately excludes timeouts. A request that was sent but did not answer in time means
/// the node is present but slow or busy — that must surface as a real failure (exit 1), not
/// be downgraded to [`ValidatorStatus::Idle`]/exit 0 where cron and systemd see success and
/// raise no alarm. See M7: `s.contains("timed out")` used to swallow exactly that case.
///
/// Genuine node-down signals still match: `connection refused`, `connection reset`,
/// `network is unreachable`, `tcp connect error`, and the generic reqwest transport error
/// when it is not a timeout.
pub(crate) fn is_node_unreachable(err: &impl std::fmt::Display) -> bool {
    let s = err.to_string().to_lowercase();
    // Timeouts are never "unreachable": fail loudly instead of reporting Idle/exit 0.
    if s.contains("timed out") || s.contains("timeout") {
        return false;
    }
    s.contains("connection refused") ||
        s.contains("tcp connect error") ||
        s.contains("connection reset") ||
        s.contains("network is unreachable") ||
        s.contains("error sending request")
}

pub(crate) fn format_rpc_error(
    node_url: &str,
    method: &str,
    err: &impl std::fmt::Display,
) -> String {
    if is_node_unreachable(err) {
        return format!(
            "execution node unreachable at {node_url} ({method} failed) — start `smartnet` or check NODE_URL in config"
        );
    }
    format!("{method} failed against {node_url}: {err}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_level_failures_are_unreachable() {
        assert!(is_node_unreachable(&"connection refused"));
        assert!(is_node_unreachable(&"tcp connect error"));
        assert!(is_node_unreachable(&"connection reset by peer"));
        assert!(is_node_unreachable(&"network is unreachable"));
        assert!(is_node_unreachable(&"error sending request for url (http://node.example/)"));
    }

    /// M7 regression: a slow-but-healthy request must NOT be classified as unreachable,
    /// otherwise it is reported Idle/exit 0 and cron never sees the failure.
    #[test]
    fn timeouts_are_failures_not_unreachable() {
        assert!(!is_node_unreachable(&"operation timed out"));
        assert!(!is_node_unreachable(&"the operation timed out"));
        assert!(!is_node_unreachable(
            &"error sending request for url (http://node.example/): operation timed out"
        ));
        // an ordinary application error is never unreachable
        assert!(!is_node_unreachable(&"execution reverted: custom error 0x1234"));
    }

    #[test]
    fn unreachable_message_is_rendered() {
        let msg = format_rpc_error("http://node.example", "eth_chainId", &"connection refused");
        assert!(msg.contains("unreachable"), "got: {msg}");
        let msg = format_rpc_error("http://node.example", "eth_chainId", &"operation timed out");
        assert!(msg.contains("operation timed out"), "got: {msg}");
    }
}

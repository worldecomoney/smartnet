//! Pre-consensus parent/child timestamp policy (Tier 0 sparse time).

use super::seal::CalibriLiteSealError;
use std::time::{SystemTime, UNIX_EPOCH};

/// Tier 0 sparse time — `timestamp >= parent_timestamp`, with optional cap vs wall clock.
///
/// Multiple blocks may close within the same second (equal timestamps) while providing
/// deterministic parent-relative ordering. The local clock, if available, is used only
/// for an additional freshness/drift constraint — it never bypasses the parent ts
/// monotonicity check, even when the clock returns 0 or a pre-Unix-epoch value.
pub fn validate_tier0_sparse_timestamp(
    parent_timestamp: u64,
    timestamp: u64,
    max_future_drift_secs: Option<u64>,
) -> Result<(), CalibriLiteSealError> {
    if timestamp < parent_timestamp {
        return Err(CalibriLiteSealError::InvalidTier0Timestamp {
            parent_timestamp,
            timestamp,
        });
    }
    if let Some(drift) = max_future_drift_secs {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let max_ts = now_secs.saturating_add(drift);
        if timestamp > max_ts {
            return Err(CalibriLiteSealError::TimestampTooFarInFuture {
                timestamp,
                max_allowed: max_ts,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_timestamps_allowed() {
        assert!(validate_tier0_sparse_timestamp(100, 100, None).is_ok());
    }

    #[test]
    fn child_after_parent_allowed() {
        assert!(validate_tier0_sparse_timestamp(100, 101, None).is_ok());
    }

    #[test]
    fn child_before_parent_rejected() {
        assert!(validate_tier0_sparse_timestamp(100, 99, None).is_err());
    }

    #[test]
    fn zero_parent_zero_child_allowed() {
        assert!(validate_tier0_sparse_timestamp(0, 0, None).is_ok());
    }

    #[test]
    fn drift_check_passes_when_no_drift_limit() {
        assert!(validate_tier0_sparse_timestamp(0, u64::MAX, None).is_ok());
    }
}

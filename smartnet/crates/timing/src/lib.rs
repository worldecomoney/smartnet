//! Wall-clock duration formatting for `SmartNet` logs.
//!
//! `Duration`'s `Debug`/`Display` render decimal seconds (`1.757742ms`, `0.016701s`), which
//! is easy to misread when scanning import-path logs: `1.757742ms` and `1757742ms` differ
//! only by punctuation. Every wall-clock field in a `SmartNet` log instead renders through
//! [`fmt_wall`] / [`fmt_wall_opt`] as whole units — `0s120ms`, `1s13ms` — so a timing can
//! be read unambiguously from `grep` output.

use std::time::Duration;

/// Formats `d` as whole wall-clock units: `{seconds}s{milliseconds}ms` with a trailing
/// `{microseconds}µs` only when a sub-millisecond remainder exists.
///
/// Examples:
/// - `120ms` → `0s120ms`
/// - `1.013s` → `1s13ms`
/// - `1.757ms` → `0s1ms757µs`
/// - `833µs` → `0s0ms833µs`
///
/// The `µs` remainder is what keeps sub-millisecond phases (state-root task waits,
/// canonicalization) from collapsing to a misleading `0s0ms`.
pub fn fmt_wall(d: Duration) -> String {
    let total_ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let (seconds, millis) = (total_ms / 1000, total_ms % 1000);
    let micros = d.subsec_micros() % 1000;
    if micros == 0 {
        format!("{seconds}s{millis}ms")
    } else {
        format!("{seconds}s{millis}ms{micros}µs")
    }
}

/// [`fmt_wall`] for optional timings; `None` (phase not measured) renders as `-`.
pub fn fmt_wall_opt(d: Option<Duration>) -> String {
    match d {
        Some(d) => fmt_wall(d),
        None => "-".to_string(),
    }
}

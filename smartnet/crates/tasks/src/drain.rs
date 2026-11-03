//! Cooperative drain-on-exit coordination between the CLI runner and launcher futures.
//!
//! The runner receives ctrl-c/SIGTERM while the command future may still be working
//! (currently `smartnet import`, which drives one engine block at a time). Dropping that
//! future mid-flight abandons the work half-done — a block submitted to the engine but
//! never recorded — and lets the clean-shutdown marker (`SHUTDOWN_CLEAN`) land while a
//! block is still executing. Instead the runner flips [`request_exit`]; a command that
//! registered itself with [`register`] observes the flag at its next safe point (block
//! boundary), finishes the work already in flight, and returns. Only then does the
//! runner proceed with shutdown, so the clean-shutdown marker is written strictly after
//! the last block was fully processed.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Set once an exit signal (ctrl-c / SIGTERM) has been received.
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Number of currently running drainable commands.
static DRAINABLE: AtomicUsize = AtomicUsize::new(0);

/// Records that an exit signal was received.
///
/// Called by the CLI runner as soon as ctrl-c/SIGTERM wins its select, before it decides
/// whether to wait for the command future.
pub fn request_exit() {
    EXIT_REQUESTED.store(true, Ordering::SeqCst);
}

/// Returns `true` once an exit signal has been received.
pub fn exit_requested() -> bool {
    EXIT_REQUESTED.load(Ordering::SeqCst)
}

/// Registers a long-running command as drainable and returns a guard that unregisters it
/// on drop.
///
/// While at least one drainable command is registered, the runner waits (bounded by its
/// drain timeout) for the command future to return after an exit signal instead of
/// dropping it immediately.
pub fn register() -> DrainGuard {
    DRAINABLE.fetch_add(1, Ordering::SeqCst);
    DrainGuard
}

/// Returns `true` while at least one drainable command is registered.
pub fn is_drainable() -> bool {
    DRAINABLE.load(Ordering::SeqCst) > 0
}

/// Unregisters its drainable command when dropped.
#[derive(Debug)]
pub struct DrainGuard;

impl Drop for DrainGuard {
    fn drop(&mut self) {
        DRAINABLE.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_flag_starts_clear_and_latches() {
        // The flag is process-global; this test only asserts the latch semantics that
        // `request_exit` guarantees (set-once, visible to every reader).
        if !exit_requested() {
            assert!(!exit_requested());
            request_exit();
            assert!(exit_requested());
            request_exit(); // idempotent
            assert!(exit_requested());
        }
    }

    #[test]
    fn drain_guard_registers_and_unregisters() {
        let before = DRAINABLE.load(Ordering::SeqCst);
        let guard = register();
        assert_eq!(DRAINABLE.load(Ordering::SeqCst), before + 1);
        let deeper = register();
        assert_eq!(DRAINABLE.load(Ordering::SeqCst), before + 2);
        drop(deeper);
        assert_eq!(DRAINABLE.load(Ordering::SeqCst), before + 1);
        drop(guard);
        assert_eq!(DRAINABLE.load(Ordering::SeqCst), before);
    }
}

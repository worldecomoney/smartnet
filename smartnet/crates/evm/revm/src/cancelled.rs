use alloc::sync::Arc;
use core::sync::atomic::AtomicBool;

/// A marker that can be used to cancel execution.
///
/// If dropped, it will set the `cancelled` flag to true.
///
/// This is most useful when a payload job needs to be cancelled.
#[derive(Default, Clone, Debug)]
pub struct CancelOnDrop(Arc<AtomicBool>);

// === impl CancelOnDrop ===

impl CancelOnDrop {
    /// Returns true if the job was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(core::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, core::sync::atomic::Ordering::Relaxed);
    }
}

/// A marker that can be used to cancel execution.
///
/// If dropped, it will NOT set the `cancelled` flag to true.
/// If `cancel` is called, the `cancelled` flag will be set to true.
///
/// This is useful in prewarming, when an external signal is received to cancel many prewarming
/// tasks.
#[derive(Default, Clone, Debug)]
pub struct ManualCancel(Arc<AtomicBool>);

// === impl ManualCancel ===

impl ManualCancel {
    /// Returns true if the job was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Drops the [`ManualCancel`], setting the cancelled flag to true.
    pub fn cancel(self) {
        self.0.store(true, core::sync::atomic::Ordering::Relaxed);
    }
}

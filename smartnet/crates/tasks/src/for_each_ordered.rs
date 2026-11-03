use crossbeam_utils::CachePadded;
use parking_lot::{Condvar, Mutex};
use rayon::iter::{IndexedParallelIterator, ParallelIterator};
use std::sync::atomic::{AtomicBool, Ordering};

/// Extension trait for [`IndexedParallelIterator`]
/// that streams results to a sequential consumer in index order.
pub trait ForEachOrdered: IndexedParallelIterator {
    /// Executes the parallel iterator, calling `f` on each result **sequentially in index
    /// order**.
    ///
    /// Items are computed in parallel, but `f` is invoked as `f(item_0)`, `f(item_1)`, …,
    /// `f(item_{n-1})` on the calling thread. The calling thread receives each item as soon
    /// as it (and all preceding items) are ready.
    ///
    /// `f` does **not** need to be [`Send`] — it runs exclusively on the calling thread.
    ///
    /// # Blocking
    ///
    /// The calling thread blocks (via [`Condvar`]) while waiting for the next item to become
    /// ready. It does **not** participate in rayon's work-stealing while blocked. Callers
    /// should invoke this from a dedicated blocking thread (e.g. via
    /// [`tokio::task::spawn_blocking`]) rather than from within the rayon thread pool.
    fn for_each_ordered<F>(self, f: F)
    where
        Self::Item: Send,
        F: FnMut(Self::Item);

    /// Like [`for_each_ordered`](Self::for_each_ordered), but runs the parallel work on the
    /// given `pool` instead of the global rayon thread pool.
    fn for_each_ordered_in<F>(self, pool: &rayon::ThreadPool, f: F)
    where
        Self::Item: Send,
        F: FnMut(Self::Item);
}

impl<I: IndexedParallelIterator> ForEachOrdered for I {
    fn for_each_ordered<F>(self, f: F)
    where
        Self::Item: Send,
        F: FnMut(Self::Item),
    {
        ordered_impl(self, None, f);
    }

    fn for_each_ordered_in<F>(self, pool: &rayon::ThreadPool, f: F)
    where
        Self::Item: Send,
        F: FnMut(Self::Item),
    {
        ordered_impl(self, Some(pool), f);
    }
}

/// A slot holding an optional value and a condvar for notification.
struct Slot<T> {
    value: Mutex<Option<T>>,
    notify: Condvar,
}

impl<T> Slot<T> {
    const fn new() -> Self {
        Self { value: Mutex::new(None), notify: Condvar::new() }
    }
}

struct Shared<T> {
    slots: Box<[CachePadded<Slot<T>>]>,
    panicked: AtomicBool,
}

impl<T> Shared<T> {
    fn new(n: usize) -> Self {
        let slots =
            (0..n).map(|_| CachePadded::new(Slot::new())).collect::<Vec<_>>().into_boxed_slice();
        Self { slots, panicked: AtomicBool::new(false) }
    }

    /// Writes a value into slot `i`. Must only be called once per index.
    #[inline]
    fn write(&self, i: usize, val: T) {
        let slot = &self.slots[i];
        *slot.value.lock() = Some(val);
        slot.notify.notify_one();
    }

    /// Blocks until slot `i` is ready and takes the value.
    /// Returns `None` if the producer panicked.
    fn take(&self, i: usize) -> Option<T> {
        let slot = &self.slots[i];
        let mut guard = slot.value.lock();
        loop {
            if let Some(val) = guard.take() {
                return Some(val);
            }
            if self.panicked.load(Ordering::Acquire) {
                return None;
            }
            slot.notify.wait(&mut guard);
        }
    }
}

/// Executes a parallel iterator and delivers results to a sequential callback in index order.
///
/// Each slot has its own [`Condvar`], so the consumer blocks precisely on the slot it needs
/// with zero spurious wakeups.
fn ordered_impl<I, F>(iter: I, pool: Option<&rayon::ThreadPool>, mut f: F)
where
    I: IndexedParallelIterator,
    I::Item: Send,
    F: FnMut(I::Item),
{
    use std::panic::{catch_unwind, AssertUnwindSafe};

    let n = iter.len();
    if n == 0 {
        return;
    }

    let shared = Shared::<I::Item>::new(n);

    in_place_scope_in(pool, |s| {
        // Producer: compute items in parallel and write them into their slots.
        s.spawn(|_| {
            let res = catch_unwind(AssertUnwindSafe(|| {
                iter.enumerate().for_each(|(i, item)| {
                    shared.write(i, item);
                });
            }));
            if let Err(payload) = res {
                shared.panicked.store(true, Ordering::Release);
                // Wake all slots so the consumer doesn't hang. Lock each slot's mutex
                // first to serialize with the consumer's panicked check → wait sequence.
                for slot in &*shared.slots {
                    let _guard = slot.value.lock();
                    slot.notify.notify_one();
                }
                std::panic::resume_unwind(payload);
            }
        });

        // Consumer: sequential, ordered, on the calling thread.
        for i in 0..n {
            let Some(value) = shared.take(i) else {
                return;
            };
            f(value);
        }
    });
}

fn in_place_scope_in<'scope, F, R>(pool: Option<&rayon::ThreadPool>, f: F)
where
    F: FnOnce(&rayon::Scope<'scope>) -> R,
{
    if let Some(pool) = pool {
        pool.in_place_scope(f);
    } else {
        rayon::in_place_scope(f);
    }
}

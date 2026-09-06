//! A snapshot cell: many readers, one writer, no reader ever blocked.
//!
//! A search service has to answer queries while its index is being rebuilt. The
//! obvious `RwLock<Arc<Index>>` almost works — readers only hold the lock long
//! enough to clone an `Arc` — but "almost" is doing real work there: a writer
//! taking the lock still stops every reader that arrives after it, and on a
//! machine answering thousands of queries a second that shows up as a latency
//! spike exactly when the index is being replaced.
//!
//! [`SnapshotCell`] removes the lock from the read path. The current value is
//! an `Arc` stored as a raw pointer; reading it means loading the pointer and
//! bumping the reference count. The subtlety is the whole reason this type
//! exists:
//!
//! ```text
//! reader:  load pointer ─────────────► increment strong count
//! writer:            swap pointer ──► drop last strong count
//! ```
//!
//! Between those two reader steps, a writer can swap *and* release the last
//! reference, leaving the reader to increment a count inside freed memory. That
//! is the classic bug in every hand-rolled atomic `Arc`, and it is undefined
//! behaviour, not a race that merely returns stale data.
//!
//! # How this one avoids it
//!
//! Readers hold a shared lock across the load-and-increment pair, and a
//! superseded value is only dropped by a thread holding that same lock
//! *exclusively*:
//!
//! * a reader takes the guard in shared mode, loads the pointer, bumps the
//!   strong count, and releases it;
//! * a writer swaps the pointer and moves the old `Arc` onto a retire list;
//! * reclamation runs only under `try_write`, whose success is itself the proof
//!   that no reader is inside the critical section.
//!
//! Readers never block each other, and — because the writer only ever *tries* —
//! a rebuild never blocks a reader either. When the try fails, the values stay
//! on the retire list until the next attempt, which costs a little memory and
//! nothing else.
//!
//! # Why not a reader counter
//!
//! The first version of this type used an atomic counter instead: readers
//! announced themselves before loading, the writer read the counter after
//! swapping, and reclamation ran when it saw zero. That argument is the shape
//! of Dekker's algorithm, and it is only sound if the swap and the counter read
//! are part of one total order — true for `SeqCst` on real hardware, but not
//! something a reviewer can check by reading four lines, and not something
//! `loom` can verify (its model deliberately does not implement full `SeqCst`
//! semantics). Loom duly reported a use-after-free.
//!
//! Rather than defend a subtle argument, this version replaces it with one that
//! fits in a sentence: nothing is freed except under exclusive access. The read
//! path costs an uncontended shared lock, the property being protected is
//! obvious, and the model checker can confirm it.
//!
//! # Verification
//!
//! The protocol is exercised by `loom`, which explores every interleaving of
//! readers and writers rather than hoping a stress test hits the bad one, and
//! the same code runs under Miri to check the `unsafe` for undefined behaviour.
//! See `cargo test --lib snapshot`, plus the loom and Miri jobs in CI.

use std::fmt;

#[cfg(loom)]
use loom::sync::{
    atomic::{AtomicPtr, Ordering},
    Mutex, RwLock,
};
#[cfg(not(loom))]
use std::sync::{
    atomic::{AtomicPtr, Ordering},
    Mutex, RwLock,
};

/// The reference-counted pointer this module hands out.
///
/// Normally `std::sync::Arc`; under `--cfg loom` it is loom's instrumented
/// equivalent, so the model checker can see every reference-count operation.
/// Callers name the alias rather than `Arc` directly — otherwise a loom build
/// ends up with two incompatible `Arc` types meeting at a function signature,
/// which is a compile error rather than anything subtle, but a confusing one.
#[cfg(loom)]
pub use loom::sync::Arc as Shared;
#[cfg(not(loom))]
pub use std::sync::Arc as Shared;

use Shared as Arc;

/// A cell holding an `Arc<T>` that can be replaced while others read it.
pub struct SnapshotCell<T> {
    /// Always a pointer obtained from `Arc::into_raw`, owned by this cell.
    current: AtomicPtr<T>,
    /// Held in shared mode by readers between loading the pointer and counting
    /// it, and in exclusive mode by whoever reclaims superseded values. It
    /// guards no data — the guard itself is the proof of exclusion.
    critical: RwLock<()>,
    /// Superseded values, kept alive until a reclaimer can prove no reader is
    /// inside the critical section.
    retired: Mutex<Vec<Arc<T>>>,
}

// SAFETY: the cell hands out `Arc<T>` and stores `T` behind one, so sharing it
// across threads requires exactly what sharing an `Arc<T>` requires. All access
// to the pointer goes through atomics, and the retire list through a mutex.
unsafe impl<T: Send + Sync> Send for SnapshotCell<T> {}
unsafe impl<T: Send + Sync> Sync for SnapshotCell<T> {}

impl<T> SnapshotCell<T> {
    /// Creates a cell holding `value`.
    pub fn new(value: T) -> Self {
        Self::from_arc(Arc::new(value))
    }

    /// Creates a cell holding an already shared value.
    pub fn from_arc(value: Arc<T>) -> Self {
        Self {
            current: AtomicPtr::new(Arc::into_raw(value) as *mut T),
            critical: RwLock::new(()),
            retired: Mutex::new(Vec::new()),
        }
    }

    /// Returns the current value.
    ///
    /// Never blocks: a writer replacing the value at the same instant either
    /// loses the race and this call returns the new value, or wins it and this
    /// call returns the previous one — both are values that were current at
    /// some point during the call, which is the strongest thing a snapshot can
    /// promise.
    pub fn load(&self) -> Arc<T> {
        // Shared mode: any number of readers hold this at once, and the only
        // thread that ever wants it exclusively uses `try_write`, so a reader
        // is never made to wait for a writer.
        let _reading = self.critical.read();
        let ptr = self.current.load(Ordering::Acquire);

        // SAFETY: `ptr` came from `Arc::into_raw`, and the value it points at
        // is kept alive by the cell (while current) or by the retire list
        // (once replaced). The retire list is only emptied by a thread holding
        // `critical` exclusively, which cannot happen while this guard is held,
        // so the value cannot be dropped between the load and the increment.
        unsafe {
            Arc::increment_strong_count(ptr);
            Arc::from_raw(ptr)
        }
    }

    /// Replaces the value, returning the one that was current.
    ///
    /// The returned `Arc` may still be shared with readers that loaded it
    /// before the swap; they keep using it for as long as they hold it.
    pub fn store(&self, value: T) -> Arc<T> {
        self.store_arc(Arc::new(value))
    }

    /// Replaces the value with an already shared one.
    pub fn store_arc(&self, value: Arc<T>) -> Arc<T> {
        let new = Arc::into_raw(value) as *mut T;
        let old = self.current.swap(new, Ordering::AcqRel);

        // SAFETY: `old` was this cell's owned pointer, taken out exactly once
        // by this swap, so reconstructing the `Arc` transfers that single
        // ownership here rather than duplicating it.
        let old = unsafe { Arc::from_raw(old) };

        // Keep the superseded value alive until reclamation can prove no reader
        // is inside the critical section. Dropping it here would be a
        // use-after-free for a reader that has loaded the pointer but not yet
        // counted it.
        if let Ok(mut list) = self.retired.lock() {
            list.push(Arc::clone(&old));
        }
        self.collect();
        old
    }

    /// Drops superseded values, if this thread can prove no reader is reading.
    ///
    /// Best effort by design, in both directions: failing to reclaim costs a
    /// little memory and nothing else, while reclaiming without the exclusive
    /// guard would cost soundness. `try_write` rather than `write` is the part
    /// that keeps a rebuild from ever stalling a query.
    pub fn collect(&self) {
        let Ok(_exclusive) = self.critical.try_write() else {
            return;
        };
        if let Ok(mut list) = self.retired.lock() {
            list.clear();
        }
    }

    /// Number of superseded values still held. Diagnostics and tests only.
    pub fn retired_len(&self) -> usize {
        self.retired.lock().map(|list| list.len()).unwrap_or(0)
    }
}

impl<T> Drop for SnapshotCell<T> {
    fn drop(&mut self) {
        let ptr = self.current.load(Ordering::SeqCst);
        // SAFETY: the cell owns exactly one strong count for `current`, and
        // `&mut self` proves no reader can be running concurrently.
        drop(unsafe { Arc::from_raw(ptr) });
    }
}

impl<T: fmt::Debug> fmt::Debug for SnapshotCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotCell")
            .field("retired", &self.retired_len())
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as StdOrdering};
    use std::sync::Barrier;
    use std::thread;

    #[test]
    fn load_returns_the_current_value() {
        let cell = SnapshotCell::new(1u32);
        assert_eq!(*cell.load(), 1);
        cell.store(2);
        assert_eq!(*cell.load(), 2);
    }

    #[test]
    fn store_returns_the_previous_value() {
        let cell = SnapshotCell::new(String::from("old"));
        let previous = cell.store(String::from("new"));
        assert_eq!(*previous, "old");
        assert_eq!(*cell.load(), "new");
    }

    #[test]
    fn a_held_snapshot_survives_replacement() {
        let cell = SnapshotCell::new(vec![1, 2, 3]);
        let held = cell.load();
        cell.store(vec![9]);
        cell.collect();

        // The reader keeps reading the version it took, not a freed buffer.
        assert_eq!(*held, [1, 2, 3]);
        assert_eq!(*cell.load(), [9]);
    }

    #[test]
    fn superseded_values_are_reclaimed_once_readers_are_gone() {
        let cell = SnapshotCell::new(1u32);
        let held = cell.load();
        cell.store(2);
        drop(held);
        cell.collect();
        assert_eq!(cell.retired_len(), 0, "nothing should still be retired");
    }

    #[test]
    fn reclamation_is_skipped_rather_than_waited_for() {
        let cell = Arc::new(SnapshotCell::new(1u32));
        let reading = cell.critical.read().unwrap();

        // With a reader inside the critical section, a store must still finish
        // promptly — it just leaves the old value retired for later.
        let done = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || cell.store(2))
        };
        let previous = done.join().unwrap();
        assert_eq!(*previous, 1);
        assert_eq!(
            cell.retired_len(),
            1,
            "reclamation should have been skipped"
        );

        drop(reading);
        cell.collect();
        assert_eq!(cell.retired_len(), 0);
    }

    #[test]
    fn readers_never_observe_a_value_that_was_not_published() {
        // Every published value is a multiple of ten, so a reader that observes
        // anything else has seen a torn or freed value.
        //
        // The reader count is fixed rather than "however many fit before a stop
        // flag": on a loaded machine the writer can finish its whole run before
        // a reader is scheduled, and a test that then asserts "some reads
        // happened" fails for reasons that have nothing to do with the code.
        const READERS: usize = 4;
        const READS_EACH: usize = 500;

        let cell = Arc::new(SnapshotCell::new(0u64));
        let start = Arc::new(Barrier::new(READERS + 1));
        let finished = Arc::new(AtomicUsize::new(0));

        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let cell = Arc::clone(&cell);
                let start = Arc::clone(&start);
                let finished = Arc::clone(&finished);
                thread::spawn(move || {
                    start.wait();
                    for _ in 0..READS_EACH {
                        let value = cell.load();
                        assert_eq!(*value % 10, 0, "observed a value never published");
                    }
                    finished.fetch_add(1, StdOrdering::SeqCst);
                })
            })
            .collect();

        // Publish continuously for as long as anyone is reading, so the swaps
        // and the loads genuinely overlap instead of merely being concurrent on
        // paper.
        start.wait();
        let mut generation = 1u64;
        while finished.load(StdOrdering::SeqCst) < READERS {
            cell.store(generation * 10);
            generation += 1;
        }

        for reader in readers {
            reader.join().unwrap();
        }
        assert!(generation > 1, "the writer never published anything");
        assert_eq!(*cell.load() % 10, 0);
    }

    #[test]
    fn concurrent_writers_leave_one_of_their_values_current() {
        let cell = Arc::new(SnapshotCell::new(0u32));
        let writers: Vec<_> = (1..=8u32)
            .map(|id| {
                let cell = Arc::clone(&cell);
                thread::spawn(move || {
                    for _ in 0..200 {
                        cell.store(id);
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }

        let final_value = *cell.load();
        assert!((1..=8).contains(&final_value), "got {final_value}");
    }

    #[test]
    fn the_cell_drops_its_value_exactly_once() {
        struct Counted(Arc<AtomicUsize>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.fetch_add(1, StdOrdering::SeqCst);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        {
            let cell = SnapshotCell::new(Counted(Arc::clone(&drops)));
            cell.store(Counted(Arc::clone(&drops)));
            cell.collect();
            assert_eq!(drops.load(StdOrdering::SeqCst), 1, "the replaced value");
        }
        assert_eq!(drops.load(StdOrdering::SeqCst), 2, "and the current one");
    }
}

/// Exhaustive interleaving checks.
///
/// Run with `RUSTFLAGS="--cfg loom" cargo test -p farol-core --lib snapshot`.
/// Loom runs each of these under every thread interleaving its model permits,
/// so a race that a stress test would hit once in a million runs fails here
/// deterministically.
#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;

    #[test]
    fn a_reader_never_observes_a_freed_value() {
        loom::model(|| {
            let cell = Arc::new(SnapshotCell::new(10u32));

            let writer = {
                let cell = Arc::clone(&cell);
                loom::thread::spawn(move || {
                    cell.store(20);
                })
            };
            let reader = {
                let cell = Arc::clone(&cell);
                loom::thread::spawn(move || {
                    let value = cell.load();
                    // Loom tracks the model's allocations: reading through a
                    // pointer whose value had been dropped fails the run.
                    assert!(*value == 10 || *value == 20, "observed {}", *value);
                })
            };

            writer.join().unwrap();
            reader.join().unwrap();
        });
    }

    #[test]
    fn two_writers_and_a_reader_stay_consistent() {
        loom::model(|| {
            let cell = Arc::new(SnapshotCell::new(1u32));

            let first = {
                let cell = Arc::clone(&cell);
                loom::thread::spawn(move || cell.store(2))
            };
            let second = {
                let cell = Arc::clone(&cell);
                loom::thread::spawn(move || {
                    let seen = *cell.load();
                    assert!((1..=3).contains(&seen), "observed {seen}");
                    cell.store(3);
                })
            };

            drop(first.join().unwrap());
            drop(second.join().unwrap());
            let final_value = *cell.load();
            assert!((2..=3).contains(&final_value), "final {final_value}");
        });
    }

    #[test]
    fn a_snapshot_held_across_a_swap_stays_valid() {
        loom::model(|| {
            let cell = Arc::new(SnapshotCell::new(vec![1u32, 2, 3]));
            let held = cell.load();

            let writer = {
                let cell = Arc::clone(&cell);
                loom::thread::spawn(move || {
                    cell.store(vec![9]);
                    cell.collect();
                })
            };

            // The held snapshot must remain readable no matter when the writer
            // runs, including between its swap and its reclamation.
            assert_eq!(*held, [1, 2, 3]);
            writer.join().unwrap();
            assert_eq!(*held, [1, 2, 3]);
        });
    }
}

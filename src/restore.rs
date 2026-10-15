//! Crash-safety bookkeeping: remember each touched window's original position and hand
//! it back for restoration (signal handler, panic hook and normal exit may race).
//!
//! [`RestoreRegistry`] is the plain list, written for the M0 spike. [`SharedRegistry`] is
//! the one the daemon shares between the reactor (which parks and unparks windows) and the
//! termination handler (which must read it from a signal thread or a panic hook, where
//! blocking on a lock the interrupted thread holds would hang the exit).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::TryLockError;
use std::time::{Duration, Instant};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::geometry::{Point, Size};

#[derive(Debug)]
pub struct RestoreRegistry<T> {
    entries: Vec<(u64, T, Point)>,
}

impl<T> Default for RestoreRegistry<T> {
    fn default() -> Self {
        Self { entries: Vec::new() }
    }
}

impl<T> RestoreRegistry<T> {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `handle`'s original position. A second registration for the same `key` is
    /// ignored so the true original is never overwritten by a parked position.
    pub fn register(&mut self, key: u64, handle: T, original: Point) {
        if !self.entries.iter().any(|(k, _, _)| *k == key) {
            self.entries.push((key, handle, original));
        }
    }

    /// How many windows are registered.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Change the handle stored for `key` in place. Returns whether `key` is registered.
    pub fn update_handle(&mut self, key: u64, change: impl FnOnce(&mut T)) -> bool {
        match self.entries.iter_mut().find(|(k, _, _)| *k == key) {
            Some(entry) => {
                change(&mut entry.1);
                true
            }
            None => false,
        }
    }

    /// Whether `key` is registered.
    pub fn contains(&self, key: u64) -> bool {
        self.entries.iter().any(|(k, _, _)| *k == key)
    }

    /// Change where `key` is restored to (its display changed while it was parked).
    /// Returns whether `key` is registered.
    pub fn set_target(&mut self, key: u64, target: Point) -> bool {
        match self.entries.iter_mut().find(|(k, _, _)| *k == key) {
            Some(entry) => {
                entry.2 = target;
                true
            }
            None => false,
        }
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Take every entry (registration order). Subsequent calls return nothing.
    pub fn drain(&mut self) -> Vec<(T, Point)> {
        self.entries.drain(..).map(|(_, h, p)| (h, p)).collect()
    }

    /// Non-draining view for repeatable restoration (registration order).
    pub fn snapshot(&self) -> Vec<(T, Point)>
    where
        T: Clone,
    {
        self.entries.iter().map(|(_, h, p)| (h.clone(), *p)).collect()
    }

    /// Forget a window after it has been restored deliberately.
    pub fn forget(&mut self, key: u64) {
        self.entries.retain(|(k, _, _)| *k != key);
    }
}

/// What the termination handler needs to find a parked window again without any
/// reactor state: its owner, its WindowServer id and where it sits while parked (the
/// handler runs when the reactor cannot be asked).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParkedHandle {
    pub pid: i32,
    /// The WindowServer id (`CGWindowID`) of the window, when known. The restorer matches
    /// AX windows by this; the parked frame is only the fallback when it is `None`.
    pub window_id: Option<u32>,
    /// Where the window was parked (AX coordinates, top-left).
    pub parked: Point,
    pub size: Size,
}

/// [`RestoreRegistry`] shared between writers and a reader that must never block.
///
/// Writers serialise on `writers`. After every completed change they publish an
/// immutable copy of the restore set under `published`, which is held only to swap or
/// clone an `Arc`; nothing that can panic or block runs under it. [`restore_set`]
/// reads that copy and takes no other lock, so a panic hook or signal thread gets the
/// last consistent set even while the thread it interrupted is holding the writer lock
/// (e.g. panicking inside [`update`]).
///
/// [`restore_set`]: SharedRegistry::restore_set
/// [`update`]: SharedRegistry::update
pub struct SharedRegistry<H> {
    writers: Mutex<RestoreRegistry<H>>,
    published: Mutex<Arc<Vec<(H, Point)>>>,
    /// Set once shutdown has begun: nothing may be parked after that.
    shutdown: AtomicBool,
    /// How many displays the reactor last saw; the restorer's sweep stands down when fewer
    /// are active (a sleeping or inactive monitor).
    displays: AtomicUsize,
}

/// Longest `begin_shutdown` waits for a writer that is mid-update.
const SHUTDOWN_BARRIER: Duration = Duration::from_millis(50);

impl<H> Default for SharedRegistry<H> {
    fn default() -> Self {
        Self {
            writers: Mutex::default(),
            published: Mutex::new(Arc::new(Vec::new())),
            shutdown: AtomicBool::new(false),
            displays: AtomicUsize::new(0),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl<H: Clone> SharedRegistry<H> {
    /// An empty registry that accepts parks.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stop accepting parks. The termination handler calls this before its first restore
    /// pass, so a hide still in flight cannot register (and strand) a window afterwards.
    ///
    /// `park` checks the flag under the writer lock, so after the flag is set no park can
    /// complete. This also waits (briefly, and never forever: the caller may be the panic
    /// hook of a thread that holds the lock) for a writer already inside its critical
    /// section, so that its entry is published before the first restore pass reads.
    pub fn begin_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + SHUTDOWN_BARRIER;
        loop {
            match self.writers.try_lock() {
                Ok(_) | Err(TryLockError::Poisoned(_)) => return,
                Err(TryLockError::WouldBlock) if Instant::now() >= deadline => return,
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    /// Whether [`Self::begin_shutdown`] has been called.
    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Run `change` on the registry, then publish the result. If `change` panics, nothing
    /// is published and the last published set stays what [`Self::restore_set`] returns.
    pub fn update<R>(&self, change: impl FnOnce(&mut RestoreRegistry<H>) -> R) -> R {
        let mut registry = lock(&self.writers);
        let result = change(&mut registry);
        let snapshot = Arc::new(registry.snapshot());
        *lock(&self.published) = snapshot;
        result
    }

    /// A window was parked: remember where to put it back. Keeps the first target if the
    /// window is already registered.
    /// Returns whether the window was registered: `false` once shutdown has begun, and the
    /// caller must then not move the window.
    pub fn park(&self, key: u64, handle: H, restore_to: Point) -> bool {
        self.update(|r| {
            if self.is_shutting_down() {
                return false;
            }
            r.register(key, handle, restore_to);
            true
        })
    }

    /// Like [`Self::park`], but replaces an existing entry for `key` in one step. When
    /// refused (shutdown), the existing entry is left in place.
    pub fn park_replacing(&self, key: u64, handle: H, restore_to: Point) -> bool {
        self.update(|r| {
            if self.is_shutting_down() {
                return false;
            }
            r.forget(key);
            r.register(key, handle, restore_to);
            true
        })
    }

    /// Record how many displays the reactor currently sees.
    pub fn set_display_count(&self, count: usize) {
        self.displays.store(count, Ordering::SeqCst);
    }

    /// How many displays the reactor last saw (0 if never told).
    pub fn display_count(&self) -> usize {
        self.displays.load(Ordering::SeqCst)
    }

    /// Change a parked window's handle (its parked position, once the app has settled).
    pub fn update_handle(&self, key: u64, change: impl FnOnce(&mut H)) -> bool {
        self.update(|r| r.update_handle(key, change))
    }

    /// A window was shown again (or is gone): it no longer needs restoring.
    pub fn unpark(&self, key: u64) {
        self.update(|r| r.forget(key));
    }

    /// Re-point a parked window's restore target. Returns whether it is registered.
    pub fn set_target(&self, key: u64, target: Point) -> bool {
        self.update(|r| r.set_target(key, target))
    }

    /// Everything that needs restoring, in park order. Never waits for a writer.
    pub fn restore_set(&self) -> Arc<Vec<(H, Point)>> {
        Arc::clone(&lock(&self.published))
    }

    /// How many windows currently need restoring.
    pub fn len(&self) -> usize {
        self.restore_set().len()
    }

    /// Whether nothing needs restoring.
    pub fn is_empty(&self) -> bool {
        self.restore_set().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_returns_originals_in_order_then_empty() {
        let mut r = RestoreRegistry::new();
        r.register(1, "a", Point::new(1.0, 2.0));
        r.register(2, "b", Point::new(3.0, 4.0));
        assert_eq!(r.len(), 2);
        assert_eq!(r.drain(), vec![("a", Point::new(1.0, 2.0)), ("b", Point::new(3.0, 4.0))]);
        assert!(r.is_empty());
        assert!(r.drain().is_empty());
    }

    #[test]
    fn first_registration_wins() {
        let mut r = RestoreRegistry::new();
        r.register(1, "a", Point::new(1.0, 1.0));
        r.register(1, "a2", Point::new(-999.0, 999.0)); // parked position must not overwrite
        assert_eq!(r.drain(), vec![("a", Point::new(1.0, 1.0))]);
    }

    #[test]
    fn snapshot_is_repeatable_and_does_not_drain() {
        let mut r = RestoreRegistry::new();
        r.register(1, "a", Point::new(1.0, 2.0));
        assert_eq!(r.snapshot(), vec![("a", Point::new(1.0, 2.0))]);
        assert_eq!(r.snapshot().len(), 1);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn forget_removes_only_that_key() {
        let mut r = RestoreRegistry::new();
        r.register(1, "a", Point::new(0.0, 0.0));
        r.register(2, "b", Point::new(0.0, 0.0));
        r.forget(1);
        r.forget(42);
        assert_eq!(r.drain().len(), 1);
    }

    #[test]
    fn set_target_moves_only_that_entry() {
        let mut r = RestoreRegistry::new();
        r.register(1, "a", Point::new(0.0, 0.0));
        r.register(2, "b", Point::new(5.0, 5.0));
        assert!(r.set_target(1, Point::new(9.0, 9.0)));
        assert!(!r.set_target(3, Point::new(1.0, 1.0)));
        assert!(r.contains(1) && !r.contains(3));
        assert_eq!(r.snapshot(), vec![("a", Point::new(9.0, 9.0)), ("b", Point::new(5.0, 5.0))]);
    }

    fn handle(pid: i32) -> ParkedHandle {
        ParkedHandle {
            pid,
            window_id: None,
            parked: Point::new(-399.0, 1079.0),
            size: Size::new(400.0, 300.0),
        }
    }

    fn set(reg: &SharedRegistry<ParkedHandle>) -> Vec<(i32, Point)> {
        reg.restore_set().iter().map(|(h, p)| (h.pid, *p)).collect()
    }

    #[test]
    fn park_then_unpark_leaves_exactly_what_is_still_parked() {
        let reg = SharedRegistry::new();
        for k in 1..=4u64 {
            reg.park(k, handle(k as i32), Point::new(k as f64, 10.0));
        }
        assert_eq!(set(&reg).len(), 4);
        reg.unpark(2);
        reg.unpark(4);
        reg.unpark(99); // never parked: harmless
        assert_eq!(set(&reg), vec![(1, Point::new(1.0, 10.0)), (3, Point::new(3.0, 10.0))]);
        // Re-parked after being shown: registered again, with its new target.
        reg.park(2, handle(2), Point::new(2.0, 20.0));
        assert_eq!(
            set(&reg),
            vec![(1, Point::new(1.0, 10.0)), (3, Point::new(3.0, 10.0)), (2, Point::new(2.0, 20.0))]
        );
        reg.unpark(1);
        reg.unpark(2);
        reg.unpark(3);
        assert!(reg.is_empty());
    }

    #[test]
    fn parking_an_already_parked_window_keeps_its_original_target() {
        let reg = SharedRegistry::new();
        reg.park(1, handle(1), Point::new(100.0, 100.0));
        reg.park(1, handle(1), Point::new(-399.0, 1079.0)); // the parked position
        assert_eq!(set(&reg), vec![(1, Point::new(100.0, 100.0))]);
        assert!(reg.set_target(1, Point::new(50.0, 60.0)));
        assert!(!reg.set_target(2, Point::new(0.0, 0.0)));
        assert_eq!(set(&reg), vec![(1, Point::new(50.0, 60.0))]);
    }

    #[test]
    fn the_restore_set_is_readable_while_this_thread_holds_the_writer_lock() {
        let reg = SharedRegistry::new();
        reg.park(1, handle(1), Point::new(1.0, 1.0));
        reg.update(|r| {
            r.register(2, handle(2), Point::new(2.0, 2.0));
            // The panic hook runs here, inside a writer's critical section. It must not
            // wait for the lock, and sees the last published set.
            assert_eq!(set(&reg), vec![(1, Point::new(1.0, 1.0))]);
        });
        assert_eq!(set(&reg).len(), 2);
    }

    #[test]
    fn a_writer_that_panics_publishes_nothing_and_does_not_break_the_registry() {
        let reg = SharedRegistry::new();
        reg.park(1, handle(1), Point::new(1.0, 1.0));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reg.update(|r| {
                r.register(2, handle(2), Point::new(2.0, 2.0));
                panic!("writer failed");
            })
        }));
        assert!(outcome.is_err());
        assert_eq!(set(&reg), vec![(1, Point::new(1.0, 1.0))]);
        // The poisoned writer lock is recovered, not fatal.
        reg.park(3, handle(3), Point::new(3.0, 3.0));
        assert!(set(&reg).iter().any(|(pid, _)| *pid == 3));
    }

    #[test]
    fn concurrent_park_and_unpark_converge_on_the_expected_set() {
        let reg = Arc::new(SharedRegistry::new());
        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let reg = Arc::clone(&reg);
                std::thread::spawn(move || {
                    for i in 0..50u64 {
                        let key = t * 100 + i;
                        reg.park(key, handle(key as i32), Point::new(key as f64, 0.0));
                        if i % 2 == 0 {
                            reg.unpark(key);
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let mut pids: Vec<i32> = set(&reg).iter().map(|(p, _)| *p).collect();
        pids.sort_unstable();
        let mut expected: Vec<i32> =
            (0..8u64).flat_map(|t| (0..50u64).filter(|i| i % 2 == 1).map(move |i| (t * 100 + i) as i32)).collect();
        expected.sort_unstable();
        assert_eq!(pids, expected);
    }

    #[test]
    fn nothing_is_registered_once_shutdown_has_begun() {
        let reg = SharedRegistry::new();
        assert!(reg.park(1, handle(1), Point::new(1.0, 1.0)));
        reg.begin_shutdown();
        assert!(reg.is_shutting_down());
        assert!(!reg.park(2, handle(2), Point::new(2.0, 2.0)), "a park after shutdown is refused");
        assert_eq!(set(&reg), vec![(1, Point::new(1.0, 1.0))], "and the set is unchanged");
        reg.unpark(1);
        assert!(reg.is_empty(), "removal still works");
    }

    #[test]
    fn update_handle_changes_only_that_entry() {
        let reg = SharedRegistry::new();
        reg.park(1, handle(1), Point::new(1.0, 1.0));
        reg.park(2, handle(2), Point::new(2.0, 2.0));
        assert!(reg.update_handle(2, |h| h.parked = Point::new(-100.0, 900.0)));
        assert!(!reg.update_handle(3, |h| h.pid = 0));
        let parked: Vec<_> = reg.restore_set().iter().map(|(h, _)| h.parked).collect();
        assert_eq!(parked, vec![Point::new(-399.0, 1079.0), Point::new(-100.0, 900.0)]);
    }

    #[test]
    fn a_park_waiting_on_the_writer_lock_is_refused_if_shutdown_begins_meanwhile() {
        let reg = Arc::new(SharedRegistry::new());
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = {
            let reg = Arc::clone(&reg);
            std::thread::spawn(move || {
                reg.update(|_| {
                    held_tx.send(()).unwrap();
                    let _ = release_rx.recv();
                })
            })
        };
        held_rx.recv().unwrap();
        let parker = {
            let reg = Arc::clone(&reg);
            std::thread::spawn(move || reg.park(1, handle(1), Point::new(1.0, 1.0)))
        };
        std::thread::sleep(std::time::Duration::from_millis(50)); // let it reach the lock
        reg.begin_shutdown();
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(!parker.join().unwrap(), "the flag is checked under the writer lock");
        assert!(reg.is_empty());
    }

    #[test]
    fn begin_shutdown_cannot_deadlock_on_a_writer_the_caller_interrupted() {
        let reg = SharedRegistry::<ParkedHandle>::new();
        reg.update(|_| {
            // The panic hook calling begin_shutdown inside this thread's own critical section.
            reg.begin_shutdown();
        });
        assert!(reg.is_shutting_down());
    }

    #[test]
    fn park_replacing_swaps_the_entry_and_keeps_the_old_one_if_refused() {
        let reg = SharedRegistry::new();
        reg.park(1, handle(1), Point::new(1.0, 1.0));
        assert!(reg.park_replacing(1, handle(1), Point::new(9.0, 9.0)));
        assert_eq!(set(&reg), vec![(1, Point::new(9.0, 9.0))]);
        reg.begin_shutdown();
        assert!(!reg.park_replacing(1, handle(1), Point::new(5.0, 5.0)));
        assert_eq!(set(&reg), vec![(1, Point::new(9.0, 9.0))], "the entry was not lost");
    }

    #[test]
    fn the_display_count_the_reactor_last_saw_is_shared() {
        let reg = SharedRegistry::<ParkedHandle>::new();
        assert_eq!(reg.display_count(), 0);
        reg.set_display_count(3);
        assert_eq!(reg.display_count(), 3);
    }
}

//! Crash safety: put every parked window back on-screen however dowm ends.
//!
//! Corner parking strands windows past the edge of a display. A WM that parks must restore
//! them on SIGTERM, SIGINT, SIGHUP and SIGQUIT, on a normal exit, and on a panic, or a bad
//! exit leaves the user's windows unreachable (PLAN.md Finding C). SIGKILL and a hard crash
//! cannot be caught; the manual recovery for those is in `M0_REPORT.md`.
//!
//! Design:
//! - The reactor records every parked window and its restore target in a
//!   [`SharedRegistry`]. This module only reads it.
//! - Signals are never handled in the signal handler itself. `signal-hook` runs the real
//!   (async-signal-safe) handler, which only writes to a self-pipe; a normal thread reads
//!   the pipe, restores, and exits. Anything may run there, including AX calls.
//! - The panic hook runs on the panicking thread, possibly with locks held. It reads
//!   [`SharedRegistry::restore_set`], which takes no lock a writer holds, and
//!   [`Terminator::restore_all`] never blocks on itself: a nested call on the thread that
//!   is already restoring returns at once, and another thread's restore is waited for
//!   only up to the backstop.
//! - Restoring comes first and logging after it, and logging ignores write errors: with a
//!   dead terminal (SIGHUP after the window closed) `eprintln!` would panic, and a panic
//!   in the panic hook aborts before anything is restored.
//! - Shutdown is announced before the first pass ([`SharedRegistry::begin_shutdown`] and,
//!   for the real handlers, [`process_shutting_down`]), so a hide still in flight cannot
//!   re-park a window behind the restore.
//! - Windows are restored one thread per [`Restorer::group`] (the daemon: per app), all
//!   under one deadline, so one hung app cannot starve the rest. Restoring is idempotent
//!   and done twice with a short pause, which catches a write that was still queued.
//! - A watchdog thread started by `restore_all` itself bounds the panic and exit paths
//!   too. A panic on a restore worker thread is contained (the hook ignores it).
//!
//! A second termination signal that arrives while the first restore is running is not
//! acted on: the signal thread is busy restoring and reads the next one only afterwards
//! (it is then already exiting). The restore is bounded by the backstop, so the worst case
//! for an impatient user is that wait.
//!
//! The windows are put back by a [`Restorer`]; the daemon's uses AX (`platform::ax_restorer`).

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::geometry::Point;
use crate::restore::SharedRegistry;

/// The signals that end dowm gracefully.
pub const TERMINATION_SIGNALS: [i32; 4] = [SIGTERM, SIGINT, SIGHUP, SIGQUIT];

/// Extra time the watchdog gives `restore_all` beyond the backstop before it acts.
const WATCHDOG_GRACE: Duration = Duration::from_millis(250);

static PROCESS_SHUTDOWN: AtomicBool = AtomicBool::new(false);

thread_local! {
    static IN_RESTORE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the real termination handlers have started shutting the process down. The
/// presenter and the app actors check this and drop further frame writes.
pub fn process_shutting_down() -> bool { PROCESS_SHUTDOWN.load(Ordering::SeqCst) }

/// Whether this thread is a restore worker. A panic there is contained by the worker, so
/// the panic hook must neither restore again nor chain to the abort hook.
pub fn in_restore_scope() -> bool { IN_RESTORE.with(|c| c.get()) }

/// Write a line to stderr, ignoring errors. `eprintln!` panics when stderr is broken
/// (EIO on a dead pty, EPIPE), which on a termination path would abort before restoring.
pub fn log_line(args: std::fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr(), "{args}");
}

/// Moves windows back to their restore targets. Called from restore worker threads, so it
/// must not depend on reactor state or locks.
pub trait Restorer<H>: Send + Sync {
    /// Windows with the same group are restored together on one thread, and a hang in one
    /// group does not hold up the others. The daemon groups by pid.
    fn group(&self, _handle: &H) -> i64 { 0 }

    /// Put each window back at its target. Best effort: log failures and keep going; a
    /// panic here costs the rest of the group.
    fn restore(&self, windows: &[(H, Point)]);
}

/// Timing and exit behaviour of the termination handler.
#[derive(Clone, Copy, Debug)]
pub struct TerminationConfig {
    /// Pause between the two restore passes.
    pub settle: Duration,
    /// Longest the whole restore may take, including waiting for another thread's restore.
    /// A restorer group still running then is abandoned.
    pub backstop: Duration,
    /// Run by the watchdog if `restore_all` is still going well after the backstop.
    pub on_timeout: fn(),
}

/// The default timeout action. It must not log: the stall may be stderr itself.
fn exit_on_timeout() { std::process::exit(124) }

impl Default for TerminationConfig {
    fn default() -> Self {
        Self {
            settle: Duration::from_millis(50),
            backstop: Duration::from_secs(5),
            on_timeout: exit_on_timeout,
        }
    }
}

/// Runs `on_timeout` after `after` unless dropped first.
struct Watchdog(#[allow(dead_code)] mpsc::Sender<()>);

impl Watchdog {
    fn start(after: Duration, on_timeout: fn()) -> Option<Self> {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("dowm-restore-watchdog".into())
            .spawn(move || {
                if rx.recv_timeout(after) == Err(mpsc::RecvTimeoutError::Timeout) {
                    on_timeout();
                }
            })
            .ok()?;
        Some(Watchdog(tx))
    }
}

/// Restores every window in a [`SharedRegistry`] with a [`Restorer`].
pub struct Terminator<H> {
    registry: Arc<SharedRegistry<H>>,
    restorer: Arc<dyn Restorer<H>>,
    config: TerminationConfig,
    /// Held for the duration of a restore.
    running: Mutex<()>,
    /// The thread inside `running`; only ever locked for a read or a store.
    owner: Mutex<Option<ThreadId>>,
}

impl<H: Clone + Send + Sync + 'static> Terminator<H> {
    /// A terminator that restores what `registry` holds using `restorer`.
    pub fn new(
        registry: Arc<SharedRegistry<H>>,
        restorer: Arc<dyn Restorer<H>>,
        config: TerminationConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry,
            restorer,
            config,
            running: Mutex::new(()),
            owner: Mutex::new(None),
        })
    }

    /// Restore every parked window now. Safe to call from a panic hook or a signal thread
    /// and from several at once; see the module docs. Returns within the backstop.
    pub fn restore_all(&self) { self.restore_then(|| ()) }

    /// [`Self::restore_all`], then `after`, with the watchdog still armed while `after`
    /// runs. Put the final logging, the chained panic hook or the `exit` there: a stderr
    /// that stopped draining then blocks `after`, not the exit.
    pub fn restore_then<R>(&self, after: impl FnOnce() -> R) -> R {
        let _watchdog = if in_restore_scope() {
            None
        } else {
            Watchdog::start(self.config.backstop + WATCHDOG_GRACE, self.config.on_timeout)
        };
        if !in_restore_scope() {
            self.restore_passes();
        }
        after()
    }

    fn restore_passes(&self) {
        // Before anything else, so no hide can register a window behind the passes.
        self.registry.begin_shutdown();

        let me = thread::current().id();
        let deadline = Instant::now() + self.config.backstop;
        let _running = loop {
            match self.running.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    if *self.owner.lock().unwrap_or_else(|e| e.into_inner()) == Some(me) {
                        return; // the restorer itself panicked or recursed
                    }
                    if Instant::now() >= deadline {
                        return;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
        };
        *self.owner.lock().unwrap_or_else(|e| e.into_inner()) = Some(me);
        for pass in 0..2 {
            if pass > 0 {
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(self.config.settle.min(remaining));
            }
            let windows = self.registry.restore_set();
            if windows.is_empty() || !self.run_pass(&windows, deadline) {
                break;
            }
        }
        *self.owner.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// One pass: a thread per group, all joined against `deadline`. Returns false if the
    /// deadline passed with groups still running.
    fn run_pass(&self, windows: &[(H, Point)], deadline: Instant) -> bool {
        let mut groups: BTreeMap<i64, Vec<(H, Point)>> = BTreeMap::new();
        for (handle, target) in windows {
            groups.entry(self.restorer.group(handle)).or_default().push((handle.clone(), *target));
        }
        let (done_tx, done_rx) = mpsc::channel();
        let mut running = 0;
        for (group, members) in groups {
            let restorer = Arc::clone(&self.restorer);
            let done = done_tx.clone();
            let work = {
                let members = members.clone();
                move || run_group(&*restorer, group, &members)
            };
            let spawned = thread::Builder::new().name("dowm-restore".into()).spawn(move || {
                work();
                let _ = done.send(());
            });
            match spawned {
                Ok(_) => running += 1,
                Err(e) => {
                    // No thread to be had: restoring this group late and in line beats not at all.
                    log_line(format_args!("dowm: cannot spawn a restore thread ({e}); restoring inline"));
                    run_group(&*self.restorer, group, &members);
                }
            }
        }
        drop(done_tx);
        for _ in 0..running {
            let wait = deadline.saturating_duration_since(Instant::now());
            if done_rx.recv_timeout(wait).is_err() {
                log_line(format_args!("dowm: gave up waiting for a restore group"));
                return false;
            }
        }
        true
    }
}

/// Restore one group on the current thread, containing a panic.
fn run_group<H>(restorer: &dyn Restorer<H>, group: i64, members: &[(H, Point)]) {
    let was = IN_RESTORE.with(|c| c.replace(true));
    let result = panic::catch_unwind(AssertUnwindSafe(|| restorer.restore(members)));
    IN_RESTORE.with(|c| c.set(was));
    if result.is_err() {
        log_line(format_args!("dowm: restorer panicked in group {group}"));
    }
}

/// Restores on drop, which covers a normal exit and an unwind that reaches it.
#[must_use = "dropping the guard restores the parked windows"]
pub struct TerminationGuard<H: Clone + Send + Sync + 'static> {
    terminator: Arc<Terminator<H>>,
    /// Set by [`install`]: this guard belongs to the process's real handlers.
    process_wide: bool,
}

impl<H: Clone + Send + Sync + 'static> Drop for TerminationGuard<H> {
    fn drop(&mut self) {
        if self.process_wide {
            PROCESS_SHUTDOWN.store(true, Ordering::SeqCst);
        }
        self.terminator.restore_all();
    }
}

/// Install the panic hook and the signal thread. Call once, early, after any hook that
/// should still run (the previous hook is chained and runs after the restore).
pub fn install<H: Clone + Send + Sync + 'static>(
    terminator: Arc<Terminator<H>>,
) -> io::Result<TerminationGuard<H>> {
    let previous = panic::take_hook();
    let on_panic = Arc::clone(&terminator);
    panic::set_hook(Box::new(move |info| {
        if in_restore_scope() {
            // A restore worker's panic is caught by the worker; it must not abort the
            // process (the chained hook may) or start a second restore.
            log_line(format_args!("dowm: panic while restoring (contained): {info}"));
            return;
        }
        PROCESS_SHUTDOWN.store(true, Ordering::SeqCst);
        on_panic.restore_then(|| {
            log_line(format_args!("dowm: panic, parked windows restored"));
            previous(info);
        });
    }));

    let mut signals = Signals::new(TERMINATION_SIGNALS)?;
    let on_signal = Arc::clone(&terminator);
    thread::Builder::new().name("dowm-signals".into()).spawn(move || {
        let Some(signal) = signals.forever().next() else { return };
        PROCESS_SHUTDOWN.store(true, Ordering::SeqCst);
        on_signal.restore_then(|| {
            log_line(format_args!("dowm: caught signal {signal}, parked windows restored"));
            std::process::exit(128 + signal);
        });
    })?;

    Ok(TerminationGuard { terminator, process_wide: true })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[derive(Default)]
    struct Recorder {
        passes: Mutex<Vec<Vec<(u32, Point)>>>,
    }

    impl Restorer<u32> for Recorder {
        fn restore(&self, windows: &[(u32, Point)]) {
            self.passes.lock().unwrap().push(windows.to_vec());
        }
    }

    fn fast() -> TerminationConfig {
        TerminationConfig {
            settle: Duration::ZERO,
            backstop: Duration::from_millis(500),
            on_timeout: || {},
        }
    }

    fn setup(restorer: Arc<dyn Restorer<u32>>) -> (Arc<SharedRegistry<u32>>, Arc<Terminator<u32>>) {
        let registry = Arc::new(SharedRegistry::new());
        registry.park(1, 11, Point::new(10.0, 20.0));
        registry.park(2, 22, Point::new(30.0, 40.0));
        let terminator = Terminator::new(Arc::clone(&registry), restorer, fast());
        (registry, terminator)
    }

    fn guard(terminator: Arc<Terminator<u32>>) -> TerminationGuard<u32> {
        TerminationGuard { terminator, process_wide: false }
    }

    #[test]
    fn restore_all_puts_back_exactly_the_parked_windows_twice() {
        let rec = Arc::new(Recorder::default());
        let (registry, terminator) = setup(rec.clone());
        registry.unpark(1);
        terminator.restore_all();
        let passes = rec.passes.lock().unwrap();
        assert_eq!(passes.len(), 2, "two passes catch a late park write");
        for pass in passes.iter() {
            assert_eq!(pass, &vec![(22, Point::new(30.0, 40.0))]);
        }
    }

    #[test]
    fn nothing_parked_means_nothing_restored() {
        let rec = Arc::new(Recorder::default());
        let registry = Arc::new(SharedRegistry::new());
        let terminator = Terminator::new(registry, rec.clone(), fast());
        terminator.restore_all();
        assert!(rec.passes.lock().unwrap().is_empty());
    }

    #[test]
    fn dropping_the_guard_restores_on_a_normal_exit() {
        let rec = Arc::new(Recorder::default());
        let (_registry, terminator) = setup(rec.clone());
        drop(guard(terminator));
        assert_eq!(rec.passes.lock().unwrap()[0].len(), 2);
    }

    /// Tries to park a window while the restore is running, like a hide still in flight.
    struct ParksDuringRestore {
        registry: Arc<SharedRegistry<u32>>,
        saw_shutdown: AtomicBool,
        reparked: AtomicBool,
    }

    impl Restorer<u32> for ParksDuringRestore {
        fn restore(&self, _: &[(u32, Point)]) {
            self.saw_shutdown.store(self.registry.is_shutting_down(), Ordering::SeqCst);
            let accepted = self.registry.park(99, 99, Point::new(0.0, 0.0));
            self.reparked.store(accepted, Ordering::SeqCst);
        }
    }

    #[test]
    fn shutdown_is_announced_before_the_first_pass_and_blocks_reparking() {
        let registry = Arc::new(SharedRegistry::new());
        registry.park(1, 11, Point::new(10.0, 20.0));
        let restorer = Arc::new(ParksDuringRestore {
            registry: Arc::clone(&registry),
            saw_shutdown: AtomicBool::new(false),
            reparked: AtomicBool::new(true),
        });
        let terminator = Terminator::new(Arc::clone(&registry), restorer.clone(), fast());
        terminator.restore_all();
        assert!(restorer.saw_shutdown.load(Ordering::SeqCst), "flag set before the first pass");
        assert!(!restorer.reparked.load(Ordering::SeqCst), "the park was refused");
        assert_eq!(registry.len(), 1, "the registry was not re-populated");
    }

    struct Reentrant(Mutex<Option<Arc<Terminator<u32>>>>, Mutex<u32>);

    impl Restorer<u32> for Reentrant {
        fn restore(&self, _: &[(u32, Point)]) {
            *self.1.lock().unwrap() += 1;
            let terminator = self.0.lock().unwrap().clone().unwrap();
            terminator.restore_all(); // what the panic hook does on a restore worker
        }
    }

    #[test]
    fn a_nested_restore_from_a_restore_worker_returns_instead_of_deadlocking() {
        let restorer = Arc::new(Reentrant(Mutex::new(None), Mutex::new(0)));
        let (_registry, terminator) = setup(restorer.clone());
        *restorer.0.lock().unwrap() = Some(Arc::clone(&terminator));
        terminator.restore_all();
        assert_eq!(*restorer.1.lock().unwrap(), 2, "one call per pass, none from the nesting");
        *restorer.0.lock().unwrap() = None; // break the Arc cycle
    }

    struct Panicking;

    impl Restorer<u32> for Panicking {
        fn restore(&self, _: &[(u32, Point)]) {
            panic!("restorer bug");
        }
    }

    #[test]
    fn a_panicking_restorer_is_contained_and_marks_its_thread_as_a_restore_worker() {
        let (_registry, terminator) = setup(Arc::new(Panicking));
        terminator.restore_all();
        terminator.restore_all(); // and the terminator is still usable
        assert!(!in_restore_scope(), "the calling thread is not a restore worker");
    }

    /// Groups by `h / 10`; group 0 (handles below 10) hangs.
    struct OneGroupHangs {
        restored: Mutex<Vec<u32>>,
    }

    impl Restorer<u32> for OneGroupHangs {
        fn group(&self, h: &u32) -> i64 { i64::from(*h / 10) }

        fn restore(&self, windows: &[(u32, Point)]) {
            if windows.iter().any(|(h, _)| *h < 10) {
                thread::sleep(Duration::from_secs(5));
            }
            self.restored.lock().unwrap().extend(windows.iter().map(|(h, _)| *h));
        }
    }

    #[test]
    fn one_hung_group_does_not_starve_the_others_and_restore_all_still_returns_in_time() {
        let registry = Arc::new(SharedRegistry::new());
        registry.park(1, 5, Point::new(0.0, 0.0)); // group 0: hangs
        registry.park(2, 15, Point::new(0.0, 0.0)); // group 1
        registry.park(3, 25, Point::new(0.0, 0.0)); // group 2
        let restorer = Arc::new(OneGroupHangs { restored: Mutex::default() });
        let config = TerminationConfig {
            settle: Duration::ZERO,
            backstop: Duration::from_millis(400),
            on_timeout: || {},
        };
        let terminator = Terminator::new(registry, restorer.clone(), config);
        let started = Instant::now();
        terminator.restore_all();
        assert!(started.elapsed() < Duration::from_secs(2), "bounded by the backstop");
        let mut restored = restorer.restored.lock().unwrap().clone();
        restored.sort_unstable();
        restored.dedup();
        assert_eq!(restored, vec![15, 25]);
    }

    #[test]
    fn the_watchdog_fires_unless_it_is_dropped_first() {
        static FIRED: AtomicUsize = AtomicUsize::new(0);
        let kept = Watchdog::start(Duration::from_millis(50), || {
            FIRED.fetch_add(1, Ordering::SeqCst);
        });
        let dropped = Watchdog::start(Duration::from_millis(50), || {
            FIRED.fetch_add(100, Ordering::SeqCst);
        });
        drop(dropped);
        thread::sleep(Duration::from_millis(300));
        assert_eq!(FIRED.load(Ordering::SeqCst), 1, "only the watchdog that was kept fired");
        drop(kept);
    }

    /// Counts how many restores run at once.
    struct Overlap {
        started: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<mpsc::Receiver<()>>,
        running: AtomicUsize,
        max: AtomicUsize,
        calls: AtomicUsize,
    }

    impl Restorer<u32> for Overlap {
        fn restore(&self, _: &[(u32, Point)]) {
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.max.fetch_max(now, Ordering::SeqCst);
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                if let Some(tx) = self.started.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let _ = self.release.lock().unwrap().recv_timeout(Duration::from_secs(2));
            }
            self.running.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn another_threads_restore_is_waited_for_not_raced() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let restorer = Arc::new(Overlap {
            started: Mutex::new(Some(started_tx)),
            release: Mutex::new(release_rx),
            running: AtomicUsize::new(0),
            max: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        });
        let registry = Arc::new(SharedRegistry::new());
        registry.park(1, 1, Point::new(0.0, 0.0));
        let config = TerminationConfig {
            settle: Duration::ZERO,
            backstop: Duration::from_secs(5),
            on_timeout: || {},
        };
        let terminator = Terminator::new(registry, restorer.clone(), config);

        let first = {
            let t = Arc::clone(&terminator);
            thread::spawn(move || t.restore_all())
        };
        started_rx.recv_timeout(Duration::from_secs(2)).expect("first restore began");
        let entered = Arc::new(AtomicBool::new(false));
        let second = {
            let (t, entered) = (Arc::clone(&terminator), Arc::clone(&entered));
            thread::spawn(move || {
                entered.store(true, Ordering::SeqCst);
                t.restore_all(); // the panic hook of a second thread: waits for the first
            })
        };
        while !entered.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        release_tx.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        assert_eq!(restorer.max.load(Ordering::SeqCst), 1, "restores never overlapped");
        assert!(restorer.calls.load(Ordering::SeqCst) >= 3, "the second thread restored too");
    }
}

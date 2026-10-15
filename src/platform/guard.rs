//! Crash safety and cooperative shutdown.
//!
//! Corner parking strands windows off-screen without restoration (PLAN.md Finding C).
//! Shutdown is cooperative: a signal (or a panic) raises [`STOP`]; the tick source and run
//! loop observe it, the writer finishes its current write and exits, and the main thread
//! restores windows *after* the writer is gone so a late write cannot re-park a window.
//! The signal thread keeps a hard backstop for a wedged AX call.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use super::ax::AxElement;
use crate::geometry::Point;
use crate::restore::RestoreRegistry;

/// Raised by a signal or a panic; long-running loops must observe it and wind down.
pub static STOP: AtomicBool = AtomicBool::new(false);
/// The first signal received (0 if none), for the exit status.
pub static SIGNAL: AtomicI32 = AtomicI32::new(0);

/// How long the signal thread waits for cooperative shutdown before restoring and exiting.
const BACKSTOP: Duration = Duration::from_secs(3);

pub fn stopping() -> bool {
    STOP.load(Ordering::Acquire)
}

#[derive(Default)]
struct Touched {
    windows: RestoreRegistry<AxElement>,
    enhanced_ui_apps: Vec<AxElement>,
}

fn touched() -> MutexGuard<'static, Touched> {
    static T: OnceLock<Mutex<Touched>> = OnceLock::new();
    T.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// Remember a window's original position before the first move.
pub fn register_window(key: u64, window: AxElement, original: Point) {
    touched().windows.register(key, window, original);
}

/// Drop every registered window (after [`restore_all`] has put them back), e.g. before the
/// helper process that owns them is killed between benchmark rounds.
pub fn forget_windows() {
    touched().windows.drain();
}

/// Remember that we turned `AXEnhancedUserInterface` off on this app.
pub fn register_enhanced_ui(app: AxElement) {
    touched().enhanced_ui_apps.push(app);
}

/// Move every registered window back to its original position and re-enable enhanced UI.
/// Window positions are re-applied on every call (cheap, idempotent) so a final call after
/// the writer has stopped always wins over any late write.
pub fn restore_all() {
    static SERIAL: Mutex<()> = Mutex::new(());
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (windows, apps) = {
        let mut t = touched();
        (t.windows.snapshot(), std::mem::take(&mut t.enhanced_ui_apps))
    };
    for (w, original) in windows {
        if let Err(e) = w.set_position(original) {
            eprintln!("dowm: failed to restore window to ({}, {}): {e}", original.x, original.y);
        }
    }
    for app in apps {
        if let Err(e) = app.restore_enhanced_ui() {
            eprintln!("dowm: failed to re-enable AXEnhancedUserInterface: {e}");
        }
    }
}

/// Install the panic hook and the signal thread. `on_exit` runs after restoration (e.g. to
/// kill a spawned helper process).
pub fn install_handlers(on_exit: fn()) {
    static IN_PANIC_HOOK: AtomicBool = AtomicBool::new(false);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        prev(info);
        // Tell every loop to wind down, then restore once; the re-entrancy guard keeps a
        // panic inside the restore path (or a second panicking thread) from recursing.
        STOP.store(true, Ordering::Release);
        if !IN_PANIC_HOOK.swap(true, Ordering::AcqRel) {
            restore_all();
            on_exit();
            IN_PANIC_HOOK.store(false, Ordering::Release);
        }
    }));

    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP]).expect("install signal handlers");
    std::thread::Builder::new()
        .name("dowm-signals".into())
        .spawn(move || {
            for sig in signals.forever() {
                SIGNAL.compare_exchange(0, sig, Ordering::AcqRel, Ordering::Acquire).ok();
                if !STOP.swap(true, Ordering::AcqRel) {
                    eprintln!("\ndowm: caught signal {sig}, stopping (press Ctrl-C again to force)");
                    std::thread::spawn(move || {
                        std::thread::sleep(BACKSTOP);
                        eprintln!("dowm: shutdown timed out, restoring windows and exiting");
                        restore_all();
                        on_exit();
                        std::process::exit(128 + sig);
                    });
                } else {
                    restore_all();
                    on_exit();
                    std::process::exit(128 + sig);
                }
            }
        })
        .expect("spawn signal thread");
}

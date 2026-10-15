//! CADisplayLink wrapper. The main-run-loop callback only publishes the latest tick
//! (latest-wins, bounded(1) wake), as in Rift `src/sys/display_link.rs` (Apache-2.0).
//! Blocking AX IPC never runs here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use objc2::rc::Retained;
use objc2::{AnyThread, DeclaredClass, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::NSScreen;
use objc2_foundation::{NSObject, NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes};
use objc2_quartz_core::{CACurrentMediaTime, CADisplayLink};

use crate::runner::{Tick, TickSource};

/// How often a blocked consumer re-checks the stop flag.
const POLL: Duration = Duration::from_millis(100);

struct Shared {
    latest: Mutex<Option<Tick>>,
    wake: Sender<()>,
    stop: AtomicBool,
}

impl Shared {
    fn publish(&self, tick: Tick) {
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(tick);
        let _ = self.wake.try_send(());
    }
}

define_class! {
    #[unsafe(super(NSObject))]
    #[ivars = Arc<Shared>]
    struct DisplayLinkTarget;

    impl DisplayLinkTarget {
        #[unsafe(method(tick:))]
        fn tick(&self, link: &CADisplayLink) {
            let shared = self.ivars();
            if shared.stop.load(Ordering::Acquire) {
                link.invalidate();
                return;
            }
            shared.publish(Tick { timestamp: link.timestamp(), target_timestamp: link.targetTimestamp() });
        }
    }
}

/// Main-thread-owned handle. Dropping it stops the link.
pub struct DisplayLink {
    link: Option<Retained<CADisplayLink>>,
    _target: Option<Retained<DisplayLinkTarget>>,
    shared: Arc<Shared>,
    /// Human-readable description of the tick source actually in use.
    pub kind: &'static str,
}

/// Send-able consumer side.
pub struct ChannelTickSource {
    wake: Receiver<()>,
    shared: Arc<Shared>,
    idle_timeout: Duration,
}

impl TickSource for ChannelTickSource {
    fn next_tick(&mut self) -> Option<Tick> {
        let mut idle = Duration::ZERO;
        loop {
            if super::guard::stopping() {
                return None;
            }
            match self.wake.recv_timeout(POLL) {
                Ok(()) => {
                    if let Some(t) = self.shared.latest.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        return Some(t);
                    }
                }
                Err(_) => {
                    idle += POLL;
                    if idle >= self.idle_timeout {
                        eprintln!("dowm: no display ticks for {:?}; giving up", self.idle_timeout);
                        return None;
                    }
                }
            }
        }
    }
}

impl ChannelTickSource {
    /// Discard any stale tick (between benchmark phases).
    pub fn flush(&mut self) {
        while self.wake.try_recv().is_ok() {}
        self.shared.latest.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

impl Drop for DisplayLink {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(l) = &self.link {
            l.invalidate();
        }
    }
}

/// Start a display link for `screen` on the main run loop. Falls back to a 60 Hz timer
/// thread (not vsync-aligned; flagged via `kind`) when `NSScreen.displayLinkWithTarget:selector:`
/// (macOS 14+) is unavailable.
pub fn start(_mtm: MainThreadMarker, screen: &NSScreen) -> Result<(DisplayLink, ChannelTickSource), String> {
    let (tx, rx) = bounded::<()>(1);
    let shared = Arc::new(Shared { latest: Mutex::new(None), wake: tx, stop: AtomicBool::new(false) });
    let source = ChannelTickSource { wake: rx, shared: shared.clone(), idle_timeout: Duration::from_secs(3) };

    let available = screen.respondsToSelector(sel!(displayLinkWithTarget:selector:));
    if available {
        let target = DisplayLinkTarget::alloc().set_ivars(shared.clone());
        let target: Retained<DisplayLinkTarget> = unsafe { msg_send![super(target), init] };
        let link = unsafe { screen.displayLinkWithTarget_selector(&target, sel!(tick:)) };
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };
        return Ok((
            DisplayLink { link: Some(link), _target: Some(target), shared, kind: "CADisplayLink (NSScreen.displayLinkWithTarget:selector:)" },
            source,
        ));
    }

    eprintln!("dowm: WARNING NSScreen.displayLinkWithTarget:selector: unavailable (needs macOS 14+); using 60 Hz timer fallback, results are NOT vsync-aligned");
    let sh = shared.clone();
    std::thread::Builder::new()
        .name("dowm-timer-fallback".into())
        .spawn(move || {
            let period = 1.0 / 60.0;
            while !sh.stop.load(Ordering::Acquire) {
                let now = CACurrentMediaTime();
                sh.publish(Tick { timestamp: now, target_timestamp: now + period });
                std::thread::sleep(Duration::from_secs_f64(period));
            }
        })
        .map_err(|e| format!("spawn timer fallback thread: {e}"))?;
    Ok((DisplayLink { link: None, _target: None, shared, kind: "60 Hz timer thread (FALLBACK, not vsync)" }, source))
}

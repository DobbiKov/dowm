//! Focus-follows-mouse.
//!
//! A listen-only tap on mouse-moved events feeds pointer positions to a debounce thread.
//! When the pointer has rested for [`SETTLE`] (and no button is down, so drags and
//! resizes are left alone) the thread asks the WindowServer which window is under it and
//! sends [`Event::PointerSettledOn`]. The reactor decides what to do with it: it ignores
//! the event unless `focus-follows-mouse` is on, and ignores windows that are parked,
//! hidden with their workspace, minimized or already focused.
//!
//! The tap callback only stores the latest position and signals the thread. The switch is
//! read live from [`Shared`], so toggling the setting needs no restart.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::{RecvTimeoutError, Sender, TrySendError};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventTapLocation, CGEventTapOptions, CGEventTapProxy, CGEventType,
};
use tracing::{info, warn};

use super::Shared;
use crate::actor::reactor::{Event, Sender as ReactorSender};
use crate::sys::event::{MouseState, get_mouse_state};
use crate::sys::event_tap::EventTap;
use crate::sys::window_server::get_window_at_point;

/// How long the pointer must rest before it counts as pointing at a window.
pub const SETTLE: Duration = Duration::from_millis(90);

struct TapContext {
    shared: Arc<Shared>,
    positions: Sender<CGPoint>,
    /// The debounce thread is gone; reported once, not per mouse event.
    gone_reported: AtomicBool,
    invalid: Arc<AtomicBool>,
}

unsafe extern "C-unwind" fn move_callback(
    _proxy: CGEventTapProxy,
    _etype: CGEventType,
    event: core::ptr::NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    if !user_info.is_null() {
        let context = unsafe { &*(user_info as *const TapContext) };
        if context.shared.focus_follows_mouse() {
            let point = CGEvent::location(Some(unsafe { event.as_ref() }));
            // A full channel means the debounce thread is behind; this position is stale
            // by the time it would be read anyway.
            if let Err(TrySendError::Disconnected(_)) = context.positions.try_send(point)
                && !context.gone_reported.swap(true, Ordering::Relaxed)
            {
                warn!("pointer debounce thread is gone");
            }
        }
    }
    event.as_ptr()
}

unsafe extern "C-unwind" fn tap_invalidated(user_info: *mut c_void) {
    if !user_info.is_null() {
        let context = unsafe { &*(user_info as *const TapContext) };
        context.invalid.store(true, Ordering::Relaxed);
    }
}

unsafe fn drop_context(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(ptr as *mut TapContext) });
    }
}

/// The installed tap and its debounce thread.
pub struct PointerTap {
    _tap: EventTap,
    invalid: Arc<AtomicBool>,
}

impl PointerTap {
    /// Install the tap on the current thread's run loop and start the debounce thread.
    pub fn install(shared: Arc<Shared>, events: ReactorSender) -> Option<Self> {
        let (positions, rx) = crossbeam_channel::bounded::<CGPoint>(64);
        let invalid = Arc::new(AtomicBool::new(false));
        let context = Box::into_raw(Box::new(TapContext {
            shared: Arc::clone(&shared),
            positions,
            gone_reported: AtomicBool::new(false),
            invalid: Arc::clone(&invalid),
        })) as *mut c_void;
        let mask = (1u64 << CGEventType::MouseMoved.0) | (1u64 << CGEventType::OtherMouseDragged.0);
        // SAFETY: as in `HotkeyTap::install`.
        let tap = unsafe {
            EventTap::new(
                CGEventTapLocation::SessionEventTap,
                CGEventTapOptions::ListenOnly,
                mask,
                Some(move_callback),
                context,
                Some(drop_context),
                None,
                Some(tap_invalidated),
            )
        };
        let Some(tap) = tap else {
            unsafe { drop_context(context) };
            return None;
        };
        std::thread::Builder::new()
            .name("pointer-settle".into())
            .spawn(move || debounce(&rx, &shared, &events))
            .ok()?;
        info!("pointer tap installed");
        Some(PointerTap { _tap: tap, invalid })
    }

    /// The tap's Mach port died and could not be revived: drop this and install again.
    pub fn is_invalid(&self) -> bool { self.invalid.load(Ordering::Relaxed) }
}

/// Wait for the pointer to rest, then report the window under it.
fn debounce(rx: &crossbeam_channel::Receiver<CGPoint>, shared: &Shared, events: &ReactorSender) {
    loop {
        let Ok(mut latest) = rx.recv() else { return };
        loop {
            match rx.recv_timeout(SETTLE) {
                Ok(point) => latest = point,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if !shared.focus_follows_mouse() || get_mouse_state() == Some(MouseState::Down) {
            continue;
        }
        if let Some(window) = get_window_at_point(latest) {
            events.send(Event::PointerSettledOn(window));
        }
    }
}

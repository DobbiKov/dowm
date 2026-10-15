//! CGS connection notifications (`SLSRegisterConnectionNotifyProc`).
//!
//! Derived from rift's `src/sys/window_notify.rs` (Apache-2.0; see `NOTICE`), which is
//! based on <https://github.com/koekeishiya/yabai/commit/6f9006dd957100ec13096d187a8865e85a164a9b>.
//! Trimmed to the window create/destroy/reorder subset. `SpaceWindowCreated` and
//! `SpaceWindowDestroyed` are the only per-window create/destroy notifications a
//! connection receives; their leading space id is skipped and never stored or queried.

use std::ffi::c_void;

use cgs::G_CONNECTION;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tracing::{debug, trace, warn};

use super::skylight::{
    CGSEventType, KnownCGSEvent, SLSRegisterConnectionNotifyProc,
    SLSRequestNotificationsForWindows, cid_t,
};
use crate::actor;
use crate::common::collections::HashSet;

/// `SLSRequestNotificationsForWindows` accepts fewer than 1024 windows.
const MAX_NOTIFY_WINDOWS: usize = 1023;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventData {
    pub event_type: CGSEventType,
    pub window_id: Option<u32>,
}

type EventChannel = (actor::Sender<EventData>, Mutex<Option<actor::Receiver<EventData>>>);

/// One channel carries every subscribed event type; `EventData::event_type` tells them apart.
/// It is unbounded, so a send fails only once the dispatcher is gone (shutdown).
static EVENT_CHANNEL: Lazy<EventChannel> = Lazy::new(|| {
    let (tx, rx) = actor::channel::<EventData>();
    (tx, Mutex::new(Some(rx)))
});

static REGISTERED_EVENTS: Lazy<Mutex<HashSet<CGSEventType>>> =
    Lazy::new(|| Mutex::new(HashSet::default()));

/// Register for `event` on the main CGS connection. Returns the CGS result code
/// (0 on success); registering the same event twice is a successful no-op.
pub fn init(event: CGSEventType) -> i32 {
    let mut registered = REGISTERED_EVENTS.lock();
    if registered.contains(&event) {
        debug!("CGS event {event} already registered");
        return 0;
    }
    // Force the channel before the first callback can fire.
    Lazy::force(&EVENT_CHANNEL);
    let raw: u32 = event.into();
    // The callback identifies events by their raw code, so no context is needed.
    let res = unsafe {
        SLSRegisterConnectionNotifyProc(
            *G_CONNECTION,
            connection_callback,
            raw,
            std::ptr::null_mut(),
        )
    };
    if res == 0 {
        registered.insert(event);
        debug!("registered CGS event {event} (raw={raw})");
    } else {
        warn!("failed to register CGS event {event} (raw={raw}), res={res}");
    }
    res
}

/// Receiver for every event registered through [`init`]. Returns `None` once taken.
pub fn take_receiver() -> Option<actor::Receiver<EventData>> { EVENT_CHANNEL.1.lock().take() }

/// Ask CGS for per-window notifications (e.g. `WindowClosed`) about `window_ids`.
/// The set replaces any earlier one.
pub fn update_window_notifications(window_ids: &[u32]) {
    let ids = if window_ids.len() > MAX_NOTIFY_WINDOWS {
        warn!(
            count = window_ids.len(),
            "too many windows for CGS notifications; watching the first {MAX_NOTIFY_WINDOWS}"
        );
        &window_ids[..MAX_NOTIFY_WINDOWS]
    } else {
        window_ids
    };
    let res = unsafe {
        SLSRequestNotificationsForWindows(*G_CONNECTION, ids.as_ptr(), ids.len() as i32)
    };
    if res != 0 {
        warn!(res, count = ids.len(), "SLSRequestNotificationsForWindows failed");
    }
}

fn read_u32(bytes: &[u8], off: usize) -> Option<u32> {
    let chunk = bytes.get(off..off + 4)?;
    Some(u32::from_ne_bytes(chunk.try_into().ok()?))
}

/// The window id carried by a notification payload, if `kind` carries one.
pub(crate) fn parse_window_id(kind: CGSEventType, bytes: &[u8]) -> Option<u32> {
    use KnownCGSEvent::*;
    match kind {
        // Payload: u64 space id, then u32 window id.
        CGSEventType::Known(SpaceWindowCreated | SpaceWindowDestroyed) => {
            read_u32(bytes, size_of::<u64>())
        }
        // Payload: u32 window id.
        CGSEventType::Known(WindowClosed | WindowReordered) => read_u32(bytes, 0),
        _ => None,
    }
}

extern "C" fn connection_callback(
    event_raw: u32,
    data: *mut c_void,
    len: usize,
    _context: *mut c_void,
    _cid: cid_t,
) {
    let kind = CGSEventType::from(event_raw);
    let bytes = if data.is_null() || len == 0 {
        &[][..]
    } else {
        // SAFETY: CGS hands us `len` readable bytes at `data` for the callback's duration.
        unsafe { std::slice::from_raw_parts(data as *const u8, len) }
    };
    let window_id = parse_window_id(kind, bytes);
    if window_id.is_none() {
        trace!(%kind, len, "CGS event without a window id");
    }
    let event = EventData { event_type: kind, window_id };
    trace!(?event, "received CGS event");
    if let Err(err) = EVENT_CHANNEL.0.try_send(event) {
        debug!("dropping CGS event {kind}: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(k: KnownCGSEvent) -> CGSEventType { CGSEventType::Known(k) }

    fn space_payload(sid: u64, wid: u32) -> Vec<u8> {
        let mut bytes = sid.to_ne_bytes().to_vec();
        bytes.extend_from_slice(&wid.to_ne_bytes());
        bytes
    }

    #[test]
    fn space_window_events_skip_the_space_id() {
        let bytes = space_payload(0xDEAD_BEEF_0000_0001, 4242);
        assert_eq!(parse_window_id(known(KnownCGSEvent::SpaceWindowCreated), &bytes), Some(4242));
        assert_eq!(
            parse_window_id(known(KnownCGSEvent::SpaceWindowDestroyed), &bytes),
            Some(4242)
        );
    }

    #[test]
    fn window_events_lead_with_the_window_id() {
        let bytes = 77u32.to_ne_bytes();
        assert_eq!(parse_window_id(known(KnownCGSEvent::WindowClosed), &bytes), Some(77));
        assert_eq!(parse_window_id(known(KnownCGSEvent::WindowReordered), &bytes), Some(77));
    }

    #[test]
    fn short_payloads_yield_no_window_id() {
        assert_eq!(parse_window_id(known(KnownCGSEvent::SpaceWindowCreated), &[0; 10]), None);
        assert_eq!(parse_window_id(known(KnownCGSEvent::WindowClosed), &[1, 2]), None);
        assert_eq!(parse_window_id(known(KnownCGSEvent::WindowClosed), &[]), None);
    }

    #[test]
    fn unsubscribed_kinds_carry_no_window_id() {
        let bytes = 77u32.to_ne_bytes();
        assert_eq!(parse_window_id(known(KnownCGSEvent::SpaceCreated), &bytes), None);
        assert_eq!(parse_window_id(CGSEventType::Unknown(9999), &bytes), None);
    }
}

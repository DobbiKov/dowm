//! Turns CGS connection notifications into reactor events, and keeps CGS's
//! per-window notification set in step with the windows the reactor tracks.
//!
//! Registration happens on the main thread (the main CGS connection's callbacks are
//! delivered on its run loop); translation runs on a dedicated dispatcher thread so a
//! WindowServer query never blocks the main thread or the reactor.

use tracing::{debug, trace, warn};

use super::reactor::{self, Event};
use crate::sys::skylight::{CGSEventType, KnownCGSEvent};
use crate::sys::window_notify::{self, EventData};
use crate::sys::window_server::{self, WindowServerId, WindowServerInfo};

/// The CGS notifications dowm subscribes to: window create, destroy and reorder.
pub const WINDOW_EVENTS: &[CGSEventType] = &[
    CGSEventType::Known(KnownCGSEvent::SpaceWindowCreated),
    CGSEventType::Known(KnownCGSEvent::SpaceWindowDestroyed),
    CGSEventType::Known(KnownCGSEvent::WindowClosed),
    CGSEventType::Known(KnownCGSEvent::WindowReordered),
];

#[derive(Debug)]
pub enum Request {
    /// Replace the set of WindowServer ids CGS sends per-window notifications for.
    UpdateWindowNotifications(Vec<u32>),
}

pub type Sender = crate::actor::Sender<Request>;
pub type Receiver = crate::actor::Receiver<Request>;

/// Translate one CGS notification. `lookup` queries the WindowServer for a live window.
pub(crate) fn translate(
    evt: &EventData,
    lookup: impl FnOnce(WindowServerId) -> Option<WindowServerInfo>,
) -> Option<Event> {
    let wsid = WindowServerId::new(evt.window_id?);
    let CGSEventType::Known(kind) = evt.event_type else {
        return None;
    };
    match kind {
        KnownCGSEvent::SpaceWindowCreated => match lookup(wsid) {
            Some(info) => Some(Event::WindowServerAppeared(info)),
            None => {
                trace!(?wsid, "created window already gone");
                None
            }
        },
        // Also posted when a window merely leaves a native Space (e.g. it moved to
        // another display), so only a window the WindowServer no longer has counts.
        KnownCGSEvent::SpaceWindowDestroyed => match lookup(wsid) {
            None => Some(Event::WindowServerDestroyed(wsid)),
            Some(_) => {
                // It may also be mid-destruction; the reactor verifies via inventory.
                trace!(?wsid, "window left a space but still exists");
                Some(Event::WindowServerMaybeDestroyed(wsid))
            }
        },
        KnownCGSEvent::WindowClosed => Some(Event::WindowServerDestroyed(wsid)),
        KnownCGSEvent::WindowReordered => Some(Event::WindowServerReordered(wsid)),
        _ => None,
    }
}

pub struct WindowNotify {
    requests_rx: Receiver,
    events: &'static [CGSEventType],
}

impl WindowNotify {
    /// Spawns the dispatcher thread; call [`run`](Self::run) on the main thread.
    pub fn new(
        events_tx: reactor::Sender,
        requests_rx: Receiver,
        events: &'static [CGSEventType],
    ) -> Self {
        spawn_dispatcher(events_tx);
        WindowNotify { requests_rx, events }
    }

    pub async fn run(mut self) {
        for &event in self.events {
            if window_notify::init(event) != 0 {
                warn!("CGS subscription to {event} failed; window events may be missed");
            }
        }
        while let Some((span, request)) = self.requests_rx.recv().await {
            let _guard = span.enter();
            match request {
                Request::UpdateWindowNotifications(ids) => {
                    debug!(count = ids.len(), "updating CGS window notifications");
                    window_notify::update_window_notifications(&ids);
                }
            }
        }
    }
}

fn spawn_dispatcher(events_tx: reactor::Sender) {
    let Some(mut rx) = window_notify::take_receiver() else {
        warn!("CGS notification receiver already taken; not spawning dispatcher");
        return;
    };
    std::thread::Builder::new()
        .name("window-notify".into())
        .spawn(move || {
            while let Some((_span, evt)) = rx.blocking_recv() {
                if let Some(event) = translate(&evt, window_server::get_window) {
                    events_tx.send(event);
                }
            }
        })
        .expect("failed to spawn CGS notification dispatcher");
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    use super::*;

    fn evt(kind: KnownCGSEvent, wid: Option<u32>) -> EventData {
        EventData {
            event_type: CGSEventType::Known(kind),
            window_id: wid,
        }
    }

    fn live(id: WindowServerId) -> Option<WindowServerInfo> {
        Some(WindowServerInfo {
            id,
            pid: 42,
            layer: 0,
            frame: CGRect::new(CGPoint::new(0., 0.), CGSize::new(10., 10.)),
            min_frame: CGSize::new(0., 0.),
            max_frame: CGSize::new(0., 0.),
        })
    }

    fn gone(_: WindowServerId) -> Option<WindowServerInfo> { None }

    fn unreachable_lookup(_: WindowServerId) -> Option<WindowServerInfo> {
        panic!("lookup not expected")
    }

    #[test]
    fn creation_resolves_the_owning_process() {
        let out = translate(&evt(KnownCGSEvent::SpaceWindowCreated, Some(7)), live);
        let Some(Event::WindowServerAppeared(info)) = out else { panic!("{out:?}") };
        assert_eq!((info.id, info.pid), (WindowServerId::new(7), 42));
    }

    #[test]
    fn creation_of_an_already_gone_window_is_dropped() {
        assert!(translate(&evt(KnownCGSEvent::SpaceWindowCreated, Some(7)), gone).is_none());
    }

    #[test]
    fn space_destroy_is_confirmed_only_when_the_window_is_gone() {
        let out = translate(&evt(KnownCGSEvent::SpaceWindowDestroyed, Some(7)), gone);
        assert!(matches!(out, Some(Event::WindowServerDestroyed(id)) if id.as_u32() == 7));
        let out = translate(&evt(KnownCGSEvent::SpaceWindowDestroyed, Some(7)), live);
        assert!(matches!(out, Some(Event::WindowServerMaybeDestroyed(id)) if id.as_u32() == 7));
    }

    #[test]
    fn close_and_reorder_need_no_lookup() {
        let out = translate(&evt(KnownCGSEvent::WindowClosed, Some(7)), unreachable_lookup);
        assert!(matches!(out, Some(Event::WindowServerDestroyed(id)) if id.as_u32() == 7));
        let out = translate(&evt(KnownCGSEvent::WindowReordered, Some(7)), unreachable_lookup);
        assert!(matches!(out, Some(Event::WindowServerReordered(id)) if id.as_u32() == 7));
    }

    #[test]
    fn events_without_window_or_of_other_kinds_are_dropped() {
        assert!(translate(&evt(KnownCGSEvent::WindowClosed, None), unreachable_lookup).is_none());
        assert!(translate(&evt(KnownCGSEvent::SpaceCreated, Some(7)), unreachable_lookup).is_none());
        let unknown = EventData {
            event_type: CGSEventType::Unknown(9999),
            window_id: Some(7),
        };
        assert!(translate(&unknown, unreachable_lookup).is_none());
    }
}

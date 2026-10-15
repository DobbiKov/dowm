//! Actors and the channel type they communicate over.
//!
//! Ported from rift (Apache-2.0); see `NOTICE`.

use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::Span;

// Upstream rift code: keeps its `kAX*Notification` names and the style lints listed
// here, so diffs against rift stay small.
#[allow(
    non_upper_case_globals,
    clippy::collapsible_if,
    clippy::let_unit_value,
    clippy::needless_borrow,
    clippy::unnecessary_map_or
)]
pub mod app;
pub mod input;
pub mod notification_center;
pub mod reactor;
pub mod window_notify;
pub mod wm_controller;

pub struct Sender<Event>(UnboundedSender<(Span, Event)>);
pub type Receiver<Event> = UnboundedReceiver<(Span, Event)>;

pub fn channel<Event>() -> (Sender<Event>, Receiver<Event>) {
    let (tx, rx) = unbounded_channel();
    (Sender(tx), rx)
}

impl<Event> Sender<Event> {
    pub(crate) fn same_channel(&self, other: &Self) -> bool { self.0.same_channel(&other.0) }

    pub fn send(&self, event: Event) {
        // Most of the time we can ignore send errors, they just indicate the
        // app is shutting down.
        _ = self.try_send(event)
    }

    pub fn try_send(&self, event: Event) -> Result<(), SendError<(Span, Event)>> {
        self.0.send((Span::current(), event))
    }
}

impl<Event> Clone for Sender<Event> {
    fn clone(&self) -> Self { Self(self.0.clone()) }
}

impl<Event> std::fmt::Debug for Sender<Event> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("actor::Sender(...)")
    }
}

//! Keeps the event taps alive.
//!
//! macOS refuses to create a tap until Accessibility / Input Monitoring is granted, and a
//! tap whose Mach port dies cannot be revived by re-enabling. The keeper runs on the main
//! run loop (where taps must live): every [`RETRY`] it installs whichever tap is missing
//! and replaces any that reported itself invalid, logging a clear error once per outage.

use std::sync::Arc;
use std::time::Duration;

use tracing::{error, info, warn};

use super::Shared;
use super::hotkeys::HotkeyTap;
use super::pointer::PointerTap;
use crate::actor::reactor::Sender as ReactorSender;
use crate::sys::timer::Timer;

/// How often missing or dead taps are retried.
pub const RETRY: Duration = Duration::from_secs(2);

/// Install and maintain the hotkey and pointer taps until the task is dropped.
pub async fn keep_taps(shared: Arc<Shared>, events: ReactorSender) {
    let mut hotkeys: Option<HotkeyTap> = None;
    let mut pointer: Option<PointerTap> = None;
    let (mut hotkeys_failed, mut pointer_failed) = (false, false);
    let mut timer = Timer::repeating(Duration::ZERO, RETRY);
    while timer.next().await.is_some() {
        if hotkeys.as_ref().is_some_and(HotkeyTap::is_invalid) {
            warn!("the hotkey tap died; reinstalling");
            hotkeys = None;
        }
        if hotkeys.is_none() {
            hotkeys = HotkeyTap::install(Arc::clone(&shared), events.clone());
            if hotkeys.is_some() && hotkeys_failed {
                info!("hotkey tap recovered");
            }
            if hotkeys.is_none() && !hotkeys_failed {
                error!(
                    "hotkeys are inactive: macOS refused the key event tap. Grant dowm (or your \
                     terminal) Input Monitoring and Accessibility in System Settings > Privacy & \
                     Security; retrying every {}s",
                    RETRY.as_secs()
                );
            }
            hotkeys_failed = hotkeys.is_none();
        }
        if pointer.as_ref().is_some_and(PointerTap::is_invalid) {
            warn!("the pointer tap died; reinstalling");
            pointer = None;
        }
        if pointer.is_none() {
            pointer = PointerTap::install(Arc::clone(&shared), events.clone());
            if pointer.is_none() && !pointer_failed {
                error!(
                    "focus-follows-mouse is unavailable: macOS refused the mouse event tap \
                     (needs Input Monitoring); retrying every {}s",
                    RETRY.as_secs()
                );
            }
            pointer_failed = pointer.is_none();
        }
    }
}

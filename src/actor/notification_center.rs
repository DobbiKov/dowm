//! NSWorkspace and display-reconfiguration notifications.
//!
//! App launch/terminate/activate go to the WM controller. Screen changes (display
//! reconfiguration callbacks, `NSApplicationDidChangeScreenParametersNotification`,
//! sleep and wake) are coalesced into one main-thread loop that re-reads the display
//! list and sends the reactor a full snapshot keyed by display UUID.
//!
//! Everything here runs on the main thread: AppKit delivers these notifications there
//! and `ScreenCache` reads `NSScreen`.

use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol,
};
use tracing::{debug, trace, warn};

use super::reactor::{self, Event};
use super::wm_controller::{self, WmEvent};
use crate::actor;
use crate::model::display::DisplaySnapshot;
use crate::sys::app::{AppInfo, NSRunningApplicationExt};
use crate::sys::screen::ScreenCache;
use crate::sys::skylight::{
    CGDisplayRegisterReconfigurationCallback, CGDisplayRemoveReconfigurationCallback,
    DisplayReconfigFlags,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenSignal {
    Refresh,
    Sleep,
    Wake,
}

type ScreenSender = actor::Sender<ScreenSignal>;

/// Reconfiguration callbacks arrive in a begin/end pair per change; the display list
/// is only settled after the "begin" one, so that one is skipped.
fn reconfig_needs_refresh(flags: DisplayReconfigFlags) -> bool {
    !flags.contains(DisplayReconfigFlags::BEGIN_CONFIGURATION)
}

struct HandlerIvars {
    wm_tx: wm_controller::Sender,
    screen_tx: ScreenSender,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = HandlerIvars]
    struct NotificationHandler;

    impl NotificationHandler {
        #[unsafe(method(recvAppEvent:))]
        fn recv_app_event(&self, notif: &NSNotification) { self.handle_app_event(notif); }

        #[unsafe(method(recvScreenEvent:))]
        fn recv_screen_event(&self, notif: &NSNotification) {
            trace!(name = %notif.name(), "screen notification");
            self.ivars().screen_tx.send(ScreenSignal::Refresh);
        }

        #[unsafe(method(recvSleepEvent:))]
        fn recv_sleep_event(&self, _notif: &NSNotification) {
            debug!("system will sleep");
            self.ivars().screen_tx.send(ScreenSignal::Sleep);
        }

        #[unsafe(method(recvWakeEvent:))]
        fn recv_wake_event(&self, _notif: &NSNotification) {
            debug!("system did wake");
            self.ivars().screen_tx.send(ScreenSignal::Wake);
        }
    }

    unsafe impl NSObjectProtocol for NotificationHandler {}
);

impl NotificationHandler {
    fn new(wm_tx: wm_controller::Sender, screen_tx: ScreenSender) -> Retained<Self> {
        let this = Self::alloc().set_ivars(HandlerIvars { wm_tx, screen_tx });
        unsafe { msg_send![super(this), init] }
    }

    fn handle_app_event(&self, notif: &NSNotification) {
        use objc2_app_kit::{
            NSWorkspaceDidActivateApplicationNotification,
            NSWorkspaceDidLaunchApplicationNotification,
            NSWorkspaceDidTerminateApplicationNotification,
        };
        let Some(app) = running_application(notif) else { return };
        let pid = app.pid();
        let name = &*notif.name();
        let wm_tx = &self.ivars().wm_tx;
        if unsafe { NSWorkspaceDidLaunchApplicationNotification } == name {
            wm_tx.send(WmEvent::AppLaunch(pid, AppInfo::from(&*app)));
        } else if unsafe { NSWorkspaceDidTerminateApplicationNotification } == name {
            wm_tx.send(WmEvent::AppTerminated(pid));
        } else if unsafe { NSWorkspaceDidActivateApplicationNotification } == name {
            wm_tx.send(WmEvent::AppGloballyActivated(pid));
        } else {
            warn!(%name, "unexpected app notification");
        }
    }
}

fn running_application(notif: &NSNotification) -> Option<Retained<NSRunningApplication>> {
    let info = notif.userInfo()?;
    let app = info.objectForKey(unsafe { NSWorkspaceApplicationKey })?;
    app.downcast::<NSRunningApplication>().ok()
}

unsafe extern "C" fn display_reconfig_callback(display_id: u32, flags: u32, user_info: *mut c_void) {
    if user_info.is_null() {
        return;
    }
    let flags = DisplayReconfigFlags::from_bits_truncate(flags);
    trace!(display_id, ?flags, "display reconfigured");
    if reconfig_needs_refresh(flags) {
        // SAFETY: `user_info` is the boxed sender owned by `NotificationCenter`, which
        // unregisters this callback before dropping it.
        let tx = unsafe { &*user_info.cast::<ScreenSender>() };
        tx.send(ScreenSignal::Refresh);
    }
}

pub struct NotificationCenter {
    handler: Retained<NotificationHandler>,
    wm_tx: wm_controller::Sender,
    events_tx: reactor::Sender,
    screens: ScreenCache,
    screen_rx: actor::Receiver<ScreenSignal>,
    /// Context for the display reconfiguration callback; boxed so its address is stable.
    reconfig_tx: Box<ScreenSender>,
}

impl NotificationCenter {
    pub fn new(
        mtm: MainThreadMarker,
        wm_tx: wm_controller::Sender,
        events_tx: reactor::Sender,
    ) -> Self {
        let (screen_tx, screen_rx) = actor::channel();
        let handler = NotificationHandler::new(wm_tx.clone(), screen_tx.clone());

        let workspace = NSWorkspace::sharedWorkspace();
        let workspace_center = workspace.notificationCenter();
        let default_center = NSNotificationCenter::defaultCenter();
        unsafe {
            use objc2_app_kit::*;
            for (selector, name) in [
                (sel!(recvAppEvent:), NSWorkspaceDidLaunchApplicationNotification),
                (sel!(recvAppEvent:), NSWorkspaceDidTerminateApplicationNotification),
                (sel!(recvAppEvent:), NSWorkspaceDidActivateApplicationNotification),
                (sel!(recvSleepEvent:), NSWorkspaceWillSleepNotification),
                (sel!(recvWakeEvent:), NSWorkspaceDidWakeNotification),
            ] {
                workspace_center.addObserver_selector_name_object(
                    &handler,
                    selector,
                    Some(name),
                    Some(&workspace),
                );
            }
            default_center.addObserver_selector_name_object(
                &handler,
                sel!(recvScreenEvent:),
                Some(NSApplicationDidChangeScreenParametersNotification),
                None,
            );
        }

        let reconfig_tx = Box::new(screen_tx);
        unsafe {
            CGDisplayRegisterReconfigurationCallback(
                Some(display_reconfig_callback),
                (&*reconfig_tx as *const ScreenSender).cast_mut().cast(),
            );
        }

        NotificationCenter {
            handler,
            wm_tx,
            events_tx,
            screens: ScreenCache::new(mtm),
            screen_rx,
            reconfig_tx,
        }
    }

    /// Publish the initial displays, start app discovery, then follow screen changes.
    pub async fn watch_for_notifications(mut self) {
        self.publish_screens();
        self.wm_tx.send(WmEvent::DiscoverRunningApps);
        if let Some(app) = NSWorkspace::sharedWorkspace().frontmostApplication() {
            self.wm_tx.send(WmEvent::AppGloballyActivated(app.pid()));
        }

        while let Some((_span, signal)) = self.screen_rx.recv().await {
            // Reconfiguration arrives in bursts; handle everything queued in one pass.
            let mut pending = vec![signal];
            while let Ok((_, signal)) = self.screen_rx.try_recv() {
                pending.push(signal);
            }
            for signal in pending {
                match signal {
                    ScreenSignal::Refresh => self.screens.mark_dirty(),
                    ScreenSignal::Sleep => self.screens.mark_sleeping(true),
                    ScreenSignal::Wake => self.screens.mark_sleeping(false),
                }
            }
            self.publish_screens();
        }
    }

    fn publish_screens(&mut self) {
        let Some((screens, _converter)) = self.screens.refresh() else {
            warn!("could not read the display list");
            return;
        };
        let snapshot = screens.iter().map(DisplaySnapshot::from).collect();
        self.events_tx.send(Event::ScreensChanged(snapshot));
    }
}

impl Drop for NotificationCenter {
    fn drop(&mut self) {
        unsafe {
            CGDisplayRemoveReconfigurationCallback(
                Some(display_reconfig_callback),
                (&*self.reconfig_tx as *const ScreenSender).cast_mut().cast(),
            );
            NSWorkspace::sharedWorkspace().notificationCenter().removeObserver(&self.handler);
            NSNotificationCenter::defaultCenter().removeObserver(&self.handler);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_configuration_callbacks_are_skipped() {
        assert!(!reconfig_needs_refresh(DisplayReconfigFlags::BEGIN_CONFIGURATION));
        assert!(!reconfig_needs_refresh(
            DisplayReconfigFlags::BEGIN_CONFIGURATION | DisplayReconfigFlags::ADD
        ));
    }

    #[test]
    fn settled_changes_trigger_a_refresh() {
        for flags in [
            DisplayReconfigFlags::ADD,
            DisplayReconfigFlags::REMOVE,
            DisplayReconfigFlags::MOVED,
            DisplayReconfigFlags::SET_MODE,
            DisplayReconfigFlags::SET_MAIN | DisplayReconfigFlags::DESKTOP_SHAPE_CHANGED,
        ] {
            assert!(reconfig_needs_refresh(flags), "{flags:?}");
        }
    }
}

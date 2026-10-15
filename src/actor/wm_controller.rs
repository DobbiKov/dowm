//! The WM controller owns app lifecycle: it spawns one AX actor thread per regular
//! application (the source of all AX observer events) and tells the reactor when an
//! app goes away.
//!
//! Launches arrive from NSWorkspace (`notification_center`) and, at startup, from
//! enumerating running apps. Apps that are not yet regular or not finished launching
//! are deferred through `sys::app`'s KVO observers, as in rift.

use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication};
use tracing::{debug, info, instrument};

use crate::actor::app::{self, AppInfo, AppThreadHandle, pid_t};
use crate::actor::{self, reactor};
use crate::common::collections::HashMap;
use crate::sys;
use crate::sys::app::NSRunningApplicationExt;

pub type Sender = actor::Sender<WmEvent>;
type Receiver = actor::Receiver<WmEvent>;

#[derive(Debug)]
pub enum WmEvent {
    /// Spawn actors for every app already running (sent once at startup).
    DiscoverRunningApps,
    AppLaunch(pid_t, AppInfo),
    AppTerminated(pid_t),
    AppGloballyActivated(pid_t),
    /// An app actor thread exited; sent from its drop guard.
    AppExited(pid_t, AppThreadHandle),
}

#[derive(Debug, PartialEq, Eq)]
enum SpawnDecision {
    Spawn,
    /// Accessory apps can become regular later; watch the activation policy.
    WaitForRegular,
    Skip,
}

fn spawn_decision(policy: NSApplicationActivationPolicy) -> SpawnDecision {
    match policy {
        NSApplicationActivationPolicy::Regular => SpawnDecision::Spawn,
        NSApplicationActivationPolicy::Accessory => SpawnDecision::WaitForRegular,
        _ => SpawnDecision::Skip,
    }
}

pub struct WmController {
    events_tx: reactor::Sender,
    sender: Sender,
    receiver: Receiver,
    apps: HashMap<pid_t, AppThreadHandle>,
}

impl WmController {
    pub fn new(events_tx: reactor::Sender) -> (Self, Sender) {
        let (sender, receiver) = actor::channel();
        sys::app::set_application_callback({
            let sender = sender.clone();
            move |pid, info| sender.send(WmEvent::AppLaunch(pid, info))
        });
        let this = WmController {
            events_tx,
            sender: sender.clone(),
            receiver,
            apps: HashMap::default(),
        };
        (this, sender)
    }

    pub async fn run(mut self) {
        while let Some((span, event)) = self.receiver.recv().await {
            let _guard = span.enter();
            self.handle_event(event);
        }
    }

    #[instrument(name = "wm_controller::handle_event", skip(self))]
    fn handle_event(&mut self, event: WmEvent) {
        match event {
            WmEvent::DiscoverRunningApps => {
                for (pid, info) in sys::app::running_apps(None) {
                    self.new_app(pid, info);
                }
            }
            WmEvent::AppLaunch(pid, info) => self.new_app(pid, info),
            WmEvent::AppTerminated(pid) => {
                sys::app::remove_application_observer(pid);
                if let Some(handle) = self.apps.get(&pid) {
                    _ = handle.send(app::Request::Terminate);
                }
                self.events_tx.send(reactor::Event::ApplicationTerminated(pid));
            }
            WmEvent::AppGloballyActivated(pid) => {
                self.events_tx.send(reactor::Event::ApplicationGloballyActivated(pid));
            }
            WmEvent::AppExited(pid, handle) => {
                // A relaunch under the same pid may already own the slot.
                if self.apps.get(&pid).is_some_and(|current| current.same_actor(&handle)) {
                    self.apps.remove(&pid);
                    debug!(pid, "app actor exited");
                    self.events_tx.send(reactor::Event::ApplicationTerminated(pid));
                }
            }
        }
    }

    fn new_app(&mut self, pid: pid_t, info: AppInfo) {
        if pid == std::process::id() as pid_t || self.apps.contains_key(&pid) {
            return;
        }
        let Some(running_app) = NSRunningApplication::with_process_id(pid) else {
            debug!(pid, "no NSRunningApplication for new app");
            return;
        };

        match spawn_decision(running_app.activationPolicy()) {
            SpawnDecision::Spawn => sys::app::remove_activation_policy_observer(pid),
            SpawnDecision::Skip => return,
            SpawnDecision::WaitForRegular => {
                // The observer re-sends AppLaunch once the policy turns regular.
                sys::app::ensure_activation_policy_observer(pid, running_app, info);
                return;
            }
        }

        if !running_app.isFinishedLaunching() {
            // Re-sends AppLaunch once launched (immediately if it finished meanwhile).
            sys::app::ensure_finished_launching_observer(pid, running_app, info, None);
            return;
        }
        sys::app::remove_finished_launching_observer(pid);

        info!(pid, app = ?info.localized_name, "spawning app actor");
        let (handle, rx) = AppThreadHandle::channel();
        self.apps.insert(pid, handle.clone());
        app::spawn_app_thread(
            pid,
            info,
            self.events_tx.clone(),
            None,
            self.sender.clone(),
            handle,
            rx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_regular_apps_spawn_and_accessory_apps_wait() {
        assert_eq!(spawn_decision(NSApplicationActivationPolicy::Regular), SpawnDecision::Spawn);
        assert_eq!(
            spawn_decision(NSApplicationActivationPolicy::Accessory),
            SpawnDecision::WaitForRegular
        );
        assert_eq!(spawn_decision(NSApplicationActivationPolicy::Prohibited), SpawnDecision::Skip);
    }
}

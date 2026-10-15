//! Test seam: a [`Reactor`] wired to plain channels, fed synthetic events.
//!
//! No AX, CGS or AppKit call happens here. App actors are stood in for by the
//! receiving end of their request channel, so tests can assert what the reactor
//! asked of them.

use objc2_core_foundation::{CGPoint, CGRect, CGSize};

use super::{Command, Event, Reactor};
use crate::actor;
use crate::actor::app::{self, AppInfo, AppThreadHandle, WindowId, WindowInventoryToken, pid_t};
use crate::actor::window_notify;
use crate::common::collections::HashMap;
use crate::model::display::{DisplaySnapshot, DisplayUuid};
use crate::model::window_store::WindowRecord;
use crate::model::workspace::{WorkspaceConfig, WorkspaceError, WorkspaceId};
use crate::model::window_store::tests::info;
use crate::sys::screen::DisplayId;
use crate::sys::window_server::{WindowServerId, WindowServerInfo};

pub(crate) fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

pub(crate) fn wid(pid: pid_t, idx: u32) -> WindowId { WindowId::new(pid, idx) }

pub(crate) fn uuid(s: &str) -> DisplayUuid { DisplayUuid::new(s) }

/// A synthetic window: its index within the app, WindowServer id and frame.
pub(crate) struct W {
    pub idx: u32,
    pub sys_id: Option<u32>,
    pub frame: CGRect,
}

pub(crate) fn w(idx: u32, sys_id: u32, frame: CGRect) -> W {
    W { idx, sys_id: Some(sys_id), frame }
}

pub(crate) struct Harness {
    pub reactor: Reactor,
    notify_rx: actor::Receiver<window_notify::Request>,
    apps: HashMap<pid_t, actor::Receiver<app::Request>>,
}

impl Harness {
    pub fn new() -> Self {
        let (notify_tx, notify_rx) = actor::channel();
        Harness {
            reactor: Reactor::new(Some(notify_tx)),
            notify_rx,
            apps: HashMap::default(),
        }
    }

    /// A harness whose reactor uses `config` for its workspace pool.
    pub fn with_workspaces(config: WorkspaceConfig) -> Self {
        let mut h = Harness::new();
        h.reactor = Reactor::with_workspaces(None, config).expect("valid workspace config");
        h
    }

    /// Deliver an event, then check the workspace invariants. Every scenario goes
    /// through here, so the invariants are checked after every single step.
    pub fn send(&mut self, event: Event) {
        self.reactor.handle_event(event);
        self.reactor.assert_invariants();
    }

    /// Run a workspace command, then check the invariants whether it succeeded or not.
    pub fn cmd(&mut self, command: Command) -> Result<(), WorkspaceError> {
        let result = self.reactor.run_command(command);
        self.reactor.assert_invariants();
        result
    }

    /// The workspace visible on the display `uuid`, as its number.
    pub fn active(&self, uuid: &str) -> Option<u32> {
        self.reactor.workspaces().visible_workspace(&DisplayUuid::new(uuid)).map(WorkspaceId::number)
    }

    /// The focused display's UUID.
    pub fn focused(&self) -> Option<String> {
        self.reactor.workspaces().focused_display().map(|d| d.as_str().to_owned())
    }

    /// The workspace number that owns `wid`.
    pub fn workspace_of(&self, wid: WindowId) -> Option<u32> {
        self.reactor.workspaces().workspace_of(wid).map(WorkspaceId::number)
    }

    /// Report a display set that includes display names.
    pub fn screens_named(&mut self, screens: &[(&str, u32, CGRect, Option<&str>)]) {
        let snapshot = screens
            .iter()
            .map(|&(uuid, cg_id, frame, name)| DisplaySnapshot {
                uuid: DisplayUuid::new(uuid),
                cg_id: DisplayId::new(cg_id),
                frame,
                name: name.map(str::to_owned),
            })
            .collect();
        self.send(Event::ScreensChanged(snapshot));
    }

    /// Report the display set; each entry is (uuid, CG id, frame).
    pub fn screens(&mut self, screens: &[(&str, u32, CGRect)]) {
        let snapshot = screens
            .iter()
            .map(|&(uuid, cg_id, frame)| DisplaySnapshot {
                uuid: DisplayUuid::new(uuid),
                cg_id: DisplayId::new(cg_id),
                frame,
                name: None,
            })
            .collect();
        self.send(Event::ScreensChanged(snapshot));
    }

    /// Launch an app actor for `pid` that already has `windows` open.
    pub fn launch(&mut self, pid: pid_t, windows: &[W]) {
        let (handle, rx) = AppThreadHandle::channel();
        self.apps.insert(pid, rx);
        let visible_windows = windows
            .iter()
            .map(|w| (wid(pid, w.idx), info(&format!("w{}", w.idx), w.frame, w.sys_id)))
            .collect();
        self.send(Event::ApplicationLaunched {
            pid,
            info: AppInfo {
                bundle_id: Some(format!("com.example.app{pid}")),
                localized_name: Some(format!("App {pid}")),
            },
            handle,
            is_frontmost: false,
            main_window: None,
            visible_windows,
            window_server_info: Vec::new(),
        });
    }

    /// The app actor (AX observer) reports a new window.
    pub fn ax_created(&mut self, pid: pid_t, w: W) {
        let window = info(&format!("w{}", w.idx), w.frame, w.sys_id);
        self.send(Event::WindowCreated(wid(pid, w.idx), window, None, None));
    }

    /// CGS reports a WindowServer window creation.
    pub fn cgs_created(&mut self, pid: pid_t, sys_id: u32, frame: CGRect) {
        self.send(Event::WindowServerAppeared(WindowServerInfo {
            id: WindowServerId::new(sys_id),
            pid,
            layer: 0,
            frame,
            min_frame: CGSize::new(0., 0.),
            max_frame: CGSize::new(0., 0.),
        }));
    }

    pub fn cgs_destroyed(&mut self, sys_id: u32) {
        self.send(Event::WindowServerDestroyed(WindowServerId::new(sys_id)));
    }

    /// Requests the reactor sent to `pid`'s app actor since the last call.
    pub fn app_requests(&mut self, pid: pid_t) -> Vec<app::Request> {
        let rx = self.apps.get_mut(&pid).expect("app was launched");
        std::iter::from_fn(|| rx.try_recv().ok().map(|(_, r)| r)).collect()
    }

    /// The inventory tokens among `pid`'s pending requests.
    pub fn inventory_requests(&mut self, pid: pid_t) -> Vec<WindowInventoryToken> {
        self.app_requests(pid)
            .into_iter()
            .filter_map(|r| match r {
                app::Request::RefreshWindowInventory(token) => Some(token),
                _ => None,
            })
            .collect()
    }

    /// The latest WindowServer id set the reactor asked CGS to watch, if it changed.
    pub fn watched_sys_ids(&mut self) -> Option<Vec<u32>> {
        std::iter::from_fn(|| self.notify_rx.try_recv().ok())
            .map(|(_, window_notify::Request::UpdateWindowNotifications(ids))| ids)
            .last()
    }

    pub fn window(&self, wid: WindowId) -> Option<&WindowRecord> {
        self.reactor.windows().get(wid)
    }

    pub fn display_of(&self, wid: WindowId) -> Option<DisplayUuid> {
        self.window(wid).and_then(|r| r.display.clone())
    }

    pub fn window_count(&self) -> usize { self.reactor.windows().len() }
}

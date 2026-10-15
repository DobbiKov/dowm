//! The reactor's side of IPC: answering queries from its own state, running commands
//! that arrive over the socket, and publishing change events to subscribers.
//!
//! The server (see [`crate::ipc`]) runs on its own threads and reaches the reactor only
//! through [`Event::Ipc`]; each request carries a reply channel, so the reactor never
//! blocks on a client.

use std::sync::{Arc, mpsc};

use dowm_protocol::{Event as IpcEvent, MonitorInfo, Query, Rect, Response, WindowInfo, WorkspaceInfo};
use objc2_core_foundation::CGRect;

use super::Reactor;
use crate::actor::app::WindowId;
use crate::config::Action;
use crate::ipc::EventBus;

/// A request from an IPC connection.
#[derive(Debug)]
pub enum IpcRequest {
    /// Run these actions; the reply is the first failure, in words, or `Ok`.
    Run { actions: Vec<Action>, reply: mpsc::Sender<Result<(), String>> },
    Query { query: Query, reply: mpsc::Sender<Response> },
}

/// What subscribers were last told, so only changes are published.
#[derive(Default)]
pub(super) struct IpcState {
    bus: Option<Arc<EventBus>>,
    workspace: Option<String>,
    focus: Option<WindowId>,
    monitors: Option<Vec<MonitorInfo>>,
}

/// The id clients see for a window: the app's pid in the high half, the per-app index
/// in the low half. Valid until dowm restarts, like [`WindowId`] itself.
pub fn wire_window_id(window: WindowId) -> u64 {
    (u64::from(window.pid as u32) << 32) | u64::from(window.idx.get())
}

fn rect(frame: CGRect) -> Rect {
    Rect { x: frame.origin.x, y: frame.origin.y, width: frame.size.width, height: frame.size.height }
}

impl Reactor {
    /// Publish change events to `bus` from now on.
    pub fn set_event_bus(&mut self, bus: Arc<EventBus>) { self.ipc.bus = Some(bus); }

    pub(super) fn handle_ipc(&mut self, request: IpcRequest) {
        match request {
            IpcRequest::Run { actions, reply } => {
                let result = self.run_actions(&actions, None).map_err(|e| e.to_string());
                _ = reply.send(result);
            }
            IpcRequest::Query { query, reply } => {
                _ = reply.send(self.answer_query(query));
            }
        }
    }

    pub fn answer_query(&self, query: Query) -> Response {
        match query {
            Query::ListWorkspaces => Response::Workspaces { workspaces: self.workspace_infos() },
            Query::ListWindows => Response::Windows { windows: self.window_infos() },
            Query::ListMonitors => Response::Monitors { monitors: self.monitor_infos() },
            // The binding mode lives with the hotkey tap, not the reactor.
            Query::Mode => Response::Error { message: "the mode is not tracked by the reactor".into() },
        }
    }

    fn workspace_infos(&self) -> Vec<WorkspaceInfo> {
        let focused = self.workspaces.focused_workspace();
        self.workspaces
            .ids()
            .map(|id| {
                let display = self.workspaces.display_of(id);
                WorkspaceInfo {
                    name: self.workspaces.name(id).unwrap_or_default().to_owned(),
                    focused: focused == Some(id),
                    visible: display.is_some(),
                    monitor: display.map(|d| d.as_str().to_owned()),
                    windows: self.workspaces.windows_in(id).len(),
                }
            })
            .collect()
    }

    fn window_infos(&self) -> Vec<WindowInfo> {
        let mut windows: Vec<WindowInfo> = self
            .windows
            .iter()
            .map(|record| WindowInfo {
                id: wire_window_id(record.wid),
                pid: record.wid.pid,
                app: record.bundle_id.clone(),
                title: record.title.clone(),
                workspace: self
                    .workspaces
                    .workspace_of(record.wid)
                    .and_then(|id| self.workspaces.name(id))
                    .map(str::to_owned),
                focused: self.focused_window == Some(record.wid),
                foreign: self.foreign.contains(&record.wid),
                frame: rect(record.frame),
            })
            .collect();
        windows.sort_by_key(|w| w.id);
        windows
    }

    fn monitor_infos(&self) -> Vec<MonitorInfo> {
        let focused = self.workspaces.focused_display();
        self.displays
            .iter()
            .map(|display| MonitorInfo {
                id: display.uuid.as_str().to_owned(),
                name: display.name.clone(),
                focused: focused == Some(&display.uuid),
                workspace: self
                    .workspaces
                    .visible_workspace(&display.uuid)
                    .and_then(|id| self.workspaces.name(id))
                    .map(str::to_owned),
                frame: rect(display.frame),
            })
            .collect()
    }

    /// Tell subscribers what changed since the last call. Cheap when nothing did, so
    /// the run loop calls it after every event.
    pub fn publish_ipc_changes(&mut self) {
        let Some(bus) = self.ipc.bus.clone() else { return };
        let workspace = self
            .workspaces
            .focused_workspace()
            .and_then(|id| self.workspaces.name(id))
            .map(str::to_owned);
        if workspace != self.ipc.workspace {
            if let Some(name) = workspace.clone() {
                bus.publish(IpcEvent::WorkspaceChanged {
                    workspace: name,
                    previous: self.ipc.workspace.take(),
                });
            }
            self.ipc.workspace = workspace;
        }
        let monitors = self.monitor_infos();
        if self.ipc.monitors.as_ref() != Some(&monitors) {
            self.ipc.monitors = Some(monitors.clone());
            bus.publish(IpcEvent::MonitorsChanged { monitors });
        }
        if self.focused_window != self.ipc.focus {
            self.ipc.focus = self.focused_window;
            bus.publish(IpcEvent::FocusChanged { window: self.focused_window.map(wire_window_id) });
        }
    }
}

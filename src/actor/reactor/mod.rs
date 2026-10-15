//! The reactor: the single owner of window and display state.
//!
//! Every event source funnels into [`Event`]: the per-app AX actors (rift's payloads,
//! unchanged), NSWorkspace and display reconfiguration (via `notification_center`),
//! and CGS connection notifications (via `window_notify`). [`Reactor::handle_event`]
//! is synchronous and touches no OS API, so tests drive it directly with synthetic
//! events (see `tests`).

use objc2_core_foundation::CGRect;
use tracing::{debug, info, trace, warn};

use crate::actor;
use crate::actor::window_notify;
use crate::actor::app::{
    self, AppInfo, AppThreadHandle, Quiet, WindowId, WindowInfo, WindowInventoryToken, pid_t,
};
use crate::sys::event::MouseState;
use std::collections::VecDeque;

use crate::common::collections::{HashMap, HashSet};
use crate::model::display::{DisplaySnapshot, DisplayStore, DisplayUuid};
use std::sync::Arc;

use crate::actor::app::AppThreadHandle as AppHandle;
use crate::model::tx_store::WindowTxStore;
use crate::model::visibility::{
    Clock, LayoutCall, SystemClock, VisibilityAction, VisibilityConfig, VisibilityTracker, layout_calls,
};
use crate::model::window_store::{Upsert, WindowRecord, WindowStore};
use crate::restore::{ParkedHandle, SharedRegistry};
use crate::sys::screen::DisplayId;
use animation::{AnimationManager, AnimationSender, AnimationSettings, HostWindow, Message, PresenterHost};
use objc2_core_foundation::CGRect as Rect;
use transaction_manager::TransactionManager;
use crate::model::workspace::{
    MonitorTarget, WorkspaceConfig, WorkspaceError, WorkspaceStore,
};
use crate::sys::window_server::{WindowServerId, WindowServerInfo};

pub mod animation;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod visibility_tests;
#[cfg(test)]
mod workspace_tests;
pub mod transaction_manager;

use transaction_manager::TransactionId;

pub type Sender = actor::Sender<Event>;
pub type Receiver = actor::Receiver<Event>;

/// Whether a frame change was requested by dowm (as opposed to the user or the app).
#[derive(Debug)]
pub struct Requested(pub bool);

/// The observation that caused an app actor to discard its handle.
/// None of these observations prove that the WindowServer window is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowInvalidationSource {
    AxDestroyedNotification,
    InvalidUiElement,
    StaleAxElement,
}

/// A workspace or monitor command, as the hotkey and IPC layers will issue it.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// `workspace N`.
    Workspace(u32),
    /// `move-node-to-workspace N`. `window: None` means the focused window.
    MoveNodeToWorkspace { workspace: u32, window: Option<WindowId>, follow: bool },
    /// `workspace-back-and-forth`.
    WorkspaceBackAndForth,
    /// `move-workspace-to-monitor`.
    MoveWorkspaceToMonitor(MonitorTarget),
    /// `focus-monitor`.
    FocusMonitor(MonitorTarget),
}

#[derive(Debug)]
pub enum Event {
    /// An application was launched. This event is also sent for every running
    /// application on startup.
    ///
    /// Both WindowInfo (accessibility) and WindowServerInfo are collected for
    /// any already-open windows when the launch event is sent.
    ApplicationLaunched {
        pid: pid_t,
        info: AppInfo,
        handle: AppThreadHandle,
        is_frontmost: bool,
        main_window: Option<WindowId>,
        visible_windows: Vec<(WindowId, WindowInfo)>,
        window_server_info: Vec<WindowServerInfo>,
    },
    ApplicationActivated(pid_t, Quiet),
    ApplicationDeactivated(pid_t),
    ApplicationMainWindowChanged(pid_t, Option<WindowId>, Quiet),
    WindowsDiscovered {
        pid: pid_t,
        token: WindowInventoryToken,
        successful: bool,
        new: Vec<(WindowId, WindowInfo)>,
        known_visible: Vec<WindowId>,
    },
    WindowInventoryRefreshRequested(pid_t),
    RaiseTargetsMissing {
        windows: Vec<WindowId>,
        sequence_id: u64,
    },
    WindowCreated(
        WindowId,
        WindowInfo,
        Option<WindowServerInfo>,
        Option<MouseState>,
    ),
    /// The AXUIElement became invalid, but that is not proof that its native
    /// WindowServer window was destroyed. This commonly happens before macOS
    /// publishes sleep/session lifecycle notifications.
    WindowInvalidated(WindowId, WindowInvalidationSource),
    WindowMinimized(WindowId),
    WindowDeminiaturized(WindowId),
    WindowFrameChanged(
        WindowId,
        CGRect,
        Option<TransactionId>,
        Requested,
        Option<MouseState>,
    ),
    WindowTitleChanged(WindowId, String),
    MenuOpened(pid_t),
    MenuClosed(pid_t),
    /// A raise request completed. Used to track when all raise requests in a
    /// sequence have finished.
    RaiseCompleted {
        window_id: WindowId,
        sequence_id: u64,
    },

    // dowm events: NSWorkspace, display reconfiguration and CGS.
    /// NSWorkspace reported termination, or the app's actor thread exited.
    ApplicationTerminated(pid_t),
    /// NSWorkspace reported a global activation; the app actor resolves its windows.
    ApplicationGloballyActivated(pid_t),
    /// The full display set after a reconfiguration, sleep/wake or screen change.
    ScreensChanged(Vec<DisplaySnapshot>),
    /// CGS: a window was created in the WindowServer.
    WindowServerAppeared(WindowServerInfo),
    /// CGS: a window was destroyed or closed in the WindowServer.
    WindowServerDestroyed(WindowServerId),
    /// CGS: a window left a native Space but the WindowServer still has it; it may be
    /// mid-destruction, so the owner's inventory should confirm.
    WindowServerMaybeDestroyed(WindowServerId),
    /// CGS: a window changed z-order.
    WindowServerReordered(WindowServerId),
    /// A workspace or monitor command.
    Command(Command),
    /// A periodic nudge so time-based bookkeeping (unhide confirmation TTL) runs even
    /// when nothing else happens.
    VisibilityTick,
}

struct AppState {
    info: AppInfo,
    handle: AppThreadHandle,
    /// The outstanding inventory request, if any; any other answer is stale.
    pending_inventory: Option<u64>,
    /// Windows AX reported after `pending_inventory` was issued. The answer's snapshot
    /// may predate them, so its absence must not delete them.
    created_since_request: HashSet<WindowId>,
}

/// How many destroyed WindowServer ids to remember, for late AX creates.
const TOMBSTONE_CAPACITY: usize = 256;

/// The reactor's time source (see [`Clock`]).
struct ClockHandle(Arc<dyn Clock>);

impl Default for ClockHandle {
    fn default() -> Self { ClockHandle(Arc::new(SystemClock)) }
}

/// What the presenter entry points need from the reactor.
struct Presenter {
    tx: Option<AnimationSender>,
    transactions: TransactionManager,
    settings: AnimationSettings,
}

impl Default for Presenter {
    fn default() -> Self {
        Presenter {
            tx: None,
            transactions: TransactionManager::new(WindowTxStore::new()),
            settings: AnimationSettings::default(),
        }
    }
}

#[derive(Default)]
pub struct Reactor {
    windows: WindowStore,
    displays: DisplayStore,
    workspaces: WorkspaceStore,
    /// The window last reported as an app's main window; the target of
    /// `move-node-to-workspace` when no window is named.
    focused_window: Option<WindowId>,
    apps: HashMap<pid_t, AppState>,
    next_inventory_request: u64,
    /// Bumped on every display change; inventories taken before it are stale.
    topology_revision: u64,
    window_notify_tx: Option<window_notify::Sender>,
    /// The WindowServer id set last sent to `window_notify`.
    watched_sys_ids: Vec<u32>,
    /// Recently destroyed WindowServer ids, oldest first. An AX create for one of
    /// these lost the race with its own destruction and is ignored.
    tombstones: VecDeque<WindowServerId>,
    /// Which windows are parked, and the actions not yet carried out.
    visibility: VisibilityTracker,
    visibility_config: VisibilityConfig,
    visibility_outbox: Vec<VisibilityAction>,
    /// Shared with the termination handler: every parked window and its restore target.
    parked_registry: Arc<SharedRegistry<ParkedHandle>>,
    presenter: Presenter,
    clock: ClockHandle,
    /// Windows leaving a display during a switch. They are no longer on its visible
    /// workspace but still slide, so the presenter treats them as visible on it.
    sliding_out: HashSet<WindowId>,
}

impl Reactor {
    /// A reactor with the default workspace pool (nine). The daemon uses
    /// [`Reactor::with_workspaces`] with the configured pool instead.
    pub fn new(window_notify_tx: Option<window_notify::Sender>) -> Self {
        Reactor { window_notify_tx, ..Default::default() }
    }

    pub fn windows(&self) -> &WindowStore { &self.windows }

    pub fn displays(&self) -> &DisplayStore { &self.displays }

    pub fn workspaces(&self) -> &WorkspaceStore { &self.workspaces }

    pub fn visibility(&self) -> &VisibilityTracker { &self.visibility }

    pub fn visibility_config(&self) -> VisibilityConfig { self.visibility_config }

    pub fn set_visibility_config(&mut self, config: VisibilityConfig) {
        self.visibility_config = config;
    }

    /// The registry the termination handler reads.
    pub fn parked_registry(&self) -> &Arc<SharedRegistry<ParkedHandle>> { &self.parked_registry }

    /// Share `registry` (with the termination handler) instead of the private one.
    pub fn set_parked_registry(&mut self, registry: Arc<SharedRegistry<ParkedHandle>>) {
        self.parked_registry = registry;
    }

    /// Use `clock` for the unhide-confirmation TTL (tests inject a manual one).
    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) { self.clock = ClockHandle(clock); }

    /// Route animated switches through the presenter thread behind `tx`.
    pub fn set_presenter(&mut self, tx: AnimationSender) { self.presenter.tx = Some(tx); }

    /// Park and unpark actions planned and not yet flushed, oldest first.
    pub fn pending_visibility_actions(&self) -> &[VisibilityAction] { &self.visibility_outbox }

    pub fn take_visibility_actions(&mut self) -> Vec<VisibilityAction> {
        std::mem::take(&mut self.visibility_outbox)
    }

    /// Bring the parked set in line with workspace visibility. Plans only; see
    /// [`Reactor::flush_visibility`].
    fn sync_visibility(&mut self) {
        self.visibility.expire(
            self.clock.0.now(),
            &self.windows,
            &self.displays,
            &self.parked_registry,
        );
        let actions = self.visibility.plan(
            &self.windows,
            &self.workspaces,
            &self.displays,
            &self.parked_registry,
        );
        self.visibility_outbox.extend(actions);
    }

    /// Carry out the planned actions through the presenter: animated or instant, per
    /// [`VisibilityConfig`]. Unhides always precede hides.
    pub fn flush_visibility(&mut self) {
        let mut actions = self.take_visibility_actions();
        if self.parked_registry.is_shutting_down() {
            // The restore has begun: further hides would strand windows behind it.
            actions.retain(VisibilityAction::is_unhide);
        }
        if actions.is_empty() {
            return;
        }
        self.sliding_out =
            actions.iter().filter(|a| !a.is_unhide()).map(|a| a.window()).collect();
        for call in layout_calls(&actions, self.visibility_config, &self.displays) {
            match call {
                LayoutCall::Animated { display, layout } => {
                    AnimationManager::animate_layout(self, display, &layout, false, None);
                }
                LayoutCall::Instant { layout } => {
                    AnimationManager::workspace_switch_layout(self, &layout, None);
                }
            }
        }
        self.sliding_out.clear();
        // The writes are out: from now on their answers (and only theirs) confirm them.
        let now = self.clock.0.now();
        for action in &actions {
            let wid = action.window();
            let txid = self
                .windows
                .get(wid)
                .and_then(|r| r.sys_id)
                .map(|wsid| self.presenter.transactions.get_last_sent_txid(wsid));
            self.visibility.arm(wid, txid, now);
        }
    }

    /// Panic if parking disagrees with visibility: every window that should be hidden is
    /// parked, every visible one is not, and the registry holds exactly the parked set.
    fn check_visibility(&self) -> Result<(), String> {
        let shutting_down = self.parked_registry.is_shutting_down();
        let mut parked = 0;
        for record in self.windows.iter() {
            let wid = record.wid;
            let is_parked = self.visibility.is_parked(wid);
            parked += usize::from(is_parked);
            if self.workspaces.workspace_of(wid).is_none() {
                continue;
            }
            let visible = self.workspaces.window_is_visible(wid);
            let hideable = !record.is_minimized
                && record.display.as_ref().is_some_and(|d| self.displays.get(d).is_some());
            if visible && is_parked {
                return Err(format!("{wid:?} is on a visible workspace but parked"));
            }
            if !visible && hideable && !is_parked && !shutting_down {
                return Err(format!("{wid:?} is on a hidden workspace but not parked"));
            }
        }
        if self.visibility.parked_count() != parked {
            return Err("parked window missing from the window store".into());
        }
        if shutting_down {
            return Ok(()); // the registry is frozen; the tracker may be ahead of it
        }
        let expected = parked + self.visibility.unconfirmed_count();
        let registered = self.parked_registry.restore_set();
        if registered.len() != expected {
            return Err(format!(
                "registry holds {} windows, {parked} parked and {} unconfirmed",
                registered.len(),
                expected - parked
            ));
        }
        for wid in self.visibility.parked() {
            let at = self.visibility.parked_at(wid).expect("parked");
            if !registered.iter().any(|(h, _)| h.pid == wid.pid && h.parked == at) {
                return Err(format!("{wid:?} is parked but not registered"));
            }
        }
        Ok(())
    }

    /// A reactor with a workspace pool built from `config`.
    pub fn with_workspaces(
        window_notify_tx: Option<window_notify::Sender>,
        config: WorkspaceConfig,
    ) -> Result<Self, WorkspaceError> {
        Ok(Reactor {
            window_notify_tx,
            workspaces: WorkspaceStore::new(&config)?,
            ..Default::default()
        })
    }

    /// Run a workspace or monitor command. [`Event::Command`] does this and logs
    /// the error; tests call it directly to see it.
    pub fn run_command(&mut self, command: Command) -> Result<(), WorkspaceError> {
        self.forget_destroyed_focus();
        let result = match command {
            Command::Workspace(n) => self.workspaces.switch_to_workspace(n, &self.displays),
            Command::MoveNodeToWorkspace { workspace, window, follow } => {
                match window.or(self.focused_window) {
                    Some(window) => self.workspaces.move_window_to_workspace(
                        window,
                        workspace,
                        follow,
                        &self.displays,
                    ),
                    None => self.workspaces.id_for(workspace).and(Err(WorkspaceError::NoFocusedWindow)),
                }
            }
            Command::WorkspaceBackAndForth => self.workspaces.back_and_forth(&self.displays),
            Command::MoveWorkspaceToMonitor(target) => {
                self.workspaces.move_workspace_to_monitor(&target, &self.displays)
            }
            Command::FocusMonitor(target) => self.workspaces.focus_monitor(&target, &self.displays),
        };
        self.sync_visibility();
        self.debug_check_invariants();
        result
    }

    /// A destroyed window cannot stay the target of `move-node-to-workspace`.
    fn forget_destroyed_focus(&mut self) {
        if self.focused_window.is_some_and(|wid| !self.windows.contains(wid)) {
            self.focused_window = None;
        }
    }

    /// Panic if the workspace model is inconsistent; see
    /// [`WorkspaceStore::check_invariants`].
    pub fn assert_invariants(&self) {
        if let Err(violation) =
            self.workspaces.check_invariants(&self.displays, &self.windows)
        {
            panic!("workspace invariant violated: {violation}");
        }
        if let Err(violation) = self.check_visibility() {
            panic!("visibility invariant violated: {violation}");
        }
    }

    fn debug_check_invariants(&self) {
        if cfg!(debug_assertions) {
            self.assert_invariants();
        }
    }

    pub async fn run(mut self, mut events: Receiver) {
        while let Some((span, event)) = events.recv().await {
            let _guard = span.enter();
            self.handle_event(event);
            self.flush_visibility();
        }
    }

    pub fn handle_event(&mut self, event: Event) {
        trace!(?event, "reactor event");
        match event {
            Event::ApplicationLaunched { pid, info, handle, visible_windows, .. } => {
                info!(pid, app = ?info.localized_name, bundle = ?info.bundle_id, windows = visible_windows.len(), "app launched");
                self.apps.insert(pid, AppState {
                    info,
                    handle,
                    pending_inventory: None,
                    created_since_request: HashSet::default(),
                });
                for (wid, window) in visible_windows {
                    self.upsert_window(wid, &window);
                }
            }
            Event::ApplicationTerminated(pid) => {
                if let Some(app) = self.apps.remove(&pid) {
                    info!(pid, app = ?app.info.localized_name, "app terminated");
                }
                for record in self.windows.remove_app(pid) {
                    log_destroyed(&record, "app terminated");
                }
            }
            Event::ApplicationGloballyActivated(pid) => {
                if let Some(app) = self.apps.get(&pid) {
                    _ = app.handle.send(app::Request::ApplicationGloballyActivated(pid));
                }
            }
            Event::ApplicationActivated(pid, _) => debug!(pid, "app activated"),
            Event::ApplicationDeactivated(pid) => debug!(pid, "app deactivated"),
            Event::ApplicationMainWindowChanged(pid, wid, _) => {
                debug!(pid, ?wid, "main window changed");
                if let Some(wid) = wid {
                    self.focused_window = Some(wid);
                }
            }
            Event::WindowsDiscovered { pid, token, successful, new, known_visible: _ } => {
                self.reconcile_inventory(pid, token, successful, new);
            }
            Event::WindowInventoryRefreshRequested(pid) => self.request_inventory(pid),
            Event::WindowCreated(wid, window, _, _) => {
                if self.upsert_window(wid, &window)
                    && let Some(app) = self.apps.get_mut(&wid.pid)
                    && app.pending_inventory.is_some()
                {
                    app.created_since_request.insert(wid);
                }
            }
            Event::WindowInvalidated(wid, source) => self.window_invalidated(wid, source),
            Event::WindowMinimized(wid) => self.set_minimized(wid, true),
            Event::WindowDeminiaturized(wid) => self.set_minimized(wid, false),
            Event::WindowFrameChanged(wid, frame, txid, requested, _) => {
                let Some(record) = self.windows.get_mut(wid) else { return };
                record.frame = frame;
                self.visibility.frame_observed(
                    wid,
                    frame,
                    txid,
                    requested.0,
                    &self.displays,
                    &self.parked_registry,
                );
                // Our own writes leave a parked window past a corner; only the user or
                // the system moving it re-homes it.
                self.assign_display_inner(wid, requested.0);
                // A user drag onto another display re-homes the window; our own writes
                // and parked windows never do.
                if !requested.0
                    && let Some(display) =
                        self.windows.get(wid).and_then(|r| r.display.clone())
                {
                    self.workspaces.sync_windows(&self.windows);
                    self.workspaces.window_dragged_to(wid, &display);
                }
            }
            Event::WindowTitleChanged(wid, title) => {
                if let Some(record) = self.windows.get_mut(wid) {
                    debug!(?wid, %title, "window title changed");
                    record.title = title;
                }
            }
            Event::ScreensChanged(snapshot) => self.screens_changed(snapshot),
            Event::WindowServerAppeared(server_info) => self.window_server_appeared(server_info),
            Event::WindowServerDestroyed(wsid) => {
                self.add_tombstone(wsid);
                if let Some(record) = self.windows.remove_by_sys_id(wsid) {
                    log_destroyed(&record, "window server destroyed");
                }
            }
            Event::WindowServerMaybeDestroyed(wsid) => {
                if let Some(pid) = self.windows.by_sys_id(wsid).map(|r| r.wid.pid) {
                    debug!(?wsid, pid, "window left a space; verifying");
                    self.request_inventory(pid);
                }
            }
            Event::WindowServerReordered(wsid) => {
                if let Some(record) = self.windows.by_sys_id(wsid) {
                    debug!(wid = ?record.wid, ?wsid, title = %record.title, "window reordered");
                }
            }
            Event::Command(command) => {
                if let Err(error) = self.run_command(command.clone()) {
                    warn!(?command, %error, "command failed");
                }
            }
            Event::VisibilityTick
            | Event::MenuOpened(_)
            | Event::MenuClosed(_)
            | Event::RaiseTargetsMissing { .. }
            | Event::RaiseCompleted { .. } => {}
        }
        self.workspaces.sync_windows(&self.windows);
        self.forget_destroyed_focus();
        self.sync_visibility();
        self.debug_check_invariants();
        self.sync_window_notifications();
    }

    /// Returns whether the window was newly inserted.
    fn upsert_window(&mut self, wid: WindowId, window: &WindowInfo) -> bool {
        if let Some(sys_id) = window.sys_id
            && self.tombstones.contains(&sys_id)
        {
            debug!(?wid, ?sys_id, "ignoring AX window already destroyed in the WindowServer");
            return false;
        }
        if self.windows.upsert(wid, window) == Upsert::Inserted {
            self.assign_display(wid);
            let record = &self.windows.get(wid).expect("just inserted");
            info!(
                ?wid,
                sys_id = ?record.sys_id.map(|s| s.as_u32()),
                title = %record.title,
                display = %display_label(record.display.as_ref()),
                "window created"
            );
            true
        } else {
            self.assign_display(wid);
            false
        }
    }

    fn add_tombstone(&mut self, wsid: WindowServerId) {
        if self.tombstones.contains(&wsid) {
            return;
        }
        if self.tombstones.len() == TOMBSTONE_CAPACITY {
            self.tombstones.pop_front();
        }
        self.tombstones.push_back(wsid);
    }

    fn set_minimized(&mut self, wid: WindowId, minimized: bool) {
        if let Some(record) = self.windows.get_mut(wid) {
            debug!(?wid, minimized, "window minimized state changed");
            record.is_minimized = minimized;
        }
        if minimized {
            self.visibility.window_minimized(wid, &self.parked_registry);
        }
    }

    /// AX invalidation does not prove the window is gone. A window without a
    /// WindowServer id can never get a CGS destroy, so it goes now; otherwise CGS
    /// or the inventory this requests settles it.
    fn window_invalidated(&mut self, wid: WindowId, source: WindowInvalidationSource) {
        let Some(record) = self.windows.get_mut(wid) else { return };
        if record.sys_id.is_none() {
            let record = self.windows.remove(wid).expect("present");
            log_destroyed(&record, "ax invalidated");
            return;
        }
        debug!(?wid, ?source, "window AX handle invalidated; verifying");
        record.ax_invalidated = true;
        self.request_inventory(wid.pid);
    }

    fn request_inventory(&mut self, pid: pid_t) {
        let Some(app) = self.apps.get_mut(&pid) else { return };
        self.next_inventory_request += 1;
        let token = WindowInventoryToken {
            request_id: self.next_inventory_request,
            topology_revision: self.topology_revision,
        };
        app.pending_inventory = Some(token.request_id);
        app.created_since_request.clear();
        debug!(pid, ?token, "requesting window inventory");
        _ = app.handle.send(app::Request::RefreshWindowInventory(token));
    }

    fn reconcile_inventory(
        &mut self,
        pid: pid_t,
        token: WindowInventoryToken,
        successful: bool,
        windows: Vec<(WindowId, WindowInfo)>,
    ) {
        let Some(app) = self.apps.get_mut(&pid) else { return };
        if app.pending_inventory != Some(token.request_id)
            || token.topology_revision != self.topology_revision
        {
            debug!(pid, ?token, "ignoring stale window inventory");
            return;
        }
        app.pending_inventory = None;
        let created_since = std::mem::take(&mut app.created_since_request);
        if !successful {
            debug!(pid, "window inventory failed");
            return;
        }
        let mut keep: HashSet<WindowId> = windows.iter().map(|(wid, _)| *wid).collect();
        keep.extend(created_since);
        for record in self.windows.retain_app(pid, &keep) {
            log_destroyed(&record, "missing from inventory");
        }
        for (wid, window) in windows {
            self.upsert_window(wid, &window);
        }
    }

    fn window_server_appeared(&mut self, server_info: WindowServerInfo) {
        // A live window with this id means any tombstone for it is from a recycled id.
        self.tombstones.retain(|&id| id != server_info.id);
        if self.windows.by_sys_id(server_info.id).is_some() {
            return;
        }
        // Menus, tooltips and popups live on other layers and are never managed.
        if server_info.layer != 0 {
            return;
        }
        if let Some(app) = self.apps.get(&server_info.pid) {
            if app.pending_inventory.is_some() {
                // The pending answer, or AX's own create event, will cover it.
                return;
            }
            debug!(wsid = ?server_info.id, pid = server_info.pid, "unknown window appeared");
            self.request_inventory(server_info.pid);
        } else {
            trace!(wsid = ?server_info.id, pid = server_info.pid, "window of unmanaged app");
        }
    }

    fn screens_changed(&mut self, snapshot: Vec<DisplaySnapshot>) {
        let changes = self.displays.apply(snapshot);
        if changes.is_empty() {
            return;
        }
        self.topology_revision += 1;
        self.parked_registry.set_display_count(self.displays.len());
        if let Some(tx) = &self.presenter.tx {
            let ids = self.displays.iter().map(|d| d.cg_id).collect();
            _ = tx.send(Message::Displays(ids));
        }
        self.workspaces.displays_changed(&changes, &self.displays);
        for uuid in &changes.added {
            let d = self.displays.get(uuid).expect("just added");
            info!(%uuid, cg_id = d.cg_id.as_u32(), name = ?d.name, frame = ?d.frame, "display added");
        }
        for uuid in &changes.removed {
            info!(%uuid, "display removed");
        }
        for uuid in &changes.changed {
            let d = self.displays.get(uuid).expect("still present");
            info!(%uuid, cg_id = d.cg_id.as_u32(), frame = ?d.frame, "display changed");
        }

        let wids: Vec<WindowId> = self.windows.iter().map(|r| r.wid).collect();
        for wid in wids {
            self.assign_display(wid);
        }

        // In-flight inventories are now stale; ask again.
        let pending: Vec<pid_t> = self
            .apps
            .iter()
            .filter(|(_, app)| app.pending_inventory.is_some())
            .map(|(&pid, _)| pid)
            .collect();
        for pid in pending {
            self.request_inventory(pid);
        }
    }

    /// Keep a window on the display it overlaps most. A window whose display
    /// disappeared moves to the nearest remaining one.
    fn assign_display(&mut self, wid: WindowId) { self.assign_display_inner(wid, true) }

    /// `keep_parked`: leave a parked window on the display it was parked on (its frame sits
    /// past a corner and may overlap a neighbour) unless that display is gone.
    fn assign_display_inner(&mut self, wid: WindowId, keep_parked: bool) {
        let parked = keep_parked && self.visibility.is_parked(wid);
        let Some(record) = self.windows.get_mut(wid) else { return };
        if parked
            && record.display.as_ref().is_some_and(|d| self.displays.get(d).is_some())
        {
            return;
        }
        let target = self.displays.display_for_rect(record.frame).cloned();
        if record.display != target {
            if record.display.is_some() {
                info!(
                    ?wid,
                    from = %display_label(record.display.as_ref()),
                    to = %display_label(target.as_ref()),
                    "window changed display"
                );
            }
            record.display = target;
        }
    }

    fn sync_window_notifications(&mut self) {
        let Some(tx) = &self.window_notify_tx else { return };
        let ids = self.windows.sys_ids();
        if ids != self.watched_sys_ids {
            self.watched_sys_ids = ids.clone();
            tx.send(window_notify::Request::UpdateWindowNotifications(ids));
        }
    }
}

impl PresenterHost for Reactor {
    fn animation_tx(&self) -> Option<&AnimationSender> { self.presenter.tx.as_ref() }

    fn transaction_manager(&self) -> &TransactionManager { &self.presenter.transactions }

    fn animation_settings(&self) -> AnimationSettings { self.presenter.settings }

    fn window(&self, wid: WindowId) -> Option<HostWindow> {
        self.windows.get(wid).map(|r| HostWindow { frame: r.frame, wsid: r.sys_id })
    }

    fn set_window_frame(&mut self, wid: WindowId, frame: Rect) {
        if let Some(record) = self.windows.get_mut(wid) {
            record.frame = frame;
        }
    }

    fn app_handle(&self, pid: pid_t) -> Option<AppHandle> {
        self.apps.get(&pid).map(|a| a.handle.clone())
    }

    fn is_visible_on(&self, display: DisplayId, wid: WindowId) -> bool {
        if self.sliding_out.contains(&wid) {
            return true;
        }
        let Some(snapshot) = self.displays.iter().find(|d| d.cg_id == display) else {
            return false;
        };
        let shown = self.workspaces.visible_workspace(&snapshot.uuid);
        shown.is_some() && shown == self.workspaces.workspace_of(wid)
    }
}

fn display_label(display: Option<&DisplayUuid>) -> &str {
    display.map_or("none", |d| d.as_str())
}

fn log_destroyed(record: &WindowRecord, reason: &str) {
    info!(
        wid = ?record.wid,
        sys_id = ?record.sys_id.map(|s| s.as_u32()),
        title = %record.title,
        reason,
        "window destroyed"
    );
}

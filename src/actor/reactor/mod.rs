//! The reactor: the single owner of window and display state.
//!
//! Every event source funnels into [`Event`]: the per-app AX actors (rift's payloads,
//! unchanged), NSWorkspace and display reconfiguration (via `notification_center`),
//! and CGS connection notifications (via `window_notify`). [`Reactor::handle_event`]
//! is synchronous and touches no OS API, so tests drive it directly with synthetic
//! events (see `tests`).
//!
//! The reactor also owns each workspace's tiling tree ([`TreeStore`]), kept in step with
//! workspace membership after every event and command, and runs the M3 tiling
//! commands over it. Computed frames are exposed by [`Reactor::workspace_frames`] and
//! [`Reactor::layout_targets`], and [`Reactor::flush_layout`] (run after every event)
//! sends the changed ones to the presenter as animated frame writes; see `layout_apply`.

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
use crate::layout::ops::{self, LayoutArg, ResizeDimension, SplitArg, TilingError};
use crate::layout::{self, LayoutRegistry, LayoutSystem};
use crate::config::{Action, Config};
use crate::model::tree::NormalizeOptions;
use effects::{Effect, EffectSender};
use crate::model::tree_store::TreeStore;
use crate::model::workspace::{
    Direction, MonitorTarget, WorkspaceConfig, WorkspaceError, WorkspaceId, WorkspaceStore,
};
use crate::sys::window_server::{WindowServerId, WindowServerInfo};

pub mod animation;
mod config_apply;
mod layout_apply;
#[cfg(test)]
mod layout_apply_tests;
#[cfg(test)]
mod config_tests;
pub mod effects;
pub mod ipc;
#[cfg(test)]
mod space_tests;
#[cfg(test)]
mod focus_tests;
#[cfg(test)]
mod ipc_tests;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tiling_tests;
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

/// A workspace, monitor or tiling command, as the hotkey and IPC layers will issue it.
/// The tiling commands act on the focused workspace's tree.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// `workspace <name>`.
    Workspace(String),
    /// `move-node-to-workspace <name>`. `window: None` means the focused window.
    MoveNodeToWorkspace { workspace: String, window: Option<WindowId>, follow: bool },
    /// `workspace-back-and-forth`.
    WorkspaceBackAndForth,
    /// `move-workspace-to-monitor`.
    MoveWorkspaceToMonitor(MonitorTarget),
    /// `focus-monitor`.
    FocusMonitor(MonitorTarget),
    /// `focus <direction>`; with `wrap`, past the edge to the other end of the workspace.
    Focus { direction: Direction, wrap: bool },
    /// `move <direction>`.
    Move(Direction),
    /// `join-with <direction>`.
    JoinWith(Direction),
    /// `split horizontal|vertical|opposite`.
    Split(SplitArg),
    /// `resize width|height|smart <amount>`; `amount` is in pixels, negative to shrink.
    Resize { dimension: ResizeDimension, amount: i32 },
    /// `resize width|height <size>`: make the window exactly `size` pixels along it.
    ResizeTo { dimension: ResizeDimension, size: i32 },
    /// `layout <arg>...`: one argument applies it; several cycle.
    Layout(Vec<LayoutArg>),
    /// `fullscreen`.
    Fullscreen,
    /// `balance-sizes`.
    BalanceSizes,
    /// `flatten-workspace-tree`.
    FlattenWorkspaceTree,
    /// `close-all-windows-but-current`: ask every other window of the focused workspace
    /// to close (as Command-W would).
    CloseAllWindowsButCurrent,
    /// `move-mouse ...`: becomes an [`Effect::MoveMouse`] aimed at the window or monitor.
    MoveMouse { target: MouseTarget, lazy: bool },
}

/// What `move-mouse` centers the cursor on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseTarget {
    /// The focused window.
    Window,
    /// The focused monitor.
    Monitor,
}

/// Why a [`Command`] failed.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandError {
    Workspace(WorkspaceError),
    Tiling(TilingError),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Workspace(e) => e.fmt(f),
            Self::Tiling(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for CommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Workspace(e) => Some(e),
            Self::Tiling(e) => Some(e),
        }
    }
}

/// Where the tiling layout wants a window, with what the presenter needs to apply it.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutTarget {
    pub window: WindowId,
    pub frame: CGRect,
    /// Another window may be drawn over (or under) this one, so z-order matters.
    pub overlapping: bool,
    /// Bring this window to the front: it is the focused one of an overlapping layout.
    pub raise: bool,
}

impl From<WorkspaceError> for CommandError {
    fn from(e: WorkspaceError) -> Self { Self::Workspace(e) }
}

impl From<TilingError> for CommandError {
    fn from(e: TilingError) -> Self { Self::Tiling(e) }
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
    /// Actions from a hotkey, run in order; the first failure stops the rest.
    Actions(Vec<Action>),
    /// A new, valid config (hot reload). See [`Reactor::apply_config`].
    ConfigReloaded(Arc<Config>),
    /// The start-up phase is over: `if.during-aerospace-startup` stops matching and
    /// `after-startup-command` runs.
    StartupFinished,
    /// Focus-follows-mouse: the pointer has settled over this window.
    PointerSettledOn(WindowServerId),
    /// The WindowServer windows currently in a native Space other than the visible one
    /// (the complete set, sent whenever it changes).
    ForeignWindows(Vec<WindowServerId>),
    /// A request from an IPC connection (query or command).
    Ipc(ipc::IpcRequest),
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
    /// Per-workspace tiling trees; see [`TreeStore::sync`].
    trees: TreeStore,
    layouts: LayoutRegistry,
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
    /// What the tiling layout last asked of each window; see [`Reactor::flush_layout`].
    applied_layout: HashMap<WindowId, layout_apply::AppliedTarget>,
    clock: ClockHandle,
    /// Windows leaving a display during a switch. They are no longer on its visible
    /// workspace but still slide, so the presenter treats them as visible on it.
    sliding_out: HashSet<WindowId>,
    /// The applied config; `None` until the first [`Reactor::apply_config`].
    config: Option<Arc<Config>>,
    /// Effects queued for the platform shell, oldest first.
    effects: Vec<Effect>,
    effects_tx: Option<EffectSender>,
    /// Windows first seen since the `on-window-detected` rules last ran.
    detected: Vec<WindowId>,
    startup_finished: bool,
    startup_commands_ran: bool,
    /// The window a rule's commands act on instead of the focused one.
    command_target: Option<WindowId>,
    /// What the focus callbacks last saw.
    last_focus: config_apply::FocusSnapshot,
    in_callback: bool,
    next_raise_sequence: u64,
    /// What IPC subscribers were last told.
    ipc: ipc::IpcState,
    /// Windows in a native macOS Space other than the one showing.
    foreign: HashSet<WindowId>,
    /// Asks the OS which of these WindowServer ids are in another native Space; polled on
    /// every [`Event::VisibilityTick`]. `None` in tests and where Spaces cannot be read.
    foreign_probe: Option<ForeignProbe>,
}

/// See [`Reactor::set_foreign_probe`].
type ProbeFn = Box<dyn FnMut(&[u32]) -> Vec<u32> + Send>;

struct ForeignProbe(ProbeFn);


impl Default for ForeignProbe {
    fn default() -> Self { ForeignProbe(Box::new(|_| Vec::new())) }
}

impl std::fmt::Debug for ForeignProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("ForeignProbe") }
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

    pub fn trees(&self) -> &TreeStore { &self.trees }

    pub fn layouts(&self) -> &LayoutRegistry { &self.layouts }

    /// The window commands act on: last reported as an app's main window, or set by a
    /// `focus` command.
    pub fn focused_window(&self) -> Option<WindowId> { self.focused_window }

    /// Add a layout system (bsp, master-stack, ... later); see [`LayoutRegistry::register`].
    pub fn register_layout(&mut self, system: Box<dyn LayoutSystem>) {
        self.layouts.register(system);
        self.trees.set_policies(self.layouts.policies());
    }

    /// Choose the tree clean-up passes for every workspace.
    pub fn set_normalization(&mut self, options: NormalizeOptions) { self.trees.set_options(options); }

    #[cfg(test)]
    pub(crate) fn tree_mut_for_test(&mut self, workspace: WorkspaceId) -> &mut crate::model::tree::WorkspaceTree {
        self.trees.get_mut(workspace).expect("workspace has a tree")
    }

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
                && !self.visibility.is_foreign(wid)
                && !self.visibility.is_given_up(wid)
                && record.display.as_ref().is_some_and(|d| self.displays.get(d).is_some());
            // A window in another native Space cannot be moved, so it may stay parked.
            if visible && is_parked && !self.visibility.is_foreign(wid) {
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

    /// Run a command. [`Event::Command`] does this and logs the error; tests call it
    /// directly to see it.
    pub fn run_command(&mut self, command: Command) -> Result<(), CommandError> {
        self.forget_destroyed_focus();
        self.sync_trees();
        // Commands that change which workspace the user is looking at must also move
        // real keyboard focus there, or the screen changes and the keys still go elsewhere.
        let refocus = matches!(
            command,
            Command::Workspace(_)
                | Command::WorkspaceBackAndForth
                | Command::FocusMonitor(_)
                | Command::MoveWorkspaceToMonitor(_)
        );
        let result = match command {
            Command::Workspace(name) => match self.workspaces.id_by_name(&name) {
                Ok(id) => self
                    .workspaces
                    .switch_to_workspace(id.number(), &self.displays)
                    .map_err(Into::into),
                Err(e) => Err(e.into()),
            },
            Command::MoveNodeToWorkspace { workspace, window, follow } => {
                match self.workspaces.id_by_name(&workspace) {
                    Ok(id) => match window.or(self.focused_window) {
                        Some(window) => self.workspaces.move_window_to_workspace(
                            window,
                            id.number(),
                            follow,
                            &self.displays,
                        ),
                        None => Err(WorkspaceError::NoFocusedWindow),
                    },
                    Err(e) => Err(e),
                }
                .map_err(Into::into)
            }
            Command::WorkspaceBackAndForth => {
                self.workspaces.back_and_forth(&self.displays).map_err(Into::into)
            }
            Command::MoveWorkspaceToMonitor(target) => self
                .workspaces
                .move_workspace_to_monitor(&target, &self.displays)
                .map_err(Into::into),
            Command::FocusMonitor(target) => {
                self.workspaces.focus_monitor(&target, &self.displays).map_err(Into::into)
            }
            Command::Focus { direction, wrap } => self.cmd_focus(direction, wrap).map_err(Into::into),
            Command::Move(direction) => self.cmd_move(direction).map_err(Into::into),
            Command::JoinWith(direction) => self.cmd_join_with(direction).map_err(Into::into),
            Command::Split(arg) => self.cmd_split(arg).map_err(Into::into),
            Command::Resize { dimension, amount } => {
                self.cmd_resize(dimension, amount).map_err(Into::into)
            }
            Command::Layout(args) => self.cmd_layout(&args).map_err(Into::into),
            Command::Fullscreen => self.cmd_fullscreen().map_err(Into::into),
            Command::BalanceSizes => self.cmd_balance_sizes().map_err(Into::into),
            Command::FlattenWorkspaceTree => self.cmd_flatten_workspace_tree().map_err(Into::into),
            Command::ResizeTo { dimension, size } => self.cmd_resize_to(dimension, size),
            Command::CloseAllWindowsButCurrent => self.cmd_close_all_but_current(),
            Command::MoveMouse { target, lazy } => self.cmd_move_mouse(target, lazy),
        };
        // Membership may have changed (move-node-to-workspace): the leaf follows.
        self.sync_trees();
        if refocus && result.is_ok() {
            self.refocus_after_switch();
        }
        self.check_focus_callbacks();
        self.sync_visibility();
        self.debug_check_invariants();
        result
    }

    /// After a switch: focus the window the focused workspace's tree rests on. An empty
    /// workspace has none, so focus is cleared (commands must not act on the window just
    /// hidden) and the pointer is brought to its monitor instead.
    fn refocus_after_switch(&mut self) {
        let Some(workspace) = self.workspaces.focused_workspace() else { return };
        let target = self
            .trees
            .get(workspace)
            .and_then(|tree| tree.focused_window())
            .filter(|&w| self.workspaces.workspace_of(w) == Some(workspace));
        match target {
            Some(window) => {
                self.focused_window = Some(window);
                self.request_raise_explicit(window);
            }
            None => {
                self.focused_window = None;
                let frame = self
                    .workspaces
                    .focused_display()
                    .and_then(|display| self.displays.get(display))
                    .map(|display| display.frame);
                if let Some(rect) = frame {
                    self.effects.push(Effect::MoveMouse { rect, lazy: true });
                }
            }
        }
    }

    /// The focused display is the one showing the focused window's workspace. A window
    /// on a hidden workspace (an app activated it behind our back) changes nothing.
    fn follow_focus_display(&mut self, window: WindowId) {
        let display = self
            .workspaces
            .workspace_of(window)
            .and_then(|workspace| self.workspaces.display_of(workspace))
            .cloned();
        if let Some(display) = display {
            self.workspaces.focus_display(display);
        }
    }

    /// Poll for windows in another native Space on every tick, using `probe`.
    pub fn set_foreign_probe(&mut self, probe: impl FnMut(&[u32]) -> Vec<u32> + Send + 'static) {
        self.foreign_probe = Some(ForeignProbe(Box::new(probe)));
    }

    fn poll_foreign_windows(&mut self) {
        let ids = self.windows.sys_ids();
        let Some(probe) = &mut self.foreign_probe else { return };
        let foreign: Vec<WindowServerId> = (probe.0)(&ids).into_iter().map(WindowServerId::new).collect();
        self.set_foreign_windows(&foreign);
    }

    fn set_foreign_windows(&mut self, ids: &[WindowServerId]) {
        let foreign: HashSet<WindowId> =
            ids.iter().filter_map(|&id| self.windows.by_sys_id(id)).map(|r| r.wid).collect();
        if foreign == self.foreign {
            return;
        }
        info!(count = foreign.len(), "windows in another native Space changed");
        self.visibility.set_foreign(foreign.iter().copied().collect());
        self.foreign = foreign;
    }

    /// Bring every workspace tree in line with workspace membership.
    fn sync_trees(&mut self) {
        self.orient_auto_roots();
        self.trees.sync(&self.workspaces, &self.windows);
        self.orient_auto_roots();
    }

    /// The window a tiling command acts on in `workspace`: the focused window if it is
    /// in that workspace's tree, else the tree's own focus. With `tiled_only`, a floating
    /// focused window is passed over for the tree's focus (spatial `focus` starts from
    /// the tiling, however the user got to the floating window).
    fn tiling_target(&self, workspace: WorkspaceId, tiled_only: bool) -> Result<WindowId, TilingError> {
        let tree = self.trees.get(workspace).ok_or(TilingError::NoWorkspaceHere)?;
        let usable = |w: &WindowId| tree.is_tiled(*w) || (!tiled_only && tree.is_floating(*w));
        self.command_target
            .filter(usable)
            .or_else(|| self.focused_window.filter(usable))
            .or_else(|| tree.focused_window())
            .ok_or(TilingError::NoFocusedWindow)
    }

    /// The focused workspace, which tiling commands act on.
    fn tiling_workspace(&self) -> Result<WorkspaceId, TilingError> {
        if let Some(workspace) = self.command_target.and_then(|w| self.workspaces.workspace_of(w)) {
            return Ok(workspace);
        }
        self.workspaces.focused_workspace().ok_or(TilingError::NoWorkspaceHere)
    }

    fn tree_mut(&mut self, workspace: WorkspaceId) -> Result<&mut crate::model::tree::WorkspaceTree, TilingError> {
        self.trees.get_mut(workspace).ok_or(TilingError::NoWorkspaceHere)
    }

    // Each tiling command checks everything that can fail before it changes the tree, so
    // a failed command leaves the tree as it was.

    fn cmd_focus(&mut self, direction: Direction, wrap: bool) -> Result<(), TilingError> {
        let workspace = self.tiling_workspace()?;
        let window = self.tiling_target(workspace, true)?;
        let tree = self.trees.get(workspace).ok_or(TilingError::NoWorkspaceHere)?;
        let Some(target) = ops::focus_target(tree, window, direction, wrap, &self.layouts)? else {
            return Ok(());
        };
        self.focused_window = Some(target);
        let tree = self.tree_mut(workspace)?;
        tree.set_focus(target);
        // Focus going elsewhere ends fullscreen: the target would be hidden under it.
        if tree.fullscreen() == Some(window) && target != window {
            tree.toggle_fullscreen(window);
        }
        self.request_raise_explicit(target);
        Ok(())
    }

    fn cmd_move(&mut self, direction: Direction) -> Result<(), TilingError> {
        let workspace = self.tiling_workspace()?;
        let window = self.tiling_target(workspace, false)?;
        let tree = self.trees.get_mut(workspace).ok_or(TilingError::NoWorkspaceHere)?;
        ops::move_window(tree, window, direction, &self.layouts)
    }

    fn cmd_join_with(&mut self, direction: Direction) -> Result<(), TilingError> {
        let workspace = self.tiling_workspace()?;
        let window = self.tiling_target(workspace, false)?;
        let tree = self.trees.get_mut(workspace).ok_or(TilingError::NoWorkspaceHere)?;
        ops::join_with(tree, window, direction, &self.layouts)
    }

    fn cmd_split(&mut self, arg: SplitArg) -> Result<(), TilingError> {
        let workspace = self.tiling_workspace()?;
        let window = self.tiling_target(workspace, false)?;
        ops::split(self.tree_mut(workspace)?, window, arg)
    }

    fn cmd_resize(&mut self, dimension: ResizeDimension, amount: i32) -> Result<(), TilingError> {
        let workspace = self.tiling_workspace()?;
        let window = self.tiling_target(workspace, false)?;
        let computed = self.workspace_layout(workspace).ok_or(TilingError::NoDisplay)?;
        let tree = self.trees.get_mut(workspace).ok_or(TilingError::NoWorkspaceHere)?;
        ops::resize(tree, window, dimension, amount, &computed, &self.layouts)
    }

    fn cmd_layout(&mut self, args: &[LayoutArg]) -> Result<(), TilingError> {
        let workspace = self.tiling_workspace()?;
        let window = self.tiling_target(workspace, false)?;
        let tree = self.trees.get_mut(workspace).ok_or(TilingError::NoWorkspaceHere)?;
        ops::apply_layout(tree, window, args, &self.layouts)
    }

    fn cmd_fullscreen(&mut self) -> Result<(), TilingError> {
        let workspace = self.tiling_workspace()?;
        let window = self.tiling_target(workspace, false)?;
        self.tree_mut(workspace)?.toggle_fullscreen(window).ok_or(TilingError::NotInTree(window))?;
        Ok(())
    }

    fn cmd_balance_sizes(&mut self) -> Result<(), TilingError> {
        let workspace = self.tiling_workspace()?;
        self.tree_mut(workspace)?.balance();
        Ok(())
    }

    fn cmd_flatten_workspace_tree(&mut self) -> Result<(), TilingError> {
        let workspace = self.tiling_workspace()?;
        self.tree_mut(workspace)?.flatten_to_root();
        Ok(())
    }

    // ---- computed layout ----

    /// `workspace`'s layout in the frame of the display it is, or would be, shown on:
    /// the one it is visible on, else the one it resolves to (a hidden workspace is laid
    /// out for when it returns). Floating windows keep their own frames.
    fn workspace_layout(&self, workspace: WorkspaceId) -> Option<layout::ComputedLayout> {
        let display = self
            .workspaces
            .display_of(workspace)
            .cloned()
            .or_else(|| self.workspaces.resolve_display(workspace, &self.displays))?;
        let area = to_layout_rect(self.displays.get(&display)?.frame);
        let gaps = self
            .config
            .as_ref()
            .map(|c| c.gaps.resolve(&display, &self.displays))
            .unwrap_or_default();
        let tree = self.trees.get(workspace)?;
        let frames = |window: WindowId| self.windows.get(window).map(|r| to_layout_rect(r.frame));
        Some(layout::gaps::compute_with_gaps(tree, area, &gaps, &self.layouts, &frames))
    }

    /// Where `workspace`'s windows belong, tiled and floating, by tiling layout. `None`
    /// if the workspace is unknown or no display exists.
    pub fn workspace_frames(&self, workspace: WorkspaceId) -> Option<Vec<(WindowId, CGRect)>> {
        let computed = self.workspace_layout(workspace)?;
        Some(computed.windows.into_iter().map(|(w, r)| (w, from_layout_rect(r))).collect())
    }

    /// The tiling layout's target for every window on a visible workspace, with the
    /// z-order hints the presenter needs. [`Reactor::flush_layout`] applies them.
    pub fn layout_targets(&self) -> Vec<LayoutTarget> {
        self.layout_batches().into_iter().flat_map(|(_, targets)| targets).collect()
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
        if let Err(violation) = self.trees.check_invariants(&self.workspaces, &self.windows) {
            panic!("tiling tree invariant violated: {violation}");
        }
    }

    fn debug_check_invariants(&self) {
        if cfg!(debug_assertions) {
            self.assert_invariants();
        }
    }

    pub async fn run(mut self, mut events: Receiver) {
        // Record the starting state so the first change reports the right `previous`.
        self.publish_ipc_changes();
        while let Some((span, event)) = events.recv().await {
            let _guard = span.enter();
            self.handle_event(event);
            self.flush_visibility();
            self.flush_layout();
            self.flush_effects();
            self.publish_ipc_changes();
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
                    // The tree remembers focus per container; bring it up to date.
                    self.workspaces.sync_windows(&self.windows);
                    self.sync_trees();
                    if let Some(tree) = self
                        .workspaces
                        .workspace_of(wid)
                        .and_then(|ws| self.trees.get_mut(ws))
                    {
                        tree.set_focus(wid);
                    }
                    self.follow_focus_display(wid);
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
            Event::Actions(actions) => {
                if let Err(error) = self.run_actions(&actions, None) {
                    warn!(?actions, %error, "actions stopped at the first failure");
                }
            }
            Event::ForeignWindows(ids) => self.set_foreign_windows(&ids),
            Event::Ipc(request) => self.handle_ipc(request),
            Event::ConfigReloaded(config) => self.apply_config(config),
            Event::StartupFinished => self.finish_startup(),
            Event::PointerSettledOn(wsid) => self.focus_window_under_pointer(wsid),
            Event::VisibilityTick => self.poll_foreign_windows(),
            Event::MenuOpened(_)
            | Event::MenuClosed(_)
            | Event::RaiseTargetsMissing { .. }
            | Event::RaiseCompleted { .. } => {}
        }
        self.workspaces.sync_windows(&self.windows);
        self.sync_trees();
        self.forget_destroyed_focus();
        self.run_window_rules();
        self.check_focus_callbacks();
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
            self.detected.push(wid);
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
        self.visibility.reset_retries();
        if !changes.removed.is_empty() {
            self.visibility.display_removed(&self.parked_registry);
        }
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

fn to_layout_rect(r: CGRect) -> layout::Rect {
    layout::Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

fn from_layout_rect(r: layout::Rect) -> CGRect {
    CGRect::new(
        objc2_core_foundation::CGPoint::new(r.origin.x, r.origin.y),
        objc2_core_foundation::CGSize::new(r.size.w, r.size.h),
    )
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

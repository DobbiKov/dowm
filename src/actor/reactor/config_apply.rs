//! The reactor's side of the config: applying a (re)loaded [`Config`], running action
//! lists (hotkeys, callbacks, rules), `on-window-detected` rules, the focus callbacks,
//! focus-follows-mouse and the commands that need OS help (`move-mouse`,
//! `close-all-windows-but-current`).
//!
//! Nothing here calls an OS API. Whatever needs one becomes an [`Effect`] that the
//! platform shell carries out; windows are acted on through their app actors' request
//! channels, as everywhere else in the reactor.

use std::sync::Arc;

use objc2_core_foundation::CGRect;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::effects::Effect;
use super::{CommandError, MouseTarget, Reactor};
use crate::actor::app::{self, Quiet, WindowId};
use crate::config::{Action, Config, RootLayout, RootOrientation, WindowFacts, rules_to_run};
use crate::layout::ops::{self, ResizeDimension, TilingError};
use crate::layout::{ACCORDION, Accordion, TILES};
use crate::model::display::DisplayUuid;
use crate::model::tree::Orientation;
use crate::model::workspace::{ForceAssignment, WorkspaceError};
use crate::sys::window_server::WindowServerId;

/// What the focus callbacks compare: the focused display and the window with focus.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct FocusSnapshot {
    display: Option<DisplayUuid>,
    window: Option<WindowId>,
}

impl Reactor {
    /// A reactor whose workspace pool comes from `config`, with the rest of `config`
    /// applied. The daemon starts this way.
    pub fn with_config(
        window_notify_tx: Option<crate::actor::window_notify::Sender>,
        config: Arc<Config>,
    ) -> Result<Self, WorkspaceError> {
        let mut reactor = Reactor::with_workspaces(window_notify_tx, config.workspace_config())?;
        reactor.apply_config(config);
        Ok(reactor)
    }

    /// The config in force.
    pub fn config(&self) -> Option<&Arc<Config>> { self.config.as_ref() }

    /// Send queued effects to `tx` after every event (see [`Reactor::run`]).
    pub fn set_effect_sender(&mut self, tx: super::effects::EffectSender) { self.effects_tx = Some(tx); }

    /// The effects queued and not yet sent, oldest first.
    pub fn take_effects(&mut self) -> Vec<Effect> { std::mem::take(&mut self.effects) }

    pub(super) fn flush_effects(&mut self) {
        let Some(tx) = &self.effects_tx else { return };
        for effect in self.effects.drain(..) {
            tx.send(effect);
        }
    }

    /// Apply a config live: gaps, accordion padding, normalization flags, root
    /// layout/orientation, force-assignments, rules, callbacks, bindings (through
    /// [`Effect::ConfigApplied`]) and focus-follows-mouse. Never fails.
    ///
    /// The workspace pool cannot change without a restart: a config that wants another
    /// pool is applied in every other respect, the old pool stays, and a warning says so.
    pub fn apply_config(&mut self, config: Arc<Config>) {
        let running = self.workspaces.names();
        if running != config.workspaces {
            warn!(
                running = %running.join(" "),
                wanted = %config.workspaces.join(" "),
                "the workspace pool changed; this requires restart, keeping the running pool"
            );
        }
        for warning in &config.warnings {
            warn!("config: {warning}");
        }

        self.set_normalization(config.normalization);
        self.register_layout(Box::new(Accordion::new(f64::from(config.accordion_padding))));
        let layout = match config.default_root_layout {
            RootLayout::Tiles => TILES,
            RootLayout::Accordion => ACCORDION,
        };
        let orientation = match config.default_root_orientation {
            RootOrientation::Vertical => Orientation::Vertical,
            RootOrientation::Horizontal | RootOrientation::Auto => Orientation::Horizontal,
        };
        self.trees.set_root_defaults(layout, orientation);

        self.set_visibility_config(super::VisibilityConfig {
            animate_switches: config.animate_workspace_switch,
        });
        self.presenter.settings.animate = config.animate_layout;
        self.presenter.settings.duration = config.animation_duration;

        self.workspaces.set_auto_back_and_forth(config.workspace_auto_back_and_forth);
        let assignments: Vec<ForceAssignment> = config
            .force_assignments
            .iter()
            .filter_map(|(name, monitors)| match self.workspaces.id_by_name(name) {
                Ok(id) => Some(ForceAssignment { workspace: id.number(), monitors: monitors.clone() }),
                Err(_) => {
                    warn!(workspace = %name, "force-assignment for a workspace outside the running pool; requires restart");
                    None
                }
            })
            .collect();
        if let Err(error) = self.workspaces.set_force_assignments(&assignments) {
            warn!(%error, "workspace-to-monitor-force-assignment not applied");
        }

        info!(
            modes = config.modes.len(),
            rules = config.window_rules.len(),
            focus_follows_mouse = config.focus_follows_mouse,
            "config applied"
        );
        self.config = Some(Arc::clone(&config));
        self.sync_trees();
        self.last_focus = self.current_focus();
        self.effects.push(Effect::ConfigApplied(config));
    }

    pub(super) fn finish_startup(&mut self) {
        self.startup_finished = true;
        self.last_focus = self.current_focus();
        if self.startup_commands_ran {
            return;
        }
        self.startup_commands_ran = true;
        let Some(config) = self.config.clone() else { return };
        if !config.after_login_command.is_empty() {
            debug!("after-login-command is parsed but not run: dowm installs no login item");
        }
        if let Err(error) = self.run_actions(&config.after_startup_command, None) {
            warn!(%error, "after-startup-command stopped at the first failure");
        }
    }

    /// Run `actions` in order. The first failing command stops every other command, but
    /// `mode` actions still run (and the error is returned), so a chain like
    /// `['flatten-workspace-tree', 'mode main']` cannot leave the user stuck in a mode.
    /// Reactor commands run now; `mode`, `reload-config` and `exec-and-forget` become
    /// effects. `target` is the window the commands act on (an `on-window-detected`
    /// rule's window) instead of the focused one.
    pub fn run_actions(&mut self, actions: &[Action], target: Option<WindowId>) -> Result<(), CommandError> {
        let mut failure = None;
        for action in actions {
            match action {
                Action::Mode(name) => self.effects.push(Effect::SetMode(name.clone())),
                _ if failure.is_some() => {}
                Action::Reactor(command) => {
                    let mut command = command.clone();
                    if let (super::Command::MoveNodeToWorkspace { window, .. }, Some(target)) =
                        (&mut command, target)
                        && window.is_none()
                    {
                        *window = Some(target);
                    }
                    self.command_target = target;
                    let result = self.run_command(command);
                    self.command_target = None;
                    if let Err(error) = result {
                        failure = Some(error);
                    }
                }
                Action::ReloadConfig => self.effects.push(Effect::ReloadConfig),
                Action::ExecAndForget(shell) => self.effects.push(Effect::Exec(shell.clone())),
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Run the `on-window-detected` rules for windows first seen since the last pass.
    pub(super) fn run_window_rules(&mut self) {
        let detected = std::mem::take(&mut self.detected);
        let Some(config) = self.config.clone() else { return };
        if config.window_rules.is_empty() {
            return;
        }
        for wid in detected {
            let runs: Vec<Vec<Action>> = {
                let Some(record) = self.windows.get(wid) else { continue };
                if !record.is_standard {
                    continue;
                }
                let app = self.apps.get(&wid.pid);
                let workspace = self
                    .workspaces
                    .workspace_of(wid)
                    .and_then(|id| self.workspaces.name(id));
                let facts = WindowFacts {
                    app_id: record
                        .bundle_id
                        .as_deref()
                        .or_else(|| app.and_then(|a| a.info.bundle_id.as_deref())),
                    app_name: app.and_then(|a| a.info.localized_name.as_deref()),
                    title: &record.title,
                    workspace,
                    during_startup: !self.startup_finished,
                };
                rules_to_run(&config.window_rules, &facts).iter().map(|r| r.run.clone()).collect()
            };
            for run in runs {
                if let Err(error) = self.run_actions(&run, Some(wid)) {
                    warn!(?wid, %error, "on-window-detected commands stopped at the first failure");
                    break;
                }
            }
        }
    }

    /// The focused display and the window with focus: the last reported main window if it
    /// is on the focused workspace, else that workspace's own focus.
    fn current_focus(&self) -> FocusSnapshot {
        let workspace = self.workspaces.focused_workspace();
        let window = workspace.and_then(|ws| {
            self.focused_window
                .filter(|&w| self.workspaces.workspace_of(w) == Some(ws))
                .or_else(|| self.trees.get(ws).and_then(|t| t.focused_window()))
        });
        FocusSnapshot { display: self.workspaces.focused_display().cloned(), window }
    }

    /// Fire `on-focused-monitor-changed` and `on-focus-changed` once per change. Not
    /// before start-up has finished (windows appearing is not the user moving focus), and
    /// not recursively: whatever the callbacks themselves change is not a new change.
    pub(super) fn check_focus_callbacks(&mut self) {
        let Some(config) = self.config.clone() else { return };
        if self.in_callback {
            return;
        }
        let now = self.current_focus();
        if now == self.last_focus {
            return;
        }
        let before = std::mem::replace(&mut self.last_focus, now.clone());
        if !self.startup_finished {
            return;
        }
        let monitor_changed =
            before.display.is_some() && now.display.is_some() && before.display != now.display;
        let window_changed = before.window != now.window;
        self.in_callback = true;
        if monitor_changed
            && let Err(error) = self.run_actions(&config.on_focused_monitor_changed, None)
        {
            debug!(%error, "on-focused-monitor-changed stopped at the first failure");
        }
        if window_changed
            && let Err(error) = self.run_actions(&config.on_focus_changed, None)
        {
            debug!(%error, "on-focus-changed stopped at the first failure");
        }
        self.in_callback = false;
        self.last_focus = self.current_focus();
    }

    /// Give `auto` root orientation to workspaces that are still empty: horizontal on a
    /// wide monitor, vertical on a tall one.
    pub(super) fn orient_auto_roots(&mut self) {
        if self.config.as_ref().is_none_or(|c| c.default_root_orientation != RootOrientation::Auto) {
            return;
        }
        let (workspaces, displays) = (&self.workspaces, &self.displays);
        self.trees.orient_empty_roots(&|ws| {
            let display = workspaces
                .display_of(ws)
                .cloned()
                .or_else(|| workspaces.resolve_display(ws, displays))?;
            let frame = displays.get(&display)?.frame;
            Some(if frame.size.width >= frame.size.height {
                Orientation::Horizontal
            } else {
                Orientation::Vertical
            })
        });
    }

    // ---- commands that need more than the tree ----

    /// The window's frame as the reactor last saw it.
    fn window_frame(&self, window: WindowId) -> Option<CGRect> { self.windows.get(window).map(|r| r.frame) }

    pub(super) fn cmd_move_mouse(&mut self, target: MouseTarget, lazy: bool) -> Result<(), CommandError> {
        let rect = match target {
            MouseTarget::Monitor => {
                let display = self.workspaces.focused_display().ok_or(WorkspaceError::NoDisplays)?;
                self.displays.get(display).ok_or(WorkspaceError::NoDisplays)?.frame
            }
            MouseTarget::Window => {
                let window = self.current_focus().window.ok_or(WorkspaceError::NoFocusedWindow)?;
                self.window_frame(window).ok_or(WorkspaceError::UnknownWindow(window))?
            }
        };
        self.effects.push(Effect::MoveMouse { rect, lazy });
        Ok(())
    }

    /// `resize width|height <size>`: a delta from the window's current extent.
    pub(super) fn cmd_resize_to(&mut self, dimension: ResizeDimension, size: i32) -> Result<(), CommandError> {
        let workspace = self.tiling_workspace()?;
        let window = self.tiling_target(workspace, false)?;
        let computed = self.workspace_layout(workspace).ok_or(TilingError::NoDisplay)?;
        let rect = computed.windows.get(&window).ok_or(TilingError::NotInTree(window))?;
        let current = match dimension {
            ResizeDimension::Width => rect.size.w,
            ResizeDimension::Height => rect.size.h,
            ResizeDimension::Smart => return Err(TilingError::NothingToResize.into()),
        };
        let amount = (f64::from(size) - current).round() as i32;
        if amount == 0 {
            return Ok(());
        }
        let tree = self.trees.get_mut(workspace).ok_or(TilingError::NoWorkspaceHere)?;
        ops::resize(tree, window, dimension, amount, &computed, &self.layouts).map_err(Into::into)
    }

    /// `close-all-windows-but-current`: ask every other window of the focused workspace
    /// to close, as Command-W would. Windows without a WindowServer id are skipped: the
    /// app actor could not tell which window to close, and would close the frontmost.
    pub(super) fn cmd_close_all_but_current(&mut self) -> Result<(), CommandError> {
        let workspace = self.tiling_workspace()?;
        let current = self.tiling_target(workspace, false)?;
        for window in self.workspaces.windows_in(workspace) {
            if window == current {
                continue;
            }
            let Some(record) = self.windows.get(window) else { continue };
            if record.is_minimized || record.sys_id.is_none() {
                continue;
            }
            if let Some(app) = self.apps.get(&window.pid) {
                _ = app.handle.send(app::Request::CloseWindow(record.sys_id));
            }
        }
        Ok(())
    }

    /// Ask the window's app to raise and focus it, as a side effect of layout or the
    /// pointer. Not for a window in another native Space: raising it makes macOS animate
    /// a switch to that Space, which nobody asked for.
    pub(super) fn request_raise(&mut self, window: WindowId) {
        if self.foreign.contains(&window) {
            debug!(?window, "not raising a window in another native Space");
            return;
        }
        self.request_raise_explicit(window);
    }

    /// Like [`Self::request_raise`], for a window the user asked for (`focus`, a workspace
    /// switch). If it is in another native Space, macOS switches there: the only way to
    /// reach it.
    pub(super) fn request_raise_explicit(&mut self, window: WindowId) {
        let Some(app) = self.apps.get(&window.pid) else { return };
        self.next_raise_sequence += 1;
        _ = app.handle.send(app::Request::Raise(
            vec![window],
            CancellationToken::new(),
            self.next_raise_sequence,
            Quiet::No,
        ));
    }

    /// Focus-follows-mouse: the pointer settled over `wsid`. Windows that are parked,
    /// hidden with their workspace, minimized or already focused are left alone.
    pub(super) fn focus_window_under_pointer(&mut self, wsid: WindowServerId) {
        if !self.config.as_ref().is_some_and(|c| c.focus_follows_mouse) {
            return;
        }
        let Some(record) = self.windows.by_sys_id(wsid) else { return };
        let (window, display) = (record.wid, record.display.clone());
        if record.is_minimized
            || self.visibility.is_parked(window)
            || !self.workspaces.window_is_visible(window)
            || self.current_focus().window == Some(window)
        {
            return;
        }
        debug!(?window, "focus follows mouse");
        self.focused_window = Some(window);
        if let Some(tree) = self.workspaces.workspace_of(window).and_then(|ws| self.trees.get_mut(ws)) {
            tree.set_focus(window);
        }
        if let Some(display) = display {
            self.workspaces.focus_display(display);
        }
        self.request_raise(window);
    }
}

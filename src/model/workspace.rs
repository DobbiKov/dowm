//! The workspace pool and its mapping onto displays.
//!
//! A fixed set of numbered workspaces is created from config at startup; there is no
//! lazy creation and no garbage collection. Workspaces are pure bookkeeping: no
//! native Space is involved, and nothing here touches AX or CGS.
//!
//! The central invariant, after AeroSpace's `Workspace.swift` `setActiveWorkspace`:
//! every connected display has exactly one visible workspace, and every workspace is
//! visible on at most one display. Both directions of that relation live in
//! `visible` / `visible_on` and are only ever changed together, in
//! [`WorkspaceStore::set_active_workspace`] (evict and plug) and in
//! [`WorkspaceStore::displays_changed`] (display removal). [`WorkspaceStore::
//! check_invariants`] verifies the whole structure and runs after every reactor step in
//! debug builds and tests.
//!
//! The one exception to "exactly one" is a pool smaller than the display count: then
//! the surplus displays have no workspace to show.
//!
//! Workspace -> display resolution is a priority chain
//! ([`WorkspaceStore::resolve_display`]): force-assignment, the display it is visible
//! on, the display it was last seen on (if still connected), the main display.

use std::collections::BTreeMap;
use std::fmt;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::actor::app::WindowId;
use crate::common::collections::HashMap;
use crate::model::display::{DisplayChanges, DisplaySnapshot, DisplayStore, DisplayUuid};
use crate::model::window_store::WindowStore;

/// A workspace, by its 1-based number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkspaceId(u32);

impl WorkspaceId {
    pub fn new(number: u32) -> Self { Self(number) }

    pub fn number(self) -> u32 { self.0 }

    fn index(self) -> usize { self.0 as usize - 1 }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Selects a display. Used by force-assignment and by the monitor commands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MonitorPattern {
    /// 1-based position among the displays, ordered left to right (then top to bottom).
    Sequence(usize),
    /// The main display (the first the system reports).
    Main,
    /// The only non-main display. Matches nothing unless there is exactly one.
    Secondary,
    /// A regex searched in the display's name.
    Name(String),
}

/// Which display a monitor command refers to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MonitorTarget {
    Direction { direction: Direction, wrap: bool },
    Next { wrap: bool },
    Prev { wrap: bool },
    Pattern(MonitorPattern),
}

/// Pins a workspace to a display. Patterns are tried in order; the first one that
/// matches a connected display wins. If none does, the workspace is unpinned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForceAssignment {
    /// Workspace number.
    pub workspace: u32,
    pub monitors: Vec<MonitorPattern>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct WorkspaceConfig {
    /// One entry per workspace; the pool size is `names.len()`.
    pub names: Vec<String>,
    /// `workspace N` on the focused workspace goes back to the previous one.
    pub auto_back_and_forth: bool,
    pub force_assignments: Vec<ForceAssignment>,
}

impl WorkspaceConfig {
    /// `count` workspaces named "1".."count".
    pub fn numbered(count: u32) -> Self {
        WorkspaceConfig {
            names: (1..=count).map(|n| n.to_string()).collect(),
            auto_back_and_forth: false,
            force_assignments: Vec::new(),
        }
    }

    pub fn with_force(mut self, workspace: u32, monitors: Vec<MonitorPattern>) -> Self {
        self.force_assignments.push(ForceAssignment { workspace, monitors });
        self
    }
}

impl Default for WorkspaceConfig {
    fn default() -> Self { Self::numbered(9) }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceError {
    /// The config has no workspaces.
    EmptyPool,
    /// A workspace named (or numbered) like this is not in the pool.
    NoSuchWorkspace(String),
    /// A force-assignment or monitor pattern that cannot be used.
    BadPattern(String),
    NoDisplays,
    /// The focused display has no workspace to act on (a pool smaller than the displays).
    NoWorkspaceHere,
    /// A monitor target that matches no display.
    NoSuchDisplay,
    NoPreviousWorkspace,
    NoFocusedWindow,
    UnknownWindow(WindowId),
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPool => write!(f, "no workspaces configured"),
            Self::NoSuchWorkspace(n) => write!(f, "no workspace {n}"),
            Self::BadPattern(why) => write!(f, "bad monitor pattern: {why}"),
            Self::NoDisplays => write!(f, "no displays connected"),
            Self::NoWorkspaceHere => write!(f, "the focused display has no workspace"),
            Self::NoSuchDisplay => write!(f, "no display matches"),
            Self::NoPreviousWorkspace => write!(f, "no previous workspace"),
            Self::NoFocusedWindow => write!(f, "no focused window"),
            Self::UnknownWindow(wid) => write!(f, "unknown window {wid:?}"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

/// A [`MonitorPattern`] with its regex compiled.
#[derive(Clone, Debug)]
pub enum Pattern {
    Sequence(usize),
    Main,
    Secondary,
    Name(Regex),
}

impl Pattern {
    pub fn compile(pattern: &MonitorPattern) -> Result<Self, WorkspaceError> {
        Ok(match pattern {
            MonitorPattern::Sequence(0) => {
                return Err(WorkspaceError::BadPattern("sequence numbers start at 1".into()));
            }
            MonitorPattern::Sequence(n) => Pattern::Sequence(*n),
            MonitorPattern::Main => Pattern::Main,
            MonitorPattern::Secondary => Pattern::Secondary,
            MonitorPattern::Name(re) => Pattern::Name(
                Regex::new(re).map_err(|e| WorkspaceError::BadPattern(format!("{re:?}: {e}")))?,
            ),
        })
    }

    /// Whether this pattern selects the display `uuid` among `displays`.
    pub fn matches(&self, uuid: &DisplayUuid, displays: &DisplayStore) -> bool {
        self.find(displays).is_some_and(|found| &found == uuid)
    }

    fn find(&self, displays: &DisplayStore) -> Option<DisplayUuid> {
        let sorted = left_to_right(displays);
        let main = displays.main()?;
        match self {
            Pattern::Sequence(n) => sorted.get(n - 1).map(|d| d.uuid.clone()),
            Pattern::Main => Some(main.uuid.clone()),
            Pattern::Secondary => {
                let mut others = sorted.iter().filter(|d| d.uuid != main.uuid);
                match (others.next(), others.next()) {
                    (Some(only), None) => Some(only.uuid.clone()),
                    _ => None,
                }
            }
            Pattern::Name(re) => sorted
                .iter()
                .find(|d| d.name.as_deref().is_some_and(|name| re.is_match(name)))
                .map(|d| d.uuid.clone()),
        }
    }
}

/// Whether `pattern` selects the display `uuid` among `displays`. A pattern that does not
/// compile matches nothing.
pub fn pattern_matches(pattern: &MonitorPattern, uuid: &DisplayUuid, displays: &DisplayStore) -> bool {
    Pattern::compile(pattern).is_ok_and(|p| p.matches(uuid, displays))
}

/// Where a workspace lived when its display was unplugged. Reconnecting that display
/// (matched by UUID) brings the workspace back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Home {
    pub display: DisplayUuid,
    /// It was the workspace showing on that display, so it shows there again.
    pub was_active: bool,
}

#[derive(Debug)]
struct Workspace {
    name: String,
    /// The display it was last visible on, whether or not that is still connected.
    last_display: Option<DisplayUuid>,
    home: Option<Home>,
    force: Vec<Pattern>,
}

#[derive(Debug)]
pub struct WorkspaceStore {
    workspaces: Vec<Workspace>,
    /// display -> its visible workspace. Mirror of `visible_on`.
    visible: BTreeMap<DisplayUuid, WorkspaceId>,
    /// workspace -> the display it is visible on. Mirror of `visible`.
    visible_on: BTreeMap<WorkspaceId, DisplayUuid>,
    /// Which workspace owns each window. Hidden workspaces keep their windows.
    membership: HashMap<WindowId, WorkspaceId>,
    /// The workspace that was focused before the current one, for back-and-forth.
    previous: Option<WorkspaceId>,
    focused_display: Option<DisplayUuid>,
    auto_back_and_forth: bool,
}

impl Default for WorkspaceStore {
    fn default() -> Self {
        Self::new(&WorkspaceConfig::default()).expect("the default config is valid")
    }
}

impl WorkspaceStore {
    /// Create the pool. Fails on an empty pool, on a force-assignment for a workspace
    /// that does not exist, and on a pattern that does not compile.
    pub fn new(config: &WorkspaceConfig) -> Result<Self, WorkspaceError> {
        if config.names.is_empty() {
            return Err(WorkspaceError::EmptyPool);
        }
        let mut workspaces: Vec<Workspace> = config
            .names
            .iter()
            .map(|name| Workspace {
                name: name.clone(),
                last_display: None,
                home: None,
                force: Vec::new(),
            })
            .collect();
        for rule in &config.force_assignments {
            let slot = (rule.workspace as usize)
                .checked_sub(1)
                .and_then(|i| workspaces.get_mut(i))
                .ok_or(WorkspaceError::NoSuchWorkspace(rule.workspace.to_string()))?;
            for pattern in &rule.monitors {
                slot.force.push(Pattern::compile(pattern)?);
            }
        }
        Ok(WorkspaceStore {
            workspaces,
            visible: BTreeMap::new(),
            visible_on: BTreeMap::new(),
            membership: HashMap::default(),
            previous: None,
            focused_display: None,
            auto_back_and_forth: config.auto_back_and_forth,
        })
    }

    // ---- queries ----

    pub fn len(&self) -> usize { self.workspaces.len() }

    pub fn is_empty(&self) -> bool { self.workspaces.is_empty() }

    pub fn ids(&self) -> impl Iterator<Item = WorkspaceId> + use<> {
        (1..=self.workspaces.len() as u32).map(WorkspaceId)
    }

    pub fn name(&self, id: WorkspaceId) -> Option<&str> {
        self.get(id).map(|w| w.name.as_str())
    }

    /// The workspace showing on `display`.
    pub fn visible_workspace(&self, display: &DisplayUuid) -> Option<WorkspaceId> {
        self.visible.get(display).copied()
    }

    /// The display `id` is showing on, if it is visible.
    pub fn display_of(&self, id: WorkspaceId) -> Option<&DisplayUuid> { self.visible_on.get(&id) }

    pub fn focused_display(&self) -> Option<&DisplayUuid> { self.focused_display.as_ref() }

    pub fn focused_workspace(&self) -> Option<WorkspaceId> {
        self.focused_display.as_ref().and_then(|d| self.visible_workspace(d))
    }

    pub fn previous_workspace(&self) -> Option<WorkspaceId> { self.previous }

    /// Where `id` was parked when its display was unplugged, until that display returns.
    pub fn home_display(&self, id: WorkspaceId) -> Option<&Home> {
        self.get(id).and_then(|w| w.home.as_ref())
    }

    pub fn workspace_of(&self, window: WindowId) -> Option<WorkspaceId> {
        self.membership.get(&window).copied()
    }

    /// The windows `id` owns, visible or not, in a stable order.
    pub fn windows_in(&self, id: WorkspaceId) -> Vec<WindowId> {
        let mut windows: Vec<_> =
            self.membership.iter().filter(|&(_, &ws)| ws == id).map(|(&w, _)| w).collect();
        windows.sort();
        windows
    }

    /// Whether the window's workspace is showing on some display. The corner-parking
    /// step reads this to decide what to hide.
    pub fn window_is_visible(&self, window: WindowId) -> bool {
        self.workspace_of(window).is_some_and(|ws| self.visible_on.contains_key(&ws))
    }

    /// The display pinned to `id` by force-assignment, if any pattern matches a
    /// connected display.
    pub fn forced_display(&self, id: WorkspaceId, displays: &DisplayStore) -> Option<DisplayUuid> {
        self.get(id)?.force.iter().find_map(|p| p.find(displays))
    }

    /// Where `id` belongs: force-assignment, then the display it is visible on, then the
    /// display it was last seen on if still connected, then the main display.
    pub fn resolve_display(&self, id: WorkspaceId, displays: &DisplayStore) -> Option<DisplayUuid> {
        let workspace = self.get(id)?;
        self.forced_display(id, displays)
            .or_else(|| self.visible_on.get(&id).filter(|d| displays.get(d).is_some()).cloned())
            .or_else(|| {
                workspace.last_display.clone().filter(|d| displays.get(d).is_some())
            })
            .or_else(|| displays.main().map(|d| d.uuid.clone()))
    }

    // ---- the choke point ----

    /// Make `id` the visible workspace on `display`.
    ///
    /// Every change to the visible relation other than a display disappearing goes
    /// through here, so both sides stay consistent: the workspace is removed from the
    /// display it was visible on, the display's previous occupant is evicted, and the
    /// display left without a workspace is plugged with another one.
    pub fn set_active_workspace(
        &mut self,
        display: &DisplayUuid,
        id: WorkspaceId,
        displays: &DisplayStore,
    ) -> Result<(), WorkspaceError> {
        self.set_active_excluding(display, id, displays, &[])
    }

    /// [`Self::set_active_workspace`], where the display it vacates is not plugged with
    /// any of `exclude` (workspaces being put back hidden by a reconnect).
    fn set_active_excluding(
        &mut self,
        display: &DisplayUuid,
        id: WorkspaceId,
        displays: &DisplayStore,
        exclude: &[WorkspaceId],
    ) -> Result<(), WorkspaceError> {
        if displays.get(display).is_none() {
            return Err(WorkspaceError::NoSuchDisplay);
        }
        if self.get(id).is_none() {
            return Err(WorkspaceError::NoSuchWorkspace(id.0.to_string()));
        }
        if self.visible.get(display) == Some(&id) {
            return Ok(());
        }
        let vacated = self.visible_on.remove(&id);
        if let Some(old_display) = &vacated {
            self.visible.remove(old_display);
        }
        if let Some(evicted) = self.visible.insert(display.clone(), id) {
            self.visible_on.remove(&evicted);
        }
        self.visible_on.insert(id, display.clone());
        self.workspaces[id.index()].last_display = Some(display.clone());
        if let Some(hole) = vacated {
            self.plug_hole(&hole, displays, exclude);
        }
        Ok(())
    }

    /// Give `display` a visible workspace if it has none. Preference: a workspace that
    /// resolves to it, then one allowed on it (not pinned elsewhere) that is not parked
    /// from an unplugged display, then a parked one, then any hidden workspace.
    ///
    /// Workspaces in `exclude` are not candidates.
    fn plug_hole(&mut self, display: &DisplayUuid, displays: &DisplayStore, exclude: &[WorkspaceId]) {
        if self.visible.contains_key(display) || displays.get(display).is_none() {
            return;
        }
        let pick = self
            .ids()
            .filter(|id| !self.visible_on.contains_key(id) && !exclude.contains(id))
            .min_by_key(|&id| {
                let rank = if self.resolve_display(id, displays).as_ref() == Some(display) {
                    0
                } else if self.forced_display(id, displays).is_none_or(|d| &d == display) {
                    if self.workspaces[id.index()].home.is_none() { 1 } else { 2 }
                } else {
                    3
                };
                (rank, id)
            });
        if let Some(id) = pick {
            self.visible.insert(display.clone(), id);
            self.visible_on.insert(id, display.clone());
            self.workspaces[id.index()].last_display = Some(display.clone());
        }
    }

    // ---- displays ----

    /// React to the display set having changed (already applied to `displays`).
    ///
    /// A removed display's workspaces become hidden but keep their windows and are
    /// remembered by [`Home`]. An added display takes back the workspaces that lived on
    /// it, and every display that still has none gets one.
    pub fn displays_changed(&mut self, changes: &DisplayChanges, displays: &DisplayStore) {
        let focused_before = self.focused_workspace();
        for removed in &changes.removed {
            if let Some(id) = self.visible.remove(removed) {
                self.visible_on.remove(&id);
                let workspace = &mut self.workspaces[id.index()];
                workspace.home.get_or_insert(Home { display: removed.clone(), was_active: true });
            }
            for workspace in &mut self.workspaces {
                if workspace.last_display.as_ref() == Some(removed) && workspace.home.is_none() {
                    workspace.home = Some(Home { display: removed.clone(), was_active: false });
                }
            }
        }

        for added in &changes.added {
            let mut returning = Vec::new();
            for id in self.ids().collect::<Vec<_>>() {
                let workspace = &mut self.workspaces[id.index()];
                if let Some(home) = workspace.home.take_if(|h| &h.display == added) {
                    workspace.last_display = Some(added.clone());
                    returning.push((id, home.was_active));
                }
            }
            // One pass over the returning set. Holes opened while putting them back are
            // not plugged with a workspace that is itself still on its way home: it would
            // look unparked, be picked, and stay visible on the wrong display.
            let exclude: Vec<WorkspaceId> = returning.iter().map(|&(id, _)| id).collect();
            // The one that was showing takes the display back first...
            for &(id, _) in returning.iter().filter(|(_, was_active)| *was_active) {
                let result = self.set_active_excluding(added, id, displays, &exclude);
                debug_assert!(result.is_ok(), "added display is connected: {result:?}");
            }
            // ...and the others go home hidden, vacating wherever they were pulled to.
            for &(id, _) in returning.iter().filter(|(_, was_active)| !*was_active) {
                if self.visible_on.get(&id) == Some(added) {
                    continue;
                }
                if let Some(elsewhere) = self.visible_on.remove(&id) {
                    self.visible.remove(&elsewhere);
                    self.plug_hole(&elsewhere, displays, &exclude);
                }
            }
        }

        // A new display also claims a workspace pinned to it that is showing elsewhere,
        // unless it already has one back from its home.
        for added in &changes.added {
            if self.visible.contains_key(added) {
                continue;
            }
            let pinned = self.ids().find(|&id| {
                self.visible_on.contains_key(&id)
                    && self.forced_display(id, displays).as_ref() == Some(added)
            });
            if let Some(id) = pinned {
                let result = self.set_active_workspace(added, id, displays);
                debug_assert!(result.is_ok(), "added display is connected: {result:?}");
            }
        }

        for display in displays.iter() {
            self.plug_hole(&display.uuid, displays, &[]);
        }
        if self.focused_display.as_ref().is_none_or(|d| displays.get(d).is_none()) {
            self.focused_display = displays.main().map(|d| d.uuid.clone());
        }
        // Focus may have landed on another workspace; "back" must return to the old one.
        if let (Some(before), now) = (focused_before, self.focused_workspace())
            && Some(before) != now
        {
            self.previous = Some(before);
        }
    }

    // ---- commands ----

    pub fn id_for(&self, number: u32) -> Result<WorkspaceId, WorkspaceError> {
        if number >= 1 && number as usize <= self.workspaces.len() {
            Ok(WorkspaceId(number))
        } else {
            Err(WorkspaceError::NoSuchWorkspace(number.to_string()))
        }
    }

    /// The workspace called `name`.
    pub fn id_by_name(&self, name: &str) -> Result<WorkspaceId, WorkspaceError> {
        self.workspaces
            .iter()
            .position(|w| w.name == name)
            .map(|i| WorkspaceId(i as u32 + 1))
            .ok_or_else(|| WorkspaceError::NoSuchWorkspace(name.to_owned()))
    }

    /// Replace the force-assignments (config reload). Workspaces already visible stay
    /// where they are until the next command that resolves their display.
    pub fn set_force_assignments(
        &mut self,
        assignments: &[ForceAssignment],
    ) -> Result<(), WorkspaceError> {
        let mut force: Vec<Vec<Pattern>> = vec![Vec::new(); self.workspaces.len()];
        for rule in assignments {
            let slot = (rule.workspace as usize)
                .checked_sub(1)
                .and_then(|i| force.get_mut(i))
                .ok_or_else(|| WorkspaceError::NoSuchWorkspace(rule.workspace.to_string()))?;
            for pattern in &rule.monitors {
                slot.push(Pattern::compile(pattern)?);
            }
        }
        for (workspace, patterns) in self.workspaces.iter_mut().zip(force) {
            workspace.force = patterns;
        }
        Ok(())
    }

    /// Change what `workspace N` does on the focused workspace (config reload).
    pub fn set_auto_back_and_forth(&mut self, on: bool) { self.auto_back_and_forth = on; }

    /// The names of the pool, in order.
    pub fn names(&self) -> Vec<String> { self.workspaces.iter().map(|w| w.name.clone()).collect() }

    /// Focus `display` (the one a window was focused on, say).
    pub fn focus_display(&mut self, display: DisplayUuid) { self.set_focused_display(display); }

    /// `workspace N`: show workspace `number` on the display it belongs to and focus
    /// that display.
    pub fn switch_to_workspace(
        &mut self,
        number: u32,
        displays: &DisplayStore,
    ) -> Result<(), WorkspaceError> {
        let id = self.id_for(number)?;
        if self.auto_back_and_forth && self.focused_workspace() == Some(id) {
            return match self.previous_other_than_focused() {
                Some(previous) => self.focus_workspace(previous, displays),
                None => Ok(()),
            };
        }
        self.focus_workspace(id, displays)
    }

    /// `workspace-back-and-forth`.
    pub fn back_and_forth(&mut self, displays: &DisplayStore) -> Result<(), WorkspaceError> {
        let previous =
            self.previous_other_than_focused().ok_or(WorkspaceError::NoPreviousWorkspace)?;
        self.focus_workspace(previous, displays)
    }

    /// The previous workspace, unless it is the focused one (going "back" there would do
    /// nothing at all, which must not look like success).
    fn previous_other_than_focused(&self) -> Option<WorkspaceId> {
        self.previous.filter(|&p| Some(p) != self.focused_workspace())
    }

    /// `move-node-to-workspace N`: the window changes owner; with `follow`, focus
    /// goes with it.
    pub fn move_window_to_workspace(
        &mut self,
        window: WindowId,
        number: u32,
        follow: bool,
        displays: &DisplayStore,
    ) -> Result<(), WorkspaceError> {
        let id = self.id_for(number)?;
        if !self.membership.contains_key(&window) {
            return Err(WorkspaceError::UnknownWindow(window));
        }
        // Everything that can fail comes before the first change.
        if follow && self.resolve_display(id, displays).is_none() {
            return Err(WorkspaceError::NoDisplays);
        }
        self.membership.insert(window, id);
        if follow {
            self.focus_workspace(id, displays)?;
        }
        Ok(())
    }

    /// `move-workspace-to-monitor`: send the focused display's workspace to `target`,
    /// where it replaces that display's workspace. Focus follows. The vacated display
    /// is plugged by [`Self::set_active_workspace`].
    ///
    /// Force-assignment is not consulted: the user's explicit move wins until the next
    /// command that re-resolves the workspace.
    pub fn move_workspace_to_monitor(
        &mut self,
        target: &MonitorTarget,
        displays: &DisplayStore,
    ) -> Result<(), WorkspaceError> {
        let source = self.focused_display.clone().ok_or(WorkspaceError::NoDisplays)?;
        let id = self.visible_workspace(&source).ok_or(WorkspaceError::NoWorkspaceHere)?;
        let destination = resolve_target(target, &source, displays)?;
        if destination == source {
            return Ok(());
        }
        self.workspaces[id.index()].home = None;
        self.set_active_workspace(&destination, id, displays)?;
        self.focused_display = Some(destination);
        Ok(())
    }

    /// `focus-monitor`.
    pub fn focus_monitor(
        &mut self,
        target: &MonitorTarget,
        displays: &DisplayStore,
    ) -> Result<(), WorkspaceError> {
        let source = self.focused_display.clone().ok_or(WorkspaceError::NoDisplays)?;
        let destination = resolve_target(target, &source, displays)?;
        self.set_focused_display(destination);
        Ok(())
    }

    fn focus_workspace(
        &mut self,
        id: WorkspaceId,
        displays: &DisplayStore,
    ) -> Result<(), WorkspaceError> {
        let display = self.resolve_display(id, displays).ok_or(WorkspaceError::NoDisplays)?;
        let before = self.focused_workspace();
        self.set_active_workspace(&display, id, displays)?;
        self.focused_display = Some(display);
        if let Some(before) = before.filter(|&b| b != id) {
            self.previous = Some(before);
        }
        Ok(())
    }

    fn set_focused_display(&mut self, display: DisplayUuid) {
        let before = self.focused_workspace();
        self.focused_display = Some(display);
        if let (Some(before), Some(now)) = (before, self.focused_workspace())
            && before != now
        {
            self.previous = Some(before);
        }
    }

    // ---- windows ----

    /// Bring window ownership in line with the window store: forget windows that are
    /// gone, and give new ones the workspace visible on their display (or the focused
    /// one, or the first, while no display is known).
    pub fn sync_windows(&mut self, windows: &WindowStore) {
        if self.membership.len() != windows.len()
            || self.membership.keys().any(|&w| !windows.contains(w))
        {
            self.membership.retain(|&w, _| windows.contains(w));
        }
        if self.membership.len() == windows.len() {
            return;
        }
        for record in windows.iter() {
            if self.membership.contains_key(&record.wid) {
                continue;
            }
            let id = record
                .display
                .as_ref()
                .and_then(|d| self.visible_workspace(d))
                .or_else(|| self.focused_workspace())
                .unwrap_or(WorkspaceId(1));
            self.membership.insert(record.wid, id);
        }
    }

    /// The user dragged `window` onto `display`: it joins that display's workspace,
    /// but only if its own workspace is showing. A hidden workspace owns its windows
    /// wherever they are (they are parked, or macOS moved them).
    pub fn window_dragged_to(&mut self, window: WindowId, display: &DisplayUuid) {
        let Some(&current) = self.membership.get(&window) else { return };
        let Some(shown_on) = self.visible_on.get(&current) else { return };
        if shown_on == display {
            return;
        }
        if let Some(&target) = self.visible.get(display) {
            self.membership.insert(window, target);
        }
    }

    // ---- invariants ----

    /// Check every structural invariant; the error says which one broke.
    pub fn check_invariants(
        &self,
        displays: &DisplayStore,
        windows: &WindowStore,
    ) -> Result<(), String> {
        for (display, id) in &self.visible {
            if self.get(*id).is_none() {
                return Err(format!("{display} shows unknown workspace {id}"));
            }
            if displays.get(display).is_none() {
                return Err(format!("workspace {id} is visible on disconnected {display}"));
            }
            if self.visible_on.get(id) != Some(display) {
                return Err(format!("visible and visible_on disagree on {display} / {id}"));
            }
        }
        for (id, display) in &self.visible_on {
            if self.visible.get(display) != Some(id) {
                return Err(format!("visible_on and visible disagree on {id} / {display}"));
            }
        }
        if self.visible.len() != self.visible_on.len() {
            return Err("a workspace is visible on more than one display".into());
        }
        for display in displays.iter() {
            if !self.visible.contains_key(&display.uuid)
                && self.visible_on.len() < self.workspaces.len()
            {
                return Err(format!("{} has no workspace though one is free", display.uuid));
            }
        }
        for (i, workspace) in self.workspaces.iter().enumerate() {
            if let Some(home) = &workspace.home
                && displays.get(&home.display).is_some()
            {
                return Err(format!("workspace {} is parked on connected {}", i + 1, home.display));
            }
        }
        match &self.focused_display {
            Some(d) if displays.get(d).is_none() => {
                return Err(format!("focused display {d} is disconnected"));
            }
            None if !displays.is_empty() => return Err("displays exist but none is focused".into()),
            _ => {}
        }
        if self.previous.is_some_and(|p| self.get(p).is_none()) {
            return Err("previous workspace does not exist".into());
        }
        for (window, id) in &self.membership {
            if self.get(*id).is_none() {
                return Err(format!("{window:?} belongs to unknown workspace {id}"));
            }
            if !windows.contains(*window) {
                return Err(format!("{window:?} is owned but unknown to the window store"));
            }
        }
        if let Some(record) = windows.iter().find(|r| !self.membership.contains_key(&r.wid)) {
            return Err(format!("{:?} is in no workspace", record.wid));
        }
        Ok(())
    }

    fn get(&self, id: WorkspaceId) -> Option<&Workspace> {
        (id.0 as usize).checked_sub(1).and_then(|i| self.workspaces.get(i))
    }
}

/// Displays ordered left to right, ties top to bottom.
fn left_to_right(displays: &DisplayStore) -> Vec<&DisplaySnapshot> {
    let mut sorted: Vec<_> = displays.iter().collect();
    sorted.sort_by(|a, b| {
        a.frame
            .origin
            .x
            .total_cmp(&b.frame.origin.x)
            .then(a.frame.origin.y.total_cmp(&b.frame.origin.y))
    });
    sorted
}

fn center(display: &DisplaySnapshot) -> (f64, f64) {
    let f = display.frame;
    (f.origin.x + f.size.width / 2.0, f.origin.y + f.size.height / 2.0)
}

/// The display `target` names, seen from `from`.
fn resolve_target(
    target: &MonitorTarget,
    from: &DisplayUuid,
    displays: &DisplayStore,
) -> Result<DisplayUuid, WorkspaceError> {
    let found = match target {
        MonitorTarget::Pattern(pattern) => Pattern::compile(pattern)?.find(displays),
        MonitorTarget::Next { wrap } => cycle(from, displays, 1, *wrap),
        MonitorTarget::Prev { wrap } => cycle(from, displays, -1, *wrap),
        MonitorTarget::Direction { direction, wrap } => {
            neighbor(from, displays, *direction, *wrap)
        }
    };
    found.ok_or(WorkspaceError::NoSuchDisplay)
}

fn cycle(from: &DisplayUuid, displays: &DisplayStore, step: i64, wrap: bool) -> Option<DisplayUuid> {
    let sorted = left_to_right(displays);
    let at = sorted.iter().position(|d| &d.uuid == from)? as i64;
    let len = sorted.len() as i64;
    let next = at + step;
    let next = if (0..len).contains(&next) {
        next
    } else if wrap {
        next.rem_euclid(len)
    } else {
        return None;
    };
    sorted.get(next as usize).map(|d| d.uuid.clone())
}

/// The nearest display in `direction` by center distance; with `wrap`, when there is
/// none, the farthest one on the opposite side.
fn neighbor(
    from: &DisplayUuid,
    displays: &DisplayStore,
    direction: Direction,
    wrap: bool,
) -> Option<DisplayUuid> {
    let origin = center(displays.get(from)?);
    let (horizontal, sign) = match direction {
        Direction::Left => (true, -1.0),
        Direction::Right => (true, 1.0),
        Direction::Up => (false, -1.0),
        Direction::Down => (false, 1.0),
    };
    let axis = |d: &DisplaySnapshot| {
        let c = center(d);
        if horizontal { c.0 } else { c.1 }
    };
    let axis_origin = if horizontal { origin.0 } else { origin.1 };
    let others = || displays.iter().filter(|d| &d.uuid != from);
    others()
        .filter(|d| (axis(d) - axis_origin) * sign > 0.0)
        .min_by(|a, b| {
            let dist = |d: &DisplaySnapshot| {
                let c = center(d);
                (c.0 - origin.0).powi(2) + (c.1 - origin.1).powi(2)
            };
            dist(a).total_cmp(&dist(b))
        })
        .or_else(|| {
            wrap.then(|| others().max_by(|a, b| (axis(a) * -sign).total_cmp(&(axis(b) * -sign))))
                .flatten()
        })
        .map(|d| d.uuid.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::display::tests::{display, rect};

    fn setup(count: u32) -> (WorkspaceStore, DisplayStore, WindowStore) {
        let mut displays = DisplayStore::default();
        let changes = displays.apply(vec![
            display("A", 1, rect(0., 0., 1000., 1000.)),
            display("B", 2, rect(1000., 0., 1000., 1000.)),
        ]);
        let mut store = WorkspaceStore::new(&WorkspaceConfig::numbered(count)).unwrap();
        store.displays_changed(&changes, &displays);
        (store, displays, WindowStore::default())
    }

    fn id(n: u32) -> DisplayUuid { DisplayUuid::new(["", "A", "B"][n as usize]) }

    #[test]
    fn a_fresh_store_satisfies_the_invariants() {
        let (store, displays, windows) = setup(4);
        assert_eq!(store.check_invariants(&displays, &windows), Ok(()));
        assert_eq!(store.visible_workspace(&id(1)), Some(WorkspaceId::new(1)));
        assert_eq!(store.visible_workspace(&id(2)), Some(WorkspaceId::new(2)));
    }

    #[test]
    fn set_active_workspace_evicts_both_sides() {
        let (mut store, displays, windows) = setup(4);
        let (one, two) = (WorkspaceId::new(1), WorkspaceId::new(2));

        // 1 moves from A to B: B's occupant 2 is evicted, A is plugged.
        store.set_active_workspace(&id(2), one, &displays).unwrap();

        assert_eq!(store.visible_workspace(&id(2)), Some(one));
        assert_eq!(store.display_of(one), Some(&id(2)));
        assert_eq!(store.display_of(two), None);
        assert!(store.visible_workspace(&id(1)).is_some_and(|ws| ws != one && ws != two));
        assert_eq!(store.check_invariants(&displays, &windows), Ok(()));
    }

    #[test]
    fn set_active_workspace_rejects_unknown_display_and_workspace() {
        let (mut store, displays, _) = setup(2);
        assert_eq!(
            store.set_active_workspace(&DisplayUuid::new("Z"), WorkspaceId::new(1), &displays),
            Err(WorkspaceError::NoSuchDisplay)
        );
        assert_eq!(
            store.set_active_workspace(&id(1), WorkspaceId::new(3), &displays),
            Err(WorkspaceError::NoSuchWorkspace("3".into()))
        );
    }

    #[test]
    fn the_checker_catches_one_sided_edits() {
        let (mut store, displays, windows) = setup(4);
        // A workspace claimed by two displays.
        store.visible.insert(id(2), WorkspaceId::new(1));
        assert!(store.check_invariants(&displays, &windows).is_err());

        let (mut store, displays, windows) = setup(4);
        store.visible_on.remove(&WorkspaceId::new(1));
        assert!(store.check_invariants(&displays, &windows).is_err());

        let (mut store, displays, windows) = setup(4);
        store.visible_on.insert(WorkspaceId::new(3), id(1));
        assert!(store.check_invariants(&displays, &windows).is_err());
    }

    #[test]
    fn the_checker_catches_a_display_left_without_a_workspace() {
        let (mut store, displays, windows) = setup(4);
        let gone = store.visible.remove(&id(2)).unwrap();
        store.visible_on.remove(&gone);
        assert!(store.check_invariants(&displays, &windows).unwrap_err().contains("no workspace"));
    }

    #[test]
    fn the_checker_catches_stale_state() {
        // Visible on a disconnected display.
        let (mut store, displays, windows) = setup(4);
        store.visible.insert(DisplayUuid::new("Z"), WorkspaceId::new(3));
        store.visible_on.insert(WorkspaceId::new(3), DisplayUuid::new("Z"));
        assert!(store.check_invariants(&displays, &windows).is_err());

        // A home on a display that is connected.
        let (mut store, displays, windows) = setup(4);
        store.workspaces[2].home = Some(Home { display: id(1), was_active: false });
        assert!(store.check_invariants(&displays, &windows).is_err());

        // Focus on a display that is gone.
        let (mut store, displays, windows) = setup(4);
        store.focused_display = Some(DisplayUuid::new("Z"));
        assert!(store.check_invariants(&displays, &windows).is_err());

        // A window owned by a workspace that does not exist.
        let (mut store, displays, windows) = setup(4);
        store.membership.insert(WindowId::new(1, 1), WorkspaceId::new(40));
        assert!(store.check_invariants(&displays, &windows).is_err());
    }

    #[test]
    fn sequence_and_secondary_patterns_order_displays_left_to_right() {
        let mut displays = DisplayStore::default();
        displays.apply(vec![
            display("MAIN", 1, rect(1000., 0., 1000., 1000.)),
            display("OTHER", 2, rect(0., 0., 1000., 1000.)),
        ]);
        let find = |p: MonitorPattern| Pattern::compile(&p).unwrap().find(&displays);
        assert_eq!(find(MonitorPattern::Sequence(1)), Some(DisplayUuid::new("OTHER")));
        assert_eq!(find(MonitorPattern::Sequence(2)), Some(DisplayUuid::new("MAIN")));
        assert_eq!(find(MonitorPattern::Sequence(3)), None);
        assert_eq!(find(MonitorPattern::Main), Some(DisplayUuid::new("MAIN")));
        assert_eq!(find(MonitorPattern::Secondary), Some(DisplayUuid::new("OTHER")));
        // Unnamed displays never match a name pattern.
        assert_eq!(find(MonitorPattern::Name(".*".into())), None);
    }

    #[test]
    fn back_and_forth_errors_when_the_previous_workspace_is_the_focused_one() {
        let (mut store, displays, _) = setup(4);
        store.previous = store.focused_workspace();
        assert_eq!(store.back_and_forth(&displays), Err(WorkspaceError::NoPreviousWorkspace));

        // `workspace N` on the focused workspace with auto back-and-forth has nothing to go
        // back to either, and stays put.
        store.auto_back_and_forth = true;
        let focused = store.focused_workspace().unwrap();
        assert_eq!(store.switch_to_workspace(focused.number(), &displays), Ok(()));
        assert_eq!(store.focused_workspace(), Some(focused));
    }

    #[test]
    fn move_window_with_follow_leaves_membership_alone_when_it_cannot_follow() {
        let (mut store, _, _) = setup(4);
        let window = WindowId::new(1, 1);
        store.membership.insert(window, WorkspaceId::new(1));

        let result = store.move_window_to_workspace(window, 3, true, &DisplayStore::default());

        assert_eq!(result, Err(WorkspaceError::NoDisplays));
        assert_eq!(store.workspace_of(window), Some(WorkspaceId::new(1)));
    }
    #[test]
    fn workspaces_are_found_by_name_and_unknown_names_are_errors() {
        let mut names = WorkspaceConfig::numbered(2);
        names.names.push("T".into());
        let store = WorkspaceStore::new(&names).unwrap();
        assert_eq!(store.id_by_name("1"), Ok(WorkspaceId::new(1)));
        assert_eq!(store.id_by_name("T"), Ok(WorkspaceId::new(3)));
        assert_eq!(store.names(), ["1", "2", "T"]);
        assert_eq!(store.id_by_name("t"), Err(WorkspaceError::NoSuchWorkspace("t".into())));
        assert_eq!(store.id_by_name(""), Err(WorkspaceError::NoSuchWorkspace(String::new())));
    }

    #[test]
    fn pattern_matches_selects_one_display_and_ignores_bad_regexes() {
        let (_, displays, _) = setup(2);
        assert!(pattern_matches(&MonitorPattern::Main, &id(1), &displays));
        assert!(!pattern_matches(&MonitorPattern::Main, &id(2), &displays));
        assert!(pattern_matches(&MonitorPattern::Secondary, &id(2), &displays));
        assert!(pattern_matches(&MonitorPattern::Sequence(2), &id(2), &displays));
        assert!(!pattern_matches(&MonitorPattern::Name("(".into()), &id(1), &displays));
    }

    #[test]
    fn force_assignments_can_be_replaced_live() {
        let (mut store, displays, _) = setup(4);
        let three = WorkspaceId::new(3);
        assert_eq!(store.forced_display(three, &displays), None);
        let rule = ForceAssignment { workspace: 3, monitors: vec![MonitorPattern::Secondary] };
        store.set_force_assignments(&[rule]).unwrap();
        assert_eq!(store.forced_display(three, &displays), Some(id(2)));
        store.set_force_assignments(&[]).unwrap();
        assert_eq!(store.forced_display(three, &displays), None, "an empty list clears them");
        let bad = ForceAssignment { workspace: 9, monitors: vec![] };
        assert_eq!(store.set_force_assignments(&[bad]), Err(WorkspaceError::NoSuchWorkspace("9".into())));
    }
}

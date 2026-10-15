use serde::{Deserialize, Serialize};

/// What a client asks of the daemon. Unknown fields are rejected so a typo is an error,
/// not a silently ignored option.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    /// Run a command line (`workspace 3`, `layout tiles horizontal`, ...). The daemon
    /// parses it with the same parser as the config file.
    Command { command: String },
    /// Read state.
    Query { query: Query },
    /// Turn this connection into an event stream.
    Subscribe { topics: Vec<Topic> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Query {
    ListWorkspaces,
    ListWindows,
    ListMonitors,
    /// The active binding mode.
    Mode,
}

/// An event category a subscriber can ask for. An empty list means all of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Topic {
    Workspace,
    Focus,
    Mode,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Response {
    Ok,
    Error { message: String },
    /// The connection is now an event stream; [`Response::Event`] frames follow.
    Subscribed,
    Workspaces { workspaces: Vec<WorkspaceInfo> },
    Windows { windows: Vec<WindowInfo> },
    Monitors { monitors: Vec<MonitorInfo> },
    Mode { mode: String },
    Event { event: Event },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    WorkspaceChanged { workspace: String, previous: Option<String> },
    FocusChanged { window: Option<u64> },
    ModeChanged { mode: String },
    /// Any display's workspace, the focused display, or the set of displays changed.
    MonitorsChanged { monitors: Vec<MonitorInfo> },
}

impl Event {
    pub fn topic(&self) -> Topic {
        match self {
            Event::WorkspaceChanged { .. } | Event::MonitorsChanged { .. } => Topic::Workspace,
            Event::FocusChanged { .. } => Topic::Focus,
            Event::ModeChanged { .. } => Topic::Mode,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub focused: bool,
    /// Shown on some monitor right now.
    pub visible: bool,
    /// The monitor (by UUID) showing it, if visible.
    pub monitor: Option<String>,
    pub windows: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u64,
    pub pid: i32,
    pub app: Option<String>,
    pub title: String,
    pub workspace: Option<String>,
    pub focused: bool,
    /// In a native macOS Space other than the one showing: dowm cannot see or move it
    /// and leaves it alone until it returns.
    #[serde(default)]
    pub foreign: bool,
    pub frame: Rect,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// The display UUID.
    pub id: String,
    pub name: Option<String>,
    pub focused: bool,
    /// The workspace it shows.
    pub workspace: Option<String>,
    pub frame: Rect,
}

//! Client for the dowm daemon's Unix-socket IPC.
//!
//! [`Client::connect`] opens the socket; [`Client::command`] and the `list_*` methods
//! are one request, one response; [`Client::subscribe`] turns the connection into an
//! iterator of [`Event`]s.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dowm_protocol::{
    Event, MAX_REQUEST_FRAME, MAX_RESPONSE_FRAME, MonitorInfo, Query, Request, Response, Topic,
    WindowInfo, WorkspaceInfo, read_message, write_message,
};
pub use dowm_protocol::FrameError;

/// How long a request waits for the daemon before giving up.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("dowm is not running (cannot connect to {})", .0.display())]
    NotRunning(PathBuf),
    #[error("{0}")]
    Daemon(String),
    #[error("the daemon closed the connection without answering")]
    Closed,
    #[error("unexpected response from the daemon: {0}")]
    Unexpected(String),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// `DOWM_SOCKET` wins; otherwise `<TMPDIR>/dowm-<uid>/dowm.sock`. macOS's `TMPDIR` is
/// already private to the user; the daemon also makes the `dowm-<uid>` directory 0700.
pub fn socket_path_from(override_path: Option<PathBuf>, tmpdir: Option<PathBuf>, uid: u32) -> PathBuf {
    if let Some(path) = override_path {
        return path;
    }
    tmpdir
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(format!("dowm-{uid}"))
        .join("dowm.sock")
}

pub fn default_socket_path() -> PathBuf {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    socket_path_from(
        std::env::var_os("DOWM_SOCKET").map(PathBuf::from),
        std::env::var_os("TMPDIR").map(PathBuf::from),
        uid,
    )
}

pub struct Client {
    stream: UnixStream,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                ClientError::NotRunning(path.to_owned())
            }
            _ => ClientError::Io(e),
        })?;
        // macOS refuses setsockopt (EINVAL) on a socket whose peer already closed, which is
        // what a server that turns us away ("too many connections") does. Carry on: the
        // first read then delivers the server's reason instead of a bare EINVAL.
        for result in [
            stream.set_read_timeout(Some(REQUEST_TIMEOUT)),
            stream.set_write_timeout(Some(REQUEST_TIMEOUT)),
        ] {
            match result {
                Err(e) if e.kind() != io::ErrorKind::InvalidInput => return Err(e.into()),
                _ => {}
            }
        }
        Ok(Self { stream })
    }

    pub fn connect_default() -> Result<Self, ClientError> { Self::connect(&default_socket_path()) }

    fn send(&mut self, request: &Request) -> Result<(), ClientError> {
        Ok(write_message(&mut self.stream, request, MAX_REQUEST_FRAME)?)
    }

    fn receive(&mut self) -> Result<Response, ClientError> {
        match read_message::<Response>(&mut self.stream, MAX_RESPONSE_FRAME)? {
            Some(Response::Error { message }) => Err(ClientError::Daemon(message)),
            Some(response) => Ok(response),
            None => Err(ClientError::Closed),
        }
    }

    fn call(&mut self, request: Request) -> Result<Response, ClientError> {
        self.send(&request)?;
        self.receive()
    }

    /// Run a command line such as `workspace 3`. Returns once the daemon has accepted it.
    pub fn command(&mut self, command: &str) -> Result<Response, ClientError> {
        self.call(Request::Command { command: command.to_owned() })
    }

    pub fn list_workspaces(&mut self) -> Result<Vec<WorkspaceInfo>, ClientError> {
        match self.call(Request::Query { query: Query::ListWorkspaces })? {
            Response::Workspaces { workspaces } => Ok(workspaces),
            other => Err(ClientError::Unexpected(format!("{other:?}"))),
        }
    }

    pub fn list_windows(&mut self) -> Result<Vec<WindowInfo>, ClientError> {
        match self.call(Request::Query { query: Query::ListWindows })? {
            Response::Windows { windows } => Ok(windows),
            other => Err(ClientError::Unexpected(format!("{other:?}"))),
        }
    }

    pub fn list_monitors(&mut self) -> Result<Vec<MonitorInfo>, ClientError> {
        match self.call(Request::Query { query: Query::ListMonitors })? {
            Response::Monitors { monitors } => Ok(monitors),
            other => Err(ClientError::Unexpected(format!("{other:?}"))),
        }
    }

    pub fn mode(&mut self) -> Result<String, ClientError> {
        match self.call(Request::Query { query: Query::Mode })? {
            Response::Mode { mode } => Ok(mode),
            other => Err(ClientError::Unexpected(format!("{other:?}"))),
        }
    }

    /// Subscribe to `topics` (empty means all). The iterator ends when the daemon closes
    /// the connection.
    pub fn subscribe(mut self, topics: &[Topic]) -> Result<EventStream, ClientError> {
        match self.call(Request::Subscribe { topics: topics.to_vec() })? {
            Response::Subscribed => {}
            other => return Err(ClientError::Unexpected(format!("{other:?}"))),
        }
        // Events arrive whenever they happen; waiting is the point.
        // macOS refuses setsockopt (EINVAL) once the peer has closed; the next read then
        // reports the close, so that one error is not worth failing for.
        match self.stream.set_read_timeout(None) {
            Err(e) if e.kind() != io::ErrorKind::InvalidInput => return Err(e.into()),
            _ => {}
        }
        Ok(EventStream { client: self, done: false })
    }
}

pub struct EventStream {
    client: Client,
    done: bool,
}

impl Iterator for EventStream {
    type Item = Result<Event, ClientError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.client.receive() {
            Ok(Response::Event { event }) => Some(Ok(event)),
            Ok(other) => {
                self.done = true;
                Some(Err(ClientError::Unexpected(format!("{other:?}"))))
            }
            Err(ClientError::Closed) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

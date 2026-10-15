//! dowm's IPC wire protocol: typed requests, responses and events, plus the
//! length-framed JSON codec that carries them over a Unix domain socket.
//!
//! Nothing here touches an OS API, so the daemon, the CLI and any third-party client
//! share one definition of the wire format.
//!
//! # Framing
//!
//! Each message is a 4-byte big-endian length followed by that many bytes of UTF-8 JSON.
//! A reader states the largest frame it will accept; a longer length is rejected from the
//! header alone, before any body is read or allocated.

mod frame;
mod types;

pub use frame::{FrameError, read_frame, read_message, write_frame, write_message};
pub use types::{
    Event, MonitorInfo, Query, Rect, Request, Response, Topic, WindowInfo, WorkspaceInfo,
};

/// Largest request frame the daemon accepts. Requests are one command line or a short
/// query, so this is generous.
pub const MAX_REQUEST_FRAME: usize = 64 * 1024;

/// Largest response frame a client accepts. Window listings are the biggest payload.
pub const MAX_RESPONSE_FRAME: usize = 4 * 1024 * 1024;

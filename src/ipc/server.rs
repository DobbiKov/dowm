//! The IPC server: accepts connections on the Unix socket and answers framed requests.
//!
//! One thread per connection, blocking I/O, with a cap on concurrent connections and
//! read/write timeouts so a stalled client cannot hold resources. The server knows
//! nothing about the reactor; it talks to a [`Backend`].
//!
//! Hostile or broken input never reaches the backend: an oversized or malformed frame
//! gets an error response (when the frame boundary is still known) and the connection
//! is closed, because after a bad frame the stream cannot be trusted to resync.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crossbeam_channel::{RecvTimeoutError, TrySendError, bounded};
use dowm_protocol::{
    Event, FrameError, MAX_REQUEST_FRAME, MAX_RESPONSE_FRAME, Query, Request, Response, Topic,
    read_message, write_message,
};
use tracing::{debug, warn};

use super::bus::EventBus;
use super::socket::peer_is_current_user;

/// What the server needs from the daemon.
pub trait Backend: Send + Sync + 'static {
    /// Parse and run a command line. `Err` carries a message for the user.
    fn command(&self, command: &str) -> Result<(), String>;
    fn query(&self, query: Query) -> Result<Response, String>;
}

pub const MAX_CONNECTIONS: usize = 32;
/// An idle request connection is closed after this long without a request.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Undelivered events a subscriber may fall behind by before it is dropped.
const SUBSCRIBER_BACKLOG: usize = 256;
const LIVENESS_CHECK: Duration = Duration::from_secs(10);

/// Start accepting on `listener` in a background thread.
pub fn spawn(listener: UnixListener, backend: Arc<dyn Backend>, bus: Arc<EventBus>) -> io::Result<()> {
    std::thread::Builder::new().name("ipc-accept".into()).spawn(move || {
        let active = Arc::new(AtomicUsize::new(0));
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(stream) => stream,
                Err(e) => {
                    // Persistent errors (out of file descriptors) must not spin a core.
                    warn!(%e, "ipc accept failed");
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            if !peer_is_current_user(&stream) {
                warn!("ipc: rejected a connection from another user");
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                active.fetch_sub(1, Ordering::AcqRel);
                reject_busy(stream);
                continue;
            }
            let (backend, bus, active_for_thread) =
                (Arc::clone(&backend), Arc::clone(&bus), Arc::clone(&active));
            let spawned = std::thread::Builder::new().name("ipc-conn".into()).spawn(move || {
                // Released on every exit, a panic in the backend included.
                let _slot = Slot(active_for_thread);
                serve(stream, &*backend, &bus);
            });
            if let Err(e) = spawned {
                warn!(%e, "ipc: cannot start a connection thread");
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    })?;
    Ok(())
}

struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) { self.0.fetch_sub(1, Ordering::AcqRel); }
}

fn reject_busy(mut stream: UnixStream) {
    _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    _ = write_message(&mut stream, &Response::Error { message: "too many connections".into() }, MAX_RESPONSE_FRAME);
}

fn error(message: impl Into<String>) -> Response { Response::Error { message: message.into() } }

fn serve(mut stream: UnixStream, backend: &dyn Backend, bus: &Arc<EventBus>) {
    if stream.set_read_timeout(Some(IDLE_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
    {
        return;
    }
    loop {
        let request = match read_message::<Request>(&mut stream, MAX_REQUEST_FRAME) {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(e @ (FrameError::TooLarge { .. } | FrameError::Malformed(_) | FrameError::Empty)) => {
                debug!(%e, "ipc: bad frame");
                _ = write_message(&mut stream, &error(e.to_string()), MAX_RESPONSE_FRAME);
                return;
            }
            Err(e) => {
                debug!(%e, "ipc: connection ended");
                return;
            }
        };
        let response = match request {
            Request::Subscribe { topics } => return stream_events(stream, &topics, bus),
            Request::Command { command } => match backend.command(&command) {
                Ok(()) => Response::Ok,
                Err(message) => error(message),
            },
            Request::Query { query } => backend.query(query).unwrap_or_else(error),
        };
        match write_message(&mut stream, &response, MAX_RESPONSE_FRAME) {
            Ok(()) => {}
            Err(e @ FrameError::TooLarge { .. }) => {
                // The answer does not fit; say so rather than going silent.
                if write_message(&mut stream, &error(e.to_string()), MAX_RESPONSE_FRAME).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

fn stream_events(mut stream: UnixStream, topics: &[Topic], bus: &Arc<EventBus>) {
    let (tx, rx) = bounded::<Event>(SUBSCRIBER_BACKLOG);
    // Register before acknowledging, so no event between the two is missed.
    bus.subscribe(topics, move |event| match tx.try_send(event) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => false, // too slow: drop it rather than block the reactor
        Err(TrySendError::Disconnected(_)) => false,
    });
    if write_message(&mut stream, &Response::Subscribed, MAX_RESPONSE_FRAME).is_err() {
        return;
    }
    loop {
        match rx.recv_timeout(LIVENESS_CHECK) {
            Ok(event) => {
                if write_message(&mut stream, &Response::Event { event }, MAX_RESPONSE_FRAME).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if peer_hung_up(&stream) {
                    return;
                }
            }
            // The bus dropped this subscriber (too slow); end the stream.
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// A subscriber sends nothing after subscribing, so any readable byte or EOF means the
/// peer is done or misbehaving.
fn peer_hung_up(stream: &UnixStream) -> bool {
    use std::io::Read;
    if stream.set_nonblocking(true).is_err() {
        return true;
    }
    let mut byte = [0u8; 1];
    let gone = match (&*stream).read(&mut byte) {
        Ok(_) => true,
        Err(e) => e.kind() != io::ErrorKind::WouldBlock,
    };
    _ = stream.set_nonblocking(false);
    gone
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::sync::Mutex;

    use dowm_protocol::{WorkspaceInfo, read_frame, write_frame};

    use super::*;
    use crate::ipc::socket;

    #[derive(Default)]
    struct Fake {
        commands: Mutex<Vec<String>>,
    }

    impl Backend for Fake {
        fn command(&self, command: &str) -> Result<(), String> {
            if command == "bogus" {
                return Err("unknown command `bogus`".into());
            }
            self.commands.lock().unwrap().push(command.to_owned());
            Ok(())
        }

        fn query(&self, query: Query) -> Result<Response, String> {
            match query {
                Query::ListWorkspaces => Ok(Response::Workspaces {
                    workspaces: vec![WorkspaceInfo {
                        name: "1".into(),
                        focused: true,
                        visible: true,
                        monitor: None,
                        windows: 2,
                    }],
                }),
                Query::Mode => Ok(Response::Mode { mode: "main".into() }),
                _ => Err("unsupported".into()),
            }
        }
    }

    struct Harness {
        path: PathBuf,
        backend: Arc<Fake>,
        bus: Arc<EventBus>,
    }

    fn start(name: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!("dowm-srv-{}-{name}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("s.sock");
        let listener = socket::bind(&path).unwrap();
        let backend = Arc::new(Fake::default());
        let bus = Arc::new(EventBus::new());
        spawn(listener, backend.clone(), bus.clone()).unwrap();
        Harness { path, backend, bus }
    }

    fn connect(h: &Harness) -> UnixStream {
        let stream = UnixStream::connect(&h.path).unwrap();
        // EINVAL here means the server already closed (it turned us away); the read
        // below then reports why.
        _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        stream
    }

    fn ask(stream: &mut UnixStream, request: &Request) -> Response {
        write_message(stream, request, MAX_REQUEST_FRAME).unwrap();
        read_message::<Response>(stream, MAX_RESPONSE_FRAME).unwrap().unwrap()
    }

    #[test]
    fn commands_reach_the_backend_and_errors_come_back() {
        let h = start("cmd");
        let mut s = connect(&h);
        assert_eq!(ask(&mut s, &Request::Command { command: "workspace 3".into() }), Response::Ok);
        assert_eq!(*h.backend.commands.lock().unwrap(), vec!["workspace 3".to_owned()]);
        assert_eq!(
            ask(&mut s, &Request::Command { command: "bogus".into() }),
            Response::Error { message: "unknown command `bogus`".into() }
        );
        // The connection survives a command error.
        assert_eq!(ask(&mut s, &Request::Query { query: Query::Mode }), Response::Mode { mode: "main".into() });
    }

    #[test]
    fn queries_are_answered() {
        let h = start("query");
        let mut s = connect(&h);
        match ask(&mut s, &Request::Query { query: Query::ListWorkspaces }) {
            Response::Workspaces { workspaces } => assert_eq!(workspaces[0].windows, 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_oversized_frame_is_rejected_and_the_connection_closed() {
        let h = start("big");
        let mut s = connect(&h);
        s.write_all(&((MAX_REQUEST_FRAME as u32) + 1).to_be_bytes()).unwrap();
        match read_message::<Response>(&mut s, MAX_RESPONSE_FRAME).unwrap().unwrap() {
            Response::Error { message } => assert!(message.contains("exceeds"), "{message}"),
            other => panic!("{other:?}"),
        }
        assert!(read_frame(&mut s, MAX_RESPONSE_FRAME).unwrap().is_none(), "closed");
        assert!(h.backend.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn a_huge_claimed_length_never_makes_the_server_allocate_or_wait() {
        let h = start("huge");
        let mut s = connect(&h);
        s.write_all(&u32::MAX.to_be_bytes()).unwrap(); // and no body at all
        assert!(matches!(
            read_message::<Response>(&mut s, MAX_RESPONSE_FRAME).unwrap().unwrap(),
            Response::Error { .. }
        ));
    }

    #[test]
    fn malformed_json_is_rejected_and_never_reaches_the_backend() {
        let h = start("junk");
        for body in [&b"not json"[..], b"{\"type\":\"command\",\"command\":\"x\",\"extra\":1}", b"\xff\xff"] {
            let mut s = connect(&h);
            write_frame(&mut s, body, MAX_REQUEST_FRAME).unwrap();
            assert!(matches!(
                read_message::<Response>(&mut s, MAX_RESPONSE_FRAME).unwrap().unwrap(),
                Response::Error { .. }
            ));
            assert!(read_frame(&mut s, MAX_RESPONSE_FRAME).unwrap().is_none(), "closed");
        }
        assert!(h.backend.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn a_truncated_frame_just_ends_the_connection() {
        let h = start("trunc");
        let mut s = connect(&h);
        s.write_all(&10u32.to_be_bytes()).unwrap();
        s.write_all(b"abc").unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut rest = Vec::new();
        s.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        // The server is still healthy.
        let mut again = connect(&h);
        assert_eq!(ask(&mut again, &Request::Query { query: Query::Mode }), Response::Mode { mode: "main".into() });
    }

    #[test]
    fn subscribers_receive_published_events_filtered_by_topic() {
        let h = start("sub");
        let mut s = connect(&h);
        assert_eq!(ask(&mut s, &Request::Subscribe { topics: vec![Topic::Workspace] }), Response::Subscribed);
        h.bus.publish(Event::ModeChanged { mode: "resize".into() });
        h.bus.publish(Event::WorkspaceChanged { workspace: "4".into(), previous: Some("1".into()) });
        let got = read_message::<Response>(&mut s, MAX_RESPONSE_FRAME).unwrap().unwrap();
        assert_eq!(
            got,
            Response::Event { event: Event::WorkspaceChanged { workspace: "4".into(), previous: Some("1".into()) } }
        );
    }

    #[test]
    fn a_subscriber_that_never_reads_is_dropped_without_blocking_publishers() {
        let h = start("slow");
        let mut s = connect(&h);
        ask(&mut s, &Request::Subscribe { topics: vec![] });
        let started = std::time::Instant::now();
        for i in 0..(SUBSCRIBER_BACKLOG * 50) {
            h.bus.publish(Event::WorkspaceChanged { workspace: i.to_string(), previous: None });
        }
        assert!(started.elapsed() < Duration::from_secs(5), "publishing must not block");
    }

    #[test]
    fn connections_beyond_the_cap_are_refused() {
        let h = start("cap");
        let held: Vec<UnixStream> = (0..MAX_CONNECTIONS)
            .map(|_| {
                let mut s = connect(&h);
                assert_eq!(ask(&mut s, &Request::Query { query: Query::Mode }), Response::Mode { mode: "main".into() });
                s
            })
            .collect();
        let mut extra = connect(&h);
        match read_message::<Response>(&mut extra, MAX_RESPONSE_FRAME).unwrap().unwrap() {
            Response::Error { message } => assert!(message.contains("too many")),
            other => panic!("{other:?}"),
        }
        drop(held);
    }
}

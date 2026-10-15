use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;

use dowm_client::{Client, ClientError, socket_path_from};
use dowm_protocol::*;

fn temp_socket(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dowm-client-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("s.sock")
}

#[test]
fn a_request_gets_the_servers_response() {
    let path = temp_socket("req");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request: Request = read_message(&mut stream, MAX_REQUEST_FRAME).unwrap().unwrap();
        assert_eq!(request, Request::Command { command: "workspace 3".into() });
        write_message(&mut stream, &Response::Ok, MAX_RESPONSE_FRAME).unwrap();
    });
    let mut client = Client::connect(&path).unwrap();
    assert_eq!(client.command("workspace 3").unwrap(), Response::Ok);
    server.join().unwrap();
}

#[test]
fn an_error_response_becomes_a_client_error() {
    let path = temp_socket("err");
    let listener = UnixListener::bind(&path).unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _: Option<Request> = read_message(&mut stream, MAX_REQUEST_FRAME).unwrap();
        write_message(&mut stream, &Response::Error { message: "unknown command".into() }, MAX_RESPONSE_FRAME)
            .unwrap();
    });
    let mut client = Client::connect(&path).unwrap();
    match client.command("bogus") {
        Err(ClientError::Daemon(message)) => assert_eq!(message, "unknown command"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn queries_return_typed_results() {
    let path = temp_socket("query");
    let listener = UnixListener::bind(&path).unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _: Option<Request> = read_message(&mut stream, MAX_REQUEST_FRAME).unwrap();
        let workspaces = vec![WorkspaceInfo {
            name: "1".into(),
            focused: true,
            visible: true,
            monitor: None,
            windows: 0,
        }];
        write_message(&mut stream, &Response::Workspaces { workspaces }, MAX_RESPONSE_FRAME).unwrap();
    });
    let mut client = Client::connect(&path).unwrap();
    assert_eq!(client.list_workspaces().unwrap()[0].name, "1");
}

#[test]
fn connecting_to_nothing_says_the_daemon_is_not_running() {
    let path = temp_socket("none");
    assert!(matches!(Client::connect(&path), Err(ClientError::NotRunning(_))));
}

#[test]
fn subscribe_yields_events_until_the_daemon_closes() {
    let path = temp_socket("sub");
    let listener = UnixListener::bind(&path).unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request: Request = read_message(&mut stream, MAX_REQUEST_FRAME).unwrap().unwrap();
        assert_eq!(request, Request::Subscribe { topics: vec![Topic::Workspace] });
        write_message(&mut stream, &Response::Subscribed, MAX_RESPONSE_FRAME).unwrap();
        for ws in ["2", "3"] {
            let event = Event::WorkspaceChanged { workspace: ws.into(), previous: None };
            write_message(&mut stream, &Response::Event { event }, MAX_RESPONSE_FRAME).unwrap();
        }
        stream.flush().unwrap();
    });
    let events: Vec<_> =
        Client::connect(&path).unwrap().subscribe(&[Topic::Workspace]).unwrap().collect();
    assert_eq!(events.len(), 2);
    assert!(matches!(events[1], Ok(Event::WorkspaceChanged { ref workspace, .. }) if workspace == "3"));
}

#[test]
fn an_oversized_response_is_refused() {
    let path = temp_socket("big");
    let listener = UnixListener::bind(&path).unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _: Option<Request> = read_message(&mut stream, MAX_REQUEST_FRAME).unwrap();
        stream.write_all(&u32::MAX.to_be_bytes()).unwrap();
    });
    let mut client = Client::connect(&path).unwrap();
    assert!(matches!(client.mode(), Err(ClientError::Frame(FrameError::TooLarge { .. }))));
}

#[test]
fn the_socket_path_honours_the_override_and_otherwise_is_per_user() {
    assert_eq!(socket_path_from(Some("/x/y.sock".into()), None, 501), PathBuf::from("/x/y.sock"));
    assert_eq!(
        socket_path_from(None, Some("/var/folders/T".into()), 501),
        PathBuf::from("/var/folders/T/dowm-501/dowm.sock")
    );
    assert_eq!(socket_path_from(None, None, 7), PathBuf::from("/tmp/dowm-7/dowm.sock"));
}

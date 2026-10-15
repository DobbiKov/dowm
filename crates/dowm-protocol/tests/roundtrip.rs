use std::io::Cursor;

use dowm_protocol::*;

fn framed<T: serde::Serialize>(msg: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    write_message(&mut buf, msg, MAX_REQUEST_FRAME).unwrap();
    buf
}

fn round_trip_request(request: Request) {
    let bytes = framed(&request);
    let back: Request = read_message(&mut Cursor::new(bytes), MAX_REQUEST_FRAME).unwrap().unwrap();
    assert_eq!(back, request);
}

#[test]
fn every_request_round_trips() {
    round_trip_request(Request::Command { command: "workspace 3".into() });
    round_trip_request(Request::Query { query: Query::ListWorkspaces });
    round_trip_request(Request::Query { query: Query::ListWindows });
    round_trip_request(Request::Query { query: Query::ListMonitors });
    round_trip_request(Request::Query { query: Query::Mode });
    round_trip_request(Request::Subscribe { topics: vec![Topic::Workspace, Topic::Focus, Topic::Mode] });
    round_trip_request(Request::Subscribe { topics: vec![] });
}

#[test]
fn every_response_round_trips() {
    let responses = vec![
        Response::Ok,
        Response::Error { message: "unknown command `frobnicate`".into() },
        Response::Subscribed,
        Response::Mode { mode: "resize".into() },
        Response::Workspaces {
            workspaces: vec![WorkspaceInfo {
                name: "2".into(),
                focused: true,
                visible: true,
                monitor: Some("uuid-1".into()),
                windows: 3,
            }],
        },
        Response::Windows {
            windows: vec![WindowInfo {
                id: 42,
                pid: 99,
                app: Some("com.apple.Safari".into()),
                title: "héllo \u{1F600}".into(),
                workspace: Some("1".into()),
                focused: false,
                foreign: false,
                frame: Rect { x: 0.0, y: 24.0, width: 800.0, height: 600.5 },
            }],
        },
        Response::Monitors {
            monitors: vec![MonitorInfo {
                id: "uuid-1".into(),
                name: Some("Built-in".into()),
                focused: true,
                workspace: Some("2".into()),
                frame: Rect { x: 0.0, y: 0.0, width: 1512.0, height: 982.0 },
            }],
        },
        Response::Event { event: Event::WorkspaceChanged { workspace: "3".into(), previous: Some("2".into()) } },
        Response::Event {
            event: Event::MonitorsChanged {
                monitors: vec![MonitorInfo {
                    id: "uuid-1".into(),
                    name: None,
                    focused: true,
                    workspace: Some("2".into()),
                    frame: Rect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 },
                }],
            },
        },
        Response::Event { event: Event::FocusChanged { window: Some(7) } },
        Response::Event { event: Event::ModeChanged { mode: "main".into() } },
    ];
    for response in responses {
        let mut buf = Vec::new();
        write_message(&mut buf, &response, MAX_RESPONSE_FRAME).unwrap();
        let back: Response =
            read_message(&mut Cursor::new(buf), MAX_RESPONSE_FRAME).unwrap().unwrap();
        assert_eq!(back, response);
    }
}

#[test]
fn the_wire_format_is_a_big_endian_length_then_json() {
    let bytes = framed(&Request::Query { query: Query::Mode });
    let len = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    assert_eq!(len, bytes.len() - 4);
    let json: serde_json::Value = serde_json::from_slice(&bytes[4..]).unwrap();
    assert_eq!(json["type"], "query");
    assert_eq!(json["query"], "mode");
}

#[test]
fn several_frames_on_one_stream_are_read_in_order() {
    let mut buf = framed(&Request::Command { command: "a".into() });
    buf.extend(framed(&Request::Command { command: "b".into() }));
    let mut cursor = Cursor::new(buf);
    let first: Request = read_message(&mut cursor, MAX_REQUEST_FRAME).unwrap().unwrap();
    let second: Request = read_message(&mut cursor, MAX_REQUEST_FRAME).unwrap().unwrap();
    assert_eq!(first, Request::Command { command: "a".into() });
    assert_eq!(second, Request::Command { command: "b".into() });
    assert!(read_message::<Request>(&mut cursor, MAX_REQUEST_FRAME).unwrap().is_none(), "clean EOF");
}

#[test]
fn an_oversized_length_is_rejected_before_any_body_is_read() {
    // Header claims 4 GiB; no body follows. Must fail on the header alone.
    let header = u32::MAX.to_be_bytes().to_vec();
    let err = read_frame(&mut Cursor::new(header), MAX_REQUEST_FRAME).unwrap_err();
    assert!(matches!(err, FrameError::TooLarge { len, max } if len == u32::MAX as usize && max == MAX_REQUEST_FRAME));

    let just_over = ((MAX_REQUEST_FRAME + 1) as u32).to_be_bytes().to_vec();
    assert!(matches!(
        read_frame(&mut Cursor::new(just_over), MAX_REQUEST_FRAME),
        Err(FrameError::TooLarge { .. })
    ));
}

#[test]
fn a_frame_exactly_at_the_limit_is_accepted() {
    let body = vec![b' '; MAX_REQUEST_FRAME];
    let mut buf = (MAX_REQUEST_FRAME as u32).to_be_bytes().to_vec();
    buf.extend(&body);
    assert_eq!(read_frame(&mut Cursor::new(buf), MAX_REQUEST_FRAME).unwrap().unwrap().len(), MAX_REQUEST_FRAME);
}

#[test]
fn writing_an_oversized_message_fails_without_emitting_bytes() {
    let request = Request::Command { command: "x".repeat(MAX_REQUEST_FRAME) };
    let mut buf = Vec::new();
    let err = write_message(&mut buf, &request, MAX_REQUEST_FRAME).unwrap_err();
    assert!(matches!(err, FrameError::TooLarge { .. }));
    assert!(buf.is_empty());
}

#[test]
fn a_zero_length_frame_is_malformed() {
    let err = read_frame(&mut Cursor::new(0u32.to_be_bytes().to_vec()), MAX_REQUEST_FRAME).unwrap_err();
    assert!(matches!(err, FrameError::Empty));
}

#[test]
fn truncated_header_and_body_are_errors_not_eof() {
    let err = read_frame(&mut Cursor::new(vec![0, 0]), MAX_REQUEST_FRAME).unwrap_err();
    assert!(matches!(err, FrameError::Truncated));

    let mut buf = 10u32.to_be_bytes().to_vec();
    buf.extend(b"abc");
    let err = read_frame(&mut Cursor::new(buf), MAX_REQUEST_FRAME).unwrap_err();
    assert!(matches!(err, FrameError::Truncated));
}

#[test]
fn malformed_json_and_wrong_shapes_are_errors() {
    for body in [
        &b"not json"[..],
        b"{}",
        b"{\"type\":\"nope\"}",
        b"{\"type\":\"command\"}",
        b"{\"type\":\"command\",\"command\":5}",
        b"{\"type\":\"command\",\"command\":\"x\",\"extra\":1}",
        b"[1,2,3]",
        b"\xff\xfe",
    ] {
        let mut buf = (body.len() as u32).to_be_bytes().to_vec();
        buf.extend(body);
        let err = read_message::<Request>(&mut Cursor::new(buf), MAX_REQUEST_FRAME).unwrap_err();
        assert!(matches!(err, FrameError::Malformed(_)), "{:?}", String::from_utf8_lossy(body));
    }
}

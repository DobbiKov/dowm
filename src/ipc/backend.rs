//! The daemon's [`Backend`]: parses command lines with the config parser and hands the
//! result to the reactor, then waits (bounded) for its answer.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use dowm_protocol::{Query, Response};

use super::server::Backend;
use crate::actor::input::Shared;
use crate::actor::reactor::ipc::IpcRequest;
use crate::actor::reactor::{Event, Sender};
use crate::config::{Action, parse_command};

/// Longest the reactor gets to answer one request.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct ReactorBackend {
    events: Sender,
    shared: Arc<Shared>,
}

impl ReactorBackend {
    pub fn new(events: Sender, shared: Arc<Shared>) -> Self { Self { events, shared } }

    fn ask<T>(&self, make: impl FnOnce(mpsc::Sender<T>) -> IpcRequest) -> Result<T, String> {
        let (reply, answer) = mpsc::channel();
        self.events
            .try_send(Event::Ipc(make(reply)))
            .map_err(|_| "dowm is shutting down".to_owned())?;
        answer.recv_timeout(REPLY_TIMEOUT).map_err(|_| "dowm did not answer in time".to_owned())
    }
}

impl Backend for ReactorBackend {
    fn command(&self, command: &str) -> Result<(), String> {
        let action = parse_command(command).map_err(|e| e.to_string())?;
        // The socket is user-only, but a client that can talk to it should still not be
        // able to run arbitrary shell through dowm: `exec-and-forget` is config-only.
        if matches!(action, Action::ExecAndForget(_)) {
            return Err("`exec-and-forget` is only available from the config file".into());
        }
        self.ask(|reply| IpcRequest::Run { actions: vec![action], reply })?
    }

    fn query(&self, query: Query) -> Result<Response, String> {
        match query {
            Query::Mode => Ok(Response::Mode { mode: self.shared.modes.lock().current_mode().to_owned() }),
            query => self.ask(|reply| IpcRequest::Query { query, reply }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor;

    fn backend() -> (ReactorBackend, actor::Receiver<Event>) {
        let (tx, rx) = actor::channel();
        let config = crate::config::parse("[mode.main.binding]\nalt-r = 'mode r'\n[mode.r.binding]\n").unwrap();
        (ReactorBackend::new(tx, Shared::new(&config)), rx)
    }

    /// Answer the next IPC request on a stand-in reactor thread.
    fn answer_with(mut rx: actor::Receiver<Event>, run: Result<(), String>) -> std::thread::JoinHandle<Vec<Action>> {
        std::thread::spawn(move || {
            let (_, event) = rx.blocking_recv().unwrap();
            match event {
                Event::Ipc(IpcRequest::Run { actions, reply }) => {
                    reply.send(run).unwrap();
                    actions
                }
                other => panic!("{other:?}"),
            }
        })
    }

    #[test]
    fn a_valid_command_is_parsed_and_sent_to_the_reactor() {
        let (backend, rx) = backend();
        let reactor = answer_with(rx, Ok(()));
        assert_eq!(backend.command("workspace 3"), Ok(()));
        assert_eq!(reactor.join().unwrap(), vec![parse_command("workspace 3").unwrap()]);
    }

    #[test]
    fn the_reactors_failure_reaches_the_caller() {
        let (backend, rx) = backend();
        let reactor = answer_with(rx, Err("no such workspace".into()));
        assert_eq!(backend.command("workspace 99"), Err("no such workspace".into()));
        reactor.join().unwrap();
    }

    #[test]
    fn a_bad_command_is_rejected_without_bothering_the_reactor() {
        let (backend, mut rx) = backend();
        assert!(backend.command("frobnicate").unwrap_err().contains("unknown command"));
        assert!(backend.command("workspace").is_err());
        assert!(rx.try_recv().is_err(), "nothing was sent");
    }

    #[test]
    fn exec_and_forget_is_refused_over_the_socket() {
        let (backend, mut rx) = backend();
        let error = backend.command("exec-and-forget touch /tmp/pwned").unwrap_err();
        assert!(error.contains("config file"), "{error}");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn the_mode_query_reads_the_hotkey_state() {
        let (backend, _rx) = backend();
        assert_eq!(backend.query(Query::Mode), Ok(Response::Mode { mode: "main".into() }));
        backend.shared.modes.lock().set_mode("r").unwrap();
        assert_eq!(backend.query(Query::Mode), Ok(Response::Mode { mode: "r".into() }));
    }

    #[test]
    fn a_stopped_reactor_is_an_error_not_a_hang() {
        let (backend, rx) = backend();
        drop(rx);
        assert!(backend.command("workspace 1").unwrap_err().contains("shutting down"));
    }
}

//! The config watcher: a plain thread that polls the config file and sends valid changes
//! to the reactor as [`Event::ConfigReloaded`]. A change that does not parse is logged
//! and dropped, so the running config stays as it was.
//!
//! `reload-config` reaches it through [`WatchCmd::ReloadNow`], which makes the next poll
//! report the file even if its content is unchanged.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use tracing::{debug, error, info};

use crate::actor::reactor::{Event, Sender as ReactorSender};
use crate::config::watch::WatchState;

/// How often the file is checked.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchCmd {
    /// Re-read and re-apply now (`reload-config`).
    ReloadNow,
}

pub type WatchHandle = Sender<WatchCmd>;

/// Start the watcher thread. It exits when every [`WatchHandle`] is dropped.
pub fn spawn(mut state: WatchState, events: ReactorSender, interval: Duration) -> (WatchHandle, JoinHandle<()>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    let thread = std::thread::Builder::new()
        .name("config-watcher".into())
        .spawn(move || run(&mut state, &events, &rx, interval))
        .expect("failed to spawn the config watcher thread");
    (tx, thread)
}

fn run(state: &mut WatchState, events: &ReactorSender, cmds: &Receiver<WatchCmd>, interval: Duration) {
    loop {
        match cmds.recv_timeout(interval) {
            Ok(WatchCmd::ReloadNow) => {
                if state.source().path().is_none() {
                    debug!("reload-config: the built-in config has nothing to reload");
                }
                state.force();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        match state.poll() {
            Some(Ok(config)) => {
                info!("config file changed; reloading");
                events.send(Event::ConfigReloaded(Arc::new(config)));
            }
            Some(Err(errors)) => {
                error!("config reload rejected, keeping the running config:\n{errors}");
            }
            None => {}
        }
    }
}

//! Carries out the reactor's [`Effect`]s.
//!
//! The reactor queues effects; [`run`] drains them off a channel on an async task and does
//! the OS work: spawn processes, warp the cursor, switch binding modes, swap in a config.
//! None of it blocks the reactor.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;

use tracing::{debug, info, warn};

use super::Shared;
use super::mouse::warp_target;
use super::watcher::{WatchCmd, WatchHandle};
use crate::actor::reactor::effects::{Effect, EffectReceiver};
use crate::sys::event::{current_cursor_location, warp_mouse};

/// Run effects until the channel closes.
pub async fn run(mut effects: EffectReceiver, shared: Arc<Shared>, watcher: WatchHandle) {
    while let Some((span, effect)) = effects.recv().await {
        let _guard = span.enter();
        apply(effect, &shared, &watcher);
    }
}

fn apply(effect: Effect, shared: &Shared, watcher: &WatchHandle) {
    match effect {
        Effect::Exec(shell) => exec_and_forget(&shell),
        Effect::SetMode(name) => match shared.set_mode(&name) {
            Ok(()) => info!(mode = %name, "binding mode"),
            Err(error) => warn!(%error, "mode not switched"),
        },
        Effect::ReloadConfig => {
            if watcher.send(WatchCmd::ReloadNow).is_err() {
                warn!("the config watcher is gone; cannot reload");
            }
        }
        Effect::MoveMouse { rect, lazy } => {
            let cursor = current_cursor_location().ok();
            if let Some(target) = warp_target(rect, cursor, lazy)
                && let Err(error) = warp_mouse(target)
            {
                warn!(?error, "could not move the mouse");
            }
        }
        Effect::ConfigApplied(config) => shared.apply(&config),
    }
}

/// Run `shell` through `/bin/bash -c`, detached: its own process group (so dowm's signals
/// do not reach it), no stdio, and a thread to reap it. Failure to start is logged.
fn exec_and_forget(shell: &str) {
    let mut command = Command::new("/bin/bash");
    command.arg("-c").arg(shell).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    command.process_group(0);
    match command.spawn() {
        Ok(mut child) => {
            debug!(command = shell, pid = child.id(), "exec-and-forget");
            let spawned = std::thread::Builder::new().name("exec-reaper".into()).spawn(move || {
                _ = child.wait();
            });
            if let Err(error) = spawned {
                warn!(%error, "could not start a reaper thread; the child becomes a zombie until dowm exits");
            }
        }
        Err(error) => warn!(command = shell, %error, "exec-and-forget failed to start"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_and_forget_runs_the_command_through_bash_and_does_not_block() {
        let dir = std::env::temp_dir().join(format!("dowm-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran");
        // A pipeline and a redirect prove it went through a shell.
        exec_and_forget(&format!("echo hello | tr a-z A-Z > '{}'", marker.display()));
        let mut waited = 0;
        while !marker.exists() && waited < 100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            waited += 1;
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "HELLO");
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_command_is_not_an_error_for_the_caller() {
        exec_and_forget("exit 3");
        exec_and_forget("this-command-does-not-exist-xyz 2>/dev/null");
    }

    #[test]
    fn effects_that_need_no_os_are_applied_to_shared_state() {
        let config = crate::config::parse("[mode.main.binding]\nalt-r = 'mode r'\n[mode.r.binding]\nesc = 'mode main'\n").unwrap();
        let shared = Shared::new(&config);
        let (watcher, rx) = crossbeam_channel::unbounded();
        apply(Effect::SetMode("r".into()), &shared, &watcher);
        assert_eq!(shared.modes.lock().current_mode(), "r");
        apply(Effect::SetMode("nope".into()), &shared, &watcher);
        assert_eq!(shared.modes.lock().current_mode(), "r", "an unknown mode changes nothing");
        apply(Effect::ReloadConfig, &shared, &watcher);
        assert_eq!(rx.try_recv(), Ok(WatchCmd::ReloadNow));
        let next = crate::config::parse("focus-follows-mouse = true\n[mode.main.binding]\n").unwrap();
        apply(Effect::ConfigApplied(Arc::new(next)), &shared, &watcher);
        assert!(shared.focus_follows_mouse());
        assert_eq!(shared.modes.lock().current_mode(), "main", "mode r vanished");
    }
}

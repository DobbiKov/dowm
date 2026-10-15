//! The platform shell of M4: everything that touches the OS on behalf of the config.
//!
//! Each piece is thin and keeps its logic in pure code that is tested elsewhere:
//!
//! - [`hotkeys`]: a CGEventTap that looks key-downs up in the live mode's bindings
//!   ([`BindingModes`]), swallows hits and hands their actions to the reactor.
//! - [`pointer`]: a listen-only mouse-moved tap, debounced, for focus-follows-mouse.
//! - [`watcher`]: polls the config file ([`WatchState`]) and sends valid changes to
//!   the reactor.
//! - [`runner`]: carries out the reactor's [`Effect`]s: `exec-and-forget`, `move-mouse`,
//!   `mode`, `reload-config`, and swapping in a newly applied config.
//!
//! [`Shared`] is the one piece of state the tap callbacks read: the hotkey table and
//! the focus-follows-mouse switch, both replaced whenever a config is applied.
//!
//! [`Effect`]: crate::actor::reactor::effects::Effect
//! [`WatchState`]: crate::config::watch::WatchState

pub mod hotkeys;
pub mod keeper;
pub mod mouse;
pub mod pointer;
pub mod runner;
pub mod watcher;

use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use dowm_protocol::Event;

use crate::config::{BindingModes, Config};
use crate::ipc::EventBus;

/// State shared with the event-tap callbacks.
pub struct Shared {
    /// The hotkey table and the current binding mode. Locked for a hash lookup only.
    pub modes: Mutex<BindingModes>,
    pub focus_follows_mouse: AtomicBool,
    /// Where mode changes are announced; set once the daemon has a bus.
    bus: OnceLock<Arc<EventBus>>,
}

impl Shared {
    pub fn new(config: &Config) -> Arc<Self> {
        Arc::new(Shared {
            modes: Mutex::new(BindingModes::new(config)),
            focus_follows_mouse: AtomicBool::new(config.focus_follows_mouse),
            bus: OnceLock::new(),
        })
    }

    /// Swap in a newly applied config. The current binding mode survives if it still
    /// exists (see [`BindingModes::reload`]).
    pub fn apply(&self, config: &Config) {
        let mut modes = self.modes.lock();
        let before = modes.current_mode().to_owned();
        modes.reload(config);
        self.announce_mode_change(&before, &modes);
        drop(modes);
        self.focus_follows_mouse.store(config.focus_follows_mouse, Ordering::Relaxed);
    }

    /// Announce mode changes to `bus` from now on.
    pub fn attach_bus(&self, bus: Arc<EventBus>) { _ = self.bus.set(bus); }

    /// Switch the binding mode and announce it. Everything that changes the mode goes
    /// through here (or [`Shared::apply`]), so subscribers never miss a change.
    pub fn set_mode(&self, name: &str) -> Result<(), crate::config::modes::UnknownMode> {
        let mut modes = self.modes.lock();
        let before = modes.current_mode().to_owned();
        modes.set_mode(name)?;
        self.announce_mode_change(&before, &modes);
        Ok(())
    }

    /// Announce the mode if it differs from `before`. Called with the lock held, so the
    /// comparison and the change are one step; publishing never blocks.
    pub(crate) fn announce_mode_change(&self, before: &str, modes: &BindingModes) {
        if modes.current_mode() != before
            && let Some(bus) = self.bus.get()
        {
            bus.publish(Event::ModeChanged { mode: modes.current_mode().to_owned() });
        }
    }

    pub fn focus_follows_mouse(&self) -> bool { self.focus_follows_mouse.load(Ordering::Relaxed) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Hotkey, KeyPreset, parse};

    fn config(text: &str) -> Config { parse(text).expect("valid") }

    #[test]
    fn applying_a_config_swaps_bindings_and_the_pointer_switch() {
        let shared = Shared::new(&config("[mode.main.binding]\nalt-h = 'focus left'\n"));
        let key = |s: &str| Hotkey::parse(s, KeyPreset::Qwerty).unwrap();
        assert!(!shared.focus_follows_mouse());
        assert!(shared.modes.lock().dispatch(key("alt-h")).is_some());
        shared.apply(&config("focus-follows-mouse = true\n[mode.main.binding]\nalt-j = 'focus down'\n"));
        assert!(shared.focus_follows_mouse());
        assert!(shared.modes.lock().dispatch(key("alt-h")).is_none());
        assert!(shared.modes.lock().dispatch(key("alt-j")).is_some());
    }

    #[test]
    fn applying_a_config_keeps_the_mode_unless_it_vanished() {
        let two = "[mode.main.binding]\nalt-r = 'mode r'\n[mode.r.binding]\nesc = 'mode main'\n";
        let shared = Shared::new(&config(two));
        shared.modes.lock().set_mode("r").unwrap();
        shared.apply(&config(two));
        assert_eq!(shared.modes.lock().current_mode(), "r");
        shared.apply(&config("[mode.main.binding]\n"));
        assert_eq!(shared.modes.lock().current_mode(), "main");
    }

    #[test]
    fn every_mode_change_is_published_and_no_ops_are_not() {
        use std::sync::Mutex;
        let text = "[mode.main.binding]\nalt-r = 'mode r'\n[mode.r.binding]\nesc = 'mode main'\n";
        let shared = Shared::new(&config(text));
        let bus = Arc::new(EventBus::new());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        bus.subscribe(&[], move |e| {
            sink.lock().unwrap().push(e);
            true
        });
        shared.attach_bus(bus);
        shared.set_mode("r").unwrap();
        shared.set_mode("r").unwrap();
        assert!(shared.set_mode("nope").is_err());
        shared.set_mode("main").unwrap();
        shared.set_mode("r").unwrap();
        shared.apply(&config("[mode.main.binding]\n")); // mode r vanished
        let modes: Vec<_> = seen.lock().unwrap().iter().map(|e| format!("{e:?}")).collect();
        assert_eq!(modes.len(), 4, "{modes:?}");
        assert!(modes[3].contains("main"));
    }
}

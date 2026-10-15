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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use crate::config::{BindingModes, Config};

/// State shared with the event-tap callbacks.
pub struct Shared {
    /// The hotkey table and the current binding mode. Locked for a hash lookup only.
    pub modes: Mutex<BindingModes>,
    pub focus_follows_mouse: AtomicBool,
}

impl Shared {
    pub fn new(config: &Config) -> Arc<Self> {
        Arc::new(Shared {
            modes: Mutex::new(BindingModes::new(config)),
            focus_follows_mouse: AtomicBool::new(config.focus_follows_mouse),
        })
    }

    /// Swap in a newly applied config. The current binding mode survives if it still
    /// exists (see [`BindingModes::reload`]).
    pub fn apply(&self, config: &Config) {
        self.modes.lock().reload(config);
        self.focus_follows_mouse.store(config.focus_follows_mouse, Ordering::Relaxed);
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
}

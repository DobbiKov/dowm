//! Binding modes: which set of hotkeys is live right now.
//!
//! [`BindingModes`] is the pure state machine behind the hotkey tap: it holds every
//! mode's bindings and the current mode, answers "what does this chord do?", and
//! switches modes. It does no I/O; the tap looks chords up here, and the
//! outbound effect `SetMode` (produced when a `mode <name>` action runs) switches it.
//!
//! `main` always exists and is where dowm starts. A reload keeps the current mode if
//! the new config still has it and falls back to `main` otherwise.

use std::collections::HashMap;

use super::command::Action;
use super::hotkey::Hotkey;
use super::Config;

/// The name of the mandatory default mode.
pub const MAIN_MODE: &str = "main";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownMode(pub String);

impl std::fmt::Display for UnknownMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no binding mode called `{}`", self.0)
    }
}

impl std::error::Error for UnknownMode {}

#[derive(Clone, Debug)]
pub struct BindingModes {
    modes: HashMap<String, HashMap<Hotkey, Vec<Action>>>,
    current: String,
}

impl BindingModes {
    /// Every mode of `config`, starting in `main`.
    pub fn new(config: &Config) -> Self {
        BindingModes { modes: Self::table(config), current: MAIN_MODE.to_owned() }
    }

    fn table(config: &Config) -> HashMap<String, HashMap<Hotkey, Vec<Action>>> {
        config
            .modes
            .iter()
            .map(|(name, mode)| {
                let bindings = mode
                    .bindings
                    .iter()
                    .map(|(&hotkey, binding)| (hotkey, binding.actions.clone()))
                    .collect();
                (name.clone(), bindings)
            })
            .collect()
    }

    pub fn current_mode(&self) -> &str { &self.current }

    pub fn has_mode(&self, name: &str) -> bool { self.modes.contains_key(name) }

    /// What `hotkey` does in the current mode. Chords bound in other modes do not
    /// match; `None` means the key is not ours and passes through.
    pub fn dispatch(&self, hotkey: Hotkey) -> Option<Vec<Action>> {
        self.modes.get(&self.current)?.get(&hotkey).cloned()
    }

    /// Whether `hotkey` is bound in the current mode (without cloning the actions).
    pub fn is_bound(&self, hotkey: Hotkey) -> bool {
        self.modes.get(&self.current).is_some_and(|m| m.contains_key(&hotkey))
    }

    /// Switch to `name`. An unknown mode changes nothing.
    pub fn set_mode(&mut self, name: &str) -> Result<(), UnknownMode> {
        if !self.modes.contains_key(name) {
            return Err(UnknownMode(name.to_owned()));
        }
        self.current = name.to_owned();
        Ok(())
    }

    /// Swap in `config`'s bindings. The current mode survives if it still exists;
    /// otherwise the state falls back to `main`.
    pub fn reload(&mut self, config: &Config) {
        self.modes = Self::table(config);
        if !self.modes.contains_key(&self.current) {
            self.current = MAIN_MODE.to_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::reactor::Command;
    use crate::config::keys::KeyPreset;

    const TWO_MODES: &str = "
[mode.main.binding]
alt-h = 'focus left'
alt-r = 'mode resize'
alt-shift-r = ['reload-config', 'mode main']
[mode.resize.binding]
h = 'resize width -50'
enter = 'mode main'
esc = 'mode main'
alt-h = 'balance-sizes'
";

    fn modes(text: &str) -> BindingModes { BindingModes::new(&crate::config::parse(text).expect("valid config")) }

    fn key(text: &str) -> Hotkey { Hotkey::parse(text, KeyPreset::Qwerty).unwrap() }

    /// What the reactor's `mode` effect does, applied to the key's actions.
    fn press(m: &mut BindingModes, chord: &str) {
        for action in m.dispatch(key(chord)).unwrap_or_default() {
            if let Action::Mode(name) = action {
                m.set_mode(&name).unwrap();
            }
        }
    }

    #[test]
    fn it_starts_in_main_and_dispatches_its_bindings() {
        let m = modes(TWO_MODES);
        assert_eq!(m.current_mode(), "main");
        assert_eq!(
            m.dispatch(key("alt-h")),
            Some(vec![Action::Reactor(Command::Focus { direction: crate::model::workspace::Direction::Left, wrap: false })])
        );
        assert!(m.is_bound(key("alt-h")));
    }

    #[test]
    fn an_unbound_chord_does_not_match() {
        let m = modes(TWO_MODES);
        assert_eq!(m.dispatch(key("alt-z")), None);
        assert_eq!(m.dispatch(key("h")), None, "h is bound in resize, not main");
        assert!(!m.is_bound(key("alt-shift-h")), "modifiers must match exactly");
    }

    #[test]
    fn main_to_resize_and_back_via_esc_and_enter() {
        let mut m = modes(TWO_MODES);
        press(&mut m, "alt-r");
        assert_eq!(m.current_mode(), "resize");
        assert!(m.dispatch(key("h")).is_some());
        press(&mut m, "esc");
        assert_eq!(m.current_mode(), "main");

        press(&mut m, "alt-r");
        press(&mut m, "enter");
        assert_eq!(m.current_mode(), "main");
    }

    #[test]
    fn a_binding_in_one_mode_does_not_fire_in_another() {
        let mut m = modes(TWO_MODES);
        let main_alt_h = m.dispatch(key("alt-h")).unwrap();
        m.set_mode("resize").unwrap();
        let resize_alt_h = m.dispatch(key("alt-h")).unwrap();
        assert_ne!(main_alt_h, resize_alt_h, "the same chord means different things per mode");
        assert_eq!(resize_alt_h, vec![Action::Reactor(Command::BalanceSizes)]);
        assert_eq!(m.dispatch(key("alt-r")), None, "main's bindings are not live in resize");
    }

    #[test]
    fn switching_to_an_unknown_mode_is_refused_and_changes_nothing() {
        let mut m = modes(TWO_MODES);
        m.set_mode("resize").unwrap();
        assert_eq!(m.set_mode("nope"), Err(UnknownMode("nope".into())));
        assert_eq!(m.current_mode(), "resize");
        assert!(UnknownMode("nope".into()).to_string().contains("nope"));
        assert!(m.has_mode("main") && !m.has_mode("nope"));
    }

    #[test]
    fn reload_keeps_the_current_mode_when_it_still_exists() {
        let mut m = modes(TWO_MODES);
        m.set_mode("resize").unwrap();
        let reloaded = crate::config::parse(&TWO_MODES.replace("resize width -50", "resize width -10")).unwrap();
        m.reload(&reloaded);
        assert_eq!(m.current_mode(), "resize");
        assert_eq!(
            m.dispatch(key("h")),
            Some(vec![Action::Reactor(Command::Resize {
                dimension: crate::layout::ops::ResizeDimension::Width,
                amount: -10
            })]),
            "the new bindings are live"
        );
    }

    #[test]
    fn reload_while_in_a_removed_mode_falls_back_to_main() {
        let mut m = modes(TWO_MODES);
        m.set_mode("resize").unwrap();
        m.reload(&crate::config::parse("[mode.main.binding]\nalt-h = 'focus left'\n").unwrap());
        assert_eq!(m.current_mode(), "main");
        assert!(!m.has_mode("resize"));
        assert!(m.dispatch(key("alt-h")).is_some());
    }

    #[test]
    fn a_reload_can_add_and_remove_bindings_within_a_mode() {
        let mut m = modes(TWO_MODES);
        m.reload(&crate::config::parse("[mode.main.binding]\nalt-j = 'focus down'\n").unwrap());
        assert!(m.dispatch(key("alt-h")).is_none());
        assert!(m.dispatch(key("alt-j")).is_some());
    }

    #[test]
    fn multi_command_bindings_dispatch_every_action_in_order() {
        let m = modes(TWO_MODES);
        assert_eq!(m.dispatch(key("alt-shift-r")), Some(vec![Action::ReloadConfig, Action::Mode("main".into())]));
    }
}

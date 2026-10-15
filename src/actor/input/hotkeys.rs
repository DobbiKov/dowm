//! The hotkey tap.
//!
//! An active CGEventTap on key-down events. For each one the callback builds a
//! [`Hotkey`] from the keycode and modifier flags, looks it up in the *current* binding
//! mode, and on a hit swallows the event (returns null) and sends the binding's actions
//! to the reactor as [`Event::Actions`]. A miss passes the event through untouched.
//!
//! The callback runs inside the window server's event delivery, so it does a hash lookup
//! and a channel send and nothing else; the reactor does the work. Tap-disabled
//! notifications (timeout, user input) are handled by the [`EventTap`] wrapper, which
//! re-enables the tap.
//!
//! `mode` actions of a binding are applied inside the callback, under the same lock as
//! the lookup, so the very next key is looked up in the new mode. They are not forwarded:
//! the reactor only sees the other actions, and no late effect can revert a newer switch.
//! (A `mode` action runs even if an earlier command of the chain fails; see
//! `Reactor::run_actions`.)
//!
//! A swallowed key-down's key-up is swallowed too (see [`Swallowed`]), so apps never see
//! a lone key-up.
//!
//! Auto-repeat: repeated key-downs of a bound chord are swallowed (letting them through
//! would type into the focused app), but only re-dispatched when every action of the
//! binding is repeatable (`resize`).

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapLocation, CGEventTapOptions, CGEventTapProxy, CGEventType,
};
use tracing::{info, warn};

use super::Shared;
use crate::actor::reactor::{Event, Sender as ReactorSender};
use crate::config::{Action, Hotkey, Mods};
use crate::sys::event::is_rift_synthetic_event;
use crate::sys::event_tap::EventTap;

struct TapContext {
    shared: Arc<Shared>,
    events: ReactorSender,
    swallowed: Mutex<Swallowed>,
    /// Set when the tap's Mach port could not be revived; the keeper reinstalls the tap.
    invalid: Arc<AtomicBool>,
}

/// Key-downs the tap swallowed whose key-up has not arrived yet.
#[derive(Debug, Default)]
pub(crate) struct Swallowed(HashSet<u16>);

impl Swallowed {
    pub(crate) fn key_down(&mut self, keycode: u16, swallowed: bool) {
        if swallowed {
            self.0.insert(keycode);
        } else {
            self.0.remove(&keycode);
        }
    }

    /// Whether this key-up belongs to a swallowed key-down (and so is swallowed too).
    pub(crate) fn key_up(&mut self, keycode: u16) -> bool { self.0.remove(&keycode) }
}

/// What the callback should do with a key-down.
#[derive(Debug, PartialEq)]
pub(crate) enum KeyOutcome {
    /// Not ours: deliver it to the focused app.
    PassThrough,
    /// Ours: swallow it, and run these actions (none for a swallowed auto-repeat).
    Swallow(Vec<Action>),
}

/// The decision for one key-down, separated from the FFI so it can be tested.
pub(crate) fn decide(shared: &Shared, keycode: u16, flags: u64, autorepeat: bool) -> KeyOutcome {
    let hotkey = Hotkey { mods: Mods::from_cg_flags(flags), keycode };
    let mut modes = shared.modes.lock();
    let Some(actions) = modes.dispatch(hotkey) else {
        return KeyOutcome::PassThrough;
    };
    if autorepeat && (actions.is_empty() || !actions.iter().all(Action::is_repeatable)) {
        return KeyOutcome::Swallow(Vec::new());
    }
    // Mode switches happen now, in order, so the next key sees the new mode.
    let mut forwarded = Vec::with_capacity(actions.len());
    for action in actions {
        match action {
            Action::Mode(name) => {
                if let Err(error) = modes.set_mode(&name) {
                    warn!(%error, "mode not switched");
                }
            }
            other => forwarded.push(other),
        }
    }
    KeyOutcome::Swallow(forwarded)
}

unsafe extern "C-unwind" fn key_callback(
    _proxy: CGEventTapProxy,
    etype: CGEventType,
    event: core::ptr::NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    if user_info.is_null() || !matches!(etype, CGEventType::KeyDown | CGEventType::KeyUp) {
        return event.as_ptr();
    }
    let context = unsafe { &*(user_info as *const TapContext) };
    let event_ref = unsafe { event.as_ref() };
    // Our own synthetic key events (Command-W for `close-all-windows-but-current`) are
    // never hotkeys.
    if is_rift_synthetic_event(event_ref) {
        return event.as_ptr();
    }
    let keycode = CGEvent::integer_value_field(Some(event_ref), CGEventField::KeyboardEventKeycode);
    let Ok(keycode) = u16::try_from(keycode) else { return event.as_ptr() };
    if etype == CGEventType::KeyUp {
        return if context.swallowed.lock().key_up(keycode) { core::ptr::null_mut() } else { event.as_ptr() };
    }
    let autorepeat =
        CGEvent::integer_value_field(Some(event_ref), CGEventField::KeyboardEventAutorepeat) != 0;
    let flags = CGEvent::flags(Some(event_ref)).0;
    match decide(&context.shared, keycode, flags, autorepeat) {
        KeyOutcome::PassThrough => {
            context.swallowed.lock().key_down(keycode, false);
            event.as_ptr()
        }
        KeyOutcome::Swallow(actions) => {
            context.swallowed.lock().key_down(keycode, true);
            if !actions.is_empty() {
                context.events.send(Event::Actions(actions));
            }
            core::ptr::null_mut()
        }
    }
}

unsafe extern "C-unwind" fn tap_invalidated(user_info: *mut c_void) {
    if !user_info.is_null() {
        let context = unsafe { &*(user_info as *const TapContext) };
        context.invalid.store(true, Ordering::Relaxed);
    }
}

unsafe fn drop_context(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(ptr as *mut TapContext) });
    }
}

/// The installed tap; dropping it removes the tap.
pub struct HotkeyTap {
    _tap: EventTap,
    invalid: Arc<AtomicBool>,
}

impl HotkeyTap {
    /// Install the tap on the current thread's run loop (the main thread, in the daemon).
    /// `None` if macOS refuses (Accessibility / Input Monitoring not granted); the caller
    /// logs and retries.
    pub fn install(shared: Arc<Shared>, events: ReactorSender) -> Option<Self> {
        let invalid = Arc::new(AtomicBool::new(false));
        let context = Box::into_raw(Box::new(TapContext {
            shared,
            events,
            swallowed: Mutex::new(Swallowed::default()),
            invalid: Arc::clone(&invalid),
        })) as *mut c_void;
        let mask = (1u64 << CGEventType::KeyDown.0) | (1u64 << CGEventType::KeyUp.0);
        // SAFETY: `context` stays valid until the tap drops it through `drop_context`; on
        // failure `EventTap::new` leaves ownership with us and we free it below.
        let tap = unsafe {
            EventTap::new(
                CGEventTapLocation::HIDEventTap,
                CGEventTapOptions::Default,
                mask,
                Some(key_callback),
                context,
                Some(drop_context),
                None,
                Some(tap_invalidated),
            )
        };
        match tap {
            Some(tap) => {
                info!("hotkey tap installed");
                Some(HotkeyTap { _tap: tap, invalid })
            }
            None => {
                unsafe { drop_context(context) };
                None
            }
        }
    }

    /// The tap's Mach port died and could not be revived: drop this and install again.
    pub fn is_invalid(&self) -> bool { self.invalid.load(Ordering::Relaxed) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, parse};

    const SHIFT: u64 = 0x0002_0000;
    const ALT: u64 = 0x0008_0000;
    const CAPS: u64 = 0x0001_0000;
    const FN: u64 = 0x0080_0000;

    fn shared() -> Arc<Shared> {
        let config: Config = parse(
            "[mode.main.binding]\nalt-h = 'focus left'\nalt-r = 'mode resize'\n\
             [mode.resize.binding]\nh = 'resize width -50'\nj = ['resize height +50', 'resize width +1']\nk = ['resize height -5', 'balance-sizes']\nesc = 'mode main'\n",
        )
        .expect("valid");
        Shared::new(&config)
    }

    const H: u16 = 0x04;
    const J: u16 = 0x26;
    const K: u16 = 0x28;

    #[test]
    fn a_bound_chord_is_swallowed_with_its_actions() {
        let s = shared();
        let KeyOutcome::Swallow(actions) = decide(&s, H, ALT, false) else { panic!("should swallow") };
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn unbound_chords_and_wrong_modifiers_pass_through() {
        let s = shared();
        assert_eq!(decide(&s, H, 0, false), KeyOutcome::PassThrough);
        assert_eq!(decide(&s, H, ALT | SHIFT, false), KeyOutcome::PassThrough, "modifiers must match exactly");
        assert_eq!(decide(&s, 0x00, ALT, false), KeyOutcome::PassThrough);
    }

    #[test]
    fn caps_lock_and_fn_do_not_defeat_a_binding() {
        let s = shared();
        assert!(matches!(decide(&s, H, ALT | CAPS | FN, false), KeyOutcome::Swallow(a) if a.len() == 1));
    }

    #[test]
    fn the_current_mode_decides() {
        let s = shared();
        assert_eq!(decide(&s, H, 0, false), KeyOutcome::PassThrough, "plain h is a resize-mode key");
        s.modes.lock().set_mode("resize").unwrap();
        assert!(matches!(decide(&s, H, 0, false), KeyOutcome::Swallow(a) if a.len() == 1));
        assert_eq!(decide(&s, H, ALT, false), KeyOutcome::PassThrough, "main's alt-h is not live in resize");
    }

    #[test]
    fn auto_repeat_of_a_non_repeatable_binding_is_swallowed_without_running_it() {
        let s = shared();
        assert_eq!(decide(&s, H, ALT, true), KeyOutcome::Swallow(Vec::new()));
        assert!(matches!(decide(&s, H, ALT, false), KeyOutcome::Swallow(a) if !a.is_empty()));
    }

    #[test]
    fn auto_repeat_reruns_resize_only_bindings() {
        let s = shared();
        s.modes.lock().set_mode("resize").unwrap();
        assert!(matches!(decide(&s, H, 0, true), KeyOutcome::Swallow(a) if a.len() == 1));
        assert!(matches!(decide(&s, J, 0, true), KeyOutcome::Swallow(a) if a.len() == 2), "all resize commands");
        assert_eq!(decide(&s, K, 0, true), KeyOutcome::Swallow(Vec::new()), "balance-sizes does not repeat");
    }

    #[test]
    fn a_mode_chord_switches_the_mode_before_the_next_key_and_is_not_forwarded() {
        let s = shared();
        assert_eq!(decide(&s, 0x0F, ALT, false), KeyOutcome::Swallow(Vec::new()), "alt-r: only a mode switch");
        assert_eq!(s.modes.lock().current_mode(), "resize", "applied synchronously");
        assert!(matches!(decide(&s, H, 0, false), KeyOutcome::Swallow(a) if a.len() == 1), "the next key sees resize");
        assert_eq!(decide(&s, 0x35, 0, false), KeyOutcome::Swallow(Vec::new()));
        assert_eq!(s.modes.lock().current_mode(), "main");
        assert_eq!(decide(&s, H, 0, false), KeyOutcome::PassThrough);
    }

    #[test]
    fn mode_switches_are_stripped_from_mixed_bindings_in_order() {
        let config = parse("[mode.main.binding]\nalt-s = ['focus left', 'mode x', 'focus right']\n[mode.x.binding]\n").unwrap();
        let s = Shared::new(&config);
        let KeyOutcome::Swallow(actions) = decide(&s, 0x01, ALT, false) else { panic!() };
        assert_eq!(actions.len(), 2);
        assert_eq!(s.modes.lock().current_mode(), "x");
    }

    #[test]
    fn a_key_up_is_swallowed_only_after_a_swallowed_key_down() {
        let mut sw = Swallowed::default();
        assert!(!sw.key_up(H), "never swallowed: passes");
        sw.key_down(H, true);
        assert!(sw.key_up(H));
        assert!(!sw.key_up(H), "once");
        sw.key_down(H, true);
        sw.key_down(H, false);
        assert!(!sw.key_up(H), "a later passed-through down clears it");
    }

    #[test]
    fn an_unbound_repeat_still_passes_through() {
        let s = shared();
        assert_eq!(decide(&s, 0x00, ALT, true), KeyOutcome::PassThrough);
    }
}

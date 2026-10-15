//! Which windows live in a native macOS Space other than the one showing.
//!
//! dowm does not use Spaces, but macOS does: when a display is unplugged it appends that
//! display's Space to the one that remains and switches to it, leaving everything else in
//! the old Space. Windows there cannot be listed or moved through AX, and raising one
//! makes macOS animate a Space switch. The reactor leaves such windows alone.
//!
//! Read-only: only `CGSCopyManagedDisplaySpaces` and `CGSCopySpacesForWindows`.

use std::collections::HashSet;
use std::ffi::c_int;

use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUserDefaults};

unsafe extern "C" {
    fn CGSMainConnectionID() -> c_int;
    fn CGSCopyManagedDisplaySpaces(cid: c_int) -> *mut NSArray<NSDictionary>;
    fn CGSCopySpacesForWindows(cid: c_int, mask: c_int, windows: &NSArray<NSNumber>) -> *mut NSArray<NSNumber>;
}

/// `kCGSAllSpacesMask`.
const ALL_SPACES: c_int = 7;

/// The windows among `ids` whose Spaces are all outside `current`. A window in no Space at
/// all (minimized, or not yet placed) is not foreign, and neither is one that is in the
/// current Space among others (a sticky window).
pub fn foreign_ids(current: &HashSet<i64>, spaces_of: impl Fn(u32) -> Vec<i64>, ids: &[u32]) -> Vec<u32> {
    ids.iter()
        .copied()
        .filter(|&id| {
            let spaces = spaces_of(id);
            !spaces.is_empty() && spaces.iter().all(|s| !current.contains(s))
        })
        .collect()
}

fn number(dict: &NSDictionary, key: &str) -> Option<i64> {
    let object = dict.objectForKey(&*NSString::from_str(key).clone().into_super())?;
    let number: &NSNumber = object.downcast_ref()?;
    Some(number.as_i64())
}

/// The Space showing on each display.
fn current_spaces(cid: c_int) -> HashSet<i64> {
    // SAFETY: a Copy function returns an owned (+1) array, or null.
    let Some(displays) = (unsafe { Retained::from_raw(CGSCopyManagedDisplaySpaces(cid)) }) else {
        return HashSet::new();
    };
    displays
        .iter()
        .filter_map(|display| {
            let current = display.objectForKey(&*NSString::from_str("Current Space").clone().into_super())?;
            let current: &NSDictionary = current.downcast_ref()?;
            number(current, "ManagedSpaceID")
        })
        .collect()
}

fn spaces_of_window(cid: c_int, id: u32) -> Vec<i64> {
    let ids = NSArray::from_retained_slice(&[NSNumber::new_i64(i64::from(id))]);
    // SAFETY: as above; `ids` outlives the call.
    let Some(spaces) = (unsafe { Retained::from_raw(CGSCopySpacesForWindows(cid, ALL_SPACES, &ids)) }) else {
        return Vec::new();
    };
    spaces.iter().map(|n| n.as_i64()).collect()
}

/// The probe the daemon gives the reactor: which of these WindowServer ids are in a
/// Space that is not showing.
pub fn system_probe() -> impl FnMut(&[u32]) -> Vec<u32> + Send {
    move |ids| {
        // SAFETY: no preconditions.
        let cid = unsafe { CGSMainConnectionID() };
        let current = current_spaces(cid);
        if current.is_empty() {
            return Vec::new(); // cannot tell; treat every window as reachable
        }
        foreign_ids(&current, |id| spaces_of_window(cid, id), ids)
    }
}

/// Advice for a user whose Mission Control setting makes an unplug split their windows
/// across native Spaces. `spans_displays` is the `com.apple.spaces` `spans-displays` value:
/// true means "Displays have separate Spaces" is OFF, which is what dowm wants. An absent
/// key is macOS's default, separate Spaces ON.
pub fn separate_spaces_advice(spans_displays: Option<bool>) -> Option<&'static str> {
    match spans_displays {
        Some(true) => None,
        _ => Some(
            "\"Displays have separate Spaces\" is on: unplugging a display makes macOS move its \
             Space onto the remaining display and strand windows there. Turn it off in System \
             Settings > Desktop & Dock > Mission Control, then log out and back in.",
        ),
    }
}

/// The current `spans-displays` setting, if the preference exists.
pub fn spans_displays() -> Option<bool> {
    let defaults = NSUserDefaults::standardUserDefaults();
    let domain = defaults.persistentDomainForName(&NSString::from_str("com.apple.spaces"))?;
    let value = domain.objectForKey(&NSString::from_str("spans-displays"))?;
    let number: &NSNumber = value.downcast_ref()?;
    Some(number.as_bool())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_advice_appears_unless_separate_spaces_are_off() {
        assert!(separate_spaces_advice(Some(true)).is_none(), "the recommended setting");
        assert!(separate_spaces_advice(Some(false)).unwrap().contains("Mission Control"));
        assert!(separate_spaces_advice(None).is_some(), "an absent key is macOS's default: separate Spaces on");
    }

    #[test]
    fn reading_the_setting_does_not_panic() {
        let _ = spans_displays();
    }

    #[test]
    fn only_windows_wholly_outside_the_current_spaces_are_foreign() {
        let current: HashSet<i64> = [6].into_iter().collect();
        let spaces = |id: u32| match id {
            1 => vec![6],     // here
            2 => vec![3],     // another Space
            3 => vec![],      // in no Space (minimized)
            4 => vec![3, 6],  // sticky: also here
            5 => vec![3, 4],  // two other Spaces
            _ => vec![],
        };
        assert_eq!(foreign_ids(&current, spaces, &[1, 2, 3, 4, 5]), vec![2, 5]);
    }

    #[test]
    fn the_system_probe_runs_and_returns_ids_it_was_given() {
        // Real CGS: whatever the Spaces are, the answer is a subset of the question.
        let mut probe = system_probe();
        let ids = [1u32, 2, 3, 99_999_999];
        assert!(probe(&ids).iter().all(|id| ids.contains(id)));
    }
}

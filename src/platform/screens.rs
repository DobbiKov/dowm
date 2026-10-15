//! Display enumeration (main thread only). One `NSScreen` snapshot serves both the
//! geometry and the later lookup of the screen to attach a display link to, so the
//! monitor index can never refer to a different array.

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::NSScreen;

use crate::geometry::{Monitor, Rect};

pub struct Screens {
    pub monitors: Vec<Monitor>,
    /// Height of the primary display: the pivot for Cocoa <-> AX y flips.
    pub primary_height: f64,
    screens: Vec<Retained<NSScreen>>,
}

impl Screens {
    /// The `NSScreen` for `Monitor.id == index` from this same snapshot.
    pub fn screen(&self, index: usize) -> Option<&NSScreen> {
        self.screens.get(index).map(|s| &**s)
    }
}

fn rect(r: objc2_foundation::NSRect) -> Rect {
    Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

/// `NSScreen.screens[0]` is the primary display (the one with the menu bar / origin 0,0).
pub fn read(mtm: MainThreadMarker) -> Screens {
    let screens: Vec<Retained<NSScreen>> = NSScreen::screens(mtm).iter().collect();
    let monitors: Vec<Monitor> = screens
        .iter()
        .enumerate()
        .map(|(i, s)| Monitor { id: i as u32, frame: rect(s.frame()), visible: rect(s.visibleFrame()) })
        .collect();
    let primary_height = monitors.first().map(|m| m.frame.size.h).unwrap_or(0.0);
    Screens { monitors, primary_height, screens }
}

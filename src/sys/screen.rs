use std::cmp::Ordering;
use std::mem::MaybeUninit;
use std::ptr::NonNull;

use cgs::G_CONNECTION;
use objc2::{ClassType, msg_send};
use objc2_app_kit::NSScreen;
use objc2_core_foundation::{CFRetained, CFString, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGDisplayBounds, CGError, CGGetActiveDisplayList, CGMainDisplayID};
use objc2_foundation::{MainThreadMarker, NSNumber, ns_string};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::skylight::{
    CFRelease, CFUUIDCreateFromString, CFUUIDCreateString, CGDisplayCreateUUIDFromDisplayID,
    CGDisplayGetDisplayIDFromUUID, CGSCopyBestManagedDisplayForRect, CGSCopyManagedDisplays,
    CoreDockGetAutoHideEnabled, CoreDockGetOrientationAndPinning,
    SLSCopyActiveMenuBarDisplayIdentifier, SLSGetDisplayMenubarHeight, SLSGetDockRectWithReason,
    SLSGetMenuBarAutohideEnabled,
};
use crate::sys::geometry::CGRectDef;

#[derive(Debug, Clone)]
struct ScreenState {
    screens: Vec<ScreenInfo>,
    converter: CoordinateConverter,
}

pub struct ScreenCache<S: System = Actual> {
    system: S,
    state: Option<ScreenState>,
    pending_generation: u64,
    processed_generation: u64,
    sleeping: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenInfo {
    pub id: DisplayId,
    #[serde(with = "CGRectDef")]
    pub frame: CGRect,
    #[serde(default = "unit_scale")]
    pub backing_scale: f64,
    pub display_uuid: String,
    pub name: Option<String>,
}

fn unit_scale() -> f64 { 1.0 }

impl ScreenInfo {
    pub fn display_uuid_opt(&self) -> Option<&str> {
        if self.display_uuid.is_empty() {
            None
        } else {
            Some(self.display_uuid.as_str())
        }
    }

    pub fn display_uuid_owned(&self) -> Option<String> {
        self.display_uuid_opt().map(|uuid| uuid.to_string())
    }
}

impl ScreenCache<Actual> {
    pub fn new(mtm: MainThreadMarker) -> Self { Self::new_with(Actual { mtm }) }
}

impl<S: System> ScreenCache<S> {
    fn new_with(system: S) -> ScreenCache<S> {
        ScreenCache {
            system,
            state: None,
            pending_generation: 0,
            processed_generation: 0,
            sleeping: false,
        }
    }

    pub fn mark_dirty(&mut self) {
        self.pending_generation = self.pending_generation.wrapping_add(1);
    }

    pub fn mark_sleeping(&mut self, sleeping: bool) {
        self.sleeping = sleeping;
        if !sleeping {
            self.mark_dirty();
        }
    }

    pub fn refresh(&mut self) -> Option<(Vec<ScreenInfo>, CoordinateConverter)> {
        self.refresh_snapshot(false).map(|s| (s.screens, s.converter))
    }

    fn refresh_snapshot(&mut self, force: bool) -> Option<ScreenState> {
        if self.sleeping {
            return self.state.clone();
        }

        let dirty = self.pending_generation != self.processed_generation;
        let should_rebuild = force || self.state.is_none() || dirty;

        if !should_rebuild {
            return self.state.clone();
        }

        let ns_screens = self.system.ns_screens();
        debug!("ns_screens={ns_screens:?}");
        let mut cg_screens = self.system.cg_screens().ok()?;
        debug!("cg_screens={cg_screens:?}");

        if cg_screens.is_empty() {
            let state = ScreenState {
                screens: Vec::new(),
                converter: CoordinateConverter::default(),
            };
            self.state = Some(state.clone());
            self.processed_generation = self.pending_generation;
            return Some(state);
        }

        cg_screens.sort_by(|a, b| {
            let x_order = a.bounds.origin.x.total_cmp(&b.bounds.origin.x);
            if x_order == Ordering::Equal {
                a.bounds.origin.y.total_cmp(&b.bounds.origin.y)
            } else {
                x_order
            }
        });

        let main_id = CGMainDisplayID();
        if let Some(main_screen_idx) = cg_screens.iter().position(|s| s.cg_id.0 == main_id) {
            cg_screens.swap(0, main_screen_idx);
        } else {
            warn!("Could not find main screen. cg_screens={cg_screens:?}");
        }

        let uuid_strings: Vec<String> =
            cg_screens.iter().map(|screen| self.system.display_uuid(screen).to_string()).collect();

        let union_max_y = cg_screens
            .iter()
            .map(|screen| screen.bounds.max().y)
            .fold(f64::NEG_INFINITY, f64::max);
        let converter = CoordinateConverter { screen_height: union_max_y };

        let screens: Vec<ScreenInfo> = cg_screens
            .iter()
            .enumerate()
            .map(|(idx, &CGScreenInfo { cg_id, bounds })| {
                let notch_height = self.system.notch_height(cg_id.as_u32());
                let frame = constrain_display_bounds(cg_id.as_u32(), bounds, notch_height);
                let display_uuid =
                    uuid_strings.get(idx).cloned().filter(|uuid| !uuid.is_empty()).unwrap_or_else(
                        || {
                            warn!("Missing cached UUID for {:?}; using fallback", cg_id);
                            format!("cgdisplay-{}", cg_id.as_u32())
                        },
                    );
                ScreenInfo {
                    id: cg_id,
                    frame,
                    backing_scale: ns_screens
                        .iter()
                        .find(|s| s.cg_id == cg_id)
                        .map_or(1.0, |s| s.backing_scale),
                    display_uuid,
                    name: ns_screens.iter().find(|s| s.cg_id == cg_id).and_then(|s| s.name.clone()),
                }
            })
            .collect();

        self.processed_generation = self.pending_generation;
        self.state = Some(ScreenState { screens, converter });
        self.state.clone()
    }
}

const DOCK_ORIENTATION_TOP: i32 = 1;
const DOCK_ORIENTATION_BOTTOM: i32 = 2;
const DOCK_ORIENTATION_LEFT: i32 = 3;
const DOCK_ORIENTATION_RIGHT: i32 = 4;

fn menu_bar_hidden() -> bool {
    let mut status = 0;
    unsafe { SLSGetMenuBarAutohideEnabled(*G_CONNECTION, &mut status) };
    status != 0
}

pub fn menu_bar_height(did: u32) -> f64 {
    let mut height: u32 = 0;
    unsafe { SLSGetDisplayMenubarHeight(did, &mut height) };
    height as f64
}

fn menu_bar_inset(hidden: bool, height: f64, notch_height: f64) -> f64 {
    if hidden {
        // An auto-hidden menu bar does not reserve space on displays without a notch.
        // Notched built-in displays must retain their safe-area inset.
        notch_height
    } else {
        // macOS reports the menubar height without the topmost usable pixel; add 1 to
        // avoid leaving a dead strip or placing windows under the bar.
        height + 1.0
    }
}

fn dock_hidden() -> bool { unsafe { CoreDockGetAutoHideEnabled() } }

fn dock_orientation() -> i32 {
    let mut orientation = 0;
    let mut pinning = 0;
    unsafe { CoreDockGetOrientationAndPinning(&mut orientation, &mut pinning) };
    orientation
}

fn dock_rect() -> CGRect {
    let mut rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0));
    let mut reason = 0;
    unsafe { SLSGetDockRectWithReason(*G_CONNECTION, &mut rect, &mut reason) };
    rect
}

fn dock_rect_with_reason() -> (CGRect, i32) {
    let mut rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0));
    let mut reason = 0;
    unsafe { SLSGetDockRectWithReason(*G_CONNECTION, &mut rect, &mut reason) };
    (rect, reason)
}

fn dock_display_id() -> Option<u32> {
    unsafe {
        let dock = dock_rect();
        let uuid_ref = CGSCopyBestManagedDisplayForRect(*G_CONNECTION, dock);
        if uuid_ref.is_null() {
            return None;
        }
        let uuid = CFUUIDCreateFromString(std::ptr::null_mut(), uuid_ref);
        if uuid.is_null() {
            CFRelease(uuid_ref as *mut _);
            return None;
        }
        let did = CGDisplayGetDisplayIDFromUUID(uuid);
        CFRelease(uuid as *mut _);
        CFRelease(uuid_ref as *mut _);
        if did == 0 { None } else { Some(did) }
    }
}

fn rects_intersect(a: &CGRect, b: &CGRect) -> bool {
    let ax2 = a.origin.x + a.size.width;
    let ay2 = a.origin.y + a.size.height;
    let bx2 = b.origin.x + b.size.width;
    let by2 = b.origin.y + b.size.height;

    !(ax2 <= b.origin.x || bx2 <= a.origin.x || ay2 <= b.origin.y || by2 <= a.origin.y)
}

fn constrain_display_bounds(did: u32, raw: CGRect, notch_height: f64) -> CGRect {
    let mut frame = raw;

    let menu_bar_inset = menu_bar_inset(menu_bar_hidden(), menu_bar_height(did), notch_height);
    if menu_bar_inset > 0.0 {
        frame.origin.y += menu_bar_inset;
        frame.size.height = (frame.size.height - menu_bar_inset).max(0.0);
    }

    let auto_hide = dock_hidden();
    let (dock, dock_reason) = dock_rect_with_reason();

    let dock_display = dock_display_id();

    let dock_visible = (!auto_hide || dock_reason == 0)
        && dock_display.map(|dock_did| dock_did == did).unwrap_or(false)
        && rects_intersect(&frame, &dock);

    if dock_visible {
        match dock_orientation() {
            DOCK_ORIENTATION_LEFT => {
                frame.origin.x += dock.size.width;
                frame.size.width = (frame.size.width - dock.size.width).max(0.0);
            }
            DOCK_ORIENTATION_RIGHT => {
                frame.size.width = (frame.size.width - dock.size.width).max(0.0);
            }
            DOCK_ORIENTATION_BOTTOM => {
                frame.size.height = (frame.size.height - dock.size.height).max(0.0);
            }
            DOCK_ORIENTATION_TOP => {
                frame.origin.y += dock.size.height;
                frame.size.height = (frame.size.height - dock.size.height).max(0.0);
            }
            _ => {
                if dock.size.width > dock.size.height {
                    frame.origin.y += dock.size.height;
                    frame.size.height = (frame.size.height - dock.size.height).max(0.0);
                } else {
                    frame.origin.x += dock.size.width;
                    frame.size.width = (frame.size.width - dock.size.width).max(0.0);
                }
            }
        }
    }

    frame
}

/// Converts between Quartz and Cocoa coordinate systems.
#[derive(Clone, Copy, Debug)]
pub struct CoordinateConverter {
    /// The y offset of the Cocoa origin in the Quartz coordinate system, and
    /// vice versa. This is the height of the first screen. The origins
    /// are the bottom left and top left of the screen, respectively.
    screen_height: f64,
}

/// Creates a `CoordinateConverter` that returns None for any conversion.
impl Default for CoordinateConverter {
    fn default() -> Self { Self { screen_height: f64::NAN } }
}

impl CoordinateConverter {
    pub fn from_height(height: f64) -> Self { Self { screen_height: height } }

    pub fn from_screen(screen: &NSScreen) -> Option<Self> {
        let screen_id = screen.get_number().ok()?;
        let bounds = CGDisplayBounds(screen_id.as_u32());
        Some(Self::from_height(bounds.origin.y + bounds.size.height))
    }

    pub fn screen_height(&self) -> Option<f64> {
        if self.screen_height.is_nan() {
            None
        } else {
            Some(self.screen_height)
        }
    }

    pub fn convert_point(&self, point: CGPoint) -> Option<CGPoint> {
        if self.screen_height.is_nan() {
            return None;
        }
        Some(CGPoint::new(point.x, self.screen_height - point.y))
    }

    pub fn convert_rect(&self, rect: CGRect) -> Option<CGRect> {
        if self.screen_height.is_nan() {
            return None;
        }
        Some(CGRect::new(
            CGPoint::new(rect.origin.x, self.screen_height - rect.max().y),
            rect.size,
        ))
    }
}

#[allow(private_interfaces)]
pub trait System {
    fn cg_screens(&self) -> Result<Vec<CGScreenInfo>, CGError>;
    fn display_uuid(&self, screen: &CGScreenInfo) -> CFRetained<CFString>;
    fn ns_screens(&self) -> Vec<NSScreenInfo>;
    fn notch_height(&self, _did: u32) -> f64 { 0.0 }
}

#[derive(Debug, Clone)]
struct CGScreenInfo {
    cg_id: DisplayId,
    bounds: CGRect,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct NSScreenInfo {
    frame: CGRect,
    visible_frame: CGRect,
    backing_scale: f64,
    cg_id: DisplayId,
    name: Option<String>,
}

pub struct Actual {
    mtm: MainThreadMarker,
}
#[allow(private_interfaces)]
impl System for Actual {
    fn cg_screens(&self) -> Result<Vec<CGScreenInfo>, CGError> {
        const MAX_SCREENS: usize = 64;
        let mut ids: MaybeUninit<[CGDirectDisplayID; MAX_SCREENS]> = MaybeUninit::uninit();
        let mut count: u32 = 0;
        let ids = unsafe {
            let err = CGGetActiveDisplayList(
                MAX_SCREENS as u32,
                ids.as_mut_ptr() as *mut CGDirectDisplayID,
                &mut count,
            );
            if err != CGError::Success {
                return Err(err);
            }
            std::slice::from_raw_parts(ids.as_ptr() as *const u32, count as usize)
        };
        Ok(ids
            .iter()
            .map(|&cg_id| CGScreenInfo {
                cg_id: DisplayId(cg_id),
                bounds: CGDisplayBounds(cg_id),
            })
            .collect())
    }

    fn display_uuid(&self, screen: &CGScreenInfo) -> CFRetained<CFString> {
        unsafe {
            if let Some(uuid) = NonNull::new(CGDisplayCreateUUIDFromDisplayID(screen.cg_id.0)) {
                let uuid_str = CFUUIDCreateString(std::ptr::null_mut(), uuid.as_ptr());
                CFRelease(uuid.as_ptr());
                if let Some(uuid_str) = NonNull::new(uuid_str) {
                    return CFRetained::from_raw(uuid_str);
                } else {
                    warn!(
                        "CGDisplayCreateUUIDFromDisplayID returned invalid string for {:?}",
                        screen
                    );
                }
            } else {
                warn!(
                    "CGDisplayCreateUUIDFromDisplayID returned null for display {:?}",
                    screen.cg_id
                );
            }
            let managed = CGSCopyBestManagedDisplayForRect(cgs::main_connection(), screen.bounds);
            if let Some(managed) = NonNull::new(managed) {
                CFRetained::from_raw(managed)
            } else {
                warn!(
                    "CGSCopyBestManagedDisplayForRect returned null for display {:?}",
                    screen.cg_id
                );
                CFString::from_str("")
            }
        }
    }

    fn ns_screens(&self) -> Vec<NSScreenInfo> {
        NSScreen::screens(self.mtm)
            .iter()
            .flat_map(|s| {
                let name = s.localizedName().to_string();
                Some(NSScreenInfo {
                    frame: s.frame(),
                    visible_frame: s.visibleFrame(),
                    backing_scale: s.backingScaleFactor(),
                    cg_id: s.get_number().ok()?,
                    name: Some(name),
                })
            })
            .collect()
    }

    fn notch_height(&self, did: u32) -> f64 {
        let screens = NSScreen::screens(self.mtm);
        let builtin = unsafe { super::skylight::CGDisplayIsBuiltin(did) };
        if !builtin {
            return 0.0;
        }

        for screen in screens {
            if let Ok(screen_id) = screen.get_number() {
                if screen_id.as_u32() == did {
                    #[allow(deprecated)]
                    let insets = screen.safeAreaInsets();
                    return insets.top;
                }
            }
        }
        0.0
    }
}

type CGDirectDisplayID = u32;

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DisplayId(CGDirectDisplayID);

impl DisplayId {
    pub const fn new(id: u32) -> Self { DisplayId(id) }

    pub fn as_u32(&self) -> u32 { self.0 }
}

pub trait NSScreenExt {
    fn get_number(&self) -> Result<DisplayId, ()>;
}
impl NSScreenExt for NSScreen {
    fn get_number(&self) -> Result<DisplayId, ()> {
        let desc = self.deviceDescription();
        match desc.objectForKey(ns_string!("NSScreenNumber")) {
            Some(val) if unsafe { msg_send![&*val, isKindOfClass:NSNumber::class() ] } => {
                let number: &NSNumber = unsafe { std::mem::transmute(val) };
                Ok(DisplayId(number.as_u32()))
            }
            val => {
                warn!(
                    "Could not get NSScreenNumber for screen with name {:?}: {:?}",
                    self.localizedName(),
                    val,
                );
                Err(())
            }
        }
    }
}

pub fn active_menu_bar_display_uuid() -> Option<String> {
    Some(
        unsafe {
            CFRetained::<CFString>::from_raw(NonNull::new(SLSCopyActiveMenuBarDisplayIdentifier(
                cgs::main_connection(),
            ))?)
        }
        .to_string(),
    )
}

pub fn set_active_menu_bar_display_uuid(display_uuid: &str) -> bool {
    if display_uuid.is_empty() {
        return false;
    }
    #[cfg(test)]
    {
        TEST_ACTIVE_DISPLAY.with(|display| display.replace(Some(display_uuid.to_string())));
        return true;
    }
    #[cfg(not(test))]
    {
        let uuid = CFString::from_str(display_uuid);
        let ptr = CFRetained::as_ptr(&uuid).as_ptr();
        let result = crate::sys::cg_ok(unsafe {
            super::skylight::SLSSetActiveMenuBarDisplayIdentifier(cgs::main_connection(), ptr, ptr)
        });
        if let Err(error) = result {
            warn!(?error, display_uuid, "Failed to activate menu-bar display");
            return false;
        }
        let active = unsafe {
            NonNull::new(SLSCopyActiveMenuBarDisplayIdentifier(cgs::main_connection()))
                .map(|ptr| CFRetained::<CFString>::from_raw(ptr))
        };
        let confirmed = active.as_deref().is_some_and(|active| active == &*uuid);
        if !confirmed {
            debug!(display_uuid, "Menu-bar display activation was not confirmed");
        }
        confirmed
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_ACTIVE_DISPLAY: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Utilities for querying the current system configuration. For diagnostic purposes only.
#[allow(dead_code)]
pub mod diagnostic {
    use objc2_core_foundation::CFArray;

    use super::*;

    pub fn managed_displays() -> CFRetained<CFArray> {
        unsafe {
            CFRetained::from_raw(NonNull::new_unchecked(CGSCopyManagedDisplays(
                cgs::main_connection(),
            )))
        }
    }

}

#[cfg(test)]
mod test {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use objc2_core_foundation::{CFRetained, CFString, CGPoint, CGRect, CGSize};
    use objc2_core_graphics::CGError;

    use super::{CGScreenInfo, DisplayId, NSScreenInfo, ScreenCache, System};

    #[test]
    fn auto_hidden_menu_bar_does_not_inset_a_display_without_a_notch() {
        assert_eq!(super::menu_bar_inset(true, 24.0, 0.0), 0.0);
    }

    #[test]
    fn auto_hidden_menu_bar_preserves_the_notch_safe_area() {
        assert_eq!(super::menu_bar_inset(true, 37.0, 32.0), 32.0);
    }

    #[test]
    fn visible_menu_bar_reserves_its_reported_height() {
        assert_eq!(super::menu_bar_inset(false, 24.0, 0.0), 25.0);
    }

    struct Stub {
        cg_screens: Vec<CGScreenInfo>,
        ns_screens: Vec<NSScreenInfo>,
    }
    impl System for Stub {
        fn cg_screens(&self) -> Result<Vec<CGScreenInfo>, CGError> { Ok(self.cg_screens.clone()) }

        fn display_uuid(&self, _screen: &CGScreenInfo) -> CFRetained<CFString> {
            CFString::from_str("stub")
        }

        fn ns_screens(&self) -> Vec<NSScreenInfo> { self.ns_screens.clone() }

        fn notch_height(&self, _did: u32) -> f64 { 0.0 }
    }

    struct SequenceSystem {
        cg_screens: RefCell<VecDeque<Vec<CGScreenInfo>>>,
        ns_screens: RefCell<VecDeque<Vec<NSScreenInfo>>>,
        uuids: RefCell<VecDeque<CFRetained<CFString>>>,
    }

    impl SequenceSystem {
        fn new(
            cg_screens: Vec<Vec<CGScreenInfo>>,
            ns_screens: Vec<Vec<NSScreenInfo>>,
            uuids: Vec<CFRetained<CFString>>,
        ) -> Self {
            Self {
                cg_screens: RefCell::new(VecDeque::from(cg_screens)),
                ns_screens: RefCell::new(VecDeque::from(ns_screens)),
                uuids: RefCell::new(VecDeque::from(uuids)),
            }
        }
    }

    impl System for SequenceSystem {
        fn cg_screens(&self) -> Result<Vec<CGScreenInfo>, CGError> {
            Ok(self.cg_screens.borrow_mut().pop_front().unwrap_or_default())
        }

        fn display_uuid(&self, _screen: &CGScreenInfo) -> CFRetained<CFString> {
            self.uuids
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| CFString::from_str("missing-uuid"))
        }

        fn ns_screens(&self) -> Vec<NSScreenInfo> {
            self.ns_screens.borrow_mut().pop_front().unwrap_or_default()
        }

        fn notch_height(&self, _did: u32) -> f64 { 0.0 }
    }

    #[test]
    fn it_calculates_the_visible_frame() {
        let stub = Stub {
            cg_screens: vec![
                CGScreenInfo {
                    cg_id: DisplayId(1),
                    bounds: CGRect::new(CGPoint::new(3840.0, 1080.0), CGSize::new(1512.0, 982.0)),
                },
                CGScreenInfo {
                    cg_id: DisplayId(3),
                    bounds: CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(3840.0, 2160.0)),
                },
            ],
            ns_screens: vec![
                NSScreenInfo {
                    cg_id: DisplayId(3),
                    frame: CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(3840.0, 2160.0)),
                    backing_scale: 1.0,
                    visible_frame: CGRect::new(
                        CGPoint::new(0.0, 76.0),
                        CGSize::new(3840.0, 2059.0),
                    ),
                    name: None,
                },
                NSScreenInfo {
                    cg_id: DisplayId(1),
                    frame: CGRect::new(CGPoint::new(3840.0, 98.0), CGSize::new(1512.0, 982.0)),
                    backing_scale: 2.0,
                    visible_frame: CGRect::new(
                        CGPoint::new(3840.0, 98.0),
                        CGSize::new(1512.0, 950.0),
                    ),
                    name: None,
                },
            ],
        };
        let mut sc = ScreenCache::new_with(stub);
        let (screens, _) = sc.refresh().unwrap();

        let secondary = screens.iter().find(|screen| screen.id == DisplayId(1)).unwrap();
        assert_eq!(secondary.backing_scale, 2.0);
        assert_eq!(
            secondary.frame,
            super::constrain_display_bounds(
                1,
                CGRect::new(CGPoint::new(3840.0, 1080.0), CGSize::new(1512.0, 982.0)),
                0.0,
            )
        );

        let primary = screens.iter().find(|screen| screen.id == DisplayId(3)).unwrap();
        assert_eq!(
            primary.frame,
            super::constrain_display_bounds(
                3,
                CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(3840.0, 2160.0)),
                0.0,
            )
        );
    }

    #[test]
    fn empty_display_list_clears_screens_and_converter() {
        let bounds = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1440.0, 900.0));
        let visible_frame = CGRect::new(CGPoint::new(0.0, 22.0), CGSize::new(1440.0, 878.0));

        let system = SequenceSystem::new(
            vec![vec![CGScreenInfo { cg_id: DisplayId(1), bounds }], vec![]],
            vec![
                vec![NSScreenInfo {
                    cg_id: DisplayId(1),
                    frame: bounds,
                    visible_frame,
                    backing_scale: 1.0,
                    name: None,
                }],
                vec![],
            ],
            vec![CFString::from_str("uuid-1")],
        );

        let mut cache = ScreenCache::new_with(system);

        let (screens, _) = cache.refresh().unwrap();
        assert_eq!(screens.len(), 1);
        assert_eq!(screens[0].display_uuid, "uuid-1");

        cache.mark_dirty();
        let (screens, converter) = cache.refresh().unwrap();
        assert!(screens.is_empty());
        assert!(converter.convert_point(CGPoint::new(0.0, 0.0)).is_none());
    }

}

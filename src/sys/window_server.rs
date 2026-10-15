#[cfg(test)]
use std::cell::RefCell;
use std::ffi::c_int;
use std::num::NonZeroU32;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cgs::G_CONNECTION;
use objc2_app_kit::NSWindowLevel;
use objc2_application_services::AXError;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint, CGRect,
    CGSize, Type, kCFBooleanTrue,
};
use objc2_core_graphics::{
    CGError, CGWindowID, CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID,
    kCGWindowLayer, kCGWindowName, kCGWindowOwnerName,
};
use serde::{Deserialize, Serialize};

use super::geometry::{CGRectDef, CGSizeDef};
use crate::actor::app::WindowId;
#[cfg(test)]
use crate::common::collections::HashMap;
use crate::sys::app::pid_t;
use crate::sys::axuielement::{AXUIElement, Error as AxError};
use crate::sys::cg_ok;
#[cfg(not(test))]
use crate::sys::geometry::CGRectExt;
use crate::sys::process::ProcessSerialNumber;
use crate::sys::screen::ScreenInfo;
use crate::sys::skylight::*;

static LAST_WINDOWSERVER_ACTIVITY_US: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
thread_local! {
    static TEST_WINDOW_ORDERED_IN_OVERRIDE: RefCell<HashMap<u32, bool>> = RefCell::new(HashMap::default());
}

pub const WINDOWSERVER_QUIET_US: u64 = 350_000;
#[cfg_attr(test, allow(dead_code))]
const EFFECTIVELY_INVISIBLE_WINDOW_ALPHA: f32 = 0.01;

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WindowServerId(pub CGWindowID);

impl WindowServerId {
    #[inline]
    pub fn new(id: CGWindowID) -> Self { Self(id) }

    #[inline]
    pub fn as_u32(self) -> u32 { self.0 }

    #[inline]
    pub fn as_nonzero(self) -> Option<NonZeroU32> { NonZeroU32::new(self.0) }
}

impl From<WindowServerId> for u32 {
    #[inline]
    fn from(id: WindowServerId) -> Self { id.0 }
}

impl TryFrom<&AXUIElement> for WindowServerId {
    type Error = AxError;

    fn try_from(element: &AXUIElement) -> Result<Self, Self::Error> {
        let mut id = 0;
        let res = unsafe { _AXUIElementGetWindow(element.raw_ptr().as_ptr(), &mut id) };
        if res != AXError::Success {
            return Err(AxError::Ax(res));
        }
        if id == 0 {
            return Err(AxError::NotFound);
        }
        Ok(Self(id))
    }
}

impl From<WindowId> for WindowServerId {
    fn from(id: WindowId) -> Self { Self(id.idx.into()) }
}

#[inline]
fn now_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros() as u64
}

pub fn note_windowserver_activity(wsid: u32) {
    LAST_WINDOWSERVER_ACTIVITY_US.store(now_us(), Ordering::SeqCst);
    // Keep this trace low-cost; it's only used to stabilize display churn.
    tracing::trace!(wsid, "windowserver activity");
}

pub fn windowserver_quiet_for_us(quiet_us: u64) -> bool {
    let last = LAST_WINDOWSERVER_ACTIVITY_US.load(Ordering::SeqCst);
    if last == 0 {
        return true;
    }
    now_us().saturating_sub(last) >= quiet_us
}

#[inline]
fn cf_array_from_ids(ids: &[WindowServerId]) -> CFRetained<CFArray<CFNumber>> {
    if let [id] = ids {
        let number = CFNumber::new_i64(id.as_u32() as i64);
        return CFArray::from_retained_objects(std::slice::from_ref(&number));
    }
    let nums: Vec<CFRetained<CFNumber>> =
        ids.iter().map(|w| CFNumber::new_i64(w.as_u32() as i64)).collect();
    CFArray::from_retained_objects(&nums)
}

pub struct WindowIterator {
    iter: *mut CFType,
}

impl WindowIterator {
    pub fn new(ids: &[WindowServerId]) -> Option<Self> {
        if ids.is_empty() {
            return None;
        }
        let cf_numbers = cf_array_from_ids(ids);
        Self::new_from_cfarray(CFRetained::as_ptr(&cf_numbers).as_ptr(), 0)
    }

    /// `flags` controls optional result decoding; bit 0 requests window titles.
    fn new_from_cfarray(cf_numbers: *mut CFArray<CFNumber>, flags: c_int) -> Option<Self> {
        let query = unsafe { SLSWindowQueryWindows(*G_CONNECTION, cf_numbers, flags) };
        if query.is_null() {
            return None;
        }
        let iter = unsafe { SLSWindowQueryResultCopyWindows(query) };
        unsafe { CFRelease(query) };
        if iter.is_null() {
            return None;
        }
        Self::from_owned_iterator(iter)
    }

    fn from_owned_iterator(iter: *mut CFType) -> Option<Self> {
        if iter.is_null() {
            return None;
        }
        let iterator = Self { iter };
        // Settle the SLS iterator before its first Advance. Some reply shapes
        // otherwise produce a valid iterator that initially appears empty.
        let _ = iterator.count();
        Some(iterator)
    }

    #[inline]
    pub fn count(&self) -> i32 { unsafe { SLSWindowIteratorGetCount(self.iter) } }

    #[inline]
    pub fn advance<'a>(&'a self) -> Option<&'a Self> {
        if unsafe { SLSWindowIteratorAdvance(self.iter) } {
            return Some(self);
        }

        None
    }

    #[inline]
    pub fn window_id(&self) -> u32 { unsafe { SLSWindowIteratorGetWindowID(self.iter) } }

    #[inline]
    pub fn level(&self) -> i32 { unsafe { SLSWindowIteratorGetLevel(self.iter) } }

    #[inline]
    pub fn pid(&self) -> i32 { unsafe { SLSWindowIteratorGetPID(self.iter) } }

    #[inline]
    pub fn parent_id(&self) -> u32 { unsafe { SLSWindowIteratorGetParentID(self.iter) } }

    #[inline]
    pub fn bounds(&self) -> CGRect { unsafe { SLSWindowIteratorGetBounds(self.iter) } }

    #[inline]
    pub fn alpha(&self) -> f32 { unsafe { SLSWindowIteratorGetAlpha(self.iter) } }

    #[inline]
    #[allow(dead_code)]
    pub fn tags(&self) -> u64 { unsafe { SLSWindowIteratorGetTags(self.iter) } }

    #[inline]
    #[allow(dead_code)]
    pub fn attributes(&self) -> u64 { unsafe { SLSWindowIteratorGetAttributes(self.iter) } }

    #[inline]
    pub fn constraints(&self) -> (CGSize, CGSize) {
        let mut min = CGSize::ZERO;
        let mut max = CGSize::ZERO;
        let mut cur = CGSize::ZERO;
        unsafe { SLSWindowIteratorGetConstraints(self.iter, &mut min, &mut max, &mut cur) };

        if min.width == 0.0 && min.height == 0.0 && max.width == 0.0 && max.height == 0.0 {
            unsafe {
                SLSPackagesGetWindowConstraints(
                    *G_CONNECTION,
                    self.window_id(),
                    &mut min,
                    &mut max,
                    &mut cur,
                )
            };
        }

        (min, max)
    }
}

pub fn window_title(id: WindowServerId) -> Option<(i32, String)> {
    let ids = cf_array_from_ids(&[id]);
    let query = WindowIterator::new_from_cfarray(CFRetained::as_ptr(&ids).as_ptr(), 1)?;
    query.advance()?;
    if query.window_id() != id.as_u32() {
        return None;
    }
    let title = NonNull::new(unsafe { SLSWindowIteratorCopyTitle(query.iter) })?;
    Some((query.pid(), unsafe { CFRetained::from_raw(title) }.to_string()))
}

impl Drop for WindowIterator {
    fn drop(&mut self) { unsafe { CFRelease(self.iter) } }
}

#[derive(Debug, Clone, Serialize, Deserialize, Copy)]
#[allow(unused)]
pub struct WindowServerInfo {
    pub id: WindowServerId,
    pub pid: pid_t,
    pub layer: i32,
    #[serde(with = "CGRectDef")]
    pub frame: CGRect,
    #[serde(with = "CGSizeDef")]
    pub min_frame: CGSize,
    #[serde(with = "CGSizeDef")]
    pub max_frame: CGSize,
}

/// Returns whether Mission Control's Dock-owned layer-18 overlay is still
/// visible in the global on-screen window list.
///
/// This intentionally uses the global CG on-screen snapshot because the signal
/// we want is the presence of the Dock's Mission Control UI itself.
pub fn mission_control_dock_overlay_visible() -> bool {
    #[cfg(test)]
    if let Some(override_value) =
        TEST_MISSION_CONTROL_DOCK_OVERLAY_VISIBLE.with(|value| *value.borrow())
    {
        return override_value;
    }

    const MISSION_CONTROL_DOCK_LAYER: i64 = 18;

    get_visible_windows_raw::<CFDictionary<CFString, CFType>>()
        .iter()
        .any(|window| {
            if window.get(unsafe { kCGWindowName }).is_some() {
                return false;
            }

            let Some(owner_name) = get_string(&window, unsafe { kCGWindowOwnerName }) else {
                return false;
            };
            if owner_name != "Dock" {
                return false;
            }

            get_num(&window, unsafe { kCGWindowLayer }) == Some(MISSION_CONTROL_DOCK_LAYER)
        })
}

pub fn window_parent(id: WindowServerId) -> Option<WindowServerId> {
    let query = WindowIterator::new(&[id])?;
    if query.count() == 1 {
        let p = query.advance()?.parent_id();
        (p != 0).then(|| WindowServerId::new(p))
    } else {
        None
    }
}

pub fn window_ordered_in(id: WindowServerId) -> Option<bool> {
    #[cfg(test)]
    if let Some(ordered) = TEST_WINDOW_ORDERED_IN_OVERRIDE
        .with(|override_ordered| override_ordered.borrow().get(&id.as_u32()).copied())
    {
        return Some(ordered);
    }

    let mut ordered: u8 = 0;
    if let Ok(_) = cg_ok(unsafe { SLSWindowIsOrderedIn(*G_CONNECTION, id.as_u32(), &mut ordered) })
    {
        return Some(ordered != 0);
    }

    None
}

pub fn window_is_ordered_in(id: WindowServerId) -> bool { window_ordered_in(id).unwrap_or(false) }

fn get_windows_raw<T: Type>(
    options: CGWindowListOption,
    relative_to_window: CGWindowID,
) -> CFRetained<CFArray<T>> {
    unsafe {
        // TODO: cgwindowlistcopywindowinfo does not appear to order windows properly
        // SAFETY: this will almost always return (pre objc2 was not a result and just a cfarray)
        if let Some(windows) = CGWindowListCopyWindowInfo(options, relative_to_window) {
            CFRetained::cast_unchecked(windows)
        } else {
            CFArray::empty()
        }
    }
}

fn get_visible_windows_raw<T: Type>() -> CFRetained<CFArray<T>> {
    get_windows_raw(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        kCGNullWindowID,
    )
}

#[cfg(test)]
pub fn get_windows(ids: &[WindowServerId]) -> Vec<WindowServerInfo> {
    ids.iter()
        .map(|&id| WindowServerInfo {
            id,
            pid: 1234,
            layer: 0,
            frame: CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(800.0, 600.0)),
            min_frame: CGSize::ZERO,
            max_frame: CGSize::ZERO,
        })
        .collect()
}

#[cfg(not(test))]
pub fn get_windows(ids: &[WindowServerId]) -> Vec<WindowServerInfo> {
    let Some(query) = WindowIterator::new(ids) else {
        return Vec::new();
    };

    let mut out = Vec::with_capacity(ids.len());
    while query.advance().is_some() {
        if let Some(info) = window_info_from_query(&query) {
            out.push(info);
        }
    }
    out
}

pub fn get_window(id: WindowServerId) -> Option<WindowServerInfo> {
    #[cfg(test)]
    {
        return get_windows(&[id]).into_iter().next();
    }

    #[cfg(not(test))]
    {
        let query = WindowIterator::new(&[id])?;
        if query.count() != 1 || query.advance().is_none() {
            return None;
        }
        return window_info_from_query(&query);
    }
}

fn get_num(dict: &CFDictionary<CFString, CFType>, key: &'static CFString) -> Option<i64> {
    dict.get(key)?.downcast::<CFNumber>().ok()?.as_i64()
}

fn get_string(dict: &CFDictionary<CFString, CFType>, key: &'static CFString) -> Option<String> {
    Some(dict.get(key)?.downcast::<CFString>().ok()?.to_string())
}

#[cfg(not(test))]
pub fn focus_desktop_window(screen: &ScreenInfo) -> bool {
    let Some(display_uuid) = screen.display_uuid_opt() else {
        return false;
    };
    let uuid = CFString::from_str(display_uuid);
    let displays = CFArray::from_objects(&[&*uuid]);
    let Some(windows) = NonNull::new(unsafe {
        SLSManagedDisplaysCopyRoleWindows(*G_CONNECTION, CFRetained::as_ptr(&displays).as_ptr(), 1)
    }) else {
        return false;
    };
    let windows = unsafe { CFRetained::from_raw(windows) };
    windows.iter().any(|number| {
        let Some(id) = number.as_i64().and_then(|id| u32::try_from(id).ok()) else {
            return false;
        };
        let wsid = WindowServerId::new(id);
        let Some(info) = get_window(wsid) else {
            return false;
        };
        info.layer < 0
            && screen.frame.contains(info.frame.mid())
            && make_key_window(info.pid, wsid).is_ok()
    })
}

#[cfg(test)]
pub fn focus_desktop_window(_screen: &ScreenInfo) -> bool { false }

#[cfg_attr(test, allow(dead_code))]
fn window_is_effectively_invisible(alpha: f32, layer: i32) -> bool {
    layer == 0 && alpha <= EFFECTIVELY_INVISIBLE_WINDOW_ALPHA
}

#[cfg_attr(test, allow(dead_code))]
fn window_info_from_query(query: &WindowIterator) -> Option<WindowServerInfo> {
    let layer = query.level();
    if window_is_effectively_invisible(query.alpha(), layer) {
        return None;
    }
    let (min_frame, max_frame) = query.constraints();
    Some(WindowServerInfo {
        id: WindowServerId::new(query.window_id()),
        pid: query.pid() as i32,
        layer,
        frame: query.bounds(),
        min_frame,
        max_frame,
    })
}

#[cfg(test)]
thread_local! {
    static TEST_MISSION_CONTROL_DOCK_OVERLAY_VISIBLE: RefCell<Option<bool>> = const { RefCell::new(None) };
}

/// Find the topmost window at `point`, or the next window below
/// `below_window_id` when given. Returns `(window_id, owner_connection_id)`,
/// or `None` when no window is found.
fn find_window_at_point(point: &mut CGPoint, below_window_id: Option<u32>) -> Option<(u32, i32)> {
    let mut window_point = CGPoint { x: 0.0, y: 0.0 };
    let (mut wid, mut wcid) = (0u32, 0i32);

    let (start_id, direction) = match below_window_id {
        Some(id) => (id as i32, -1),
        None => (0, 1),
    };

    unsafe {
        SLSFindWindowAndOwner(
            *G_CONNECTION,
            start_id,
            direction,
            0,
            point,
            &mut window_point,
            &mut wid,
            &mut wcid,
        );
    }

    (wid != 0).then_some((wid, wcid))
}

fn is_own_window(cid: i32) -> bool { *G_CONNECTION == cid }

/// Settings is Rift's only document window; its sheets are modal windows.
/// Floating surfaces (menus/popovers) and compositor overlays are not targets.
fn is_ignored_own_window(wid: u32, cid: i32) -> bool {
    if !is_own_window(cid) {
        return false;
    }
    let query = WindowIterator::new(&[WindowServerId::new(wid)]);
    let tags = query
        .as_ref()
        .and_then(|q| q.advance())
        .map(|w| SLSWindowTags::from_bits_retain(w.tags()));
    !tags.is_some_and(|tags| tags.intersects(SLSWindowTags::DOCUMENT | SLSWindowTags::MODAL))
}

pub fn get_window_at_point(mut point: CGPoint) -> Option<WindowServerId> {
    let (mut wid, mut cid) = find_window_at_point(&mut point, None)?;
    while is_ignored_own_window(wid, cid) {
        (wid, cid) = find_window_at_point(&mut point, Some(wid))?;
    }
    Some(WindowServerId(wid))
}

/// Returns `true` if an application window at normal level or above
/// occludes the given screen point.
///
/// Walks down the window stack at `point`, skipping Rift-owned surfaces except
/// Settings and its sheets. Desktop/wallpaper windows sit well below
/// `NSNormalWindowLevel` and are not considered occluders.
pub fn is_point_occluded_by_external_window(mut point: CGPoint) -> bool {
    use objc2_app_kit::NSNormalWindowLevel;

    let mut hit = find_window_at_point(&mut point, None);

    // Skip other Rift surfaces without looking through Settings or its sheets.
    while let Some((wid, cid)) = hit {
        if !is_ignored_own_window(wid, cid) {
            let level = window_level(wid).unwrap_or(NSWindowLevel::MIN);
            return level >= NSNormalWindowLevel;
        }
        hit = find_window_at_point(&mut point, Some(wid));
    }

    false
}

pub fn current_cursor_location() -> Result<CGPoint, CGError> {
    let mut point = CGPoint::new(0.0, 0.0);
    cg_ok(unsafe { SLSGetCurrentCursorLocation(*G_CONNECTION, &mut point) })?;
    Ok(point)
}

pub fn window_under_cursor() -> Option<WindowServerId> {
    let point = current_cursor_location().ok()?;
    get_window_at_point(point)
}

#[cfg(test)]
pub fn window_level(_wid: u32) -> Option<NSWindowLevel> { Some(0) }

#[cfg(not(test))]
pub fn window_level(wid: u32) -> Option<NSWindowLevel> {
    let query = WindowIterator::new(&[WindowServerId::new(wid)])?;
    Some(query.advance()?.level() as NSWindowLevel)
}

/// Returns the typed Skylight tags exposed by a window-query iterator.
fn iterator_window_tags(iterator: *mut CFType) -> SLSWindowTags {
    SLSWindowTags::from_bits_retain(unsafe { SLSWindowIteratorGetTags(iterator) })
}

/// Returns whether the tags describe a document or floating app window.
fn tags_match_app_window_role(tags: SLSWindowTags) -> bool {
    tags.contains(SLSWindowTags::DOCUMENT) || tags.contains(SLSWindowTags::FLOATING)
}

/// Returns whether the iterator points at a top-level application window.
fn iterator_window_suitable(iterator: *mut CFType) -> bool {
    let tags = iterator_window_tags(iterator);
    let parent_wid = unsafe { SLSWindowIteratorGetParentID(iterator) };

    // Previous Rust filter also required attribute/high-bit hints plus
    // ATTACHED, IGNORES_CYCLE, and DOCUMENT or (FLOATING && MODAL).
    parent_wid == 0 && tags_match_app_window_role(tags)
}

#[cfg(test)]
pub fn set_window_ordered_in_override(id: WindowServerId, ordered: Option<bool>) {
    TEST_WINDOW_ORDERED_IN_OVERRIDE.with(|override_ordered| {
        let mut override_ordered = override_ordered.borrow_mut();
        if let Some(ordered) = ordered {
            override_ordered.insert(id.as_u32(), ordered);
        } else {
            override_ordered.remove(&id.as_u32());
        }
    });
}

pub fn app_window_suitability(id: WindowServerId) -> Option<bool> {
    let query = WindowIterator::new(&[id])?;

    if query.count() > 0 && query.advance().is_some() {
        Some(iterator_window_suitable(query.iter))
    } else {
        Some(false)
    }
}

pub fn app_window_suitable(id: WindowServerId) -> bool {
    app_window_suitability(id).unwrap_or(false)
}

// credit: https://github.com/Hammerspoon/hammerspoon/issues/370#issuecomment-545545468
pub fn make_key_window(pid: pid_t, wsid: WindowServerId) -> Result<(), CGError> {
    #[allow(non_upper_case_globals)]
    const kCPSUserGenerated: u32 = 0x200;

    let mut event1 = [0u8; 0x100];
    event1[0x04] = 0xf8;
    event1[0x08] = 0x01;
    event1[0x3a] = 0x10;
    event1[0x3c..0x40].copy_from_slice(&wsid.0.to_le_bytes());
    event1[0x20..0x30].fill(0xff);

    let mut event2 = event1;
    event2[0x08] = 0x02;

    let psn = ProcessSerialNumber::for_pid(pid)?;

    unsafe {
        cg_ok(_SLPSSetFrontProcessWithOptions(&psn, wsid.0, kCPSUserGenerated))?;
        cg_ok(SLPSPostEventRecordTo(&psn, event1.as_ptr()))?;
        cg_ok(SLPSPostEventRecordTo(&psn, event2.as_ptr()))?;
    }
    Ok(())
}

pub fn allow_hide_mouse() -> Result<(), CGError> {
    let cid = cgs::main_connection();
    let property = CFString::from_str("SetsCursorInBackground");
    let value = CFBoolean::retain(unsafe { kCFBooleanTrue.unwrap_unchecked() });

    cg_ok(unsafe {
        CGSSetConnectionProperty(
            cid,
            cid,
            CFRetained::<CFString>::as_ptr(&property).as_ptr(),
            CFRetained::<CFBoolean>::as_ptr(&value).as_ptr() as *mut CFType,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::WindowServerId;

    #[test]
    fn zero_window_server_id_is_not_a_window_id() {
        assert!(WindowServerId::new(0).as_nonzero().is_none());
        assert_eq!(WindowServerId::new(42).as_nonzero().map(|id| id.get()), Some(42));
    }
}

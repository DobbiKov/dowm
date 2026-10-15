//! Minimal Accessibility wrapper. Adapted from acsandmann/rift `src/sys/axuielement.rs`
//! and `src/sys/enhanced_ui.rs`, Apache-2.0.
//!
//! Every fallible call surfaces the `AXError` code in its error string.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2_application_services::{AXError, AXIsProcessTrusted, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{CFArray, CFBoolean, CFRetained, CFString, CFType, CGPoint, CGSize};

use crate::geometry::{Point, Size};

pub fn is_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// An AX element handle.
#[derive(Clone)]
pub struct AxElement(CFRetained<AXUIElement>);

// SAFETY: `AXUIElementRef` is an immutable, reference-counted CF object. CF retain/release
// is atomic, and the AX client API (copy/set attribute value) may be called from any
// thread; each call is an independent IPC round trip with no shared mutable state on our
// side. We only move elements to the single writer thread.
unsafe impl Send for AxElement {}

/// Per-element AX timeout so a hung app cannot block the writer forever.
const MESSAGING_TIMEOUT_SECS: f32 = 1.0;

thread_local! {
    // Immutable attribute name reused for every write on this thread.
    static AX_POSITION: CFRetained<CFString> = CFString::from_static_str("AXPosition");
}

fn err(what: &str, e: AXError) -> String {
    format!("{what}: AX error {e:?} ({})", e.0)
}

impl AxElement {
    pub fn application(pid: i32) -> Self {
        let el = unsafe { AXUIElement::new_application(pid) };
        unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
        Self(el)
    }

    fn copy_attr(&self, name: &'static str) -> Result<Option<CFRetained<CFType>>, AXError> {
        let attr = CFString::from_static_str(name);
        let mut value: *const CFType = ptr::null();
        let status = unsafe {
            self.0.copy_attribute_value(&attr, NonNull::new((&mut value) as *mut *const CFType).expect("local"))
        };
        match status {
            AXError::Success => Ok(NonNull::new(value as *mut CFType).map(|p| unsafe { CFRetained::from_raw(p) })),
            AXError::NoValue => Ok(None),
            e => Err(e),
        }
    }

    pub fn windows(&self) -> Result<Vec<AxElement>, String> {
        let Some(v) = self.copy_attr("AXWindows").map_err(|e| err("read AXWindows", e))? else {
            return Ok(Vec::new());
        };
        let arr = v.downcast::<CFArray>().map_err(|_| "AXWindows is not an array".to_string())?;
        let arr = unsafe { CFRetained::cast_unchecked::<CFArray<CFType>>(arr) };
        let mut out = Vec::with_capacity(arr.len());
        for item in arr.iter() {
            let el = item.downcast::<AXUIElement>().map_err(|_| "AXWindows entry is not an AXUIElement".to_string())?;
            unsafe { el.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
            out.push(AxElement(el));
        }
        Ok(out)
    }

    fn string_attr(&self, name: &'static str) -> Result<Option<String>, String> {
        match self.copy_attr(name).map_err(|e| err(&format!("read {name}"), e))? {
            None => Ok(None),
            Some(v) => v
                .downcast::<CFString>()
                .map(|s| Some(s.to_string()))
                .map_err(|_| format!("{name} is not a string")),
        }
    }

    pub fn is_standard_window(&self) -> Result<bool, String> {
        Ok(self.string_attr("AXRole")?.as_deref() == Some("AXWindow")
            && self.string_attr("AXSubrole")?.as_deref() == Some("AXStandardWindow"))
    }

    /// Best-effort title for log output only.
    pub fn title(&self) -> String {
        match self.string_attr("AXTitle") {
            Ok(t) => t.unwrap_or_default(),
            Err(e) => format!("<unreadable: {e}>"),
        }
    }

    fn ax_value(&self, name: &'static str) -> Result<CFRetained<AXValue>, String> {
        self.copy_attr(name)
            .map_err(|e| err(&format!("read {name}"), e))?
            .ok_or_else(|| format!("{name} has no value"))?
            .downcast::<AXValue>()
            .map_err(|_| format!("{name} is not an AXValue"))
    }

    pub fn position(&self) -> Result<Point, String> {
        let v = self.ax_value("AXPosition")?;
        let mut p = CGPoint { x: 0.0, y: 0.0 };
        let dst = NonNull::new((&mut p as *mut CGPoint).cast::<c_void>()).expect("local");
        if unsafe { v.value(AXValueType::CGPoint, dst) } { Ok(Point::new(p.x, p.y)) } else { Err("AXPosition is not a CGPoint".into()) }
    }

    pub fn size(&self) -> Result<Size, String> {
        let v = self.ax_value("AXSize")?;
        let mut s = CGSize { width: 0.0, height: 0.0 };
        let dst = NonNull::new((&mut s as *mut CGSize).cast::<c_void>()).expect("local");
        if unsafe { v.value(AXValueType::CGSize, dst) } { Ok(Size::new(s.width, s.height)) } else { Err("AXSize is not a CGSize".into()) }
    }

    /// The WindowServer id (`CGWindowID`) of this window, via `_AXUIElementGetWindow`.
    pub fn window_id(&self) -> Option<u32> {
        let mut id: u32 = 0;
        let elem = CFRetained::as_ptr(&self.0).as_ptr();
        // SAFETY: `elem` points at the live AXUIElement `self.0` retains for the whole call;
        // the private function only reads it. `id` is a valid out-pointer to a `CGWindowID`
        // (u32) that lives across the call. A non-success status leaves `id` unused.
        let status = unsafe { crate::sys::skylight::_AXUIElementGetWindow(elem, &mut id) };
        (status == AXError::Success && id != 0).then_some(id)
    }

    /// Whether the window is minimized (`false` if the app does not say).
    pub fn is_minimized(&self) -> bool {
        self.bool_attr("AXMinimized").unwrap_or(false)
    }

    /// Position and size together; `None` if either cannot be read.
    pub fn frame(&self) -> Option<crate::geometry::Rect> {
        let (p, s) = (self.position().ok()?, self.size().ok()?);
        Some(crate::geometry::Rect { origin: p, size: s })
    }

    /// Writes `AXPosition` only; size is never touched.
    pub fn set_position(&self, p: Point) -> Result<(), String> {
        let mut pt = CGPoint { x: p.x, y: p.y };
        let value = unsafe {
            AXValue::new(AXValueType::CGPoint, NonNull::new((&mut pt as *mut CGPoint).cast::<c_void>()).expect("local"))
        }
        .ok_or("AXValueCreate failed")?;
        let status = AX_POSITION.with(|attr| unsafe { self.0.set_attribute_value(attr, &value) });
        if status == AXError::Success { Ok(()) } else { Err(err("write AXPosition", status)) }
    }

    fn bool_attr(&self, name: &'static str) -> Result<bool, AXError> {
        let v = self.copy_attr(name)?.ok_or(AXError::NoValue)?;
        Ok(v.downcast::<CFBoolean>().map_err(|_| AXError::Failure)?.value())
    }

    fn set_bool_attr(&self, name: &'static str, value: bool) -> Result<(), String> {
        let attr = CFString::from_static_str(name);
        let status = unsafe { self.0.set_attribute_value(&attr, CFBoolean::new(value).as_ref()) };
        if status == AXError::Success { Ok(()) } else { Err(err(&format!("write {name}"), status)) }
    }

    /// If `AXEnhancedUserInterface` is on, switch it off and return `Ok(true)` (the caller
    /// must restore it). Apps that do not expose the attribute yield `Ok(false)`.
    pub fn disable_enhanced_ui(&self) -> Result<bool, String> {
        match self.bool_attr("AXEnhancedUserInterface") {
            Ok(true) => self.set_bool_attr("AXEnhancedUserInterface", false).map(|()| true),
            Ok(false) => Ok(false),
            Err(AXError::AttributeUnsupported | AXError::NoValue) => Ok(false),
            Err(e) => Err(err("read AXEnhancedUserInterface", e)),
        }
    }

    pub fn restore_enhanced_ui(&self) -> Result<(), String> {
        self.set_bool_attr("AXEnhancedUserInterface", true)
    }
}

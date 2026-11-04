use super::*;
use core_foundation::{
    base::TCFType,
    runloop::{CFRunLoop, kCFRunLoopCommonModes, kCFRunLoopDefaultMode},
    string::CFString,
};
use core_foundation_sys::base::{CFHash, CFRelease, CFTypeRef};
use core_graphics::{
    event::{
        CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions,
        CGEventTapPlacement, CGEventType, CallbackResult, EventField,
    },
    geometry::{CGPoint, CGSize},
};
use objc2::{
    msg_send,
    rc::{Retained, autoreleasepool},
    runtime::AnyObject,
};
use objc2_app_kit::{NSPasteboard, NSWorkspace};
use objc2_foundation::NSString;
use std::{ffi::c_void, ptr};

type Ax = *const c_void;
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> std::ffi::c_uchar;
    fn AXUIElementCreateSystemWide() -> Ax;
    fn AXUIElementCreateApplication(pid: i32) -> Ax;
    fn AXUIElementCopyAttributeValue(
        element: Ax,
        attr: *const c_void,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementCopyElementAtPosition(element: Ax, x: f32, y: f32, value: *mut Ax) -> i32;
    fn AXUIElementSetMessagingTimeout(element: Ax, timeout: f32) -> i32;
    fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut c_void) -> bool;
}

struct Object(Ax);
impl Drop for Object {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}
fn attribute(element: Ax, name: &str) -> Option<Object> {
    if element.is_null() {
        return None;
    }
    let name = CFString::new(name);
    let mut value = ptr::null();
    (unsafe {
        AXUIElementCopyAttributeValue(element, name.as_concrete_TypeRef().cast(), &mut value)
    } == 0
        && !value.is_null())
    .then_some(Object(value))
}
fn string_attr(element: Ax, name: &str) -> Option<String> {
    let value = attribute(element, name)?;
    if unsafe { core_foundation_sys::base::CFGetTypeID(value.0) }
        != unsafe { core_foundation_sys::string::CFStringGetTypeID() }
    {
        return None;
    }
    Some(unsafe { CFString::wrap_under_get_rule(value.0.cast()) }.to_string())
}

fn url_attr(element: Ax, name: &str) -> Option<PathBuf> {
    let value = attribute(element, name)?;
    let text = if unsafe { core_foundation_sys::base::CFGetTypeID(value.0) }
        == unsafe { core_foundation_sys::url::CFURLGetTypeID() }
    {
        let string = unsafe { core_foundation_sys::url::CFURLGetString(value.0.cast()) };
        if string.is_null() {
            return None;
        }
        unsafe { CFString::wrap_under_get_rule(string) }.to_string()
    } else {
        string_attr(element, name)?
    };
    file_url(&text)
}

fn hovered_folder(element: Ax) -> (Option<PathBuf>, bool) {
    // Only search inside the hit row/cell, never across the entire file list.
    fn descendants(element: Ax, depth: u8) -> Option<PathBuf> {
        if let Some(path) = url_attr(element, "AXURL") {
            return Some(path);
        }
        if depth == 0 {
            return None;
        }
        let children = attribute(element, "AXChildren")?;
        if unsafe { core_foundation_sys::base::CFGetTypeID(children.0) }
            != unsafe { core_foundation_sys::array::CFArrayGetTypeID() }
        {
            return None;
        }
        let count =
            unsafe { core_foundation_sys::array::CFArrayGetCount(children.0.cast()) }.min(32);
        for index in 0..count {
            let child = unsafe {
                core_foundation_sys::array::CFArrayGetValueAtIndex(children.0.cast(), index)
            };
            if let Some(path) = descendants(child, depth - 1) {
                return Some(path);
            }
        }
        None
    }
    if let Some(path) = url_attr(element, "AXURL") {
        return (path.is_dir().then_some(path), false);
    }
    let role = string_attr(element, "AXRole").unwrap_or_default();
    if matches!(role.as_str(), "AXRow" | "AXCell") {
        let path = descendants(element, 3);
        let uncertain = path.is_none();
        return (path.filter(|path| path.is_dir()), uncertain);
    }
    (None, false)
}

pub(super) fn marker_id() -> Option<[u8; 16]> {
    autoreleasepool(|_| {
        let board = NSPasteboard::generalPasteboard();
        let value = board.stringForType(&NSString::from_str(MARKER_TYPE))?;
        decode_id(&value.to_string())
    })
}
pub(super) fn publish_id(id: [u8; 16]) -> io::Result<()> {
    if NSPasteboard::generalPasteboard().setString_forType(
        &NSString::from_str(&encode_id(id)),
        &NSString::from_str(MARKER_TYPE),
    ) {
        Ok(())
    } else {
        Err(io::Error::other(
            "Cannot prepare Finder's native Paste command",
        ))
    }
}

fn file_url(value: &str) -> Option<PathBuf> {
    let path = value.strip_prefix("file://")?;
    let path = path.strip_prefix("localhost").unwrap_or(path);
    if !path.starts_with('/') {
        return None;
    }
    let mut bytes = Vec::new();
    let raw = path.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' {
            let hex = std::str::from_utf8(raw.get(index + 1..index + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            bytes.push(raw[index]);
            index += 1;
        }
    }
    let path = PathBuf::from(String::from_utf8(bytes).ok()?);
    path.exists().then_some(path)
}

/// Finder's scripting API supplies its insertion location when AXDocument is
/// absent (notably the desktop). It is a read-only query, not GUI scripting.
fn finder_insertion_location() -> Option<PathBuf> {
    unsafe {
        let class = objc2::runtime::AnyClass::get(c"NSAppleScript")?;
        let script: *mut AnyObject = msg_send![class, alloc];
        let script: *mut AnyObject = msg_send![script, initWithSource: &*NSString::from_str("tell application \"Finder\" to POSIX path of (insertion location as alias)")];
        let script = Retained::from_raw(script)?;
        let value: Option<Retained<AnyObject>> =
            msg_send![&*script, executeAndReturnError: ptr::null_mut::<*mut AnyObject>()];
        let value = value?;
        let value: Option<Retained<NSString>> = msg_send![&*value, stringValue];
        let path = PathBuf::from(value?.to_string());
        path.is_absolute()
            .then_some(path)
            .filter(|path| path.is_dir())
    }
}

fn paste_element(mut element: Object) -> Option<(f64, f64, f64, f64)> {
    for _ in 0..6 {
        if string_attr(element.0, "AXRole").as_deref() == Some("AXMenuItem")
            && string_attr(element.0, "AXTitle").is_some_and(|title| is_paste_title(&title))
        {
            let enabled = attribute(element.0, "AXEnabled")?;
            if !unsafe { core_foundation_sys::number::CFBooleanGetValue(enabled.0.cast()) } {
                return None;
            }
            let position = attribute(element.0, "AXPosition")?;
            let size = attribute(element.0, "AXSize")?;
            let mut point = CGPoint::new(0.0, 0.0);
            let mut extent = CGSize::new(0.0, 0.0);
            if unsafe {
                AXValueGetValue(position.0, 1, (&mut point as *mut CGPoint).cast())
                    && AXValueGetValue(size.0, 2, (&mut extent as *mut CGSize).cast())
            } {
                return Some((point.x, point.y, extent.width, extent.height));
            }
        }
        element = attribute(element.0, "AXParent")?;
    }
    None
}

fn menu_paste_rects(start: Ax) -> Vec<(f64, f64, f64, f64)> {
    fn children(element: Ax) -> Vec<Object> {
        let Some(array) = attribute(element, "AXChildren") else {
            return Vec::new();
        };
        if unsafe { core_foundation_sys::base::CFGetTypeID(array.0) }
            != unsafe { core_foundation_sys::array::CFArrayGetTypeID() }
        {
            return Vec::new();
        }
        let count = unsafe { core_foundation_sys::array::CFArrayGetCount(array.0.cast()) }.min(128);
        (0..count)
            .filter_map(|index| {
                let value = unsafe {
                    core_foundation_sys::array::CFArrayGetValueAtIndex(array.0.cast(), index)
                };
                if value.is_null() {
                    return None;
                }
                unsafe {
                    core_foundation_sys::base::CFRetain(value);
                }
                Some(Object(value))
            })
            .collect()
    }
    fn collect(menu: Ax) -> Vec<(f64, f64, f64, f64)> {
        children(menu)
            .into_iter()
            .filter(|item| {
                string_attr(item.0, "AXTitle").is_some_and(|title| is_paste_title(&title))
            })
            .filter_map(paste_element)
            .filter(|rect| rect.2 > 0.0 && rect.3 > 0.0)
            .collect()
    }
    if start.is_null() {
        return Vec::new();
    }
    unsafe {
        core_foundation_sys::base::CFRetain(start);
    }
    let mut element = Object(start);
    for _ in 0..6 {
        let role = string_attr(element.0, "AXRole").unwrap_or_default();
        if role == "AXMenu" {
            return collect(element.0);
        }
        if role == "AXMenuBarItem" {
            return children(element.0)
                .into_iter()
                .filter(|menu| string_attr(menu.0, "AXRole").as_deref() == Some("AXMenu"))
                .flat_map(|menu| collect(menu.0))
                .collect();
        }
        let Some(parent) = attribute(element.0, "AXParent") else {
            break;
        };
        element = parent;
    }
    Vec::new()
}

fn context() -> Context {
    autoreleasepool(|_| {
        let sampled = Instant::now();
        let Some(app) = NSWorkspace::sharedWorkspace().frontmostApplication() else {
            return Context::default();
        };
        if app
            .bundleIdentifier()
            .is_none_or(|id| id.to_string() != "com.apple.finder")
        {
            return Context::default();
        }
        let application = Object(unsafe { AXUIElementCreateApplication(app.processIdentifier()) });
        let _ = unsafe { AXUIElementSetMessagingTimeout(application.0, 0.08) };
        let system = Object(unsafe { AXUIElementCreateSystemWide() });
        let _ = unsafe { AXUIElementSetMessagingTimeout(system.0, 0.08) };
        let focus = attribute(system.0, "AXFocusedUIElement")
            .or_else(|| attribute(application.0, "AXFocusedUIElement"));
        let editing = focus
            .as_ref()
            .and_then(|element| string_attr(element.0, "AXRole"))
            .is_some_and(|role| {
                matches!(
                    role.as_str(),
                    "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox"
                )
            });
        let window = attribute(application.0, "AXFocusedWindow");
        let folder = window
            .as_ref()
            .and_then(|window| url_attr(window.0, "AXDocument"))
            .filter(|path| path.is_dir())
            .or_else(finder_insertion_location);
        let window_id = window
            .as_ref()
            .map(|window| unsafe { CFHash(window.0) } as usize)
            .unwrap_or(app.processIdentifier() as usize);
        let point = core_graphics::event_source::CGEventSource::new(
            core_graphics::event_source::CGEventSourceStateID::CombinedSessionState,
        )
        .ok()
        .and_then(|source| CGEvent::new(source).ok())
        .map(|event| event.location());
        let mut hit = ptr::null();
        let hit = point.and_then(|point| {
            (unsafe {
                AXUIElementCopyElementAtPosition(system.0, point.x as f32, point.y as f32, &mut hit)
            } == 0
                && !hit.is_null())
            .then_some(Object(hit))
        });
        let (hovered_folder, hover_uncertain) = hit
            .as_ref()
            .map(|hit| hovered_folder(hit.0))
            .unwrap_or_default();
        let mut all_menu_rects = hit
            .as_ref()
            .map(|hit| menu_paste_rects(hit.0))
            .unwrap_or_default();
        if let Some(focus) = focus.as_ref() {
            all_menu_rects.extend(menu_paste_rects(focus.0));
        }
        let paste_rect = hit.and_then(paste_element);
        let paste_focused = focus.and_then(paste_element).is_some();
        if NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .is_none_or(|current| current.processIdentifier() != app.processIdentifier())
        {
            return Context::default();
        }
        let current_window = attribute(application.0, "AXFocusedWindow");
        let current_id = current_window
            .as_ref()
            .map(|window| unsafe { CFHash(window.0) } as usize)
            .unwrap_or(app.processIdentifier() as usize);
        if current_id != window_id {
            return Context::default();
        }
        Context {
            updated: Some(sampled),
            window: window_id,
            folder,
            editing,
            paste_rect,
            menu_paste_rects: all_menu_rects,
            paste_focused,
            hovered_folder,
            pointer: point.map(|point| (point.x, point.y)),
            hover_uncertain,
        }
    })
}

#[cfg(not(test))]
pub(super) fn cancel_menu() -> bool {
    // Called by the accessibility worker after verifying the live menu,
    // never from a low-level event callback.
    let Ok(source) = core_graphics::event_source::CGEventSource::new(
        core_graphics::event_source::CGEventSourceStateID::CombinedSessionState,
    ) else {
        return false;
    };
    for down in [true, false] {
        let Ok(event) = CGEvent::new_keyboard_event(source.clone(), 53, down) else {
            return false;
        };
        event.set_integer_value_field(
            EventField::EVENT_SOURCE_USER_DATA,
            input_event::MACOS_INPUT_EVENT_TAG,
        );
        event.post(CGEventTapLocation::HID);
    }
    true
}

pub(super) fn start(shared: Arc<Shared>) -> io::Result<()> {
    if unsafe { AXIsProcessTrusted() } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Mousehop needs Accessibility permission for automatic Finder paste",
        ));
    }
    let worker = shared.clone();
    std::thread::spawn(move || {
        while !worker.stop.load(Ordering::Relaxed) {
            if worker.has_work() {
                let current = context();
                worker.confirm(&current);
                *worker.context.lock().unwrap() = current;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    });
    let (ready, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let callback_shared = shared.clone();
        let suppressed = Mutex::new((None::<i64>, false, None::<PathBuf>));
        let tap = CGEventTap::new(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            vec![
                CGEventType::KeyDown,
                CGEventType::KeyUp,
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp,
                CGEventType::RightMouseDown,
            ],
            move |_, kind, event| {
                let mut consumed = suppressed.lock().unwrap();
                let key = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                if matches!(kind, CGEventType::KeyDown) && consumed.0 == Some(key) {
                    return CallbackResult::Drop;
                }
                if matches!(kind, CGEventType::KeyUp) && consumed.0 == Some(key) {
                    consumed.0 = None;
                    return CallbackResult::Drop;
                }
                if matches!(kind, CGEventType::LeftMouseUp) && consumed.1 {
                    consumed.1 = false;
                    return CallbackResult::Drop;
                }
                if matches!(kind, CGEventType::KeyDown) && key == 53 {
                    consumed.2 = None;
                }
                if !callback_shared.accepts_input(
                    event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA)
                        == input_event::MACOS_INPUT_EVENT_TAG,
                ) {
                    return CallbackResult::Keep;
                }
                if !callback_shared.pending() {
                    return CallbackResult::Keep;
                }
                let context = callback_shared.context.lock().unwrap().clone();
                if matches!(kind, CGEventType::RightMouseDown) {
                    let point = event.location();
                    consumed.2 = if !context.hover_uncertain
                        && context.pointer.is_some_and(|(x, y)| {
                            (x - point.x).abs() <= 2.0 && (y - point.y).abs() <= 2.0
                        }) {
                        context.hovered_folder.clone().or(context.folder.clone())
                    } else {
                        Some(PathBuf::new())
                    };
                    return CallbackResult::Keep;
                }
                if matches!(kind, CGEventType::KeyDown) {
                    let flags = event.get_flags();
                    let command_v = key == 9
                        && flags.contains(CGEventFlags::CGEventFlagCommand)
                        && !flags.intersects(
                            CGEventFlags::CGEventFlagControl
                                | CGEventFlags::CGEventFlagAlternate
                                | CGEventFlags::CGEventFlagShift,
                        );
                    let menu_enter = key == 36 && context.paste_focused;
                    let context_shortcut =
                        consumed.2.is_some() || !context.menu_paste_rects.is_empty();
                    let confirmation = if menu_enter {
                        Confirmation::MenuKey
                    } else if context_shortcut {
                        Confirmation::ContextShortcut
                    } else {
                        Confirmation::Shortcut
                    };
                    let destination = if menu_enter || context_shortcut {
                        consumed.2.take()
                    } else {
                        None
                    };
                    if (command_v && !context.editing || menu_enter)
                        && callback_shared.trigger(&context, destination, confirmation)
                    {
                        consumed.0 = Some(key);
                        return CallbackResult::Drop;
                    }
                }
                if matches!(kind, CGEventType::LeftMouseDown)
                    && context.paste_at(event.location().x, event.location().y)
                    && callback_shared.trigger(
                        &context,
                        consumed.2.take(),
                        Confirmation::MenuClick(event.location().x, event.location().y),
                    )
                {
                    consumed.1 = true;
                    return CallbackResult::Drop;
                }
                if matches!(kind, CGEventType::LeftMouseDown) {
                    consumed.2 = None;
                }
                CallbackResult::Keep
            },
        );
        let Ok(tap) = tap else {
            let _ = ready.send(Err(io::Error::other(
                "Cannot monitor Finder Paste; enable Mousehop Input Monitoring",
            )));
            return;
        };
        let Ok(source) = tap.mach_port().create_runloop_source(0) else {
            let _ = ready.send(Err(io::Error::other("Cannot start Finder paste monitor")));
            return;
        };
        unsafe {
            CFRunLoop::get_current().add_source(&source, kCFRunLoopCommonModes);
        }
        tap.enable();
        let _ = ready.send(Ok(()));
        while !shared.stop.load(Ordering::Relaxed) {
            tap.enable();
            unsafe {
                CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, Duration::from_millis(100), false);
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(3))
        .map_err(io::Error::other)?
}

#![allow(clashing_extern_declarations)]

use std::{
    cell::RefCell,
    ffi::{CStr, CString, c_char, c_double, c_uint, c_void},
    sync::OnceLock,
};

use crate::agent::TrayCmd;
use std::sync::mpsc::{Receiver, Sender, channel};

type Id = *mut c_void;
type Class = *mut c_void;
type Sel = *mut c_void;
type Bool = i8;

struct StatusItem {
    status_label: Id,
    tx: Sender<TrayCmd>,
    _delegate: Id,
    _status_item: Id,
}
thread_local! { static STATUS_ITEM: RefCell<Option<StatusItem>> = const { RefCell::new(None) }; }
pub(crate) struct Tray {
    rx: Receiver<TrayCmd>,
}
impl Tray {
    pub fn new() -> std::io::Result<Self> {
        let (tx, rx) = channel();
        unsafe {
            let app = msg_send_id(class(c"NSApplication"), sel(c"sharedApplication"));
            msg_send_bool_usize(app, sel(c"setActivationPolicy:"), 1);
            let delegate = new_delegate();
            let status_label = menu_item(c"Mousehop: connecting", c"");
            let menu = menu(&[
                status_label,
                menu_item(c"Open Mousehop", c"showMousehop:"),
                menu_item(c"Pause / Resume sharing", c"toggleSharing:"),
                menu_item(c"View diagnostic logs", c"showLogs:"),
                separator_item(),
                menu_item(c"Quit Mousehop", c"quitMousehop:"),
            ]);
            let bar = msg_send_id(class(c"NSStatusBar"), sel(c"systemStatusBar"));
            let item = msg_send_id_f64(bar, sel(c"statusItemWithLength:"), -1.0);
            if item.is_null() {
                return Err(std::io::Error::other("Cannot create menu-bar item"));
            }
            let item = msg_send_id(item, sel(c"retain"));
            let button = msg_send_id(item, sel(c"button"));
            set_button_image(button);
            msg_send_void_id(button, sel(c"setToolTip:"), nsstring(c"Mousehop"));
            msg_send_void_id(item, sel(c"setMenu:"), menu);
            for item in menu_items(menu) {
                msg_send_void_id(item, sel(c"setTarget:"), delegate);
            }
            msg_send_void(app, sel(c"finishLaunching"));
            install_reopen_handler(delegate);
            STATUS_ITEM.with(|state| {
                state.replace(Some(StatusItem {
                    status_label,
                    tx,
                    _delegate: delegate,
                    _status_item: item,
                }));
            });
        }
        Ok(Self { rx })
    }
    pub fn set_state(&self, state: Option<mousehop_ipc::RuntimeState>) {
        STATUS_ITEM.with(|item| {
            if let Some(item) = item.borrow().as_ref() {
                unsafe {
                    let title =
                        CString::new(crate::agent::state_label(state)).expect("static tray title");
                    msg_send_void_id(item.status_label, sel(c"setTitle:"), nsstring(&title));
                }
            }
        });
    }
    pub fn poll(&self) -> Option<TrayCmd> {
        unsafe {
            let pool = msg_send_id(class(c"NSAutoreleasePool"), sel(c"new"));
            let app = msg_send_id(class(c"NSApplication"), sel(c"sharedApplication"));
            let date = msg_send_id(class(c"NSDate"), sel(c"distantPast"));
            loop {
                let event = next_event(
                    app,
                    sel(c"nextEventMatchingMask:untilDate:inMode:dequeue:"),
                    usize::MAX,
                    date,
                    nsstring(c"kCFRunLoopDefaultMode"),
                    1,
                );
                if event.is_null() {
                    break;
                }
                msg_send_void_id(app, sel(c"sendEvent:"), event);
            }
            msg_send_void(app, sel(c"updateWindows"));
            msg_send_void(pool, sel(c"drain"));
        }
        self.rx.try_recv().ok()
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        STATUS_ITEM.with(|state| {
            if let Some(item) = state.borrow_mut().take() {
                unsafe {
                    let bar = msg_send_id(class(c"NSStatusBar"), sel(c"systemStatusBar"));
                    msg_send_void_id(bar, sel(c"removeStatusItem:"), item._status_item);
                    msg_send_void(item._status_item, sel(c"release"));
                    msg_send_void(item._delegate, sel(c"release"));
                }
            }
        });
    }
}
fn send(command: TrayCmd) {
    STATUS_ITEM.with(|state| {
        if let Some(item) = state.borrow().as_ref() {
            let _ = item.tx.send(command);
        }
    });
}
extern "C" fn toggle_sharing(_: Id, _: Sel, _: Id) {
    send(TrayCmd::ToggleSharing);
}
extern "C" fn show_logs(_: Id, _: Sel, _: Id) {
    send(TrayCmd::Logs);
}

// Prefer a pre-rendered template PNG (black silhouette with alpha) so macOS
// auto-tints the glyph to match the menu bar in light and dark modes.
// Falls back to the full-color icns, then to "LM" text.
unsafe fn set_button_image(button: Id) {
    if let Some(image) = load_menubar_template() {
        msg_send_void_bool(image, sel(c"setTemplate:"), 1);
        msg_send_void_id(button, sel(c"setImage:"), image);
        return;
    }
    if let Some(image) = load_app_icon() {
        msg_send_void_id(button, sel(c"setImage:"), image);
        return;
    }
    log::warn!("no menu bar image available; falling back to text title");
    msg_send_void_id(button, sel(c"setTitle:"), nsstring(c"LM"));
}

unsafe fn load_menubar_template() -> Option<Id> {
    load_resource_image(c"menubar-template", c"png", MENUBAR_ICON_SIZE)
}

unsafe fn load_app_icon() -> Option<Id> {
    load_resource_image(c"icon", c"icns", MENUBAR_ICON_SIZE)
}

unsafe fn load_resource_image(name: &CStr, ext: &CStr, size_pt: c_double) -> Option<Id> {
    let bundle = msg_send_id(class(c"NSBundle"), sel(c"mainBundle"));
    if bundle.is_null() {
        return None;
    }
    let path = msg_send_id_id_id(
        bundle,
        sel(c"pathForResource:ofType:"),
        nsstring(name),
        nsstring(ext),
    );
    if path.is_null() {
        return None;
    }
    let image = msg_send_id_id(
        msg_send_id(class(c"NSImage"), sel(c"alloc")),
        sel(c"initWithContentsOfFile:"),
        path,
    );
    if image.is_null() {
        return None;
    }
    // Render at menu bar height; 22pt is the full status bar icon height.
    msg_send_void_size(image, sel(c"setSize:"), size_pt, size_pt);
    Some(image)
}

const MENUBAR_ICON_SIZE: c_double = 22.0;

unsafe fn menu(items: &[Id]) -> Id {
    let menu = msg_send_id(msg_send_id(class(c"NSMenu"), sel(c"alloc")), sel(c"init"));
    for item in items {
        msg_send_void_id(menu, sel(c"addItem:"), *item);
    }
    menu
}

unsafe fn menu_item(title: &CStr, action: &CStr) -> Id {
    msg_send_id_id_sel_id(
        msg_send_id(class(c"NSMenuItem"), sel(c"alloc")),
        sel(c"initWithTitle:action:keyEquivalent:"),
        nsstring(title),
        sel(action),
        nsstring(c""),
    )
}

unsafe fn separator_item() -> Id {
    msg_send_id(class(c"NSMenuItem"), sel(c"separatorItem"))
}

unsafe fn menu_items(menu: Id) -> Vec<Id> {
    let count = msg_send_usize(menu, sel(c"numberOfItems"));
    (0..count)
        .map(|idx| msg_send_id_usize(menu, sel(c"itemAtIndex:"), idx))
        .collect()
}

unsafe fn new_delegate() -> Id {
    let class = delegate_class();
    msg_send_id(msg_send_id(class, sel(c"alloc")), sel(c"init"))
}

fn delegate_class() -> Class {
    static CLASS: OnceLock<usize> = OnceLock::new();

    *CLASS.get_or_init(|| unsafe {
        let superclass = class(c"NSObject");
        let class_name = CString::new("MousehopStatusItemDelegate").unwrap();
        let class = objc_allocateClassPair(superclass, class_name.as_ptr(), 0);
        assert!(!class.is_null(), "failed to allocate status item delegate");

        class_addMethod(
            class,
            sel(c"showMousehop:"),
            show_mousehop as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"quitMousehop:"),
            quit_mousehop as *const c_void,
            c"v@:@".as_ptr(),
        );
        // kAEReopenApplication handler — fires when the user re-launches
        // the .app while it's already running (Finder, `open`, Dock).
        class_addMethod(
            class,
            sel(c"handleReopenEvent:withReplyEvent:"),
            handle_reopen_event as *const c_void,
            c"v@:@@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"toggleSharing:"),
            toggle_sharing as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"showLogs:"),
            show_logs as *const c_void,
            c"v@:@".as_ptr(),
        );
        objc_registerClassPair(class);
        class as usize
    }) as Class
}

extern "C" fn show_mousehop(_this: Id, _cmd: Sel, _sender: Id) {
    present_window();
}

extern "C" fn handle_reopen_event(_this: Id, _cmd: Sel, _event: Id, _reply: Id) {
    log::debug!("kAEReopenApplication received — presenting main window");
    present_window();
}

fn present_window() {
    send(TrayCmd::TogglePresent);
}

// Register the status-item delegate as the handler for the
// kAEReopenApplication Apple Event ('aevt'/'rapp'). NSApplication
// installs a default handler at -finishLaunching that just delegates to
// applicationShouldHandleReopen:hasVisibleWindows: — which is a no-op
// here because GApplication owns NSApp's delegate. Replacing it lets us
// re-present the window when the user double-clicks the .app while
// we're already running.
unsafe fn install_reopen_handler(delegate: Id) {
    const K_CORE_EVENT_CLASS: c_uint = 0x6165_7674; // 'aevt'
    const K_AE_REOPEN_APPLICATION: c_uint = 0x7261_7070; // 'rapp'

    let manager = msg_send_id(
        class(c"NSAppleEventManager"),
        sel(c"sharedAppleEventManager"),
    );
    if manager.is_null() {
        log::warn!("NSAppleEventManager unavailable; re-launch will not re-open window");
        return;
    }
    msg_send_void_id_sel_u32_u32(
        manager,
        sel(c"setEventHandler:andSelector:forEventClass:andEventID:"),
        delegate,
        sel(c"handleReopenEvent:withReplyEvent:"),
        K_CORE_EVENT_CLASS,
        K_AE_REOPEN_APPLICATION,
    );
}

extern "C" fn quit_mousehop(_: Id, _: Sel, _: Id) {
    send(TrayCmd::Quit);
}

unsafe fn class(name: &CStr) -> Class {
    let class = objc_getClass(name.as_ptr());
    assert!(!class.is_null(), "missing Objective-C class {name:?}");
    class
}

unsafe fn sel(name: &CStr) -> Sel {
    sel_registerName(name.as_ptr())
}

unsafe fn nsstring(value: &CStr) -> Id {
    msg_send_id_ptr(
        class(c"NSString"),
        sel(c"stringWithUTF8String:"),
        value.as_ptr(),
    )
}

#[link(name = "objc")]
extern "C" {
    fn objc_allocateClassPair(superclass: Class, name: *const c_char, extra_bytes: usize) -> Class;
    fn objc_getClass(name: *const c_char) -> Class;
    fn objc_registerClassPair(class: Class);
    fn sel_registerName(name: *const c_char) -> Sel;
    fn class_addMethod(class: Class, name: Sel, imp: *const c_void, types: *const c_char) -> Bool;
}

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

#[link(name = "objc")]
extern "C" {
    #[link_name = "objc_msgSend"]
    fn msg_send_id(receiver: Id, selector: Sel) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_void(receiver: Id, selector: Sel);
    #[link_name = "objc_msgSend"]
    fn next_event(
        receiver: Id,
        selector: Sel,
        mask: usize,
        date: Id,
        mode: Id,
        dequeue: Bool,
    ) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_id_f64(receiver: Id, selector: Sel, value: c_double) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_id_id_sel_id(receiver: Id, selector: Sel, a: Id, b: Sel, c: Id) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_id_id_id(receiver: Id, selector: Sel, a: Id, b: Id) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_id_id(receiver: Id, selector: Sel, a: Id) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_void_size(receiver: Id, selector: Sel, width: c_double, height: c_double);
    #[link_name = "objc_msgSend"]
    fn msg_send_id_ptr(receiver: Id, selector: Sel, value: *const c_char) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_id_usize(receiver: Id, selector: Sel, value: usize) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_usize(receiver: Id, selector: Sel) -> usize;
    #[link_name = "objc_msgSend"]
    fn msg_send_void_bool(receiver: Id, selector: Sel, value: Bool);
    #[link_name = "objc_msgSend"]
    fn msg_send_void_id(receiver: Id, selector: Sel, value: Id);
    #[link_name = "objc_msgSend"]
    fn msg_send_bool_usize(receiver: Id, selector: Sel, value: usize) -> Bool;
    #[link_name = "objc_msgSend"]
    fn msg_send_void_id_sel_u32_u32(
        receiver: Id,
        selector: Sel,
        a: Id,
        b: Sel,
        c: c_uint,
        d: c_uint,
    );
}

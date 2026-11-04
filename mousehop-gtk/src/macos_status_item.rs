#![allow(clashing_extern_declarations)]
use std::ffi::{c_char, c_void};
type Id = *mut c_void;
pub(crate) fn configure_accessory() {
    unsafe {
        let app = send_id(
            objc_getClass(c"NSApplication".as_ptr()),
            sel_registerName(c"sharedApplication".as_ptr()),
        );
        send_policy(
            app,
            sel_registerName(c"setActivationPolicy:".as_ptr()),
            1, // NSApplicationActivationPolicyAccessory: interactive windows without a Dock icon.
        );
    }
}
pub(crate) fn activate_window() {
    // GTK initializes a regular NSApplication. Restore accessory mode after
    // GTK startup/presentation, including in the separate transfer process.
    configure_accessory();
    unsafe {
        let app = send_id(
            objc_getClass(c"NSApplication".as_ptr()),
            sel_registerName(c"sharedApplication".as_ptr()),
        );
        send_bool(
            app,
            sel_registerName(c"activateIgnoringOtherApps:".as_ptr()),
            1,
        );
    }
}
#[link(name = "AppKit", kind = "framework")]
extern "C" {}
#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Id;
    #[link_name = "objc_msgSend"]
    fn send_id(receiver: Id, selector: Id) -> Id;
    #[link_name = "objc_msgSend"]
    fn send_policy(receiver: Id, selector: Id, value: usize) -> i8;
    #[link_name = "objc_msgSend"]
    fn send_bool(receiver: Id, selector: Id, value: i8);
}

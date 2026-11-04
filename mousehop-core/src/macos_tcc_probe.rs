//! Fresh-process probe of TCC Accessibility permission.
//!
//! `AXIsProcessTrusted()` in already-running processes can return
//! cached-true for an unbounded window after the user *removes* the
//! app's entry from System Settings → Privacy & Security →
//! Accessibility (vs *toggling* it off, which flips the cache
//! promptly). To detect the "remove" case, the daemon spawns a fresh
//! subprocess of the same binary that runs this probe — a fresh
//! process consults the current TCC state without inheriting the
//! parent's cached trust. See `macos_tcc_watch` for the watcher loop.

use std::ffi::c_uchar;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    // ApplicationServices declares this legacy Boolean as unsigned char.
    fn AXIsProcessTrusted() -> c_uchar;
    fn CGPreflightListenEventAccess() -> bool;
}

pub fn is_accessibility_granted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}

/// Check without prompting; startup uses this to expose the permission UI.
pub fn is_input_monitoring_granted() -> bool {
    unsafe { CGPreflightListenEventAccess() }
}

/// Exit-status bitmask: 1 = Accessibility missing, 2 = Input Monitoring missing.
pub fn permission_probe_status() -> i32 {
    if !is_accessibility_granted() {
        // Do not touch another protected API while Accessibility is missing.
        return 3;
    }
    if is_input_monitoring_granted() { 0 } else { 2 }
}

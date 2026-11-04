use std::sync::atomic::{AtomicBool, Ordering};
static STOP: AtomicBool = AtomicBool::new(false);
pub(crate) fn stopping() -> bool {
    STOP.load(Ordering::Relaxed)
}
#[cfg(unix)]
pub(crate) fn install() {
    extern "C" fn handle(_: libc::c_int) {
        STOP.store(true, Ordering::Relaxed);
    }
    unsafe {
        libc::signal(libc::SIGINT, handle as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handle as *const () as libc::sighandler_t);
    }
}
#[cfg(windows)]
pub(crate) fn install() {
    use windows::{Win32::System::Console::SetConsoleCtrlHandler, core::BOOL};
    unsafe extern "system" fn handle(_: u32) -> BOOL {
        STOP.store(true, Ordering::Relaxed);
        true.into()
    }
    if let Err(error) = unsafe { SetConsoleCtrlHandler(Some(handle), true) } {
        log::warn!("Install shutdown handler: {error}");
    }
}

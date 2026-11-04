//! Best-effort panic logging to a durable file.
//!
//! The daemon runs as a LaunchServices-spawned child whose stderr is
//! routed to `/dev/null`, so a Rust panic — and, with `panic =
//! "abort"`, the process death that immediately follows — leaves no
//! trace the user can find. (That is precisely why the
//! screensaver-lockup panic went undiagnosed for so long.) [`install`]
//! adds a panic hook that appends the panic's location, message,
//! thread, and a backtrace to a logfile under the platform log
//! directory, then chains to the previous hook so the usual stderr
//! output and the `abort` still happen.

use std::path::PathBuf;

/// Panic files use the same dated-file retention policy as ordinary logs.
pub fn install(role: &'static str) {
    if let Some(path) = log_file_path(role) {
        if let Some(directory) = path.parent() {
            mousehop_logging::install_panic(role, directory.to_owned());
        }
    }
}

#[cfg(target_os = "macos")]
fn log_file_path(_role: &str) -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os("HOME")?);
    p.push("Library/Logs/Mousehop/panic.log");
    Some(p)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn log_file_path(_role: &str) -> Option<PathBuf> {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        let mut p = PathBuf::from(state);
        p.push("mousehop/panic.log");
        return Some(p);
    }
    let mut p = PathBuf::from(std::env::var_os("HOME")?);
    p.push(".local/state/mousehop/panic.log");
    Some(p)
}

#[cfg(windows)]
fn log_file_path(_role: &str) -> Option<PathBuf> {
    if _role.starts_with("windows-") {
        return crate::windows_service::data_dir()
            .ok()
            .map(|p| p.join("panic.log"));
    }
    let mut p = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    p.push("Mousehop");
    p.push("logs");
    p.push("panic.log");
    Some(p)
}

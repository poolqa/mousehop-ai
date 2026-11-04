//! Observe revocation once per Core session; never terminate the app or probe
//! repeatedly while the user is still configuring first-launch permissions.
use std::{
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const FALLBACK_INTERVAL: Duration = Duration::from_secs(30);
const TCC_DB_PATH: &str = "/Library/Application Support/com.apple.TCC/TCC.db";

fn read_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn database_stamp() -> (Option<SystemTime>, Option<SystemTime>) {
    (
        read_mtime(Path::new(TCC_DB_PATH)),
        read_mtime(Path::new(&format!("{TCC_DB_PATH}-wal"))),
    )
}

fn probe_permissions_in_fresh_subprocess() -> Option<bool> {
    // A new process avoids AX's cached trust after an entry is removed. Only
    // explicit permission exit codes indicate revocation; signals/timeouts do not.
    let mut child = Command::new(std::env::current_exe().ok()?)
        .arg("permissions-probe")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return classify_probe_status(status.code()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn classify_probe_status(code: Option<i32>) -> Option<bool> {
    match code {
        Some(0) => Some(true),
        Some(1..=3) => Some(false),
        _ => None,
    }
}

#[derive(Default)]
struct RevocationGuard {
    confirmed_missing: bool,
}
impl RevocationGuard {
    fn observe(&mut self, granted: Option<bool>) -> bool {
        if granted == Some(false) && !self.confirmed_missing {
            self.confirmed_missing = true;
            return true;
        }
        false
    }
}

pub struct PermissionWatcher {
    stop: mpsc::Sender<()>,
}
impl Drop for PermissionWatcher {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

pub fn spawn() -> Option<(PermissionWatcher, UnboundedReceiver<()>)> {
    let authorized = std::env::var_os("MOUSEHOP_DISABLE_TCC_WATCH").is_none()
        && crate::macos_tcc_probe::permission_probe_status() == 0;
    spawn_authorized_session(
        authorized,
        probe_permissions_in_fresh_subprocess,
        POLL_INTERVAL,
        FALLBACK_INTERVAL,
    )
}

fn spawn_authorized_session(
    authorized: bool,
    mut probe: impl FnMut() -> Option<bool> + Send + 'static,
    interval: Duration,
    fallback_interval: Duration,
) -> Option<(PermissionWatcher, UnboundedReceiver<()>)> {
    if !authorized {
        // First-launch Settings owns requesting permissions. Do not keep
        // spawning probes that re-register entries the user just removed.
        log::info!(
            "Permission watcher waiting for an explicitly restarted, authorized app session"
        );
        return None;
    }
    let (notify, revoked) = unbounded_channel();
    let (stop, stopped) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("mousehop-tcc-watch".into())
        .spawn(move || {
            let mut last_stamp = database_stamp();
            let mut last_probe = Instant::now();
            let mut guard = RevocationGuard::default();
            while matches!(
                stopped.recv_timeout(interval),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                let stamp = database_stamp();
                if stamp == last_stamp && last_probe.elapsed() < fallback_interval {
                    continue;
                }
                last_stamp = stamp;
                last_probe = Instant::now();
                let granted = probe();
                if guard.observe(granted) {
                    log::warn!(
                        "macOS input permissions revoked; pausing sharing until app restart"
                    );
                    let _ = notify.send(());
                    // Do not re-add deleted entries or race rapid off/on edits.
                    break;
                }
                if granted.is_none() {
                    log::warn!("Permission probe unavailable; retaining current session state");
                }
            }
        });
    match thread {
        Ok(_) => Some((PermissionWatcher { stop }, revoked)),
        Err(error) => {
            log::warn!("Cannot start permission watcher: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crashed_or_timed_out_probe_is_not_a_permission_revocation() {
        for code in [None, Some(4), Some(101), Some(255)] {
            assert_eq!(classify_probe_status(code), None);
        }
        assert_eq!(classify_probe_status(Some(0)), Some(true));
        for code in 1..=3 {
            assert_eq!(classify_probe_status(Some(code)), Some(false));
        }
    }
    #[test]
    fn rapid_repeated_revocations_notify_only_once_per_app_session() {
        let mut guard = RevocationGuard::default();
        for _ in 0..100 {
            assert!(!guard.observe(None));
            assert!(!guard.observe(Some(true)));
        }
        assert!(guard.observe(Some(false)));
        for _ in 0..100 {
            assert!(!guard.observe(Some(true)));
            assert!(!guard.observe(Some(false)));
        }
    }
    #[test]
    fn first_launch_without_permissions_never_spawns_a_probe_worker() {
        for _ in 0..100 {
            assert!(
                spawn_authorized_session(
                    false,
                    || panic!("must not probe"),
                    Duration::ZERO,
                    Duration::ZERO
                )
                .is_none()
            );
        }
    }
    #[tokio::test]
    async fn authorized_worker_reports_revocation_then_stops_probing() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let (watcher, mut changes) = spawn_authorized_session(
            true,
            move || Some(calls.fetch_add(1, Ordering::SeqCst) < 3),
            Duration::from_millis(1),
            Duration::ZERO,
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            assert_eq!(changes.recv().await, Some(()));
            assert_eq!(changes.recv().await, None);
        })
        .await
        .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 4);
        drop(watcher);
    }
}

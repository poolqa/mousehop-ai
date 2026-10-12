use arboard::Clipboard;
use input_event::{ClipboardEvent, Event};
use mousehop_ipc::AppIdent;
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::spawn_blocking;
use tokio::time::interval;

use crate::frontmost_app;
use crate::{CaptureError, CaptureEvent};

#[cfg(any(windows, test))]
pub(crate) mod windows_snapshot;

/// Internal entry point for the installed Windows user's one-shot file reader.
/// It writes a bounded JSON snapshot to the inherited stdout pipe, then exits.
#[cfg(windows)]
#[doc(hidden)]
pub fn write_windows_clipboard_snapshot() -> std::io::Result<()> {
    crate::windows::clipboard_reader::write_snapshot()
}

/// Shared, mutable suppression list. Service owns the canonical
/// `Arc<Mutex<HashSet<AppIdent>>>` and clones the handle into each
/// freshly-spawned [`ClipboardMonitor`]; mutations from
/// `Add/RemoveSuppressedApp` requests take effect immediately on
/// the next clipboard poll.
pub type SuppressionList = Arc<Mutex<HashSet<AppIdent>>>;

/// Clipboard monitor that watches for clipboard changes
pub struct ClipboardMonitor {
    event_rx: Receiver<(u64, CaptureEvent)>,
    _event_tx: Sender<(u64, CaptureEvent)>,
    last_content: Arc<Mutex<Option<u64>>>,
    last_change: Arc<Mutex<Option<Instant>>>,
    enabled: Arc<Mutex<bool>>,
    revision: Arc<AtomicU64>,
}

impl ClipboardMonitor {
    /// Construct without app-source suppression. Equivalent to
    /// `with_suppression(Default::default())` — provided as a
    /// convenience for callers that don't care about suppression
    /// (CLI smoke tests, future per-platform unit tests).
    pub fn new() -> Result<Self, CaptureError> {
        Self::with_suppression(SuppressionList::default())
    }

    /// Construct a monitor that consults `suppression` on every
    /// detected clipboard change and skips the emit when
    /// [`frontmost_app::frontmost_app()`] reports an app whose [`AppIdent`]
    /// is in the list. The content signature still advances, preventing the
    /// same sensitive value from being reconsidered after a focus change.
    pub fn with_suppression(suppression: SuppressionList) -> Result<Self, CaptureError> {
        let (event_tx, event_rx) = mpsc::channel(16);
        let last_content: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let last_change: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let enabled = Arc::new(Mutex::new(true));
        let revision = Arc::new(AtomicU64::new(0));
        let poll_revision = revision.clone();

        let last_content_clone = last_content.clone();
        let last_change_clone = last_change.clone();
        let enabled_clone = enabled.clone();
        let event_tx_clone = event_tx.clone();
        let suppression_clone = suppression.clone();

        // Spawn monitoring task. Cadence: 100 ms on macOS (cheap
        // because the pasteboard change-count gate short-circuits
        // 99% of ticks via a single integer compare); 500 ms
        // elsewhere (no cheap precheck, full content read every
        // tick). Tighter cadence on macOS shrinks the
        // frontmost-app suppression race window from 500 ms →
        // 100 ms — the user would have to Cmd+Tab faster than
        // human reaction time after copying to defeat the check.
        #[cfg(target_os = "macos")]
        const POLL_MS: u64 = 100;
        #[cfg(not(target_os = "macos"))]
        const POLL_MS: u64 = 500;

        tokio::spawn(async move {
            let mut check_interval = interval(Duration::from_millis(POLL_MS));
            #[cfg(target_os = "macos")]
            let mut pasteboard_gate = PasteboardPollGate::default();

            loop {
                check_interval.tick().await;

                // Check if enabled
                let is_enabled = {
                    let enabled = enabled_clone.lock().unwrap();
                    *enabled
                };

                if !is_enabled {
                    continue;
                }

                // macOS: skip the expensive content-read entirely
                // when NSPasteboard.changeCount hasn't advanced
                // since last tick. This is the canonical clipboard-
                // monitor optimization (Maccy / Alfred / Paste all
                // do it). Single integer compare per idle tick.
                #[cfg(target_os = "macos")]
                if !pasteboard_gate.should_read(pasteboard_change_count()) {
                    continue;
                }

                // Read clipboard in blocking task
                let last_content_clone2 = last_content_clone.clone();
                let last_change_clone2 = last_change_clone.clone();
                let event_tx_clone2 = event_tx_clone.clone();
                let suppression_clone2 = suppression_clone.clone();

                let revision_clone = poll_revision.clone();
                let observed_revision = revision_clone.load(Ordering::SeqCst);
                let read_result = spawn_blocking(move || {
                    // Create clipboard instance
                    let mut clipboard = match Clipboard::new() {
                        Ok(c) => c,
                        Err(e) => {
                            log::debug!("Failed to create clipboard: {e}");
                            return false;
                        }
                    };

                    let current = match read_clipboard_event(&mut clipboard) {
                        Some(event) => event,
                        None => return false,
                    };
                    let signature = clipboard_signature(&current);

                    // Check if content changed
                    let mut last_content = last_content_clone2.lock().unwrap();
                    let mut last_change = last_change_clone2.lock().unwrap();
                    if revision_clone.load(Ordering::SeqCst) != observed_revision {
                        return true;
                    }

                    let last_change_elapsed = last_change.map(|t| t.elapsed());
                    // App-source suppression. Frontmost-app lookup
                    // and the macOS concealed-pasteboard probe both
                    // run only after we've decided the content
                    // actually changed (see PollDecision::classify).
                    // Pre-compute the inputs here so the classifier
                    // stays a pure function.
                    let changed = last_content.as_ref() != Some(&signature);
                    let needs_decision =
                        PollDecision::content_might_emit(changed, last_change_elapsed);
                    let (concealed, suppressed) = if needs_decision {
                        let concealed = is_concealed_clipboard();
                        let suppressed = if concealed {
                            None
                        } else {
                            is_suppressed(&suppression_clone2)
                        };
                        (concealed, suppressed)
                    } else {
                        (false, None)
                    };
                    let decision = PollDecision::classify(
                        changed,
                        last_change_elapsed,
                        concealed,
                        suppressed.is_some(),
                    );
                    // Always advance `last_content` after a state-
                    // changing decision (Suppressed or Emit),
                    // regardless of which suppression branch
                    // fired. The earlier "blind to suppressed
                    // value" approach left `last_content` at the
                    // previous emitted value, which made every
                    // subsequent 500ms poll see the SAME
                    // suppressed content as "changed" and re-run
                    // the suppression check. Any focus shift
                    // between polls (user alt-tabs after copying
                    // a password) would then find a non-
                    // suppressed frontmost and leak the password.
                    // PollDecision::advances_state pins this
                    // contract — see the unit tests at the
                    // bottom of this file.
                    if decision.advances_state() {
                        *last_content = Some(signature);
                        *last_change = Some(Instant::now());
                    }
                    match decision {
                        PollDecision::Unchanged => {}
                        PollDecision::Debounced => {
                            log::trace!("Clipboard changed but debounced (too recent)");
                        }
                        PollDecision::Suppressed if concealed => {
                            log::debug!("clipboard change suppressed (concealed pasteboard type)");
                        }
                        PollDecision::Suppressed => {
                            if let Some(app) = suppressed.as_ref() {
                                log::debug!(
                                    "clipboard change suppressed (frontmost app `{}`)",
                                    app.label()
                                );
                            }
                        }
                        PollDecision::Emit => {
                            match &current {
                                ClipboardEvent::Text(text) => {
                                    log::info!("Clipboard text changed: {} bytes", text.len())
                                }
                                ClipboardEvent::ImageRgba {
                                    width,
                                    height,
                                    bytes,
                                } => log::info!(
                                    "Clipboard image changed: {width}x{height}, {} bytes RGBA",
                                    bytes.len()
                                ),
                                ClipboardEvent::Files { paths, .. } => log::info!(
                                    "Clipboard file list changed: {} item(s)",
                                    paths.len()
                                ),
                            }
                            let event = CaptureEvent::Input(Event::Clipboard(current));
                            let _ = event_tx_clone2.blocking_send((observed_revision, event));
                        }
                    }
                    decision == PollDecision::Debounced
                })
                .await;
                #[cfg(target_os = "macos")]
                {
                    pasteboard_gate.retry = read_result.unwrap_or(false);
                }
                #[cfg(not(target_os = "macos"))]
                let _ = read_result;
            }
        });

        Ok(Self {
            event_rx,
            _event_tx: event_tx,
            last_content,
            last_change,
            enabled,
            revision,
        })
    }

    /// Receive the next clipboard event
    pub async fn recv(&mut self) -> Option<CaptureEvent> {
        while let Some((revision, event)) = self.event_rx.recv().await {
            if revision == self.revision.load(Ordering::SeqCst) {
                return Some(event);
            }
        }
        None
    }

    /// Enable clipboard monitoring
    pub fn enable(&self) {
        let mut enabled = self.enabled.lock().unwrap();
        *enabled = true;
        log::info!("Clipboard monitoring enabled");
    }

    /// Disable clipboard monitoring
    pub fn disable(&self) {
        let mut enabled = self.enabled.lock().unwrap();
        *enabled = false;
        log::info!("Clipboard monitoring disabled");
    }

    /// Update the last known clipboard signature after applying a remote event.
    /// This prevents detecting our own clipboard change as an external change.
    pub fn update_last_event(&self, event: &ClipboardEvent) {
        let mut last_content = self.last_content.lock().unwrap();
        let mut last_change = self.last_change.lock().unwrap();
        // Invalidate reads and queued events from before remote publication.
        self.revision.fetch_add(1, Ordering::SeqCst);
        *last_content = Some(clipboard_signature(event));
        *last_change = Some(Instant::now());
    }

    pub fn update_last_content(&self, content: String) {
        self.update_last_event(&ClipboardEvent::Text(content));
    }
}

/// Read the richest available clipboard representation. File lists must be
/// checked before text because desktop clipboards commonly expose copied files
/// as both a native file list and `text/uri-list`.
fn read_clipboard_event(clipboard: &mut Clipboard) -> Option<ClipboardEvent> {
    if let Ok(paths) = clipboard.get().file_list() {
        if !paths.is_empty() {
            return Some(ClipboardEvent::Files {
                paths,
                owned_root: None,
            });
        }
    }
    #[cfg(windows)]
    {
        use std::sync::atomic::AtomicU32;
        use windows::Win32::System::DataExchange::{
            GetClipboardSequenceNumber, IsClipboardFormatAvailable,
        };
        // Read Explorer's OLE files in the user's process. An advertised file
        // list that cannot yet be read must not fall back to its text form.
        let files = crate::windows::clipboard_reader::read_as_desktop_user().and_then(|paths| {
            if paths.is_empty() && unsafe { IsClipboardFormatAvailable(15) }.is_ok() {
                Err(std::io::Error::other(
                    "FileDrop is available but could not be read",
                ))
            } else {
                Ok(paths)
            }
        });
        match files {
            Ok(paths) if !paths.is_empty() => {
                return Some(ClipboardEvent::Files {
                    paths,
                    owned_root: None,
                });
            }
            Ok(_) => {}
            Err(error) => {
                static LAST_FAILED_READ: AtomicU32 = AtomicU32::new(u32::MAX);
                let sequence = unsafe { GetClipboardSequenceNumber() };
                if LAST_FAILED_READ.swap(sequence, Ordering::Relaxed) != sequence {
                    log::warn!(
                        "Clipboard file read failed (sequence {sequence}); retrying: {error}"
                    );
                }
                return None;
            }
        }
    }
    if let Ok(image) = clipboard.get_image() {
        let width = u32::try_from(image.width).ok()?;
        let height = u32::try_from(image.height).ok()?;
        let pixels = u64::from(width).checked_mul(u64::from(height))?;
        if pixels <= 100_000_000 && image.bytes.len() <= 128 * 1024 * 1024 {
            return Some(ClipboardEvent::ImageRgba {
                width,
                height,
                bytes: image.bytes.into_owned(),
            });
        }
        log::warn!("clipboard image exceeds transfer limits: {width}x{height}");
        return None;
    }
    clipboard.get_text().ok().map(ClipboardEvent::Text)
}

pub fn clipboard_signature(event: &ClipboardEvent) -> u64 {
    let mut hasher = DefaultHasher::new();
    match event {
        ClipboardEvent::Text(text) => {
            0_u8.hash(&mut hasher);
            text.hash(&mut hasher);
        }
        ClipboardEvent::ImageRgba {
            width,
            height,
            bytes,
        } => {
            1_u8.hash(&mut hasher);
            width.hash(&mut hasher);
            height.hash(&mut hasher);
            bytes.hash(&mut hasher);
        }
        ClipboardEvent::Files { paths, .. } => {
            2_u8.hash(&mut hasher);
            for path in paths {
                #[cfg(windows)]
                input_event::windows_clipboard_path_key(path).hash(&mut hasher);
                #[cfg(not(windows))]
                path.hash(&mut hasher);
                if let Ok(metadata) = std::fs::metadata(path) {
                    metadata.len().hash(&mut hasher);
                    metadata.modified().ok().hash(&mut hasher);
                }
            }
        }
    }
    hasher.finish()
}

/// macOS password managers stamp `org.nspasteboard.ConcealedType`
/// on the general pasteboard so cooperating apps skip syncing
/// passwords. Returns `true` when that UTI is present on the
/// current pasteboard contents. Always `false` on non-macOS.
///
/// This is the standard "nspasteboard.com" convention — see
/// <https://nspasteboard.org/>.
#[cfg(target_os = "macos")]
fn is_concealed_clipboard() -> bool {
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::NSString;

    let pasteboard = NSPasteboard::generalPasteboard();
    let Some(types) = pasteboard.types() else {
        return false;
    };
    let concealed = NSString::from_str("org.nspasteboard.ConcealedType");
    types.iter().any(|t| t.isEqualToString(&concealed))
}

#[cfg(not(target_os = "macos"))]
fn is_concealed_clipboard() -> bool {
    false
}

/// If [`frontmost_app::frontmost_app()`] reports an app whose ident
/// is in the suppression list, return that ident. Otherwise return
/// `None`. Snapshotting the lock guard short keeps us from holding
/// the mutex across the platform call (which on Linux can shell
/// out to hyprctl/swaymsg).
fn is_suppressed(list: &SuppressionList) -> Option<AppIdent> {
    let snapshot: Vec<AppIdent> = {
        let Ok(guard) = list.lock() else {
            log::debug!("clipboard suppression: lock poisoned");
            return None;
        };
        if guard.is_empty() {
            log::debug!("clipboard suppression: list is empty");
            return None;
        }
        guard.iter().cloned().collect()
    };
    let active = frontmost_app::frontmost_app();
    log::debug!("clipboard suppression check: list={snapshot:?} active={active:?}");
    let active = active?;
    snapshot.into_iter().find(|s| s.matches(&active))
}

/// Cheap native version query for the per-monitor polling gate.
///
/// `changeCount` reads are an Apple-blessed background-thread
/// operation — the property is designed for exactly this kind of
/// polling. No autorelease pool needed: the return value is a
/// primitive `NSInteger`, not an Objective-C object.
#[cfg(target_os = "macos")]
fn pasteboard_change_count() -> i64 {
    use objc2_app_kit::NSPasteboard;
    let pb = NSPasteboard::generalPasteboard();
    pb.changeCount() as i64
}

#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
struct PasteboardPollGate {
    last_count: Option<i64>,
    retry: bool,
}

#[cfg(any(target_os = "macos", test))]
impl PasteboardPollGate {
    fn should_read(&mut self, count: i64) -> bool {
        // A debounced copy still needs processing after 200 ms, even when
        // NSPasteboard does not change again. Track this per monitor.
        self.last_count.replace(count) != Some(count) || self.retry
    }
}

/// Outcome of a single clipboard-poll cycle. Pulled into a pure
/// function so the focus-race invariant — "advance `last_content`
/// on every state-changing decision, even when we suppress" — is
/// pinned by unit tests rather than implicit in the closure
/// body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollDecision {
    /// Current text equals last recorded — nothing to do.
    Unchanged,
    /// Content changed but the 200 ms debounce window from the
    /// previous advance hasn't elapsed yet.
    Debounced,
    /// Content changed, debounce passed, but suppression
    /// (concealed pasteboard or app-list match) tells us not to
    /// emit. State must still advance so subsequent polls don't
    /// re-check the same content and leak on a focus shift.
    Suppressed,
    /// Content changed, debounce passed, suppression cleared.
    /// Caller emits the event and advances state.
    Emit,
}

impl PollDecision {
    /// Cheap pre-flight: `true` when the next call to
    /// [`PollDecision::classify`] could return `Suppressed` or
    /// `Emit`. The poll loop uses this to skip the relatively
    /// expensive `is_concealed_clipboard()` / `frontmost_app()` /
    /// `is_suppressed()` calls when the content hasn't changed
    /// or the debounce window blocks emission anyway.
    fn content_might_emit(changed: bool, last_change_elapsed: Option<Duration>) -> bool {
        if !changed {
            return false;
        }
        last_change_elapsed.is_none_or(|d| d > Duration::from_millis(200))
    }

    /// Decide what to do with this poll cycle. Pure function — no
    /// I/O, no global state — so the focus-race invariant is
    /// expressible as a series of `assert_eq!` calls.
    fn classify(
        changed: bool,
        last_change_elapsed: Option<Duration>,
        concealed: bool,
        suppressed_match: bool,
    ) -> Self {
        if !changed {
            return Self::Unchanged;
        }
        let debounce_passed = last_change_elapsed.is_none_or(|d| d > Duration::from_millis(200));
        if !debounce_passed {
            return Self::Debounced;
        }
        if concealed || suppressed_match {
            Self::Suppressed
        } else {
            Self::Emit
        }
    }

    /// True when this decision must advance `last_content` /
    /// `last_change` in the caller. Both `Suppressed` and `Emit`
    /// are state-changing — the focus-race fix lives here.
    fn advances_state(self) -> bool {
        matches!(self, Self::Suppressed | Self::Emit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounced_file_copy_is_retried_without_another_pasteboard_change() {
        let mut gate = PasteboardPollGate::default();
        assert!(gate.should_read(10));
        assert!(!gate.should_read(10));
        assert!(gate.should_read(11));
        let decision = PollDecision::classify(true, Some(Duration::from_millis(100)), false, false);
        gate.retry = decision == PollDecision::Debounced;
        assert!(
            gate.should_read(11),
            "latest copy must survive the debounce window"
        );
        let decision = PollDecision::classify(true, Some(Duration::from_millis(300)), false, false);
        assert_eq!(decision, PollDecision::Emit);
        gate.retry = decision == PollDecision::Debounced;
        assert!(!gate.should_read(11));
    }

    #[test]
    fn pasteboard_read_gate_is_independent_for_each_monitor() {
        let mut first = PasteboardPollGate::default();
        let mut second = PasteboardPollGate::default();
        assert!(first.should_read(42));
        assert!(second.should_read(42));
        assert!(!first.should_read(42));
        assert!(!second.should_read(42));
    }

    #[cfg(windows)]
    #[test]
    fn native_file_path_normalization_does_not_emit_remote_copy_again() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let canonical = std::fs::canonicalize(file.path()).unwrap();
        let native = canonical
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_ascii_lowercase();
        let received = ClipboardEvent::Files {
            paths: vec![canonical],
            owned_root: None,
        };
        let read_back = ClipboardEvent::Files {
            paths: vec![std::path::PathBuf::from(native)],
            owned_root: None,
        };
        assert_eq!(
            clipboard_signature(&received),
            clipboard_signature(&read_back)
        );
    }

    #[tokio::test]
    async fn remote_publication_discards_queued_local_snapshot() {
        let (tx, rx) = mpsc::channel(4);
        let mut monitor = ClipboardMonitor {
            event_rx: rx,
            _event_tx: tx.clone(),
            last_content: Arc::new(Mutex::new(None)),
            last_change: Arc::new(Mutex::new(None)),
            enabled: Arc::new(Mutex::new(true)),
            revision: Arc::new(AtomicU64::new(0)),
        };
        tx.send((
            0,
            CaptureEvent::Input(Event::Clipboard(ClipboardEvent::Text("stale".into()))),
        ))
        .await
        .unwrap();
        monitor.update_last_event(&ClipboardEvent::Text("remote".into()));
        tx.send((
            1,
            CaptureEvent::Input(Event::Clipboard(ClipboardEvent::Text("new local".into()))),
        ))
        .await
        .unwrap();
        assert!(
            matches!(monitor.recv().await, Some(CaptureEvent::Input(Event::Clipboard(ClipboardEvent::Text(s)))) if s == "new local")
        );
    }

    #[test]
    fn classify_unchanged_when_text_matches_last_content() {
        let d = PollDecision::classify(false, Some(Duration::from_secs(60)), false, false);
        assert_eq!(d, PollDecision::Unchanged);
        assert!(!d.advances_state());
    }

    #[test]
    fn classify_debounced_when_recent_change() {
        // Content differs but only 100ms since last advance —
        // hold off so a peer's set_clipboard echo doesn't bounce
        // back as a fresh local emit.
        let d = PollDecision::classify(true, Some(Duration::from_millis(100)), false, false);
        assert_eq!(d, PollDecision::Debounced);
        assert!(!d.advances_state());
    }

    #[test]
    fn classify_emit_when_changed_unsuppressed() {
        let d = PollDecision::classify(true, Some(Duration::from_secs(1)), false, false);
        assert_eq!(d, PollDecision::Emit);
        assert!(d.advances_state());
    }

    #[test]
    fn classify_emit_on_first_change() {
        // No prior advance: the first poll always falls through
        // to Emit (assuming nothing's suppressed) so the very
        // first clipboard read after startup is broadcast.
        let d = PollDecision::classify(true, None, false, false);
        assert_eq!(d, PollDecision::Emit);
    }

    #[test]
    fn classify_suppressed_when_concealed() {
        let d = PollDecision::classify(true, Some(Duration::from_secs(1)), true, false);
        assert_eq!(d, PollDecision::Suppressed);
    }

    #[test]
    fn classify_suppressed_when_app_list_matches() {
        let d = PollDecision::classify(true, Some(Duration::from_secs(1)), false, true);
        assert_eq!(d, PollDecision::Suppressed);
    }

    /// The focus-race invariant: a Suppressed decision MUST tell
    /// the caller to advance `last_content`. If this assert
    /// fails, the regression we hit live (1Password password
    /// leaks on Ghostty alt-tab after copy) is back.
    #[test]
    fn suppressed_decision_advances_state() {
        let d = PollDecision::classify(true, Some(Duration::from_secs(1)), false, true);
        assert!(
            d.advances_state(),
            "Suppressed must advance last_content; otherwise the \
             same content gets re-checked on the next poll and a \
             focus shift between polls will leak the password."
        );
    }

    /// Companion: Unchanged and Debounced must NOT advance state.
    /// Advancing on Debounced would defeat the 200 ms window and
    /// turn every peer-driven clipboard sync into a fresh local
    /// emit (echo loop).
    #[test]
    fn non_state_changing_decisions_do_not_advance() {
        for (changed, elapsed) in [
            (false, Some(Duration::from_secs(1))),
            (true, Some(Duration::from_millis(50))),
        ] {
            let d = PollDecision::classify(changed, elapsed, false, false);
            assert!(
                !d.advances_state(),
                "{d:?} should not advance state (changed={changed:?} elapsed={elapsed:?})"
            );
        }
    }

    #[test]
    fn content_might_emit_skips_unchanged_and_debounced() {
        // Used by the poll loop to short-circuit the expensive
        // suppression probes. Same matrix as `classify`'s "no
        // emission possible" cases.
        assert!(!PollDecision::content_might_emit(false, None));
        assert!(!PollDecision::content_might_emit(
            true,
            Some(Duration::from_millis(50))
        ));
        assert!(PollDecision::content_might_emit(
            true,
            Some(Duration::from_secs(1))
        ));
        assert!(PollDecision::content_might_emit(true, None));
    }

    #[test]
    fn signatures_distinguish_clipboard_kinds_and_ignore_staging_owner() {
        let staging = tempfile::tempdir().unwrap();
        let files = ClipboardEvent::Files {
            paths: vec!["/tmp/a".into()],
            owned_root: Some(input_event::ClipboardFileLease::new(
                staging.path().to_path_buf(),
            )),
        };
        let same_files = ClipboardEvent::Files {
            paths: vec!["/tmp/a".into()],
            owned_root: None,
        };
        assert_eq!(
            clipboard_signature(&files),
            clipboard_signature(&same_files)
        );
        assert_ne!(
            clipboard_signature(&files),
            clipboard_signature(&ClipboardEvent::Text("/tmp/a".into()))
        );
    }
}

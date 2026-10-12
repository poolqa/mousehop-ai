use super::*;
#[cfg(windows)]
use crate::windows::{Handle, SecurityDescriptor, process_sid, require_system};
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{mpsc, oneshot};
#[cfg(windows)]
use windows::Win32::{
    Foundation::HANDLE,
    System::{
        Pipes::GetNamedPipeClientProcessId,
        Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};

static LINK: OnceLock<Arc<Link>> = OnceLock::new();
pub fn current() -> Option<Arc<Link>> {
    LINK.get().cloned()
}
pub fn initialize() -> Arc<Link> {
    LINK.get_or_init(|| Arc::new(Link::default())).clone()
}
pub fn pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\Mousehop.Desktop.v2.{session}")
}

fn clipboard_matches_event(clipboard: &Clipboard, event: &ClipboardEvent) -> bool {
    match (clipboard, event) {
        (
            Clipboard::Files(actual),
            ClipboardEvent::Files {
                paths: expected, ..
            },
        ) => {
            actual.len() == expected.len()
                && actual.iter().zip(expected).all(|(a, b)| {
                    input_event::windows_clipboard_path_key(a)
                        == input_event::windows_clipboard_path_key(b)
                })
        }
        _ => Clipboard::from_event(event) == *clipboard,
    }
}

#[derive(Default)]
struct State {
    generation: u64,
    secure: bool,
    ready: bool,
    sender: Option<mpsc::Sender<Request>>,
    captures: HashMap<u8, Option<CrossingModifier>>,
    capture_tx: Option<mpsc::Sender<(u64, u8, Capture)>>,
    clipboard_tx: Option<mpsc::Sender<(u64, ClipboardEvent)>>,
    // The desktop helper may publish before Service::new installs its receiver.
    startup_clipboard: Option<(u64, ClipboardEvent)>,
    bounds: Option<(u32, u32)>,
    serial: u64,
    replies: HashMap<u64, oneshot::Sender<bool>>,
    // Keep downloaded file trees alive while the helper's clipboard references them.
    clipboard_lease: Option<ClipboardEvent>,
    clipboard_revision: u64,
    pending_clipboards: HashMap<u64, ClipboardEvent>,
    // A helper may have applied a publication before its acknowledgement was lost.
    unconfirmed_clipboards: Vec<ClipboardEvent>,
    pending_releases: Vec<u32>,
}
#[derive(Default)]
pub struct Link {
    state: Mutex<State>,
}
impl Link {
    pub fn generation(&self) -> u64 {
        self.state.lock().unwrap().generation
    }
    pub fn secure(&self) -> bool {
        let s = self.state.lock().unwrap();
        !s.ready || s.secure
    }
    pub fn ready(&self) -> bool {
        self.state.lock().unwrap().ready
    }
    pub fn bounds(&self) -> Option<(u32, u32)> {
        self.state.lock().unwrap().bounds
    }
    pub fn subscribe_capture(&self) -> mpsc::Receiver<(u64, u8, Capture)> {
        let (tx, rx) = mpsc::channel(1024);
        self.state.lock().unwrap().capture_tx = Some(tx);
        rx
    }
    pub fn subscribe_clipboard(&self) -> mpsc::Receiver<(u64, ClipboardEvent)> {
        let (tx, rx) = mpsc::channel(8);
        let mut s = self.state.lock().unwrap();
        if let Some(report) = s.startup_clipboard.take() {
            let _ = tx.try_send(report);
            log::debug!("Replaying buffered desktop clipboard report to network service");
        }
        s.clipboard_tx = Some(tx);
        rx
    }
    pub fn suspend(&self) {
        let mut s = self.state.lock().unwrap();
        s.generation += 1;
        s.sender = None;
        s.ready = false;
        s.secure = true;
        s.replies.clear();
        let pending = std::mem::take(&mut s.pending_clipboards);
        s.unconfirmed_clipboards.extend(pending.into_values());
        s.clipboard_revision = 0;
        s.startup_clipboard = None;
        if let Some(tx) = s.capture_tx.as_ref() {
            for &edge in s.captures.keys() {
                let _ = tx.try_send((s.generation, edge, Capture::AutoRelease));
            }
        }
    }
    pub fn capture_create(&self, edge: u8) {
        let mut s = self.state.lock().unwrap();
        s.captures.entry(edge).or_insert(None);
        if let Some(tx) = s.sender.as_ref() {
            let _ = tx.try_send(Request::CaptureCreate(edge));
        }
    }
    pub fn capture_destroy(&self, edge: u8) {
        let mut s = self.state.lock().unwrap();
        s.captures.remove(&edge);
        if let Some(tx) = s.sender.as_ref() {
            let _ = tx.try_send(Request::CaptureDestroy(edge));
        }
    }
    pub fn modifier(&self, edge: u8, modifier: Option<CrossingModifier>) {
        let mut s = self.state.lock().unwrap();
        s.captures.insert(edge, modifier);
        if let Some(tx) = s.sender.as_ref() {
            let _ = tx.try_send(Request::CrossingModifier(edge, modifier));
        }
    }
    pub fn send(&self, request: Request) {
        let s = self.state.lock().unwrap();
        if let Some(tx) = s.sender.as_ref() {
            if tx.try_send(request).is_err() {
                log::warn!("Desktop helper command queue unavailable; dropping input");
            }
        }
    }
    pub fn release_keys(&self, keys: Vec<u32>) {
        let mut s = self.state.lock().unwrap();
        if let Some(tx) = &s.sender {
            if tx.try_send(Request::ReleaseKeys(keys.clone())).is_ok() {
                return;
            }
        }
        s.pending_releases.extend(keys);
    }
    pub fn clipboard(&self, event: ClipboardEvent) {
        let mut s = self.state.lock().unwrap();
        if s.ready && !s.secure {
            if let Some(tx) = s.sender.clone() {
                s.serial += 1;
                let id = s.serial;
                let request = Request::Clipboard {
                    id,
                    clipboard: Clipboard::from_event(&event),
                };
                if tx.try_send(request).is_ok() {
                    s.startup_clipboard = None;
                    // Keep both the current clipboard and queued publications alive
                    // until the helper reports whether each publication succeeded.
                    s.clipboard_revision = id;
                    s.pending_clipboards.insert(id, event);
                } else {
                    log::warn!("Desktop helper command queue unavailable; dropping clipboard");
                }
            }
        }
    }
    pub async fn complete(&self, request: impl FnOnce(u64) -> Request) -> bool {
        let rx = {
            let mut s = self.state.lock().unwrap();
            // A desktop transition is a brief input gap, not a transport failure.
            let Some(tx) = s.sender.clone() else {
                return false;
            };
            s.serial += 1;
            let id = s.serial;
            let (tx_reply, rx) = oneshot::channel();
            if tx.try_send(request(id)).is_err() {
                return false;
            }
            s.replies.insert(id, tx_reply);
            (id, rx)
        };
        let ok = tokio::time::timeout(Duration::from_secs(1), rx.1)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false);
        self.state.lock().unwrap().replies.remove(&rx.0);
        ok
    }
    fn attach(
        &self,
        generation: u64,
        secure: bool,
        bounds: Option<(u32, u32)>,
        tx: mpsc::Sender<Request>,
    ) -> bool {
        let mut s = self.state.lock().unwrap();
        if s.generation != generation {
            return false;
        }
        if !s.pending_releases.is_empty() {
            let _ = tx.try_send(Request::ReleaseKeys(std::mem::take(
                &mut s.pending_releases,
            )));
        }
        for (&edge, &modifier) in &s.captures {
            let _ = tx.try_send(Request::CaptureCreate(edge));
            let _ = tx.try_send(Request::CrossingModifier(edge, modifier));
        }
        s.sender = Some(tx);
        s.secure = secure;
        s.ready = true;
        s.bounds = bounds;
        true
    }
    fn response(&self, generation: u64, response: Response) {
        let mut s = self.state.lock().unwrap();
        if s.generation != generation || !s.ready {
            return;
        }
        match response {
            Response::Geometry(b) => s.bounds = b,
            Response::Capture { edge, event } => {
                if s.secure && matches!(event, Capture::Begin | Capture::Input(_)) {
                    return;
                }
                if let Some(tx) = s.capture_tx.as_ref() {
                    let _ = tx.try_send((generation, edge, event));
                }
            }
            Response::Clipboard {
                revision,
                clipboard,
            } => {
                if !s.secure && revision == s.clipboard_revision {
                    let retained = s
                        .clipboard_lease
                        .iter()
                        .chain(s.unconfirmed_clipboards.iter())
                        .find(|lease| clipboard_matches_event(&clipboard, lease))
                        .cloned();
                    if retained.is_none()
                        && s.clipboard_lease
                            .as_ref()
                            .is_some_and(|event| matches!(event, ClipboardEvent::Files { .. }))
                    {
                        log::debug!(
                            "Desktop clipboard changed; releasing staging lease (expected {:?}, actual {:?})",
                            s.clipboard_lease.as_ref().map(Clipboard::from_event),
                            clipboard
                        );
                    }
                    s.unconfirmed_clipboards.clear();
                    s.clipboard_lease = retained.clone();
                    let event = retained.unwrap_or_else(|| clipboard.into_event());
                    if let Some(tx) = s.clipboard_tx.as_ref() {
                        // Forwarding must retain the same staging lease as the clipboard.
                        let _ = tx.try_send((generation, event));
                    } else {
                        s.startup_clipboard = Some((generation, event));
                        log::debug!(
                            "Buffering desktop clipboard report until network service subscribes"
                        );
                    }
                }
            }
            Response::ClipboardApplied { id, ok } => {
                if let Some(event) = s.pending_clipboards.remove(&id) {
                    if ok {
                        match &event {
                            ClipboardEvent::Files { paths, .. } => log::debug!(
                                "Desktop helper applied clipboard publication {id}: files, {} item(s)",
                                paths.len()
                            ),
                            ClipboardEvent::ImageRgba { width, height, .. } => log::debug!(
                                "Desktop helper applied clipboard publication {id}: image {width}x{height}"
                            ),
                            ClipboardEvent::Text(text) => log::debug!(
                                "Desktop helper applied clipboard publication {id}: text, {} bytes",
                                text.len()
                            ),
                        }
                        s.clipboard_lease = Some(event);
                    } else {
                        log::warn!("Desktop helper failed to apply clipboard publication {id}");
                    }
                }
            }
            Response::Complete { id, ok } => {
                if let Some(tx) = s.replies.remove(&id) {
                    let _ = tx.send(ok);
                }
            }
            Response::Ready { .. } => {}
        }
    }
}

#[cfg(windows)]
pub fn server(session: u32, first: bool) -> io::Result<NamedPipeServer> {
    require_system()?;
    let descriptor = SecurityDescriptor::new("D:P(A;;GA;;;SY)")?;
    let mut attrs = descriptor.attributes();
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                pipe_name(session),
                (&mut attrs as *mut windows::Win32::Security::SECURITY_ATTRIBUTES).cast(),
            )
    }
}
#[cfg(windows)]
fn verify_client(pipe: &NamedPipeServer) -> io::Result<()> {
    unsafe {
        let mut pid = 0;
        GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle()), &mut pid)?;
        let process = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?);
        if process_sid(process.0)? != "S-1-5-18" {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "desktop helper is not SYSTEM",
            ));
        }
    }
    Ok(())
}
#[cfg(windows)]
pub async fn serve(session: u32, link: Arc<Link>) -> io::Result<()> {
    let mut pending = server(session, true)?;
    loop {
        pending.connect().await?;
        let connected = pending;
        pending = server(session, false)?;
        let link = link.clone();
        tokio::task::spawn_local(async move {
            let generation = link.generation();
            if let Err(error) = serve_connection(connected, link.clone(), generation).await {
                log::debug!("Desktop helper pipe closed: {error}");
            }
            if link.generation() == generation {
                link.suspend();
            }
        });
    }
}
#[cfg(windows)]
async fn serve_connection(
    mut pipe: NamedPipeServer,
    link: Arc<Link>,
    generation: u64,
) -> io::Result<()> {
    verify_client(&pipe)?;
    let hello = tokio::time::timeout(Duration::from_secs(5), read_frame::<_, Response>(&mut pipe))
        .await
        .map_err(io::Error::other)??;
    let Response::Ready { secure, bounds } = hello else {
        return Err(io::Error::other("expected desktop helper Ready"));
    };
    let (tx, mut rx) = mpsc::channel(1024);
    if !link.attach(generation, secure, bounds, tx) {
        return Ok(());
    }
    log::info!("Desktop helper attached (secure={secure}); DTLS sessions retained");
    let (mut reader, mut writer) = tokio::io::split(pipe);
    let read = async {
        loop {
            link.response(generation, read_frame::<_, Response>(&mut reader).await?);
        }
        #[allow(unreachable_code)]
        Ok::<(), io::Error>(())
    };
    let write = async {
        while let Some(request) = rx.recv().await {
            write_frame(&mut writer, &request).await?;
        }
        Ok::<(), io::Error>(())
    };
    tokio::select! { r=read=>r, r=write=>r }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_clipboard_report_reaches_late_subscriber() {
        let link = Link::default();
        let (tx, _requests) = mpsc::channel(8);
        assert!(link.attach(0, false, None, tx));
        let files = vec![std::path::PathBuf::from(r"C:\Users\admin\test.txt")];
        link.response(
            0,
            Response::Clipboard {
                revision: 0,
                clipboard: Clipboard::Files(files.clone()),
            },
        );
        let mut captured = link.subscribe_clipboard();
        assert!(
            matches!(captured.try_recv().unwrap(), (0, ClipboardEvent::Files { paths, .. }) if paths == files)
        );
        assert!(captured.try_recv().is_err());
    }

    #[test]
    fn startup_clipboard_buffer_keeps_latest_and_drops_on_desktop_change() {
        let link = Link::default();
        let (tx, _requests) = mpsc::channel(8);
        assert!(link.attach(0, false, None, tx));
        for text in ["old", "new"] {
            link.response(
                0,
                Response::Clipboard {
                    revision: 0,
                    clipboard: Clipboard::Text(text.into()),
                },
            );
        }
        let mut captured = link.subscribe_clipboard();
        assert!(matches!(captured.try_recv().unwrap(), (0, ClipboardEvent::Text(s)) if s == "new"));
        assert!(captured.try_recv().is_err());
        let link = Link::default();
        let (tx, _requests) = mpsc::channel(8);
        assert!(link.attach(0, false, None, tx));
        link.response(
            0,
            Response::Clipboard {
                revision: 0,
                clipboard: Clipboard::Text("old desktop".into()),
            },
        );
        link.suspend();
        assert!(link.subscribe_clipboard().try_recv().is_err());
    }

    #[test]
    fn remote_publication_supersedes_buffered_startup_clipboard() {
        let link = Link::default();
        let (tx, _requests) = mpsc::channel(8);
        assert!(link.attach(0, false, None, tx));
        link.response(
            0,
            Response::Clipboard {
                revision: 0,
                clipboard: Clipboard::Text("local before startup".into()),
            },
        );
        link.clipboard(ClipboardEvent::Text("remote".into()));
        assert!(link.subscribe_clipboard().try_recv().is_err());
    }

    fn staged_file() -> (std::path::PathBuf, ClipboardEvent) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "mousehop-lease-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        std::fs::create_dir(&root).unwrap();
        let file = root.join("test.txt");
        std::fs::write(&file, b"received file").unwrap();
        let event = ClipboardEvent::Files {
            paths: vec![file],
            owned_root: Some(input_event::ClipboardFileLease::new(root.clone())),
        };
        (root, event)
    }

    #[test]
    fn native_path_matching_handles_unc_and_preserves_distinct_files() {
        assert_eq!(
            input_event::windows_clipboard_path_key(std::path::Path::new(
                r"\\?\UNC\SERVER\Share\file.txt"
            )),
            input_event::windows_clipboard_path_key(std::path::Path::new(
                r"\\server\share\file.txt"
            ))
        );
        assert_eq!(
            input_event::windows_clipboard_path_key(std::path::Path::new(
                "C:/Windows/Temp/file.txt"
            )),
            input_event::windows_clipboard_path_key(std::path::Path::new(
                r"c:\windows\temp\file.txt"
            ))
        );
        assert_ne!(
            input_event::windows_clipboard_path_key(std::path::Path::new(r"C:\Temp\one.txt")),
            input_event::windows_clipboard_path_key(std::path::Path::new(r"C:\Temp\two.txt"))
        );
        assert_ne!(
            input_event::windows_clipboard_path_key(std::path::Path::new(r"C:\Temp\一.txt")),
            input_event::windows_clipboard_path_key(std::path::Path::new(r"C:\Temp\二.txt"))
        );
    }

    #[test]
    fn normalized_native_file_report_retains_staging() {
        let link = Link::default();
        let mut captured = link.subscribe_clipboard();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(link.attach(0, false, None, tx));
        let (root, mut event) = staged_file();
        // arboard publishes GetFinalPathNameByHandleW paths and strips \?\.
        let ClipboardEvent::Files { paths, .. } = &mut event else {
            unreachable!()
        };
        *paths = vec![std::path::PathBuf::from(r"\\?\C:\WINDOWS\Temp\test.txt")];
        let native = Clipboard::Files(vec![std::path::PathBuf::from(r"C:\Windows\Temp\test.txt")]);
        link.clipboard(event);
        let Request::Clipboard { id, .. } = rx.try_recv().unwrap() else {
            panic!("publication")
        };
        link.response(0, Response::ClipboardApplied { id, ok: true });
        link.response(
            0,
            Response::Clipboard {
                revision: id,
                clipboard: native,
            },
        );
        assert!(
            root.join("test.txt").exists(),
            "native path normalization must not release staging"
        );
        let (_, event) = captured.try_recv().unwrap();
        assert!(matches!(
            event,
            ClipboardEvent::Files {
                owned_root: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn stale_clipboard_report_cannot_delete_pending_publication() {
        let link = Link::default();
        let mut captured = link.subscribe_clipboard();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(link.attach(0, false, None, tx));
        let (root, event) = staged_file();
        link.clipboard(event);
        let Request::Clipboard { id, clipboard } = rx.try_recv().unwrap() else {
            panic!("publication")
        };
        link.response(
            0,
            Response::Clipboard {
                revision: 0,
                clipboard: Clipboard::Text("old".into()),
            },
        );
        assert!(root.join("test.txt").exists());
        assert!(captured.try_recv().is_err());
        link.response(0, Response::ClipboardApplied { id, ok: true });
        link.response(
            0,
            Response::Clipboard {
                revision: id,
                clipboard,
            },
        );
        let (_, forwarded) = captured.try_recv().unwrap();
        assert!(matches!(
            &forwarded,
            ClipboardEvent::Files {
                owned_root: Some(_),
                ..
            }
        ));
        // Replacing the clipboard must not remove files still being forwarded.
        link.response(
            0,
            Response::Clipboard {
                revision: id,
                clipboard: Clipboard::Text("new".into()),
            },
        );
        assert!(root.exists());
        drop(forwarded);
        assert!(!root.exists());
    }

    #[test]
    fn failed_publication_preserves_previous_clipboard_files() {
        let link = Link::default();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(link.attach(0, false, None, tx));
        let (old_root, old_event) = staged_file();
        link.clipboard(old_event);
        let Request::Clipboard { id, .. } = rx.try_recv().unwrap() else {
            panic!("publication")
        };
        link.response(0, Response::ClipboardApplied { id, ok: true });
        let (new_root, new_event) = staged_file();
        link.clipboard(new_event);
        let Request::Clipboard { id, .. } = rx.try_recv().unwrap() else {
            panic!("publication")
        };
        assert!(old_root.exists() && new_root.exists());
        link.response(0, Response::ClipboardApplied { id, ok: false });
        assert!(old_root.exists());
        assert!(!new_root.exists());
        drop(link);
        assert!(!old_root.exists());
    }

    #[test]
    fn desktop_replacement_retains_unconfirmed_files_until_current_report() {
        let link = Link::default();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(link.attach(0, false, None, tx));
        let (root, event) = staged_file();
        link.clipboard(event);
        let Request::Clipboard { id, clipboard } = rx.try_recv().unwrap() else {
            panic!("publication")
        };
        link.suspend();
        assert!(root.exists());
        let (tx, _rx) = mpsc::channel(8);
        assert!(link.attach(1, false, None, tx));
        link.response(0, Response::ClipboardApplied { id, ok: true });
        assert!(link.state.lock().unwrap().clipboard_lease.is_none());
        link.response(
            1,
            Response::Clipboard {
                revision: 0,
                clipboard,
            },
        );
        assert!(root.exists());
        link.response(
            1,
            Response::Clipboard {
                revision: 0,
                clipboard: Clipboard::Text("new".into()),
            },
        );
        assert!(!root.exists());
    }

    #[test]
    fn replaced_helper_cannot_publish_or_replay_old_input() {
        let link = Link::default();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(link.attach(0, false, Some((1920, 1080)), tx));
        link.send(Request::Destroy(7));
        assert!(rx.try_recv().is_ok());
        link.suspend();
        assert!(link.secure());
        assert_eq!(link.bounds(), Some((1920, 1080)));
        link.response(0, Response::Geometry(Some((1, 1))));
        assert_eq!(link.bounds(), Some((1920, 1080)));
        let (tx, _) = mpsc::channel(8);
        assert!(!link.attach(0, false, None, tx));
    }
    #[tokio::test]
    async fn replacement_cancels_outstanding_completion_without_waiting_for_timeout() {
        let link = Arc::new(Link::default());
        let (tx, mut rx) = mpsc::channel(8);
        assert!(link.attach(0, false, None, tx));
        let other = link.clone();
        let (ok, ()) = tokio::join!(link.complete(Request::Release), async move {
            rx.recv().await.unwrap();
            other.suspend();
        });
        assert!(!ok);
    }

    #[test]
    fn replacement_replays_configuration_and_key_releases_but_not_input() {
        let link = Link::default();
        link.capture_create(1);
        link.modifier(1, Some(CrossingModifier::Control));
        link.suspend();
        link.release_keys(vec![29]);
        link.send(Request::Input {
            handle: 9,
            input: Input::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: 30,
                state: 1,
            }),
        });
        let (tx, mut rx) = mpsc::channel(16);
        assert!(link.attach(1, true, Some((2560, 1080)), tx));
        assert!(matches!(rx.try_recv().unwrap(), Request::ReleaseKeys(keys) if keys == vec![29]));
        assert!(matches!(rx.try_recv().unwrap(), Request::CaptureCreate(1)));
        assert!(matches!(
            rx.try_recv().unwrap(),
            Request::CrossingModifier(1, Some(CrossingModifier::Control))
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn secure_and_replaced_helpers_cannot_publish_clipboard() {
        let link = Link::default();
        let mut clipboard = link.subscribe_clipboard();
        let (tx, _) = mpsc::channel(16);
        assert!(link.attach(0, true, None, tx));
        link.response(
            0,
            Response::Clipboard {
                revision: 0,
                clipboard: Clipboard::Text("secret".into()),
            },
        );
        assert!(clipboard.try_recv().is_err());
        link.suspend();
        let (tx, _) = mpsc::channel(16);
        assert!(link.attach(1, false, None, tx));
        link.response(
            0,
            Response::Clipboard {
                revision: 0,
                clipboard: Clipboard::Text("stale".into()),
            },
        );
        assert!(clipboard.try_recv().is_err());
        link.response(
            1,
            Response::Clipboard {
                revision: 0,
                clipboard: Clipboard::Text("current".into()),
            },
        );
        assert!(
            matches!(clipboard.try_recv().unwrap(), (1,ClipboardEvent::Text(s)) if s == "current")
        );
    }
}

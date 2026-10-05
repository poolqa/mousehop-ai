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
    format!(r"\\.\pipe\Mousehop.Desktop.v1.{session}")
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
    bounds: Option<(u32, u32)>,
    serial: u64,
    replies: HashMap<u64, oneshot::Sender<bool>>,
    // Keep downloaded file trees alive while the helper's clipboard references them.
    clipboard_lease: Option<ClipboardEvent>,
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
        self.state.lock().unwrap().clipboard_tx = Some(tx);
        rx
    }
    pub fn suspend(&self) {
        let mut s = self.state.lock().unwrap();
        s.generation += 1;
        s.sender = None;
        s.ready = false;
        s.secure = true;
        s.replies.clear();
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
            if let Some(tx) = s.sender.as_ref() {
                if tx
                    .try_send(Request::Clipboard(Clipboard::from_event(&event)))
                    .is_ok()
                {
                    s.clipboard_lease = Some(event);
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
            Response::Clipboard(c) => {
                if !s.secure {
                    if s.clipboard_lease
                        .as_ref()
                        .is_some_and(|lease| Clipboard::from_event(lease) != c)
                    {
                        s.clipboard_lease = None;
                    }
                    if let Some(tx) = s.clipboard_tx.as_ref() {
                        let _ = tx.try_send((generation, c.into_event()));
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
        link.response(0, Response::Clipboard(Clipboard::Text("secret".into())));
        assert!(clipboard.try_recv().is_err());
        link.suspend();
        let (tx, _) = mpsc::channel(16);
        assert!(link.attach(1, false, None, tx));
        link.response(0, Response::Clipboard(Clipboard::Text("stale".into())));
        assert!(clipboard.try_recv().is_err());
        link.response(1, Response::Clipboard(Clipboard::Text("current".into())));
        assert!(
            matches!(clipboard.try_recv().unwrap(), (1,ClipboardEvent::Text(s)) if s == "current")
        );
    }
}

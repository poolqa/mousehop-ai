use futures::{Stream, StreamExt, stream::SelectAll};
#[cfg(unix)]
use std::io::ErrorKind;
#[cfg(unix)]
use std::path::PathBuf;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::LinesStream;

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer as PipeListener;

use crate::{FrontendEvent, FrontendRequest, IpcError, IpcListenerCreationError};

type RequestStream = Pin<Box<dyn Stream<Item = (bool, Result<FrontendRequest, IpcError>)> + Send>>;

const FRONTEND_QUEUE_CAPACITY: usize = 128;

struct OutgoingFrame {
    bytes: Arc<[u8]>,
    written: Option<oneshot::Sender<()>>,
}

struct FrontendWriter {
    frames: mpsc::Sender<OutgoingFrame>,
    task: tokio::task::JoinHandle<()>,
    close_reader: Option<oneshot::Sender<()>>,
}

impl FrontendWriter {
    fn new(writer: impl AsyncWrite + Unpin + Send + 'static) -> (Self, oneshot::Receiver<()>) {
        let (frames, mut incoming) = mpsc::channel::<OutgoingFrame>(FRONTEND_QUEUE_CAPACITY);
        let (close_reader, closed) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut writer = writer;
            while let Some(frame) = incoming.recv().await {
                if writer.write_all(&frame.bytes).await.is_err() {
                    break;
                }
                if let Some(written) = frame.written {
                    let _ = written.send(());
                }
            }
        });
        (
            Self {
                frames,
                task,
                close_reader: Some(close_reader),
            },
            closed,
        )
    }
}

impl Drop for FrontendWriter {
    fn drop(&mut self) {
        // Close both pipe directions, including a writer blocked by a native
        // menu's stalled client. A partially written frame is never reused.
        self.task.abort();
        self.close_reader.take();
    }
}

fn broadcast_frame(writers: &mut Vec<FrontendWriter>, frame: Arc<[u8]>) {
    writers.retain(|writer| {
        if writer
            .frames
            .try_send(OutgoingFrame {
                bytes: frame.clone(),
                written: None,
            })
            .is_ok()
        {
            true
        } else {
            log::warn!("Disconnecting stalled or closed frontend; Core input remains responsive");
            false
        }
    });
}

pub struct AsyncFrontendListener {
    #[cfg(windows)]
    listener: PipeListener,
    #[cfg(unix)]
    listener: UnixListener,
    #[cfg(unix)]
    socket_path: PathBuf,
    line_streams: SelectAll<RequestStream>,
    tx_streams: Vec<FrontendWriter>,
}

impl AsyncFrontendListener {
    pub async fn new() -> Result<Self, IpcListenerCreationError> {
        #[cfg(unix)]
        let (socket_path, listener) = {
            let socket_path = crate::default_socket_path()?;

            log::debug!("remove socket: {socket_path:?}");
            if socket_path.exists() {
                // try to connect to see if some other instance
                // of mousehop is already running
                match UnixStream::connect(&socket_path).await {
                    // connected -> mousehop is already running
                    Ok(_) => return Err(IpcListenerCreationError::AlreadyRunning),
                    // mousehop is not running but a socket was left behind
                    Err(e) => {
                        log::debug!("{socket_path:?}: {e} - removing left behind socket");
                        let _ = std::fs::remove_file(&socket_path);
                    }
                }
            }
            let listener = match UnixListener::bind(&socket_path) {
                Ok(ls) => ls,
                // some other mousehop instance has bound the socket in the meantime
                Err(e) if e.kind() == ErrorKind::AddrInUse => {
                    return Err(IpcListenerCreationError::AlreadyRunning);
                }
                Err(e) => return Err(IpcListenerCreationError::Bind(e)),
            };
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
                .map_err(IpcListenerCreationError::Bind)?;
            (socket_path, listener)
        };

        #[cfg(windows)]
        let listener = crate::windows::server(true).map_err(IpcListenerCreationError::Bind)?;

        let adapter = Self {
            listener,
            #[cfg(unix)]
            socket_path,
            line_streams: SelectAll::new(),
            tx_streams: vec![],
        };

        Ok(adapter)
    }

    pub async fn broadcast(&mut self, notify: FrontendEvent) {
        // encode event
        let mut json = serde_json::to_string(&notify).unwrap();
        json.push('\n');

        // A GUI or tray menu may stop draining its socket. Never await that
        // client on Core's loop, which also handles desktop liveness/control.
        broadcast_frame(&mut self.tx_streams, json.into_bytes().into());
        // Give healthy writer tasks a turn during a large initial state sync.
        tokio::task::yield_now().await;
    }

    /// Deliver queued final state before shutdown, with a bounded total wait.
    pub async fn flush(&mut self) {
        let mut pending = Vec::new();
        for writer in &self.tx_streams {
            let (written, done) = oneshot::channel();
            if writer
                .frames
                .try_send(OutgoingFrame {
                    bytes: Arc::from([]),
                    written: Some(written),
                })
                .is_ok()
            {
                pending.push(done);
            }
        }
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            futures::future::join_all(pending),
        )
        .await;
    }
}

#[cfg(unix)]
impl Drop for AsyncFrontendListener {
    fn drop(&mut self) {
        log::debug!("remove socket: {:?}", self.socket_path);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Stream for AsyncFrontendListener {
    type Item = Result<FrontendRequest, IpcError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while let Poll::Ready(Some((privileged, line))) = self.line_streams.poll_next_unpin(cx) {
            let request = line;
            match &request {
                Ok(request) if !privileged && request.requires_admin() => {
                    log::warn!("Rejected non-elevated machine configuration request");
                    continue;
                }
                _ => return Poll::Ready(Some(request)),
            }
        }
        let mut sync = false;
        loop {
            #[cfg(unix)]
            let stream = match self.listener.poll_accept(cx) {
                Poll::Ready(Ok((stream, _))) => stream,
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                Poll::Pending => break,
            };
            #[cfg(windows)]
            let stream = {
                use std::future::Future;
                let connected = {
                    let future = self.listener.connect();
                    std::pin::pin!(future).poll(cx)
                };
                match connected {
                    Poll::Ready(Ok(())) => {}
                    Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                    Poll::Pending => break,
                }
                let next = match crate::windows::server(false) {
                    Ok(next) => next,
                    Err(e) => return Poll::Ready(Some(Err(e.into()))),
                };
                std::mem::replace(&mut self.listener, next)
            };
            #[cfg(windows)]
            let privileged = if crate::windows::service_mode() {
                match crate::windows::client_is_admin(&stream) {
                    Ok(privileged) => privileged,
                    Err(error) => {
                        log::warn!("Rejected desktop IPC connection: {error}");
                        continue;
                    }
                }
            } else {
                true
            };
            #[cfg(unix)]
            let privileged = true;
            let (rx, tx) = tokio::io::split(stream);
            let (writer, closed) = FrontendWriter::new(tx);
            let buf_reader = BufReader::new(rx);
            let lines = buf_reader.lines();
            let lines = LinesStream::new(lines);
            let mut negotiated = false;
            self.line_streams.push(
                lines
                    .take_until(async move {
                        let _ = closed.await;
                    })
                    .map(move |line| {
                        let request = line
                            .map_err(IpcError::from)
                            .and_then(|line| decode_request(&line, &mut negotiated));
                        (privileged, request)
                    })
                    .boxed(),
            );
            self.tx_streams.push(writer);
            sync = true;
        }
        if sync {
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod writer_tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn stalled_menu_client_cannot_block_another_client_or_broadcast() {
        let (slow, _not_reading) = tokio::io::duplex(16);
        let (fast, mut reading) = tokio::io::duplex(4096);
        let (slow, slow_closed) = FrontendWriter::new(slow);
        let (fast, mut fast_closed) = FrontendWriter::new(fast);
        let mut writers = vec![slow, fast];
        let frame: Arc<[u8]> = vec![b'x'; 128].into();
        // Simulate a menu suspending its Agent while the other frontend drains.
        for _ in 0..FRONTEND_QUEUE_CAPACITY + 2 {
            broadcast_frame(&mut writers, frame.clone());
            let mut received = [0; 128];
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                reading.read_exact(&mut received),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(received, *frame);
        }
        assert_eq!(writers.len(), 1);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), slow_closed)
                .await
                .unwrap()
                .is_err()
        );
        assert!(matches!(
            fast_closed.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        drop(writers);
        assert!(fast_closed.await.is_err());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use crate::{FrontendEvent, connect_async};
    use std::time::Duration;

    #[tokio::test]
    async fn pipe_roundtrip_multiple_clients_and_exclusive_listener() {
        let mut listener = AsyncFrontendListener::new().await.unwrap();
        assert!(AsyncFrontendListener::new().await.is_err());
        let (mut first, mut writer) = connect_async(Some(Duration::from_secs(2))).await.unwrap();
        assert!(matches!(
            listener.next().await,
            Some(Ok(FrontendRequest::Sync))
        ));
        let (mut second, _) = connect_async(Some(Duration::from_secs(2))).await.unwrap();
        assert!(matches!(
            listener.next().await,
            Some(Ok(FrontendRequest::Sync))
        ));
        writer.request(FrontendRequest::Enumerate()).await.unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), listener.next())
                .await
                .unwrap(),
            Some(Ok(FrontendRequest::Enumerate()))
        ));
        listener
            .broadcast(FrontendEvent::Error("roundtrip".into()))
            .await;
        for reader in [&mut first, &mut second] {
            assert!(
                matches!(tokio::time::timeout(Duration::from_secs(2), reader.next()).await.unwrap(), Some(Ok(FrontendEvent::Error(value))) if value == "roundtrip")
            );
        }

        // GTK starts a blocking reader before sending button requests. A
        // synchronous Windows file handle deadlocks writes behind that read.
        let (mut reader, mut writer) = crate::connect_once().unwrap();
        assert!(matches!(
            listener.next().await,
            Some(Ok(FrontendRequest::Sync))
        ));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let reading = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            reader.next_event()
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let writing = std::thread::spawn(move || writer.request(FrontendRequest::Create));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), listener.next())
                .await
                .unwrap(),
            Some(Ok(FrontendRequest::Create))
        ));
        writing.join().unwrap().unwrap();
        listener
            .broadcast(FrontendEvent::Error("concurrent read/write".into()))
            .await;
        assert!(
            matches!(reading.join().unwrap(), Some(Ok(FrontendEvent::Error(value))) if value == "concurrent read/write")
        );
    }
}

fn decode_request(line: &str, negotiated: &mut bool) -> Result<FrontendRequest, IpcError> {
    if line.len() > 16 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "IPC command exceeds limit",
        )
        .into());
    }
    let request: FrontendRequest = serde_json::from_str(line)?;
    if let FrontendRequest::Hello { version } = request {
        *negotiated = version == crate::LOCAL_PROTOCOL_VERSION;
        if !*negotiated {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Incompatible Mousehop IPC version",
            )
            .into());
        }
        return Ok(FrontendRequest::Sync);
    }
    if !*negotiated {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "IPC version handshake required",
        )
        .into());
    }
    Ok(request)
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    #[test]
    fn old_or_unnegotiated_frontends_cannot_mutate_runtime() {
        let mut negotiated = false;
        assert!(decode_request("\"Create\"", &mut negotiated).is_err());
        let old = serde_json::to_string(&FrontendRequest::Hello {
            version: crate::LOCAL_PROTOCOL_VERSION - 1,
        })
        .unwrap();
        assert!(decode_request(&old, &mut negotiated).is_err());
        assert!(decode_request("{\"SetSharing\":true}", &mut negotiated).is_err());
        let current = serde_json::to_string(&FrontendRequest::Hello {
            version: crate::LOCAL_PROTOCOL_VERSION,
        })
        .unwrap();
        assert!(matches!(
            decode_request(&current, &mut negotiated),
            Ok(FrontendRequest::Sync)
        ));
        assert!(matches!(
            decode_request("{\"SetSharing\":false}", &mut negotiated),
            Ok(FrontendRequest::SetSharing(false))
        ));
        // A failed renegotiation must revoke a previously successful handshake.
        assert!(decode_request(&old, &mut negotiated).is_err());
        assert!(decode_request("\"Quit\"", &mut negotiated).is_err());
    }
}

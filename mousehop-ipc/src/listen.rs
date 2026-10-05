use futures::{Stream, StreamExt, stream::SelectAll};
#[cfg(unix)]
use std::io::ErrorKind;
#[cfg(unix)]
use std::path::PathBuf;
use std::{
    pin::Pin,
    task::{Context, Poll},
};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio_stream::wrappers::LinesStream;

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer as PipeListener;
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer as PipeStream;

use crate::{FrontendEvent, FrontendRequest, IpcError, IpcListenerCreationError};

pub struct AsyncFrontendListener {
    #[cfg(windows)]
    listener: PipeListener,
    #[cfg(unix)]
    listener: UnixListener,
    #[cfg(unix)]
    socket_path: PathBuf,
    #[cfg(unix)]
    line_streams: SelectAll<LinesStream<BufReader<ReadHalf<UnixStream>>>>,
    #[cfg(windows)]
    line_streams: SelectAll<LinesStream<BufReader<ReadHalf<PipeStream>>>>,
    #[cfg(unix)]
    tx_streams: Vec<WriteHalf<UnixStream>>,
    #[cfg(windows)]
    tx_streams: Vec<WriteHalf<PipeStream>>,
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

        let mut keep = vec![];
        // TODO do simultaneously
        for tx in self.tx_streams.iter_mut() {
            // write len + payload
            if tx.write_all(json.as_bytes()).await.is_err() {
                keep.push(false);
                continue;
            }
            keep.push(true);
        }

        // could not find a better solution because async
        let mut keep = keep.into_iter();
        self.tx_streams.retain(|_| keep.next().unwrap());
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
        if let Poll::Ready(Some(Ok(l))) = self.line_streams.poll_next_unpin(cx) {
            let request = serde_json::from_str(l.as_str()).map_err(|e| e.into());
            return Poll::Ready(Some(request));
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
            let (rx, tx) = tokio::io::split(stream);
            let buf_reader = BufReader::new(rx);
            let lines = buf_reader.lines();
            let lines = LinesStream::new(lines);
            self.line_streams.push(lines);
            self.tx_streams.push(tx);
            sync = true;
        }
        if sync {
            Poll::Ready(Some(Ok(FrontendRequest::Sync)))
        } else {
            Poll::Pending
        }
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

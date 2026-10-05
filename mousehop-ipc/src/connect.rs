use crate::{ConnectionError, FrontendEvent, FrontendRequest, IpcError};
use std::{
    cmp::min,
    io::{self, BufReader, LineWriter, Lines, prelude::*},
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::path::Path;

#[cfg(windows)]
use crate::windows::PipeStream;
#[cfg(windows)]
use std::fs::OpenOptions;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

#[cfg(unix)]
type FrontendStream = UnixStream;
#[cfg(windows)]
type FrontendStream = PipeStream;

pub struct FrontendEventReader {
    #[cfg(unix)]
    lines: Lines<BufReader<UnixStream>>,
    #[cfg(windows)]
    lines: Lines<BufReader<PipeStream>>,
}

pub struct FrontendRequestWriter {
    #[cfg(unix)]
    line_writer: LineWriter<UnixStream>,
    #[cfg(windows)]
    line_writer: LineWriter<PipeStream>,
}

impl FrontendEventReader {
    pub fn next_event(&mut self) -> Option<Result<FrontendEvent, IpcError>> {
        match self.lines.next()? {
            Err(e) => Some(Err(e.into())),
            Ok(l) => Some(serde_json::from_str(l.as_str()).map_err(|e| e.into())),
        }
    }
}

impl FrontendRequestWriter {
    pub fn request(&mut self, request: FrontendRequest) -> Result<(), io::Error> {
        let mut json = serde_json::to_string(&request).unwrap();
        log::debug!("requesting: {json}");
        json.push('\n');
        self.line_writer.write_all(json.as_bytes())?;
        Ok(())
    }
}

pub fn connect() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    frontend_connection(wait_for_service()?)
}

/// Attempt one connection to the frontend endpoint.
///
/// Unlike [`connect`], this returns the first connection error instead of
/// retrying until the service appears. Callers that drive their own retry
/// loop can therefore keep that loop bounded and off latency-sensitive
/// threads.
pub fn connect_once() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    connect_once_to_service()
}

#[cfg(unix)]
fn connect_once_to_service() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError>
{
    let socket_path = crate::default_socket_path()?;
    connect_once_to_path(&socket_path)
}

#[cfg(unix)]
fn connect_once_to_path(
    socket_path: &Path,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    frontend_connection(connect_to_path(socket_path)?)
}

#[cfg(windows)]
fn connect_once_to_service() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError>
{
    frontend_connection(connect_to_service()?)
}

fn frontend_connection(
    rx: FrontendStream,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let tx = rx.try_clone()?;
    let buf_reader = BufReader::new(rx);
    let lines = buf_reader.lines();
    let line_writer = LineWriter::new(tx);
    let reader = FrontendEventReader { lines };
    let writer = FrontendRequestWriter { line_writer };
    Ok((reader, writer))
}

/// wait for the mousehop socket to come online
#[cfg(unix)]
fn wait_for_service() -> Result<UnixStream, ConnectionError> {
    let socket_path = crate::default_socket_path()?;
    let mut duration = Duration::from_millis(10);
    loop {
        if let Ok(stream) = connect_to_path(&socket_path) {
            break Ok(stream);
        }
        // a signaling mechanism or inotify could be used to
        // improve this
        thread::sleep(exponential_back_off(&mut duration));
    }
}

#[cfg(windows)]
fn wait_for_service() -> Result<PipeStream, ConnectionError> {
    let mut duration = Duration::from_millis(10);
    let deadline =
        crate::windows::service_mode().then(|| std::time::Instant::now() + Duration::from_secs(30));
    loop {
        match connect_to_service() {
            Ok(stream) => break Ok(stream),
            Err(ConnectionError::Io(e)) if e.kind() == io::ErrorKind::PermissionDenied => {
                return Err(e.into());
            }
            Err(_) => {}
        }
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Err(io::Error::new(io::ErrorKind::TimedOut,
                "Mousehop input worker did not become ready within 30 seconds. See C:\\ProgramData\\Mousehop\\service.log and panic.log from an administrator terminal.").into());
        }
        thread::sleep(exponential_back_off(&mut duration));
    }
}

#[cfg(unix)]
fn connect_to_path(socket_path: &Path) -> Result<UnixStream, ConnectionError> {
    Ok(UnixStream::connect(socket_path)?)
}

#[cfg(windows)]
fn connect_to_service() -> Result<PipeStream, ConnectionError> {
    // SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION: never lend our
    // elevated token to a pipe server through impersonation.
    let stream = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(0x0010_0000 | 0x0001_0000 | 0x4000_0000)
        .open(crate::windows::pipe_name()?)?;
    crate::windows::verify_server(&stream)?;
    Ok(PipeStream(stream))
}

fn exponential_back_off(duration: &mut Duration) -> Duration {
    let new = duration.saturating_mul(2);
    *duration = min(new, Duration::from_secs(1));
    *duration
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn one_shot_connect_returns_when_socket_is_missing() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos();
        let socket_path = std::env::temp_dir().join(format!(
            "mousehop-ipc-missing-{}-{nonce}.sock",
            std::process::id()
        ));
        assert!(!socket_path.exists());

        let error = match connect_once_to_path(&socket_path) {
            Ok(_) => panic!("missing socket must fail"),
            Err(error) => error,
        };
        match error {
            ConnectionError::Io(error) => assert_eq!(error.kind(), ErrorKind::NotFound),
            other => panic!("unexpected connection error: {other}"),
        }
    }
}

//! Local desktop-helper protocol. Never used on the peer network transport.
use input_event::{ClipboardEvent, CrossingModifier, Event, KeyboardEvent, PointerEvent};
use serde::{Deserialize, Serialize};
use std::{io, path::PathBuf};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// Images use JSON byte arrays here; bound allocations even for a compromised helper.
const MAX_FRAME: usize = 768 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Input {
    Pointer(PointerEvent),
    Keyboard(KeyboardEvent),
}
impl Input {
    pub fn from_event(event: &Event) -> Option<Self> {
        match event {
            Event::Pointer(p) => Some(Self::Pointer(*p)),
            Event::Keyboard(k) => Some(Self::Keyboard(*k)),
            Event::Clipboard(_) => None,
        }
    }
    pub fn into_event(self) -> Event {
        match self {
            Self::Pointer(p) => Event::Pointer(p),
            Self::Keyboard(k) => Event::Keyboard(k),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Clipboard {
    Text(String),
    Image {
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    },
    Files(Vec<PathBuf>),
}
impl Clipboard {
    pub fn from_event(event: &ClipboardEvent) -> Self {
        match event {
            ClipboardEvent::Text(s) => Self::Text(s.clone()),
            ClipboardEvent::ImageRgba {
                width,
                height,
                bytes,
            } => Self::Image {
                width: *width,
                height: *height,
                bytes: bytes.clone(),
            },
            ClipboardEvent::Files { paths, .. } => Self::Files(paths.clone()),
        }
    }
    pub fn into_event(self) -> ClipboardEvent {
        match self {
            Self::Text(s) => ClipboardEvent::Text(s),
            Self::Image {
                width,
                height,
                bytes,
            } => ClipboardEvent::ImageRgba {
                width,
                height,
                bytes,
            },
            // Staged file lifetime is owned by the persistent network process.
            Self::Files(paths) => ClipboardEvent::Files {
                paths,
                owned_root: None,
            },
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Capture {
    Begin,
    Input(Input),
    AutoRelease,
    Locked(bool),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    CaptureCreate(u8),
    CaptureDestroy(u8),
    CrossingModifier(u8, Option<CrossingModifier>),
    Release(u64),
    Input { handle: u64, input: Input },
    Destroy(u64),
    ReleaseKeys(Vec<u32>),
    Warp { id: u64, x: i32, y: i32 },
    Clipboard { id: u64, clipboard: Clipboard },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Response {
    Ready {
        secure: bool,
        bounds: Option<(u32, u32)>,
    },
    Geometry(Option<(u32, u32)>),
    Capture {
        edge: u8,
        event: Capture,
    },
    Clipboard {
        revision: u64,
        clipboard: Clipboard,
    },
    ClipboardApplied {
        id: u64,
        ok: bool,
    },
    Complete {
        id: u64,
        ok: bool,
    },
}
pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    frame: &T,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(frame).map_err(io::Error::other)?;
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::other("desktop frame too large"));
    }
    w.write_u32(bytes.len() as u32).await?;
    w.write_all(&bytes).await
}
pub async fn read_frame<R: AsyncRead + Unpin, T: serde::de::DeserializeOwned>(
    r: &mut R,
) -> io::Result<T> {
    let len = r.read_u32().await? as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::other("invalid desktop frame length"));
    }
    let mut bytes = vec![0; len];
    r.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[cfg(any(windows, test))]
#[cfg_attr(not(windows), allow(dead_code))]
mod link;
#[cfg(windows)]
pub use link::*;

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn clipboard_publication_and_revision_round_trip() {
        let clipboard = Clipboard::Files(vec![PathBuf::from("test.txt")]);
        let mut bytes = Vec::new();
        write_frame(
            &mut bytes,
            &Request::Clipboard {
                id: 42,
                clipboard: clipboard.clone(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(read_frame::<_, Request>(&mut bytes.as_slice()).await.unwrap(), Request::Clipboard { id: 42, clipboard: value } if value == clipboard)
        );
        bytes.clear();
        write_frame(
            &mut bytes,
            &Response::ClipboardApplied { id: 42, ok: false },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<_, Response>(&mut bytes.as_slice())
                .await
                .unwrap(),
            Response::ClipboardApplied { id: 42, ok: false }
        ));
        bytes.clear();
        write_frame(
            &mut bytes,
            &Response::Clipboard {
                revision: 42,
                clipboard: clipboard.clone(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(read_frame::<_, Response>(&mut bytes.as_slice()).await.unwrap(), Response::Clipboard { revision: 42, clipboard: value } if value == clipboard)
        );
    }

    #[tokio::test]
    async fn rejects_oversized_length_before_reading_payload() {
        let mut bytes = ((MAX_FRAME + 1) as u32).to_be_bytes().as_slice().to_vec();
        assert!(
            read_frame::<_, Response>(&mut bytes.as_slice())
                .await
                .is_err()
        );
        bytes = 0u32.to_be_bytes().to_vec();
        assert!(
            read_frame::<_, Response>(&mut bytes.as_slice())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn input_and_completion_round_trip() {
        let mut bytes = Vec::new();
        write_frame(
            &mut bytes,
            &Request::Input {
                handle: 7,
                input: Input::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: 29,
                    state: 0,
                }),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<_, Request>(&mut bytes.as_slice())
                .await
                .unwrap(),
            Request::Input {
                handle: 7,
                input: Input::Keyboard(KeyboardEvent::Key { state: 0, .. })
            }
        ));
    }
}

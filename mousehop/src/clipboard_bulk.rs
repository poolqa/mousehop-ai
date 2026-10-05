//! Authenticated framing for the clipboard-v2 reliable stream.
//!
//! The one-time key is delivered inside the already authenticated DTLS offer.
//! It binds a TCP connection to that offer and encrypts every frame, so the
//! bulk path does not weaken clipboard confidentiality or peer authorization.

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use input_event::ClipboardFileLease;
use mousehop_proto::clipboard_transfer::{
    BulkFrame, ClipboardItemKind, ClipboardOffer, ClipboardStreamOffer, ClipboardTransferError,
    MAX_CLIPBOARD_CHUNK_SIZE, MAX_CLIPBOARD_FRAME_SIZE, MAX_CLIPBOARD_MANIFEST_ENTRIES,
    ManifestEntry, ManifestEntryKind, TransferId, TransferKey, TransferLimits,
    validate_relative_path,
};
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::Cursor,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{Duration, timeout};
use tokio::{fs::File, net::TcpListener};

const TAG_SIZE: usize = 16;
const MAX_ENCRYPTED_FRAME_SIZE: usize = MAX_CLIPBOARD_FRAME_SIZE + TAG_SIZE;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_HELLO: &[u8] = b"mousehop clipboard bulk client v1";
const SERVER_HELLO: &[u8] = b"mousehop clipboard bulk server v1";

/// Secret scoped to one transfer. Its bytes are intentionally not exposed via
/// `Debug`, avoiding accidental clipboard-key disclosure in logs.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BulkKey([u8; 32]);

impl BulkKey {
    pub(crate) fn generate() -> Self {
        let mut bytes = [0; 32];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    fn transfer_key(&self) -> TransferKey {
        TransferKey::from_bytes(self.0)
    }

    pub(crate) fn from_transfer_key(key: TransferKey) -> Self {
        Self(key.into_bytes())
    }

    #[cfg(test)]
    fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

#[derive(Clone)]
enum PayloadSource {
    Bytes(Arc<Vec<u8>>),
    File(PathBuf),
}

#[derive(Clone)]
struct PayloadItem {
    index: u32,
    manifest: Option<ManifestEntry>,
    source: Option<PayloadSource>,
}

#[derive(Clone)]
pub(crate) struct OutgoingPayload {
    kind: ClipboardItemKind,
    width: u32,
    height: u32,
    total_bytes: u64,
    items: Arc<Vec<PayloadItem>>,
    _lease: Option<ClipboardFileLease>,
}

impl OutgoingPayload {
    pub(crate) fn text(content: String) -> Result<Self, BulkTransportError> {
        Self::bytes(ClipboardItemKind::TextUtf8, 0, 0, content.into_bytes())
    }

    pub(crate) fn image_rgba(
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    ) -> Result<Self, BulkTransportError> {
        let image = image::RgbaImage::from_raw(width, height, rgba)
            .ok_or_else(|| BulkTransportError::InvalidPayload("invalid RGBA dimensions".into()))?;
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut png, image::ImageFormat::Png)
            .map_err(|error| BulkTransportError::InvalidPayload(error.to_string()))?;
        Self::bytes(ClipboardItemKind::ImagePng, width, height, png.into_inner())
    }

    fn bytes(
        kind: ClipboardItemKind,
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    ) -> Result<Self, BulkTransportError> {
        let value = Self {
            kind,
            width,
            height,
            total_bytes: bytes.len() as u64,
            items: Arc::new(vec![PayloadItem {
                index: 0,
                manifest: None,
                source: Some(PayloadSource::Bytes(Arc::new(bytes))),
            }]),
            _lease: None,
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn file_tree(
        paths: Vec<PathBuf>,
        lease: Option<ClipboardFileLease>,
    ) -> Result<Self, BulkTransportError> {
        if paths.is_empty() {
            return Err(BulkTransportError::InvalidPayload(
                "file list is empty".into(),
            ));
        }
        let mut items = Vec::new();
        let mut relative_paths = HashSet::new();
        let mut total_bytes = 0_u64;
        for root in paths {
            let root_name = root
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    BulkTransportError::InvalidPayload(format!("invalid clipboard path: {root:?}"))
                })?;
            for entry in walkdir::WalkDir::new(&root).follow_links(false) {
                let entry =
                    entry.map_err(|error| BulkTransportError::InvalidPayload(error.to_string()))?;
                if entry.file_type().is_symlink() {
                    return Err(BulkTransportError::InvalidPayload(format!(
                        "symbolic links are not supported: {:?}",
                        entry.path()
                    )));
                }
                let metadata = entry
                    .metadata()
                    .map_err(|error| BulkTransportError::InvalidPayload(error.to_string()))?;
                if !metadata.is_file() && !metadata.is_dir() {
                    return Err(BulkTransportError::InvalidPayload(format!(
                        "unsupported filesystem entry: {:?}",
                        entry.path()
                    )));
                }
                let suffix = entry
                    .path()
                    .strip_prefix(&root)
                    .map_err(|error| BulkTransportError::InvalidPayload(error.to_string()))?;
                let relative = if suffix.as_os_str().is_empty() {
                    PathBuf::from(root_name)
                } else {
                    PathBuf::from(root_name).join(suffix)
                };
                let path = normalized_relative_path(&relative)?;
                validate_relative_path(&path)?;
                if !relative_paths.insert(path.clone()) {
                    return Err(BulkTransportError::InvalidPayload(format!(
                        "duplicate clipboard path: {path}"
                    )));
                }
                if items.len() >= MAX_CLIPBOARD_MANIFEST_ENTRIES as usize {
                    return Err(BulkTransportError::InvalidPayload(
                        "clipboard contains too many filesystem entries".into(),
                    ));
                }
                let size = if metadata.is_file() {
                    metadata.len()
                } else {
                    0
                };
                total_bytes = total_bytes.checked_add(size).ok_or_else(|| {
                    BulkTransportError::InvalidPayload("file tree size overflow".into())
                })?;
                let index = u32::try_from(items.len())
                    .map_err(|_| BulkTransportError::InvalidPayload("too many files".into()))?;
                items.push(PayloadItem {
                    index,
                    manifest: Some(ManifestEntry {
                        index,
                        kind: if metadata.is_file() {
                            ManifestEntryKind::File
                        } else {
                            ManifestEntryKind::Directory
                        },
                        path,
                        size,
                    }),
                    source: metadata
                        .is_file()
                        .then(|| PayloadSource::File(entry.path().to_path_buf())),
                });
            }
        }
        let value = Self {
            kind: ClipboardItemKind::FileTree,
            width: 0,
            height: 0,
            total_bytes,
            items: Arc::new(items),
            _lease: lease,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), BulkTransportError> {
        ClipboardOffer {
            transfer_id: TransferId([0; 16]),
            origin_fingerprint: String::new(),
            kind: self.kind,
            total_bytes: self.total_bytes,
            item_count: self.items.len() as u32,
            image_width: self.width,
            image_height: self.height,
        }
        .validate_against(TransferLimits::default())?;
        Ok(())
    }
}

fn normalized_relative_path(path: &Path) -> Result<String, BulkTransportError> {
    let components: Option<Vec<&str>> = path
        .components()
        .map(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect();
    components
        .filter(|parts| !parts.is_empty())
        .map(|parts| parts.join("/"))
        .ok_or_else(|| {
            BulkTransportError::InvalidPayload(format!("path is not portable UTF-8: {path:?}"))
        })
}

pub(crate) struct OutgoingTransfer {
    listener: TcpListener,
    expected_peer: IpAddr,
    stream_offer: ClipboardStreamOffer,
    key: BulkKey,
    payload: OutgoingPayload,
}

impl OutgoingTransfer {
    pub(crate) async fn bind(
        expected_peer: IpAddr,
        origin_fingerprint: String,
        payload: OutgoingPayload,
    ) -> Result<Self, BulkTransportError> {
        let mut id = [0; 16];
        OsRng.fill_bytes(&mut id);
        let transfer_id = TransferId(id);
        let key = BulkKey::generate();
        let offer = ClipboardOffer {
            transfer_id,
            origin_fingerprint,
            kind: payload.kind,
            total_bytes: payload.total_bytes,
            item_count: payload.items.len() as u32,
            image_width: payload.width,
            image_height: payload.height,
        };
        offer.validate_against(TransferLimits::default())?;
        let bind_addr = match expected_peer {
            IpAddr::V4(_) => "0.0.0.0:0",
            IpAddr::V6(_) => "[::]:0",
        };
        let listener = TcpListener::bind(bind_addr).await?;
        let stream_offer = ClipboardStreamOffer {
            offer,
            port: listener.local_addr()?.port(),
            key: key.transfer_key(),
        };
        Ok(Self {
            listener,
            expected_peer,
            stream_offer,
            key,
            payload,
        })
    }

    pub(crate) fn offer(&self) -> ClipboardStreamOffer {
        self.stream_offer.clone()
    }

    pub(crate) async fn serve(self) -> Result<(), BulkTransportError> {
        let stream = timeout(Duration::from_secs(15), async {
            loop {
                let (stream, peer) = self.listener.accept().await?;
                if peer.ip() == self.expected_peer {
                    break Ok::<_, std::io::Error>(stream);
                }
                log::debug!("ignoring clipboard-v2 bulk connection from unexpected peer {peer}");
            }
        })
        .await
        .map_err(|_| BulkTransportError::HandshakeTimeout)??;
        let mut session =
            BulkSession::accept(stream, self.stream_offer.offer.transfer_id, self.key).await?;
        send_payload(&mut session, &self.payload).await?;
        match timeout(Duration::from_secs(15), session.receive()).await {
            Ok(Ok(BulkFrame::Complete)) => session.shutdown().await,
            Ok(Ok(_)) | Ok(Err(_)) => Err(BulkTransportError::Authentication),
            Err(_) => Err(BulkTransportError::HandshakeTimeout),
        }
    }
}

pub(crate) async fn send_payload<S>(
    session: &mut BulkSession<S>,
    payload: &OutgoingPayload,
) -> Result<(), BulkTransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    for item in payload.items.iter() {
        if let Some(manifest) = &item.manifest {
            session.send(&BulkFrame::Manifest(manifest.clone())).await?;
        }
        if let Some(source) = &item.source {
            send_item(session, item.index, source).await?;
        }
    }
    session.send(&BulkFrame::Complete).await?;
    Ok(())
}

async fn send_item<S>(
    session: &mut BulkSession<S>,
    item_index: u32,
    source: &PayloadSource,
) -> Result<(), BulkTransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut hasher = Sha256::new();
    let mut offset = 0_u64;
    match source {
        PayloadSource::Bytes(bytes) => {
            for chunk in bytes.chunks(MAX_CLIPBOARD_CHUNK_SIZE) {
                hasher.update(chunk);
                session
                    .send(&BulkFrame::Chunk {
                        item_index,
                        offset,
                        bytes: chunk.to_vec(),
                    })
                    .await?;
                offset += chunk.len() as u64;
            }
        }
        PayloadSource::File(path) => {
            let mut file = File::open(path).await?;
            let mut buffer = vec![0; MAX_CLIPBOARD_CHUNK_SIZE];
            loop {
                let read = file.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                let chunk = &buffer[..read];
                hasher.update(chunk);
                session
                    .send(&BulkFrame::Chunk {
                        item_index,
                        offset,
                        bytes: chunk.to_vec(),
                    })
                    .await?;
                offset += read as u64;
            }
        }
    }
    session
        .send(&BulkFrame::EndItem {
            item_index,
            digest: hasher.finalize().into(),
        })
        .await?;
    Ok(())
}

impl std::fmt::Debug for BulkKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BulkKey([REDACTED])")
    }
}

#[derive(Debug, Error)]
pub(crate) enum BulkTransportError {
    #[error("clipboard bulk I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("clipboard bulk authentication failed")]
    Authentication,
    #[error("clipboard bulk handshake timed out")]
    HandshakeTimeout,
    #[error("clipboard bulk sequence exhausted")]
    SequenceExhausted,
    #[error("clipboard bulk encrypted frame is too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("invalid clipboard payload: {0}")]
    InvalidPayload(String),
    #[error(transparent)]
    Protocol(#[from] ClipboardTransferError),
}

#[derive(Clone, Copy)]
enum Role {
    Initiator,
    Responder,
}

impl Role {
    fn send_domain(self) -> u32 {
        match self {
            Self::Initiator => 0x434c_4931, // CLI1
            Self::Responder => 0x5352_5631, // SRV1
        }
    }

    fn receive_domain(self) -> u32 {
        match self {
            Self::Initiator => Self::Responder.send_domain(),
            Self::Responder => Self::Initiator.send_domain(),
        }
    }
}

pub(crate) struct BulkSession<S> {
    stream: S,
    cipher: Aes256Gcm,
    transfer_id: TransferId,
    role: Role,
    send_sequence: u64,
    receive_sequence: u64,
}

impl<S> BulkSession<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Establish the side that opened the TCP connection.
    pub(crate) async fn initiate(
        stream: S,
        transfer_id: TransferId,
        key: BulkKey,
    ) -> Result<Self, BulkTransportError> {
        let mut session = Self::new(stream, transfer_id, key, Role::Initiator);
        timeout(HANDSHAKE_TIMEOUT, async {
            session.send_record(CLIENT_HELLO).await?;
            let reply = session.receive_record().await?;
            if reply != SERVER_HELLO {
                return Err(BulkTransportError::Authentication);
            }
            Ok(())
        })
        .await
        .map_err(|_| BulkTransportError::HandshakeTimeout)??;
        Ok(session)
    }

    /// Establish the side that accepted the TCP connection.
    pub(crate) async fn accept(
        stream: S,
        transfer_id: TransferId,
        key: BulkKey,
    ) -> Result<Self, BulkTransportError> {
        let mut session = Self::new(stream, transfer_id, key, Role::Responder);
        timeout(HANDSHAKE_TIMEOUT, async {
            let hello = session.receive_record().await?;
            if hello != CLIENT_HELLO {
                return Err(BulkTransportError::Authentication);
            }
            session.send_record(SERVER_HELLO).await
        })
        .await
        .map_err(|_| BulkTransportError::HandshakeTimeout)??;
        Ok(session)
    }

    fn new(stream: S, transfer_id: TransferId, key: BulkKey, role: Role) -> Self {
        Self {
            stream,
            cipher: Aes256Gcm::new_from_slice(&key.0).expect("AES-256 key has fixed length"),
            transfer_id,
            role,
            send_sequence: 0,
            receive_sequence: 0,
        }
    }

    pub(crate) async fn send(&mut self, frame: &BulkFrame) -> Result<(), BulkTransportError> {
        self.send_record(&frame.encode()?).await
    }

    pub(crate) async fn receive(&mut self) -> Result<BulkFrame, BulkTransportError> {
        let plaintext = self.receive_record().await?;
        Ok(BulkFrame::decode(&plaintext)?)
    }

    pub(crate) async fn shutdown(&mut self) -> Result<(), BulkTransportError> {
        self.stream.shutdown().await?;
        Ok(())
    }

    async fn send_record(&mut self, plaintext: &[u8]) -> Result<(), BulkTransportError> {
        if plaintext.len() > MAX_CLIPBOARD_FRAME_SIZE {
            return Err(BulkTransportError::FrameTooLarge(plaintext.len()));
        }
        let sequence = take_sequence(&mut self.send_sequence)?;
        let domain = self.role.send_domain();
        let nonce_bytes = nonce(domain, sequence);
        let aad = associated_data(self.transfer_id, domain, sequence);
        let ciphertext = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| BulkTransportError::Authentication)?;
        let length = u32::try_from(ciphertext.len())
            .map_err(|_| BulkTransportError::FrameTooLarge(ciphertext.len()))?;
        self.stream.write_all(&length.to_be_bytes()).await?;
        self.stream.write_all(&ciphertext).await?;
        self.stream.flush().await?;
        Ok(())
    }

    async fn receive_record(&mut self) -> Result<Vec<u8>, BulkTransportError> {
        let length = self.stream.read_u32().await? as usize;
        if !(TAG_SIZE..=MAX_ENCRYPTED_FRAME_SIZE).contains(&length) {
            return Err(BulkTransportError::FrameTooLarge(length));
        }
        // The allocation is reached only after the length is bounded.
        let mut ciphertext = vec![0; length];
        self.stream.read_exact(&mut ciphertext).await?;
        let sequence = take_sequence(&mut self.receive_sequence)?;
        let domain = self.role.receive_domain();
        let nonce_bytes = nonce(domain, sequence);
        let aad = associated_data(self.transfer_id, domain, sequence);
        self.cipher
            .decrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: &ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| BulkTransportError::Authentication)
    }
}

fn take_sequence(sequence: &mut u64) -> Result<u64, BulkTransportError> {
    let current = *sequence;
    *sequence = sequence
        .checked_add(1)
        .ok_or(BulkTransportError::SequenceExhausted)?;
    Ok(current)
}

fn nonce(domain: u32, sequence: u64) -> [u8; 12] {
    let mut bytes = [0; 12];
    bytes[..4].copy_from_slice(&domain.to_be_bytes());
    bytes[4..].copy_from_slice(&sequence.to_be_bytes());
    bytes
}

fn associated_data(transfer_id: TransferId, domain: u32, sequence: u64) -> [u8; 28] {
    let mut bytes = [0; 28];
    bytes[..16].copy_from_slice(&transfer_id.0);
    bytes[16..20].copy_from_slice(&domain.to_be_bytes());
    bytes[20..].copy_from_slice(&sequence.to_be_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, duplex};

    #[tokio::test]
    async fn authenticated_loopback_round_trip() {
        let (client, server_stream) = duplex(512 * 1024);
        let transfer_id = TransferId([3; 16]);
        let key = BulkKey::from_bytes([5; 32]);
        let server_key = key.clone();
        let server = tokio::spawn(async move {
            let mut session = BulkSession::accept(server_stream, transfer_id, server_key)
                .await
                .unwrap();
            assert_eq!(
                session.receive().await.unwrap(),
                BulkFrame::Chunk {
                    item_index: 0,
                    offset: 0,
                    bytes: b"large clipboard".to_vec(),
                }
            );
            session.send(&BulkFrame::Complete).await.unwrap();
            session.shutdown().await.unwrap();
        });

        let mut session = BulkSession::initiate(client, transfer_id, key)
            .await
            .unwrap();
        session
            .send(&BulkFrame::Chunk {
                item_index: 0,
                offset: 0,
                bytes: b"large clipboard".to_vec(),
            })
            .await
            .unwrap();
        assert_eq!(session.receive().await.unwrap(), BulkFrame::Complete);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn wrong_key_fails_the_handshake() {
        let (client, server) = duplex(4096);
        let transfer_id = TransferId([1; 16]);
        let client_task = tokio::spawn(BulkSession::initiate(
            client,
            transfer_id,
            BulkKey::from_bytes([1; 32]),
        ));
        let result = BulkSession::accept(server, transfer_id, BulkKey::from_bytes([2; 32])).await;
        assert!(matches!(result, Err(BulkTransportError::Authentication)));
        assert!(client_task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn rejects_oversized_length_before_allocation() {
        let (mut writer, reader) = duplex(64);
        let transfer_id = TransferId([1; 16]);
        writer
            .write_all(&((MAX_ENCRYPTED_FRAME_SIZE as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        let mut session = BulkSession::new(
            reader,
            transfer_id,
            BulkKey::from_bytes([1; 32]),
            Role::Responder,
        );
        assert!(matches!(
            session.receive_record().await,
            Err(BulkTransportError::FrameTooLarge(_))
        ));
    }

    #[test]
    fn debug_never_exposes_key_material() {
        let key = BulkKey::from_bytes([0xab; 32]);
        assert_eq!(format!("{key:?}"), "BulkKey([REDACTED])");
        assert!(!format!("{key:?}").contains("ab"));
        assert_ne!(BulkKey::generate(), BulkKey::generate());
    }

    #[test]
    fn image_payload_is_lossless_png() {
        let rgba = vec![1, 2, 3, 4, 10, 20, 30, 40];
        let payload = OutgoingPayload::image_rgba(2, 1, rgba.clone()).unwrap();
        assert_eq!(payload.kind, ClipboardItemKind::ImagePng);
        let PayloadSource::Bytes(png) = payload.items[0].source.as_ref().unwrap() else {
            panic!("image should be backed by encoded bytes")
        };
        let decoded = image::load_from_memory_with_format(png, image::ImageFormat::Png).unwrap();
        assert_eq!(decoded.to_rgba8().into_raw(), rgba);
    }

    #[test]
    fn file_payload_builds_portable_manifest() {
        let staging = tempfile::tempdir().unwrap();
        let root = staging.path().join("folder");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("hello.txt"), b"hello").unwrap();
        let payload = OutgoingPayload::file_tree(vec![root], None).unwrap();
        assert_eq!(payload.kind, ClipboardItemKind::FileTree);
        assert_eq!(payload.total_bytes, 5);
        let entries: Vec<_> = payload
            .items
            .iter()
            .map(|item| item.manifest.as_ref().unwrap().path.as_str())
            .collect();
        assert_eq!(entries, ["folder", "folder/hello.txt"]);
    }

    #[test]
    fn file_payload_rejects_duplicate_top_level_names() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first_file = first.path().join("same.txt");
        let second_file = second.path().join("same.txt");
        std::fs::write(&first_file, b"one").unwrap();
        std::fs::write(&second_file, b"two").unwrap();
        assert!(matches!(
            OutgoingPayload::file_tree(vec![first_file, second_file], None),
            Err(BulkTransportError::InvalidPayload(_))
        ));
    }
}

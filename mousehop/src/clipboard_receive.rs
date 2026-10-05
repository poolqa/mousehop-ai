//! Disk-backed receiver for clipboard-v2 transfers.

use crate::clipboard_bulk::{BulkKey, BulkSession, BulkTransportError};
use input_event::{ClipboardEvent, ClipboardFileLease};
use mousehop_proto::clipboard_transfer::{
    BulkFrame, ClipboardItemKind, ClipboardOffer, ClipboardStreamOffer, ClipboardTransferError,
    ManifestEntry, ManifestEntryKind, TransferLimits, validate_relative_path,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use tempfile::TempDir;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum StageError {
    #[error(transparent)]
    Protocol(#[from] ClipboardTransferError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("clipboard manifest entry {0} is duplicated")]
    DuplicateIndex(u32),
    #[error("clipboard manifest path is duplicated: {0}")]
    DuplicatePath(String),
    #[error("clipboard item {0} was not declared")]
    UnknownItem(u32),
    #[error("clipboard item {item} chunk offset {actual} does not match expected {expected}")]
    NonContiguousChunk {
        item: u32,
        actual: u64,
        expected: u64,
    },
    #[error("clipboard item {item} exceeds its declared size {declared}")]
    ItemTooLarge { item: u32, declared: u64 },
    #[error("clipboard transfer exceeds its declared total size")]
    TransferTooLarge,
    #[error("clipboard item {0} has already completed")]
    ItemAlreadyComplete(u32),
    #[error("clipboard item {0} ended before reaching its declared size")]
    IncompleteItem(u32),
    #[error("clipboard item {0} failed its digest check")]
    DigestMismatch(u32),
    #[error("clipboard transfer completed before all items arrived")]
    IncompleteTransfer,
    #[error("clipboard manifest is only valid for file-tree transfers")]
    UnexpectedManifest,
    #[error("clipboard directory entry has a non-zero size")]
    DirectoryHasContent,
    #[error("clipboard text is not valid UTF-8")]
    InvalidTextUtf8,
    #[error("clipboard PNG dimensions do not match its offer")]
    ImageDimensionsMismatch,
    #[error("clipboard image decode failed: {0}")]
    Image(#[from] image::ImageError),
    #[error(transparent)]
    Transport(#[from] BulkTransportError),
}

#[derive(Debug)]
struct StagedItem {
    relative_path: PathBuf,
    declared_size: u64,
    received: u64,
    hasher: Sha256,
    file: Option<File>,
    complete: bool,
}

/// Owns the temporary directory. Dropping the value removes all staged data.
#[derive(Debug)]
pub(crate) enum StagedClipboard {
    TextUtf8 {
        _staging: TempDir,
        payload: PathBuf,
    },
    ImagePng {
        _staging: TempDir,
        payload: PathBuf,
        width: u32,
        height: u32,
    },
    FileTree {
        _staging: TempDir,
        _roots: Vec<PathBuf>,
    },
}

impl StagedClipboard {
    /// Materialize text and image staging as the existing emulation event.
    /// File trees stay on disk and are published through platform-native file
    /// list APIs, so they intentionally return `None` here.
    pub(crate) fn to_event(&self) -> Result<Option<ClipboardEvent>, StageError> {
        match self {
            Self::TextUtf8 { payload, .. } => {
                let bytes = std::fs::read(payload)?;
                let text = String::from_utf8(bytes).map_err(|_| StageError::InvalidTextUtf8)?;
                Ok(Some(ClipboardEvent::Text(text)))
            }
            Self::ImagePng {
                payload,
                width,
                height,
                ..
            } => {
                let image = image::ImageReader::with_format(
                    std::io::BufReader::new(File::open(payload)?),
                    image::ImageFormat::Png,
                )
                .decode()?;
                if image.width() != *width || image.height() != *height {
                    return Err(StageError::ImageDimensionsMismatch);
                }
                Ok(Some(ClipboardEvent::ImageRgba {
                    width: *width,
                    height: *height,
                    bytes: image.to_rgba8().into_raw(),
                }))
            }
            Self::FileTree { .. } => Ok(None),
        }
    }

    pub(crate) fn into_event(self) -> Result<ClipboardEvent, StageError> {
        match self {
            Self::TextUtf8 { payload, .. } => {
                let bytes = std::fs::read(payload)?;
                Ok(ClipboardEvent::Text(
                    String::from_utf8(bytes).map_err(|_| StageError::InvalidTextUtf8)?,
                ))
            }
            Self::ImagePng {
                payload,
                width,
                height,
                ..
            } => {
                let image = image::ImageReader::with_format(
                    std::io::BufReader::new(File::open(payload)?),
                    image::ImageFormat::Png,
                )
                .decode()?;
                if image.width() != width || image.height() != height {
                    return Err(StageError::ImageDimensionsMismatch);
                }
                Ok(ClipboardEvent::ImageRgba {
                    width,
                    height,
                    bytes: image.to_rgba8().into_raw(),
                })
            }
            Self::FileTree {
                _staging: staging,
                _roots: roots,
            } => {
                let root = staging.keep();
                Ok(ClipboardEvent::Files {
                    paths: roots,
                    owned_root: Some(ClipboardFileLease::new(root)),
                })
            }
        }
    }
}

pub(crate) struct TransferStager {
    offer: ClipboardOffer,
    staging: TempDir,
    items: HashMap<u32, StagedItem>,
    paths: HashSet<String>,
    received_total: u64,
}

impl TransferStager {
    pub(crate) fn new(offer: ClipboardOffer, limits: TransferLimits) -> Result<Self, StageError> {
        offer.validate_against(limits)?;
        let staging = tempfile::Builder::new()
            .prefix("mousehop-clipboard-")
            .tempdir()?;
        let mut value = Self {
            offer,
            staging,
            items: HashMap::new(),
            paths: HashSet::new(),
            received_total: 0,
        };
        if value.offer.kind != ClipboardItemKind::FileTree {
            let relative_path = PathBuf::from("payload");
            let file = create_file(&value.staging, &relative_path)?;
            value.items.insert(
                0,
                StagedItem {
                    relative_path,
                    declared_size: value.offer.total_bytes,
                    received: 0,
                    hasher: Sha256::new(),
                    file: Some(file),
                    complete: false,
                },
            );
        }
        Ok(value)
    }

    pub(crate) fn apply(&mut self, frame: BulkFrame) -> Result<bool, StageError> {
        match frame {
            BulkFrame::Manifest(entry) => self.add_manifest(entry)?,
            BulkFrame::Chunk {
                item_index,
                offset,
                bytes,
            } => self.write_chunk(item_index, offset, &bytes)?,
            BulkFrame::EndItem { item_index, digest } => self.end_item(item_index, digest)?,
            BulkFrame::Complete => {
                self.validate_complete()?;
                return Ok(true);
            }
            BulkFrame::Cancel { .. } => return Err(StageError::IncompleteTransfer),
        }
        Ok(false)
    }

    fn add_manifest(&mut self, entry: ManifestEntry) -> Result<(), StageError> {
        if self.offer.kind != ClipboardItemKind::FileTree {
            return Err(StageError::UnexpectedManifest);
        }
        if self.items.len() >= self.offer.item_count as usize {
            return Err(StageError::IncompleteTransfer);
        }
        validate_relative_path(&entry.path)?;
        if self.items.contains_key(&entry.index) {
            return Err(StageError::DuplicateIndex(entry.index));
        }
        if !self.paths.insert(entry.path.clone()) {
            return Err(StageError::DuplicatePath(entry.path));
        }
        if entry.kind == ManifestEntryKind::Directory && entry.size != 0 {
            return Err(StageError::DirectoryHasContent);
        }
        let relative_path = entry.path.split('/').collect::<PathBuf>();
        let complete = match entry.kind {
            ManifestEntryKind::File => false,
            ManifestEntryKind::Directory => {
                std::fs::create_dir_all(self.staging.path().join(&relative_path))?;
                true
            }
        };
        self.items.insert(
            entry.index,
            StagedItem {
                relative_path,
                declared_size: entry.size,
                received: 0,
                hasher: Sha256::new(),
                file: None,
                complete,
            },
        );
        Ok(())
    }

    fn write_chunk(&mut self, index: u32, offset: u64, bytes: &[u8]) -> Result<(), StageError> {
        self.ensure_item_file(index)?;
        let item = self
            .items
            .get_mut(&index)
            .ok_or(StageError::UnknownItem(index))?;
        if item.complete {
            return Err(StageError::ItemAlreadyComplete(index));
        }
        if offset != item.received {
            return Err(StageError::NonContiguousChunk {
                item: index,
                actual: offset,
                expected: item.received,
            });
        }
        let next =
            item.received
                .checked_add(bytes.len() as u64)
                .ok_or(StageError::ItemTooLarge {
                    item: index,
                    declared: item.declared_size,
                })?;
        if next > item.declared_size {
            return Err(StageError::ItemTooLarge {
                item: index,
                declared: item.declared_size,
            });
        }
        let total = self
            .received_total
            .checked_add(bytes.len() as u64)
            .ok_or(StageError::TransferTooLarge)?;
        if total > self.offer.total_bytes {
            return Err(StageError::TransferTooLarge);
        }
        item.file
            .as_mut()
            .ok_or(StageError::UnknownItem(index))?
            .write_all(bytes)?;
        item.hasher.update(bytes);
        item.received = next;
        self.received_total = total;
        Ok(())
    }

    fn end_item(&mut self, index: u32, digest: [u8; 32]) -> Result<(), StageError> {
        self.ensure_item_file(index)?;
        let item = self
            .items
            .get_mut(&index)
            .ok_or(StageError::UnknownItem(index))?;
        if item.complete {
            return Err(StageError::ItemAlreadyComplete(index));
        }
        if item.received != item.declared_size {
            return Err(StageError::IncompleteItem(index));
        }
        let actual: [u8; 32] = item.hasher.clone().finalize().into();
        if actual != digest {
            return Err(StageError::DigestMismatch(index));
        }
        if let Some(file) = item.file.as_mut() {
            file.flush()?;
            file.sync_all()?;
        }
        item.file = None;
        item.complete = true;
        Ok(())
    }

    /// Open file-tree destinations lazily. An authenticated but misbehaving
    /// peer may send the entire manifest before any content; eagerly opening
    /// up to 10,000 entries would exhaust the receiver's file descriptors.
    fn ensure_item_file(&mut self, index: u32) -> Result<(), StageError> {
        let item = self
            .items
            .get(&index)
            .ok_or(StageError::UnknownItem(index))?;
        if item.complete || item.file.is_some() {
            return Ok(());
        }
        let relative_path = item.relative_path.clone();
        let file = create_file(&self.staging, &relative_path)?;
        self.items
            .get_mut(&index)
            .expect("clipboard item was checked above")
            .file = Some(file);
        Ok(())
    }

    fn validate_complete(&self) -> Result<(), StageError> {
        if self.items.len() != self.offer.item_count as usize
            || self.received_total != self.offer.total_bytes
            || self.items.values().any(|item| !item.complete)
        {
            return Err(StageError::IncompleteTransfer);
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<StagedClipboard, StageError> {
        self.validate_complete()?;
        match self.offer.kind {
            ClipboardItemKind::TextUtf8 => Ok(StagedClipboard::TextUtf8 {
                payload: self.staging.path().join("payload"),
                _staging: self.staging,
            }),
            ClipboardItemKind::ImagePng => Ok(StagedClipboard::ImagePng {
                payload: self.staging.path().join("payload"),
                width: self.offer.image_width,
                height: self.offer.image_height,
                _staging: self.staging,
            }),
            ClipboardItemKind::FileTree => {
                let all_paths: HashSet<PathBuf> = self
                    .items
                    .values()
                    .map(|item| item.relative_path.clone())
                    .collect();
                let mut roots: Vec<PathBuf> = all_paths
                    .iter()
                    .filter(|path| {
                        path.parent()
                            .filter(|parent| !parent.as_os_str().is_empty())
                            .is_none_or(|parent| !all_paths.contains(parent))
                    })
                    .map(|path| self.staging.path().join(path))
                    .collect();
                roots.sort();
                roots.dedup();
                Ok(StagedClipboard::FileTree {
                    _staging: self.staging,
                    _roots: roots,
                })
            }
        }
    }
}

pub(crate) async fn receive_stream(
    source: SocketAddr,
    stream_offer: ClipboardStreamOffer,
    limits: TransferLimits,
) -> Result<StagedClipboard, StageError> {
    stream_offer.offer.validate_against(limits)?;
    let address = SocketAddr::new(source.ip(), stream_offer.port);
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::net::TcpStream::connect(address),
    )
    .await
    .map_err(|_| BulkTransportError::HandshakeTimeout)??;
    let mut session = BulkSession::initiate(
        stream,
        stream_offer.offer.transfer_id,
        BulkKey::from_transfer_key(stream_offer.key),
    )
    .await?;
    let mut stager = TransferStager::new(stream_offer.offer, limits)?;
    loop {
        if stager.apply(session.receive().await?)? {
            let staged = stager.finish()?;
            // Decode text/PNG before acknowledging, so a corrupt payload is
            // reported as a failed transfer rather than published later.
            let _ = staged.to_event()?;
            session.send(&BulkFrame::Complete).await?;
            session.shutdown().await?;
            return Ok(staged);
        }
    }
}

fn create_file(staging: &TempDir, relative_path: &Path) -> Result<File, std::io::Error> {
    let destination = staging.path().join(relative_path);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard_bulk::{BulkKey, BulkSession, OutgoingPayload, send_payload};
    use mousehop_proto::clipboard_transfer::{MAX_CLIPBOARD_CHUNK_SIZE, TransferId};
    use tokio::io::duplex;

    fn stage_single_payload(
        offer: ClipboardOffer,
        bytes: &[u8],
    ) -> Result<StagedClipboard, StageError> {
        let mut stager = TransferStager::new(offer, TransferLimits::default())?;
        stager.apply(BulkFrame::Chunk {
            item_index: 0,
            offset: 0,
            bytes: bytes.to_vec(),
        })?;
        stager.apply(BulkFrame::EndItem {
            item_index: 0,
            digest: Sha256::digest(bytes).into(),
        })?;
        stager.apply(BulkFrame::Complete)?;
        stager.finish()
    }

    fn offer(kind: ClipboardItemKind, total_bytes: u64, item_count: u32) -> ClipboardOffer {
        ClipboardOffer {
            transfer_id: TransferId([4; 16]),
            origin_fingerprint: "aa:bb".into(),
            kind,
            total_bytes,
            item_count,
            image_width: u32::from(kind == ClipboardItemKind::ImagePng),
            image_height: u32::from(kind == ClipboardItemKind::ImagePng),
        }
    }

    #[test]
    fn stages_large_text_to_disk_and_verifies_digest() {
        let bytes = b"a clipboard larger than one datagram";
        let mut stager = TransferStager::new(
            offer(ClipboardItemKind::TextUtf8, bytes.len() as u64, 1),
            TransferLimits::default(),
        )
        .unwrap();
        stager
            .apply(BulkFrame::Chunk {
                item_index: 0,
                offset: 0,
                bytes: bytes.to_vec(),
            })
            .unwrap();
        stager
            .apply(BulkFrame::EndItem {
                item_index: 0,
                digest: Sha256::digest(bytes).into(),
            })
            .unwrap();
        assert!(stager.apply(BulkFrame::Complete).unwrap());
        let staged = stager.finish().unwrap();
        let StagedClipboard::TextUtf8 {
            _staging: staging,
            payload,
        } = staged
        else {
            panic!("wrong staged type")
        };
        assert_eq!(std::fs::read(payload).unwrap(), bytes);
        drop(staging);
    }

    #[test]
    fn rejects_non_contiguous_and_oversized_chunks() {
        let mut stager = TransferStager::new(
            offer(ClipboardItemKind::TextUtf8, 3, 1),
            TransferLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            stager.apply(BulkFrame::Chunk {
                item_index: 0,
                offset: 1,
                bytes: vec![1],
            }),
            Err(StageError::NonContiguousChunk { .. })
        ));
        assert!(matches!(
            stager.apply(BulkFrame::Chunk {
                item_index: 0,
                offset: 0,
                bytes: vec![1; 4],
            }),
            Err(StageError::ItemTooLarge { .. })
        ));
    }

    #[test]
    fn stages_file_tree_without_escaping_root() {
        let bytes = b"hello";
        let mut stager = TransferStager::new(
            offer(ClipboardItemKind::FileTree, bytes.len() as u64, 2),
            TransferLimits::default(),
        )
        .unwrap();
        stager
            .apply(BulkFrame::Manifest(ManifestEntry {
                index: 0,
                kind: ManifestEntryKind::Directory,
                path: "folder".into(),
                size: 0,
            }))
            .unwrap();
        stager
            .apply(BulkFrame::Manifest(ManifestEntry {
                index: 1,
                kind: ManifestEntryKind::File,
                path: "folder/file.txt".into(),
                size: bytes.len() as u64,
            }))
            .unwrap();
        stager
            .apply(BulkFrame::Chunk {
                item_index: 1,
                offset: 0,
                bytes: bytes.to_vec(),
            })
            .unwrap();
        stager
            .apply(BulkFrame::EndItem {
                item_index: 1,
                digest: Sha256::digest(bytes).into(),
            })
            .unwrap();
        stager.apply(BulkFrame::Complete).unwrap();
        let StagedClipboard::FileTree {
            _staging: staging,
            _roots: roots,
        } = stager.finish().unwrap()
        else {
            panic!("wrong staged type")
        };
        assert_eq!(roots, vec![staging.path().join("folder")]);
        assert_eq!(std::fs::read(roots[0].join("file.txt")).unwrap(), bytes);
    }

    #[test]
    fn stages_empty_file_created_on_end_item() {
        let mut stager = TransferStager::new(
            offer(ClipboardItemKind::FileTree, 0, 1),
            TransferLimits::default(),
        )
        .unwrap();
        stager
            .apply(BulkFrame::Manifest(ManifestEntry {
                index: 0,
                kind: ManifestEntryKind::File,
                path: "empty.txt".into(),
                size: 0,
            }))
            .unwrap();
        stager
            .apply(BulkFrame::EndItem {
                item_index: 0,
                digest: Sha256::digest([]).into(),
            })
            .unwrap();
        stager.apply(BulkFrame::Complete).unwrap();
        let StagedClipboard::FileTree {
            _staging: staging,
            _roots: roots,
        } = stager.finish().unwrap()
        else {
            panic!("wrong staged type")
        };
        assert_eq!(roots, vec![staging.path().join("empty.txt")]);
        assert_eq!(std::fs::metadata(&roots[0]).unwrap().len(), 0);
    }

    #[test]
    fn digest_mismatch_never_completes() {
        let mut stager = TransferStager::new(
            offer(ClipboardItemKind::TextUtf8, 1, 1),
            TransferLimits::default(),
        )
        .unwrap();
        stager
            .apply(BulkFrame::Chunk {
                item_index: 0,
                offset: 0,
                bytes: vec![1],
            })
            .unwrap();
        assert!(matches!(
            stager.apply(BulkFrame::EndItem {
                item_index: 0,
                digest: [0; 32],
            }),
            Err(StageError::DigestMismatch(0))
        ));
    }

    #[test]
    fn staged_png_becomes_rgba_clipboard_event() {
        let source = image::RgbaImage::from_raw(1, 1, vec![1, 2, 3, 4]).unwrap();
        let mut cursor = std::io::Cursor::new(Vec::new());
        source
            .write_to(&mut cursor, image::ImageFormat::Png)
            .unwrap();
        let png = cursor.into_inner();
        let staged = stage_single_payload(
            offer(ClipboardItemKind::ImagePng, png.len() as u64, 1),
            &png,
        )
        .unwrap();
        assert_eq!(
            staged.to_event().unwrap(),
            Some(ClipboardEvent::ImageRgba {
                width: 1,
                height: 1,
                bytes: vec![1, 2, 3, 4],
            })
        );
    }

    #[tokio::test]
    async fn encrypted_stream_stages_multi_chunk_text_end_to_end() {
        let bytes = vec![0x5a; MAX_CLIPBOARD_CHUNK_SIZE * 2 + 17];
        let transfer_offer = offer(ClipboardItemKind::TextUtf8, bytes.len() as u64, 1);
        let transfer_id = transfer_offer.transfer_id;
        let key = BulkKey::generate();
        let receiver_key = key.clone();
        let (source_stream, receiver_stream) = duplex(64 * 1024);

        let source_bytes = bytes.clone();
        let source = async move {
            let mut session = BulkSession::initiate(source_stream, transfer_id, key)
                .await
                .unwrap();
            for (chunk_index, chunk) in source_bytes.chunks(MAX_CLIPBOARD_CHUNK_SIZE).enumerate() {
                session
                    .send(&BulkFrame::Chunk {
                        item_index: 0,
                        offset: (chunk_index * MAX_CLIPBOARD_CHUNK_SIZE) as u64,
                        bytes: chunk.to_vec(),
                    })
                    .await
                    .unwrap();
            }
            session
                .send(&BulkFrame::EndItem {
                    item_index: 0,
                    digest: Sha256::digest(&source_bytes).into(),
                })
                .await
                .unwrap();
            session.send(&BulkFrame::Complete).await.unwrap();
        };

        let receiver = async move {
            let mut session = BulkSession::accept(receiver_stream, transfer_id, receiver_key)
                .await
                .unwrap();
            let mut stager =
                TransferStager::new(transfer_offer, TransferLimits::default()).unwrap();
            loop {
                if stager.apply(session.receive().await.unwrap()).unwrap() {
                    break;
                }
            }
            let StagedClipboard::TextUtf8 {
                _staging: staging,
                payload,
            } = stager.finish().unwrap()
            else {
                panic!("wrong staged type")
            };
            let bytes = std::fs::read(payload).unwrap();
            drop(staging);
            bytes
        };

        let ((), received) = tokio::join!(source, receiver);
        assert_eq!(received, bytes);
    }

    #[tokio::test]
    async fn encrypted_stream_stages_file_tree_end_to_end() {
        let source_dir = tempfile::tempdir().unwrap();
        let folder = source_dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("hello.txt"), b"hello").unwrap();
        let payload = OutgoingPayload::file_tree(vec![folder], None).unwrap();
        let transfer_offer = offer(ClipboardItemKind::FileTree, 5, 2);
        let transfer_id = transfer_offer.transfer_id;
        let key = BulkKey::generate();
        let receiver_key = key.clone();
        let (source_stream, receiver_stream) = duplex(64 * 1024);

        let source = async move {
            let mut session = BulkSession::accept(source_stream, transfer_id, key)
                .await
                .unwrap();
            send_payload(&mut session, &payload).await.unwrap();
        };
        let receiver = async move {
            let mut session = BulkSession::initiate(receiver_stream, transfer_id, receiver_key)
                .await
                .unwrap();
            let mut stager =
                TransferStager::new(transfer_offer, TransferLimits::default()).unwrap();
            loop {
                if stager.apply(session.receive().await.unwrap()).unwrap() {
                    break;
                }
            }
            stager.finish().unwrap().into_event().unwrap()
        };

        let ((), event) = tokio::join!(source, receiver);
        let ClipboardEvent::Files { paths, owned_root } = event else {
            panic!("wrong clipboard event")
        };
        assert_eq!(std::fs::read(paths[0].join("hello.txt")).unwrap(), b"hello");
        let lease = owned_root.unwrap();
        let root = lease.path().to_path_buf();
        drop(paths);
        assert!(root.exists());
        drop(lease);
        assert!(!root.exists());
    }
}

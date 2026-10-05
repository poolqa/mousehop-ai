//! Clipboard-v2 metadata and reliable-stream framing.
//!
//! Mouse and keyboard events stay on the latency-sensitive DTLS datagram
//! channel.  A small [`ClipboardOffer`] is announced there, while the frames
//! in this module are carried by a reliable, authenticated byte stream.  The
//! codec is deliberately dependency-free and bounded so peers can reject an
//! invalid transfer before allocating based on attacker-controlled lengths.

use std::fmt;

use thiserror::Error;

pub const CLIPBOARD_TRANSFER_VERSION: u8 = 1;
pub const MAX_CLIPBOARD_ORIGIN_BYTES: usize = 256;
pub const MAX_CLIPBOARD_PATH_BYTES: usize = 4 * 1024;
pub const MAX_CLIPBOARD_MANIFEST_ENTRIES: u32 = 10_000;
pub const MAX_CLIPBOARD_CHUNK_SIZE: usize = 256 * 1024;
pub const MAX_CLIPBOARD_FRAME_SIZE: usize = MAX_CLIPBOARD_CHUNK_SIZE + 8 * 1024;

/// Receiver-side resource policy. These are deliberately not wire constants:
/// callers may lower them per peer without changing protocol compatibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferLimits {
    pub max_text_bytes: u64,
    pub max_image_bytes: u64,
    pub max_file_tree_bytes: u64,
    pub max_file_entries: u32,
    pub max_image_pixels: u64,
}

impl Default for TransferLimits {
    fn default() -> Self {
        Self {
            max_text_bytes: 32 * 1024 * 1024,
            max_image_bytes: 128 * 1024 * 1024,
            max_file_tree_bytes: 2 * 1024 * 1024 * 1024,
            max_file_entries: MAX_CLIPBOARD_MANIFEST_ENTRIES,
            max_image_pixels: 100_000_000,
        }
    }
}

/// Stable identifier preserved when a clipboard transfer is forwarded.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TransferId(pub [u8; 16]);

/// One-time bulk-stream key transported only inside the authenticated DTLS
/// control offer. Debug output is redacted to keep logs from disclosing it.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct TransferKey([u8; 32]);

impl TransferKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for TransferKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TransferKey([REDACTED])")
    }
}

/// DTLS control payload that tells an authorized peer where and how to open
/// the one-transfer reliable stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardStreamOffer {
    pub offer: ClipboardOffer,
    pub port: u16,
    pub key: TransferKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ClipboardItemKind {
    TextUtf8 = 1,
    ImagePng = 2,
    FileTree = 3,
}

impl TryFrom<u8> for ClipboardItemKind {
    type Error = ClipboardTransferError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::TextUtf8),
            2 => Ok(Self::ImagePng),
            3 => Ok(Self::FileTree),
            value => Err(ClipboardTransferError::InvalidItemKind(value)),
        }
    }
}

/// Small control-plane description sent before opening the bulk stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardOffer {
    pub transfer_id: TransferId,
    pub origin_fingerprint: String,
    pub kind: ClipboardItemKind,
    pub total_bytes: u64,
    /// One for text/images; number of regular files and directories for a tree.
    pub item_count: u32,
    /// Set only for images.
    pub image_width: u32,
    pub image_height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ManifestEntryKind {
    File = 1,
    Directory = 2,
}

impl TryFrom<u8> for ManifestEntryKind {
    type Error = ClipboardTransferError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::File),
            2 => Ok(Self::Directory),
            value => Err(ClipboardTransferError::InvalidManifestKind(value)),
        }
    }
}

/// A path is always relative to the transfer root and uses `/` separators.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestEntry {
    pub index: u32,
    pub kind: ManifestEntryKind,
    pub path: String,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BulkFrame {
    Manifest(ManifestEntry),
    Chunk {
        item_index: u32,
        offset: u64,
        bytes: Vec<u8>,
    },
    EndItem {
        item_index: u32,
        /// SHA-256 of the uncompressed item bytes.
        digest: [u8; 32],
    },
    Complete,
    Cancel {
        reason: String,
    },
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ClipboardTransferError {
    #[error("clipboard transfer frame is truncated")]
    Truncated,
    #[error("unsupported clipboard transfer version: {0}")]
    UnsupportedVersion(u8),
    #[error("invalid clipboard item kind: {0}")]
    InvalidItemKind(u8),
    #[error("invalid clipboard manifest entry kind: {0}")]
    InvalidManifestKind(u8),
    #[error("invalid clipboard bulk frame tag: {0}")]
    InvalidFrameTag(u8),
    #[error("clipboard field `{field}` is too large: {actual} bytes (max {maximum})")]
    FieldTooLarge {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("clipboard manifest contains too many entries: {0}")]
    TooManyManifestEntries(u32),
    #[error("clipboard transfer exceeds the receiver limit: {actual} bytes (max {maximum})")]
    TransferTooLarge { actual: u64, maximum: u64 },
    #[error("clipboard image dimensions are invalid or exceed the receiver limit")]
    InvalidImageDimensions,
    #[error("clipboard path must be a normalized relative path: {0}")]
    UnsafePath(String),
    #[error("clipboard transfer frame has trailing bytes")]
    TrailingData,
    #[error("clipboard string is not valid UTF-8")]
    InvalidUtf8,
}

impl ClipboardOffer {
    pub fn validate(&self) -> Result<(), ClipboardTransferError> {
        checked_bytes(
            "origin_fingerprint",
            self.origin_fingerprint.as_bytes(),
            MAX_CLIPBOARD_ORIGIN_BYTES,
        )?;
        if self.item_count > MAX_CLIPBOARD_MANIFEST_ENTRIES {
            return Err(ClipboardTransferError::TooManyManifestEntries(
                self.item_count,
            ));
        }
        if self.kind != ClipboardItemKind::FileTree && self.item_count != 1 {
            return Err(ClipboardTransferError::TooManyManifestEntries(
                self.item_count,
            ));
        }
        Ok(())
    }

    pub fn validate_against(&self, limits: TransferLimits) -> Result<(), ClipboardTransferError> {
        self.validate()?;
        let maximum = match self.kind {
            ClipboardItemKind::TextUtf8 => limits.max_text_bytes,
            ClipboardItemKind::ImagePng => limits.max_image_bytes,
            ClipboardItemKind::FileTree => limits.max_file_tree_bytes,
        };
        if self.total_bytes > maximum {
            return Err(ClipboardTransferError::TransferTooLarge {
                actual: self.total_bytes,
                maximum,
            });
        }
        match self.kind {
            ClipboardItemKind::ImagePng => {
                let pixels = u64::from(self.image_width)
                    .checked_mul(u64::from(self.image_height))
                    .filter(|pixels| *pixels > 0 && *pixels <= limits.max_image_pixels)
                    .ok_or(ClipboardTransferError::InvalidImageDimensions)?;
                // A decoded RGBA image needs four bytes per pixel. Checking the
                // multiplication here prevents a compact PNG from bypassing
                // the receiver's memory budget.
                pixels
                    .checked_mul(4)
                    .filter(|bytes| *bytes <= limits.max_image_bytes)
                    .ok_or(ClipboardTransferError::InvalidImageDimensions)?;
            }
            ClipboardItemKind::TextUtf8 => {
                if self.image_width != 0 || self.image_height != 0 {
                    return Err(ClipboardTransferError::InvalidImageDimensions);
                }
            }
            ClipboardItemKind::FileTree => {
                if self.image_width != 0
                    || self.image_height != 0
                    || self.item_count > limits.max_file_entries
                {
                    return Err(ClipboardTransferError::TooManyManifestEntries(
                        self.item_count,
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, ClipboardTransferError> {
        self.validate()?;
        let origin = self.origin_fingerprint.as_bytes();
        let mut out = Vec::with_capacity(44 + origin.len());
        out.push(CLIPBOARD_TRANSFER_VERSION);
        out.extend_from_slice(&self.transfer_id.0);
        out.push(self.kind as u8);
        put_u16(&mut out, origin.len() as u16);
        out.extend_from_slice(origin);
        put_u64(&mut out, self.total_bytes);
        put_u32(&mut out, self.item_count);
        put_u32(&mut out, self.image_width);
        put_u32(&mut out, self.image_height);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ClipboardTransferError> {
        let mut input = Input::new(bytes);
        let version = input.u8()?;
        if version != CLIPBOARD_TRANSFER_VERSION {
            return Err(ClipboardTransferError::UnsupportedVersion(version));
        }
        let transfer_id = TransferId(input.array()?);
        let kind = ClipboardItemKind::try_from(input.u8()?)?;
        let origin_len = input.u16()? as usize;
        if origin_len > MAX_CLIPBOARD_ORIGIN_BYTES {
            return Err(ClipboardTransferError::FieldTooLarge {
                field: "origin_fingerprint",
                actual: origin_len,
                maximum: MAX_CLIPBOARD_ORIGIN_BYTES,
            });
        }
        let origin_fingerprint = input.string(origin_len)?;
        let offer = Self {
            transfer_id,
            origin_fingerprint,
            kind,
            total_bytes: input.u64()?,
            item_count: input.u32()?,
            image_width: input.u32()?,
            image_height: input.u32()?,
        };
        input.finish()?;
        offer.validate()?;
        Ok(offer)
    }
}

impl BulkFrame {
    /// Encodes one payload. The stream transport prefixes this with a u32
    /// length, allowing the reader to reject oversized frames before allocation.
    pub fn encode(&self) -> Result<Vec<u8>, ClipboardTransferError> {
        let mut out = Vec::new();
        match self {
            Self::Manifest(entry) => {
                validate_relative_path(&entry.path)?;
                let path = checked_bytes("path", entry.path.as_bytes(), MAX_CLIPBOARD_PATH_BYTES)?;
                out.push(1);
                put_u32(&mut out, entry.index);
                out.push(entry.kind as u8);
                put_u16(&mut out, path.len() as u16);
                out.extend_from_slice(path);
                put_u64(&mut out, entry.size);
            }
            Self::Chunk {
                item_index,
                offset,
                bytes,
            } => {
                checked_bytes("chunk", bytes, MAX_CLIPBOARD_CHUNK_SIZE)?;
                out.push(2);
                put_u32(&mut out, *item_index);
                put_u64(&mut out, *offset);
                put_u32(&mut out, bytes.len() as u32);
                out.extend_from_slice(bytes);
            }
            Self::EndItem { item_index, digest } => {
                out.push(3);
                put_u32(&mut out, *item_index);
                out.extend_from_slice(digest);
            }
            Self::Complete => out.push(4),
            Self::Cancel { reason } => {
                let reason = checked_bytes("cancel_reason", reason.as_bytes(), 1024)?;
                out.push(5);
                put_u16(&mut out, reason.len() as u16);
                out.extend_from_slice(reason);
            }
        }
        debug_assert!(out.len() <= MAX_CLIPBOARD_FRAME_SIZE);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ClipboardTransferError> {
        if bytes.len() > MAX_CLIPBOARD_FRAME_SIZE {
            return Err(ClipboardTransferError::FieldTooLarge {
                field: "frame",
                actual: bytes.len(),
                maximum: MAX_CLIPBOARD_FRAME_SIZE,
            });
        }
        let mut input = Input::new(bytes);
        let frame = match input.u8()? {
            1 => {
                let index = input.u32()?;
                let kind = ManifestEntryKind::try_from(input.u8()?)?;
                let path_len = input.u16()? as usize;
                if path_len > MAX_CLIPBOARD_PATH_BYTES {
                    return Err(ClipboardTransferError::FieldTooLarge {
                        field: "path",
                        actual: path_len,
                        maximum: MAX_CLIPBOARD_PATH_BYTES,
                    });
                }
                let path = input.string(path_len)?;
                validate_relative_path(&path)?;
                Self::Manifest(ManifestEntry {
                    index,
                    kind,
                    path,
                    size: input.u64()?,
                })
            }
            2 => {
                let item_index = input.u32()?;
                let offset = input.u64()?;
                let len = input.u32()? as usize;
                if len > MAX_CLIPBOARD_CHUNK_SIZE {
                    return Err(ClipboardTransferError::FieldTooLarge {
                        field: "chunk",
                        actual: len,
                        maximum: MAX_CLIPBOARD_CHUNK_SIZE,
                    });
                }
                Self::Chunk {
                    item_index,
                    offset,
                    bytes: input.bytes(len)?.to_vec(),
                }
            }
            3 => Self::EndItem {
                item_index: input.u32()?,
                digest: input.array()?,
            },
            4 => Self::Complete,
            5 => {
                let len = input.u16()? as usize;
                if len > 1024 {
                    return Err(ClipboardTransferError::FieldTooLarge {
                        field: "cancel_reason",
                        actual: len,
                        maximum: 1024,
                    });
                }
                Self::Cancel {
                    reason: input.string(len)?,
                }
            }
            tag => return Err(ClipboardTransferError::InvalidFrameTag(tag)),
        };
        input.finish()?;
        Ok(frame)
    }
}

/// Reject traversal, platform-rooted paths, and non-canonical separators.
pub fn validate_relative_path(path: &str) -> Result<(), ClipboardTransferError> {
    let unsafe_path = path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains('\\')
        || path.as_bytes().contains(&0)
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path.as_bytes().get(1) == Some(&b':');
    if unsafe_path {
        return Err(ClipboardTransferError::UnsafePath(path.to_owned()));
    }
    checked_bytes("path", path.as_bytes(), MAX_CLIPBOARD_PATH_BYTES)?;
    Ok(())
}

fn checked_bytes<'a>(
    field: &'static str,
    bytes: &'a [u8],
    maximum: usize,
) -> Result<&'a [u8], ClipboardTransferError> {
    if bytes.len() > maximum {
        return Err(ClipboardTransferError::FieldTooLarge {
            field,
            actual: bytes.len(),
            maximum,
        });
    }
    Ok(bytes)
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

struct Input<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Input<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8], ClipboardTransferError> {
        let end = self
            .cursor
            .checked_add(len)
            .ok_or(ClipboardTransferError::Truncated)?;
        let value = self
            .bytes
            .get(self.cursor..end)
            .ok_or(ClipboardTransferError::Truncated)?;
        self.cursor = end;
        Ok(value)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ClipboardTransferError> {
        self.bytes(N)?
            .try_into()
            .map_err(|_| ClipboardTransferError::Truncated)
    }

    fn u8(&mut self) -> Result<u8, ClipboardTransferError> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ClipboardTransferError> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, ClipboardTransferError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, ClipboardTransferError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn string(&mut self, len: usize) -> Result<String, ClipboardTransferError> {
        String::from_utf8(self.bytes(len)?.to_vec())
            .map_err(|_| ClipboardTransferError::InvalidUtf8)
    }

    fn finish(self) -> Result<(), ClipboardTransferError> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err(ClipboardTransferError::TrailingData)
        }
    }
}

impl fmt::Display for TransferId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(kind: ClipboardItemKind) -> ClipboardOffer {
        ClipboardOffer {
            transfer_id: TransferId([7; 16]),
            origin_fingerprint: "aa:bb:cc".into(),
            kind,
            total_bytes: 42,
            item_count: 1,
            image_width: 0,
            image_height: 0,
        }
    }

    #[test]
    fn offer_round_trip() {
        let mut value = offer(ClipboardItemKind::ImagePng);
        value.image_width = 12;
        value.image_height = 8;
        assert_eq!(
            ClipboardOffer::decode(&value.encode().unwrap()).unwrap(),
            value
        );
    }

    #[test]
    fn offer_limits_reject_decompression_bomb_dimensions() {
        let mut value = offer(ClipboardItemKind::ImagePng);
        value.image_width = 50_000;
        value.image_height = 50_000;
        value.total_bytes = 1024;
        assert_eq!(
            value.validate_against(TransferLimits::default()),
            Err(ClipboardTransferError::InvalidImageDimensions)
        );
    }

    #[test]
    fn offer_limits_are_applied_by_kind() {
        let mut value = offer(ClipboardItemKind::TextUtf8);
        value.total_bytes = TransferLimits::default().max_text_bytes + 1;
        assert!(matches!(
            value.validate_against(TransferLimits::default()),
            Err(ClipboardTransferError::TransferTooLarge { .. })
        ));
    }

    #[test]
    fn every_bulk_frame_round_trips() {
        let frames = [
            BulkFrame::Manifest(ManifestEntry {
                index: 3,
                kind: ManifestEntryKind::File,
                path: "folder/hello.txt".into(),
                size: 5,
            }),
            BulkFrame::Chunk {
                item_index: 3,
                offset: 1024,
                bytes: b"hello".to_vec(),
            },
            BulkFrame::EndItem {
                item_index: 3,
                digest: [9; 32],
            },
            BulkFrame::Complete,
            BulkFrame::Cancel {
                reason: "superseded".into(),
            },
        ];
        for frame in frames {
            assert_eq!(BulkFrame::decode(&frame.encode().unwrap()).unwrap(), frame);
        }
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        for path in [
            "../secret",
            "a/../secret",
            "/etc/passwd",
            "C:/secret",
            "a\\b",
        ] {
            assert!(matches!(
                validate_relative_path(path),
                Err(ClipboardTransferError::UnsafePath(_))
            ));
        }
        assert!(validate_relative_path("folder/file.txt").is_ok());
    }

    #[test]
    fn rejects_oversized_chunk_before_copying() {
        let mut encoded = vec![2];
        put_u32(&mut encoded, 0);
        put_u64(&mut encoded, 0);
        put_u32(&mut encoded, (MAX_CLIPBOARD_CHUNK_SIZE + 1) as u32);
        assert!(matches!(
            BulkFrame::decode(&encoded),
            Err(ClipboardTransferError::FieldTooLarge { field: "chunk", .. })
        ));
    }

    #[test]
    fn rejects_trailing_data() {
        assert_eq!(
            BulkFrame::decode(&[4, 0]),
            Err(ClipboardTransferError::TrailingData)
        );
    }
}

use serde::{Deserialize, Serialize};
use std::{io, path::PathBuf};

pub(crate) const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_FILES: usize = 4096;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct FileSnapshot {
    pub sequence: u32,
    pub files: Vec<PathBuf>,
    pub error: Option<String>,
}

impl FileSnapshot {
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(invalid("clipboard snapshot exceeds byte limit"));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(invalid("clipboard snapshot exceeds byte limit"));
        }
        let snapshot: Self = serde_json::from_slice(bytes)?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    fn validate(&self) -> io::Result<()> {
        if self.files.len() > MAX_FILES {
            return Err(invalid("clipboard snapshot exceeds file count limit"));
        }
        for path in &self.files {
            let key = input_event::windows_clipboard_path_key(path);
            let drive =
                key.len() >= 3 && key[0].is_ascii_alphabetic() && key[1] == b':' && key[2] == b'\\';
            if !drive && !key.starts_with(br"\\") {
                return Err(invalid("clipboard snapshot contains a relative file path"));
            }
            if key.contains(&0) {
                return Err(invalid("clipboard snapshot contains a NUL in a path"));
            }
        }
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_snapshot_preserves_unicode_and_unc_paths() {
        let files = vec![
            PathBuf::from(r"D:\download\節氣日期.csv"),
            PathBuf::from(r"\\server\share\圖片.png"),
        ];
        let snapshot = FileSnapshot {
            sequence: 8422,
            files: files.clone(),
            error: None,
        };
        let decoded = FileSnapshot::decode(&snapshot.encode().unwrap()).unwrap();
        assert_eq!(decoded.sequence, 8422);
        assert_eq!(decoded.files, files);
        assert!(decoded.error.is_none());
    }

    #[test]
    fn user_snapshot_rejects_unbounded_or_relative_data() {
        assert!(FileSnapshot::decode(&vec![b' '; MAX_SNAPSHOT_BYTES + 1]).is_err());
        assert!(FileSnapshot::decode(b"invalid json").is_err());
        for files in [
            vec![PathBuf::from("relative.txt")],
            vec![PathBuf::from(r"C:\test.txt"); MAX_FILES + 1],
        ] {
            assert!(
                FileSnapshot {
                    sequence: 1,
                    files,
                    error: None
                }
                .encode()
                .is_err()
            );
        }
    }
}

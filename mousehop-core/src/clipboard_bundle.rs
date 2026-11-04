//! Archive macOS application bundles without flattening their symbolic links.
use crate::clipboard_bulk::BulkTransportError;
use input_event::ClipboardFileLease;
use mousehop_proto::clipboard_transfer::TransferLimits;
use std::{
    io,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub(crate) fn prepare(
    paths: Vec<PathBuf>,
    cancel: &CancellationToken,
) -> Result<(Vec<PathBuf>, Option<ClipboardFileLease>), BulkTransportError> {
    let mut temporary: Option<tempfile::TempDir> = None;
    let mut prepared = Vec::with_capacity(paths.len());
    for path in paths {
        if path.extension().is_none_or(|extension| extension != "app")
            || !path.is_dir()
            || !path.join("Contents/Info.plist").is_file()
        {
            prepared.push(path);
            continue;
        }
        let limits = TransferLimits::default();
        let mut bytes = 0u64;
        for (count, entry) in walkdir::WalkDir::new(path.canonicalize()?)
            .follow_links(false)
            .into_iter()
            .enumerate()
        {
            if cancel.is_cancelled() {
                return Err(
                    io::Error::new(io::ErrorKind::Interrupted, "File copy cancelled").into(),
                );
            }
            let entry = entry.map_err(|e| BulkTransportError::InvalidPayload(e.to_string()))?;
            let metadata = entry
                .metadata()
                .map_err(|e| BulkTransportError::InvalidPayload(e.to_string()))?;
            if !metadata.is_file() && !metadata.is_dir() && !metadata.is_symlink() {
                return Err(BulkTransportError::InvalidPayload(
                    "Application contains a special filesystem entry".into(),
                ));
            }
            bytes = bytes
                .checked_add(if metadata.is_file() {
                    metadata.len()
                } else {
                    0
                })
                .ok_or_else(|| {
                    BulkTransportError::InvalidPayload("Application size overflow".into())
                })?;
            if bytes > limits.max_file_tree_bytes || count >= limits.max_file_entries as usize {
                return Err(BulkTransportError::InvalidPayload(
                    "Application exceeds file transfer limits".into(),
                ));
            }
        }
        if temporary.is_none() {
            temporary = Some(tempfile::tempdir()?);
        }
        let mut name = path
            .file_name()
            .ok_or_else(|| io::Error::other("Missing application name"))?
            .to_os_string();
        name.push(".zip");
        let archive = temporary
            .as_ref()
            .expect("temporary directory created")
            .path()
            .join(name);
        if archive.exists() {
            return Err(BulkTransportError::InvalidPayload(
                "Duplicate application archive name".into(),
            ));
        }
        log::info!(
            "Preparing application archive {}",
            archive.file_name().unwrap().to_string_lossy()
        );
        // ditto stores links as links and preserves executable bits and bundle
        // resources. The receiver receives an opaque ZIP and never extracts it.
        let mut child = Command::new("/usr/bin/ditto")
            .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
            .arg(&path)
            .arg(&archive)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        loop {
            if cancel.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Application archive cancelled",
                )
                .into());
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(
                        io::Error::other(format!("Cannot archive application: {status}")).into(),
                    );
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        prepared.push(archive);
    }
    Ok((
        prepared,
        temporary.map(|root| ClipboardFileLease::new(root.keep())),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    fn application(root: &std::path::Path) -> PathBuf {
        let bundle = root.join("Example.app");
        let directory = bundle.join("Contents/MacOS");
        fs::create_dir_all(&directory).unwrap();
        fs::write(bundle.join("Contents/Info.plist"), "bundle metadata").unwrap();
        fs::write(directory.join("program"), b"executable contents").unwrap();
        fs::set_permissions(directory.join("program"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("program", directory.join("alias")).unwrap();
        bundle
    }

    #[test]
    fn application_archive_preserves_links_permissions_and_lifetime() {
        let source = tempfile::tempdir().unwrap();
        let bundle = application(source.path());
        let (paths, lease) = prepare(vec![bundle], &CancellationToken::new()).unwrap();
        let archive = paths[0].clone();
        assert_eq!(archive.file_name().unwrap(), "Example.app.zip");
        let clone = lease.clone();
        drop(lease);
        drop(source);
        assert!(
            archive.is_file(),
            "archive must outlive the original clipboard paths"
        );
        let destination = tempfile::tempdir().unwrap();
        assert!(
            Command::new("/usr/bin/ditto")
                .args(["-x", "-k"])
                .arg(&archive)
                .arg(destination.path())
                .status()
                .unwrap()
                .success()
        );
        let directory = destination.path().join("Example.app/Contents/MacOS");
        assert_eq!(
            fs::read_link(directory.join("alias")).unwrap(),
            PathBuf::from("program")
        );
        assert_eq!(
            fs::read(directory.join("program")).unwrap(),
            b"executable contents"
        );
        assert_eq!(
            fs::metadata(directory.join("program"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        drop(clone);
        assert!(
            !archive.exists(),
            "temporary archives must be reclaimed after transfer"
        );
    }

    #[test]
    fn ordinary_files_are_unchanged_and_cancelled_bundles_are_not_archived() {
        let source = tempfile::tempdir().unwrap();
        let file = source.path().join("document.txt");
        fs::write(&file, "text").unwrap();
        let (paths, lease) = prepare(vec![file.clone()], &CancellationToken::new()).unwrap();
        assert_eq!(paths, vec![file]);
        assert!(lease.is_none());
        let token = CancellationToken::new();
        token.cancel();
        assert!(prepare(vec![application(source.path())], &token).is_err());
    }
}

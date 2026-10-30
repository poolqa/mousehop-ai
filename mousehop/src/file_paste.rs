//! Explicit file paste. Offers contain metadata only; the receive socket is
//! opened only after an ID-bound frontend request with a destination directory.
use crate::clipboard_receive::{StagedClipboard, receive_stream_with_progress};
use mousehop_ipc::{FilePasteOffer, FilePastePhase, FilePasteProgress};
use mousehop_proto::clipboard_transfer::{ClipboardStreamOffer, TransferLimits};
use std::{
    fs, io,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

pub(crate) struct RemoteFileOffer {
    pub addr: SocketAddr,
    pub stream: ClipboardStreamOffer,
    pub clipboard_version: Option<u64>,
}

impl RemoteFileOffer {
    pub fn is_current(&self, id: [u8; 16], version: Option<u64>) -> bool {
        self.stream.offer.transfer_id.0 == id && self.clipboard_version == version
    }

    pub fn summary(&self) -> FilePasteOffer {
        FilePasteOffer {
            id: self.stream.offer.transfer_id.0,
            source: self.stream.offer.origin_fingerprint.clone(),
            item_count: self.stream.offer.item_count,
            total_bytes: self.stream.offer.total_bytes,
        }
    }
}

pub(crate) struct ActivePaste {
    pub addr: SocketAddr,
    pub id: [u8; 16],
    pub cancel: CancellationToken,
    pub task: tokio::task::JoinHandle<()>,
}

impl Drop for ActivePaste {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

pub(crate) async fn paste(
    offer: RemoteFileOffer,
    destination: PathBuf,
    cancel: CancellationToken,
    tx: UnboundedSender<FilePasteProgress>,
) {
    let id = offer.stream.offer.transfer_id.0;
    let total = offer.stream.offer.total_bytes;
    let report = |received, phase, message| {
        let _ = tx.send(FilePasteProgress {
            id,
            received,
            total,
            phase,
            message,
        });
    };
    report(0, FilePastePhase::Receiving, None);
    let result = async {
        // Validate the destination before requesting any file contents.
        if !destination.is_absolute() || !destination.is_dir() {
            return Err(io::Error::other("Choose an existing destination folder"));
        }
        let preflight = destination.clone();
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            #[cfg(windows)]
            let _access = crate::windows_service::desktop_file_access()?;
            let _probe = tempfile::Builder::new()
                .prefix(".mousehop-paste-check-")
                .tempfile_in(preflight)?;
            Ok(())
        })
        .await
        .map_err(io::Error::other)??;
        let staged = receive_stream_with_progress(
            offer.addr,
            offer.stream,
            TransferLimits::default(),
            |received| report(received, FilePastePhase::Receiving, None),
        )
        .await
        .map_err(io::Error::other)?;
        if input_capture::clipboard::native_clipboard_version() != offer.clipboard_version {
            return Err(io::Error::other("Clipboard changed; copy the files again"));
        }
        report(total, FilePastePhase::Saving, None);
        tokio::task::spawn_blocking(move || publish(staged, &destination, &cancel))
            .await
            .map_err(io::Error::other)??;
        Ok::<_, io::Error>(())
    }
    .await;
    match result {
        Ok(()) => report(total, FilePastePhase::Complete, None),
        Err(error) => report(0, FilePastePhase::Failed, Some(error.to_string())),
    }
}

/// Only remove paths this operation created. Never merge into an existing
/// directory, replace a file, or recursively delete another application's data.
#[derive(Default)]
struct CreatedPaths(Vec<(PathBuf, bool)>);
impl Drop for CreatedPaths {
    fn drop(&mut self) {
        for (path, directory) in self.0.iter().rev() {
            if *directory {
                let _ = fs::remove_dir(path);
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn check_cancel(cancel: &CancellationToken) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "File paste cancelled",
        ))
    } else {
        Ok(())
    }
}

fn publish(
    staged: StagedClipboard,
    destination: &Path,
    cancel: &CancellationToken,
) -> io::Result<()> {
    let StagedClipboard::FileTree {
        _staging,
        _roots: roots,
    } = staged
    else {
        return Err(io::Error::other("Only file offers can be pasted"));
    };
    #[cfg(windows)]
    let _access = crate::windows_service::desktop_file_access()?;
    // Fail before copying anything if any top-level name already exists.
    for root in &roots {
        let name = root
            .file_name()
            .ok_or_else(|| io::Error::other("Invalid filename"))?;
        if fs::symlink_metadata(destination.join(name)).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", name.to_string_lossy()),
            ));
        }
    }
    let mut created = CreatedPaths::default();
    for root in roots {
        let name = root
            .file_name()
            .ok_or_else(|| io::Error::other("Invalid filename"))?;
        copy_new(&root, &destination.join(name), cancel, &mut created)?;
    }
    check_cancel(cancel)?;
    created.0.clear();
    Ok(())
}

fn copy_new(
    source: &Path,
    target: &Path,
    cancel: &CancellationToken,
    created: &mut CreatedPaths,
) -> io::Result<()> {
    use io::{Read, Write};
    check_cancel(cancel)?;
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir(target)?;
        created.0.push((target.into(), true));
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_new(
                &entry.path(),
                &target.join(entry.file_name()),
                cancel,
                created,
            )?;
        }
    } else if metadata.is_file() {
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)?;
        created.0.push((target.into(), false));
        let mut input = fs::File::open(source)?;
        let mut buffer = vec![0; 256 * 1024];
        loop {
            check_cancel(cancel)?;
            let length = input.read(&mut buffer)?;
            if length == 0 {
                break;
            }
            output.write_all(&buffer[..length])?;
        }
        output.flush()?;
    } else {
        return Err(io::Error::other("Unsupported staged file type"));
    }
    Ok(())
}

pub(crate) struct OutgoingFile {
    pub id: [u8; 16],
    pub task: tokio::task::JoinHandle<()>,
}
impl Drop for OutgoingFile {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard_bulk::{OutgoingPayload, OutgoingTransfer};

    fn staged(files: &[(&str, &[u8])]) -> StagedClipboard {
        let staging = tempfile::tempdir().unwrap();
        let mut roots = Vec::new();
        for (name, contents) in files {
            let path = staging.path().join(name);
            fs::write(&path, contents).unwrap();
            roots.push(path);
        }
        StagedClipboard::FileTree {
            _staging: staging,
            _roots: roots,
        }
    }

    #[test]
    fn paste_refuses_existing_file_without_overwriting_or_partial_output() {
        let destination = tempfile::tempdir().unwrap();
        fs::write(destination.path().join("existing.txt"), b"keep me").unwrap();
        let error = publish(
            staged(&[("new.txt", b"new"), ("existing.txt", b"replace")]),
            destination.path(),
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(destination.path().join("existing.txt")).unwrap(),
            b"keep me"
        );
        assert!(!destination.path().join("new.txt").exists());
    }

    #[test]
    fn cancellation_cleans_only_created_paths() {
        let destination = tempfile::tempdir().unwrap();
        fs::write(destination.path().join("unrelated"), b"keep").unwrap();
        let token = CancellationToken::new();
        token.cancel();
        assert!(publish(staged(&[("new.txt", b"new")]), destination.path(), &token).is_err());
        assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn failed_copy_rolls_back_earlier_files_and_rejects_symlinks() {
        let staging = tempfile::tempdir().unwrap();
        let first = staging.path().join("first.txt");
        let link = staging.path().join("link");
        fs::write(&first, b"first").unwrap();
        std::os::unix::fs::symlink(&first, &link).unwrap();
        let destination = tempfile::tempdir().unwrap();
        let staged = StagedClipboard::FileTree {
            _staging: staging,
            _roots: vec![first, link],
        };
        assert!(publish(staged, destination.path(), &CancellationToken::new()).is_err());
        assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn file_offer_is_idle_until_explicit_paste_then_reports_saved_completion() {
        let source = tempfile::tempdir().unwrap();
        let folder = source.path().join("資料夾");
        fs::create_dir(&folder).unwrap();
        fs::create_dir(folder.join("empty")).unwrap();
        let bytes = vec![42; 600_000];
        fs::write(folder.join("節氣日期.csv"), &bytes).unwrap();
        let transfer = OutgoingTransfer::bind(
            "127.0.0.1".parse().unwrap(),
            "source".into(),
            OutgoingPayload::file_tree(vec![folder], None).unwrap(),
        )
        .await
        .unwrap();
        let offer = RemoteFileOffer {
            addr: "127.0.0.1:4252".parse().unwrap(),
            stream: transfer.offer(),
            clipboard_version: input_capture::clipboard::native_clipboard_version(),
        };
        let id = offer.summary().id;
        assert!(offer.is_current(id, offer.clipboard_version));
        assert!(!offer.is_current([0; 16], offer.clipboard_version));
        assert!(!offer.is_current(
            id,
            Some(offer.clipboard_version.unwrap_or(0).wrapping_add(1))
        ));
        let sender = tokio::spawn(transfer.serve_if_current(None));
        let destination = tempfile::tempdir().unwrap();
        tokio::task::yield_now().await;
        // Metadata offers must remain valid beyond the previous automatic
        // stream's 15-second handshake deadline, without opening a receiver.
        tokio::time::advance(std::time::Duration::from_secs(16)).await;
        tokio::task::yield_now().await;
        assert!(
            !sender.is_finished(),
            "copy must only wait for a paste request"
        );
        assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 0);
        // Real sockets need the real clock after the idle-deadline assertion.
        tokio::time::resume();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        paste(
            offer,
            destination.path().into(),
            CancellationToken::new(),
            tx,
        )
        .await;
        sender.await.unwrap().unwrap();
        assert_eq!(
            fs::read(destination.path().join("資料夾/節氣日期.csv")).unwrap(),
            bytes
        );
        assert!(destination.path().join("資料夾/empty").is_dir());
        let mut updates = Vec::new();
        while let Some(progress) = rx.recv().await {
            updates.push(progress);
        }
        assert!(
            updates
                .iter()
                .any(|update| update.phase == FilePastePhase::Receiving
                    && update.received == 600_000)
        );
        assert!(
            updates
                .iter()
                .any(|update| update.phase == FilePastePhase::Saving)
        );
        assert_eq!(updates.last().unwrap().phase, FilePastePhase::Complete);
    }
}

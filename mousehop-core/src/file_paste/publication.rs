//! Same-directory backups make an explicitly approved replacement reversible.
use super::*;
use mousehop_ipc::{FilePasteConflict, FilePasteConflictAction, valid_paste_name};
use std::time::Duration;

pub(super) struct ConflictContext {
    pub id: [u8; 16],
    pub tx: UnboundedSender<PendingFileConflict>,
}
impl ConflictContext {
    fn ask(
        &self,
        target: &Path,
        source: &Path,
        cancel: &CancellationToken,
    ) -> io::Result<FilePasteConflictAction> {
        let (reply, receiver) = std::sync::mpsc::channel();
        self.tx
            .send(PendingFileConflict {
                question: FilePasteConflict {
                    id: self.id,
                    request_id: rand::random(),
                    name: target
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    suggested_name: available_name(target, source.is_dir())?,
                },
                reply,
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    "File conflict window is unavailable",
                )
            })?;
        loop {
            check_cancel(cancel)?;
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(action) => return Ok(action),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "File paste cancelled",
                    ));
                }
            }
        }
    }
}

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}
fn available_name(target: &Path, directory: bool) -> io::Result<String> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("Missing destination folder"))?;
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let (stem, extension) = if directory {
        (name.as_ref(), None)
    } else {
        (
            target
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or(&name),
            target.extension().and_then(|name| name.to_str()),
        )
    };
    for index in 2..=100_000 {
        let candidate = match extension {
            Some(extension) => format!("{stem} ({index}).{extension}"),
            None => format!("{stem} ({index})"),
        };
        if !exists(&parent.join(&candidate))? {
            return Ok(candidate);
        }
    }
    Err(io::Error::other("Cannot find an unused filename"))
}

#[cfg(target_os = "macos")]
fn move_new(source: &Path, target: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source = CString::new(source.as_os_str().as_bytes())?;
    let target = CString::new(target.as_os_str().as_bytes())?;
    if unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
#[cfg(target_os = "linux")]
fn move_new(source: &Path, target: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source = CString::new(source.as_os_str().as_bytes())?;
    let target = CString::new(target.as_os_str().as_bytes())?;
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
#[cfg(windows)]
fn move_new(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::Storage::FileSystem::{MOVE_FILE_FLAGS, MoveFileExW},
        core::PCWSTR,
    };
    let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<_> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(target.as_ptr()),
            MOVE_FILE_FLAGS(0),
        )
    }?;
    Ok(())
}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn move_new(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Safe replacement is unsupported on this platform",
    ))
}

#[derive(Default)]
struct Publication {
    created: CreatedPaths,
    backups: Vec<(PathBuf, PathBuf)>,
    directory: Option<tempfile::TempDir>,
    committed: bool,
}
impl Publication {
    fn backup(&mut self, target: &Path, destination: &Path) -> io::Result<()> {
        if self.directory.is_none() {
            self.directory = Some(
                tempfile::Builder::new()
                    .prefix(".mousehop-replaced-")
                    .tempdir_in(destination)?,
            );
        }
        let backup = self
            .directory
            .as_ref()
            .unwrap()
            .path()
            .join(self.backups.len().to_string());
        move_new(target, &backup)?;
        self.backups.push((target.to_owned(), backup));
        Ok(())
    }
    fn rollback(&mut self) -> io::Result<()> {
        drop(std::mem::take(&mut self.created));
        let mut failures = Vec::new();
        for (target, backup) in self.backups.drain(..).rev() {
            if let Err(error) = move_new(&backup, &target) {
                failures.push(format!("{}: {error}", target.display()));
            }
        }
        if failures.is_empty() {
            return Ok(());
        }
        let directory = self
            .directory
            .take()
            .expect("backups have a directory")
            .keep();
        let message = format!(
            "Original files retained in {} because restore failed: {}",
            directory.display(),
            failures.join("; ")
        );
        log::error!("{message}");
        Err(io::Error::other(message))
    }
}
impl Drop for Publication {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.rollback();
        }
    }
}

pub(super) fn publish(
    staged: StagedClipboard,
    destination: &Path,
    cancel: &CancellationToken,
    conflicts: Option<&ConflictContext>,
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
    let mut publication = Publication::default();
    let result = (|| {
        for root in roots {
            check_cancel(cancel)?;
            let name = root
                .file_name()
                .ok_or_else(|| io::Error::other("Invalid filename"))?;
            let mut target = destination.join(name);
            loop {
                check_cancel(cancel)?;
                if publication
                    .created
                    .0
                    .iter()
                    .any(|(path, _)| path == &target)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "Two copied items have the same destination name",
                    ));
                }
                if !exists(&target)? {
                    break;
                }
                let conflicts = conflicts.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} already exists", target.display()),
                    )
                })?;
                match conflicts.ask(&target, &root, cancel)? {
                    FilePasteConflictAction::Replace => {
                        check_cancel(cancel)?;
                        publication.backup(&target, destination)?;
                        break;
                    }
                    FilePasteConflictAction::Rename(name) => {
                        if !valid_paste_name(&name) {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "Invalid filename",
                            ));
                        }
                        target = destination.join(name);
                    }
                    FilePasteConflictAction::Cancel => {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "File paste cancelled",
                        ));
                    }
                }
            }
            // create_new prevents a file appearing after the user's decision
            // from being overwritten without another explicit decision.
            copy_new(&root, &target, cancel, &mut publication.created)?;
        }
        check_cancel(cancel)
    })();
    if let Err(error) = result {
        publication
            .rollback()
            .map_err(|rollback| io::Error::other(format!("{error}; {rollback}")))?;
        return Err(error);
    }
    publication.created.0.clear();
    publication.committed = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::staged;
    use super::*;

    fn publish_with_actions(
        staged: StagedClipboard,
        destination: &Path,
        actions: Vec<FilePasteConflictAction>,
    ) -> io::Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PendingFileConflict>();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                for action in actions {
                    let conflict = rx.blocking_recv().expect("expected a conflict");
                    assert_eq!(conflict.question.id, [7; 16]);
                    conflict.reply.send(action).unwrap();
                }
            });
            let context = ConflictContext { id: [7; 16], tx };
            publish(
                staged,
                destination,
                &CancellationToken::new(),
                Some(&context),
            )
        })
    }

    #[test]
    fn replace_is_explicit_and_rename_keeps_both_files() {
        let destination = tempfile::tempdir().unwrap();
        fs::write(destination.path().join("report.txt"), b"original").unwrap();
        publish_with_actions(
            staged(&[("report.txt", b"replacement")]),
            destination.path(),
            vec![FilePasteConflictAction::Replace],
        )
        .unwrap();
        assert_eq!(
            fs::read(destination.path().join("report.txt")).unwrap(),
            b"replacement"
        );
        publish_with_actions(
            staged(&[("report.txt", b"second")]),
            destination.path(),
            vec![FilePasteConflictAction::Rename("report (2).txt".into())],
        )
        .unwrap();
        assert_eq!(
            fs::read(destination.path().join("report.txt")).unwrap(),
            b"replacement"
        );
        assert_eq!(
            fs::read(destination.path().join("report (2).txt")).unwrap(),
            b"second"
        );
        assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 2);
    }

    #[test]
    fn cancelling_a_later_conflict_rolls_back_prior_replacements() {
        let destination = tempfile::tempdir().unwrap();
        fs::write(destination.path().join("first.txt"), b"first original").unwrap();
        fs::write(destination.path().join("last.txt"), b"last original").unwrap();
        let error = publish_with_actions(
            staged(&[
                ("first.txt", b"replace"),
                ("new.txt", b"new"),
                ("last.txt", b"cancel"),
            ]),
            destination.path(),
            vec![
                FilePasteConflictAction::Replace,
                FilePasteConflictAction::Cancel,
            ],
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            fs::read(destination.path().join("first.txt")).unwrap(),
            b"first original"
        );
        assert_eq!(
            fs::read(destination.path().join("last.txt")).unwrap(),
            b"last original"
        );
        assert!(!destination.path().join("new.txt").exists());
        assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 2);
    }

    #[test]
    fn rename_to_another_existing_name_asks_again_and_rejects_paths() {
        let destination = tempfile::tempdir().unwrap();
        for name in ["report.txt", "other.txt"] {
            fs::write(destination.path().join(name), b"original").unwrap();
        }
        publish_with_actions(
            staged(&[("report.txt", b"incoming")]),
            destination.path(),
            vec![
                FilePasteConflictAction::Rename("other.txt".into()),
                FilePasteConflictAction::Rename("new.txt".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            fs::read(destination.path().join("new.txt")).unwrap(),
            b"incoming"
        );
        assert!(
            publish_with_actions(
                staged(&[("report.txt", b"unsafe")]),
                destination.path(),
                vec![FilePasteConflictAction::Rename("../escaped.txt".into())]
            )
            .is_err()
        );
        assert_eq!(
            fs::read(destination.path().join("report.txt")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn replacing_a_folder_replaces_the_whole_item() {
        let destination = tempfile::tempdir().unwrap();
        let target = destination.path().join("folder");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("old.txt"), b"old").unwrap();
        let staging = tempfile::tempdir().unwrap();
        let source = staging.path().join("folder");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("new.txt"), b"new").unwrap();
        publish_with_actions(
            StagedClipboard::FileTree {
                _staging: staging,
                _roots: vec![source],
            },
            destination.path(),
            vec![FilePasteConflictAction::Replace],
        )
        .unwrap();
        assert!(!target.join("old.txt").exists());
        assert_eq!(fs::read(target.join("new.txt")).unwrap(), b"new");
    }

    #[test]
    fn failed_restore_preserves_backup_and_never_overwrites_a_concurrent_file() {
        let destination = tempfile::tempdir().unwrap();
        let target = destination.path().join("report.txt");
        fs::write(&target, b"original").unwrap();
        let mut publication = Publication::default();
        publication.backup(&target, destination.path()).unwrap();
        let backup = publication.directory.as_ref().unwrap().path().join("0");
        fs::write(&target, b"written by another program").unwrap();
        assert!(publication.rollback().is_err());
        assert_eq!(fs::read(&target).unwrap(), b"written by another program");
        assert_eq!(fs::read(backup).unwrap(), b"original");
    }
}

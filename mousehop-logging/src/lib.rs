//! Daily UTC log files and seven-calendar-day retention shared by all processes.
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime},
};
use time::{Date, OffsetDateTime, format_description::well_known::Rfc3339};

const ROLES: &[&str] = &[
    "agent", "core", "service", "session", "worker", "gui", "transfer", "panic",
];
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

pub fn user_log_directory() -> io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    let path = std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Logs/Mousehop"));
    #[cfg(windows)]
    let path = std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("Mousehop/logs"));
    #[cfg(not(any(target_os = "macos", windows)))]
    let path = std::env::var_os("XDG_STATE_HOME")
        .map(|p| PathBuf::from(p).join("mousehop"))
        .or_else(|| {
            std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state/mousehop"))
        });
    path.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Cannot determine the Mousehop log directory",
        )
    })
}

/// File logging is enabled at INFO unless MOUSEHOP_LOG_LEVEL overrides it.
pub fn init(role: &str, directory: PathBuf) -> io::Result<()> {
    let writer = DailyLog::new(&directory, role)?;
    start_maintenance(&directory)?;
    env_logger::Builder::from_env(
        env_logger::Env::default().filter_or("MOUSEHOP_LOG_LEVEL", "info"),
    )
    .target(env_logger::Target::Pipe(Box::new(writer)))
    .try_init()
    .map_err(io::Error::other)?;
    log::info!("File logging enabled for {role}; retaining seven UTC dates");
    Ok(())
}

/// Each process owns its file, so service workers cannot interleave records or
/// prevent one another from rotating a shared service.log on Windows.
pub struct DailyLog {
    directory: PathBuf,
    role: String,
    date: Date,
    file: File,
}
impl DailyLog {
    pub fn new(directory: &Path, role: &str) -> io::Result<Self> {
        Self::at(directory, role, OffsetDateTime::now_utc().date())
    }
    fn at(directory: &Path, role: &str, date: Date) -> io::Result<Self> {
        if !ROLES.contains(&role) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Unknown Mousehop log role",
            ));
        }
        fs::create_dir_all(directory)?;
        let file = Self::open(directory, role, date)?;
        Ok(Self {
            directory: directory.to_owned(),
            role: role.to_owned(),
            date,
            file,
        })
    }
    fn open(directory: &Path, role: &str, date: Date) -> io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join(format!("{role}-{date}-{}.log", std::process::id())))
    }
    fn write_at(&mut self, buffer: &[u8], date: Date) -> io::Result<usize> {
        if date != self.date {
            self.file = Self::open(&self.directory, &self.role, date)?;
            self.date = date;
        }
        self.file.write_all(buffer)?;
        Ok(buffer.len())
    }
}
impl Write for DailyLog {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.write_at(buffer, OffsetDateTime::now_utc().date())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn start_maintenance(directory: &Path) -> io::Result<()> {
    static DIRECTORIES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let mut directories = DIRECTORIES
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| io::Error::other("Log maintenance lock poisoned"))?;
    if directories.contains(directory) {
        return Ok(());
    }
    let root = directory.to_owned();
    std::thread::Builder::new()
        .name("mousehop-log-cleanup".into())
        .spawn(move || {
            let mut snapshots = HashMap::new();
            loop {
                if let Err(error) =
                    cleanup_with_snapshots(&root, OffsetDateTime::now_utc(), &mut snapshots)
                {
                    // stderr avoids recursively writing through a logger during cleanup.
                    eprintln!("Mousehop log cleanup in {}: {error}", root.display());
                }
                std::thread::sleep(CLEANUP_INTERVAL);
            }
        })?;
    directories.insert(directory.to_owned());
    Ok(())
}

fn dated_name(name: &str) -> Option<Date> {
    let (prefix, pid) = name.strip_suffix(".log")?.rsplit_once('-')?;
    if pid.parse::<u32>().ok()? == 0 {
        return None;
    }
    let date = prefix.get(prefix.len().checked_sub(10)?..)?;
    let role = prefix.strip_suffix(date)?.strip_suffix('-')?;
    if !ROLES.contains(&role) {
        return None;
    }
    let mut components = date.split('-');
    let year = components.next()?.parse().ok()?;
    let month = components.next()?.parse::<u8>().ok()?.try_into().ok()?;
    let day = components.next()?.parse().ok()?;
    if components.next().is_some() {
        return None;
    }
    Date::from_calendar_date(year, month, day).ok()
}

/// Keep today and the preceding six UTC dates. Only recognized Mousehop files
/// are considered; configuration, identity files and unknown logs are untouched.
pub fn cleanup(directory: &Path, now: OffsetDateTime) -> io::Result<()> {
    cleanup_with_snapshots(directory, now, &mut HashMap::new())
}

#[derive(PartialEq, Eq)]
struct Snapshot {
    length: u64,
    modified: SystemTime,
    cutoff: Date,
}
fn snapshot(path: &Path, cutoff: Date) -> io::Result<Snapshot> {
    let metadata = fs::symlink_metadata(path)?;
    Ok(Snapshot {
        length: metadata.len(),
        modified: metadata.modified()?,
        cutoff,
    })
}
fn cleanup_with_snapshots(
    directory: &Path,
    now: OffsetDateTime,
    snapshots: &mut HashMap<PathBuf, Snapshot>,
) -> io::Result<()> {
    let cutoff = now.date() - time::Duration::days(6);
    let mut first_error = None;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let temporary_created = name
            .strip_prefix(".mousehop-log-retention-")
            .filter(|_| name.ends_with(".tmp"))
            .and_then(|name| name.split('-').next()?.parse::<i64>().ok());
        let result = if temporary_created
            .is_some_and(|created| now.unix_timestamp().saturating_sub(created) > 3600)
        {
            fs::remove_file(entry.path())
        } else if let Some(date) = dated_name(name) {
            if date < cutoff {
                fs::remove_file(entry.path())
            } else {
                Ok(())
            }
        } else if name
            .strip_suffix(".log")
            .is_some_and(|role| ROLES.contains(&role))
        {
            let path = entry.path();
            let before = snapshot(&path, cutoff)?;
            if snapshots.get(&path) == Some(&before) {
                Ok(())
            } else {
                prune_legacy(&path, cutoff, now).and_then(|finished| {
                    if finished {
                        match snapshot(&path, cutoff) {
                            Ok(value) => {
                                snapshots.insert(path.clone(), value);
                            }
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                                snapshots.remove(&path);
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    Ok(())
                })
            }
        } else {
            Ok(())
        };
        if let Err(error) = result {
            if error.kind() != io::ErrorKind::NotFound && first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn record_date(line: &[u8]) -> Option<Date> {
    let text = std::str::from_utf8(line).ok()?;
    if let Some(header) = text.strip_prefix('[') {
        let timestamp = header.split_ascii_whitespace().next()?;
        return OffsetDateTime::parse(timestamp, &Rfc3339)
            .ok()
            .map(|time| time.date());
    }
    if text.starts_with("==== mousehop ") {
        let timestamp = text
            .split_once("unix ")?
            .1
            .split('s')
            .next()?
            .parse()
            .ok()?;
        return OffsetDateTime::from_unix_timestamp(timestamp)
            .ok()
            .map(|time| time.date());
    }
    None
}

fn prune_legacy(path: &Path, cutoff: Date, now: OffsetDateTime) -> io::Result<bool> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(false),
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    }
    let before = file.metadata()?;
    // Older executables do not use the retention lock. Defer files that they
    // are still writing, and recheck the snapshot before replacing anything.
    if SystemTime::from(now)
        .duration_since(before.modified()?)
        .unwrap_or_default()
        < CLEANUP_INTERVAL
    {
        return Ok(false);
    }
    let initial_keep = OffsetDateTime::from(before.modified()?).date() >= cutoff;
    let mut reader = BufReader::new(&file);
    let mut line = Vec::new();
    let mut keep = initial_keep;
    let mut expired = false;
    // Do not rewrite a large legacy file merely to discover that all records
    // are recent. The common no-expiry path performs reads only.
    while reader.read_until(b'\n', &mut line)? != 0 {
        if let Some(date) = record_date(&line) {
            keep = date >= cutoff;
        }
        if !keep {
            expired = true;
            break;
        }
        line.clear();
    }
    if !expired {
        return Ok(true);
    }
    reader.seek(SeekFrom::Start(0))?;
    line.clear();
    let mut retained = tempfile::Builder::new()
        .prefix(&format!(
            ".mousehop-log-retention-{}-{}-",
            now.unix_timestamp(),
            std::process::id()
        ))
        .suffix(".tmp")
        .tempfile_in(
            path.parent()
                .ok_or_else(|| io::Error::other("Log parent missing"))?,
        )?;
    keep = initial_keep;
    while reader.read_until(b'\n', &mut line)? != 0 {
        if let Some(date) = record_date(&line) {
            keep = date >= cutoff;
        }
        if keep {
            retained.write_all(&line)?;
        }
        line.clear();
    }
    drop(reader);
    let current = fs::symlink_metadata(path)?;
    if !current.is_file()
        || current.len() != before.len()
        || current.modified()? != before.modified()?
    {
        return Ok(false);
    }
    if retained.as_file().metadata()?.len() == 0 {
        fs::remove_file(path)?;
        return Ok(true);
    }
    retained.as_file().set_permissions(before.permissions())?;
    retained.as_file().set_modified(before.modified()?)?;
    retained.flush()?;
    retained.as_file().sync_all()?;
    // Publish a complete filtered snapshot atomically. A process exiting during
    // cleanup must not leave an existing diagnostic log partially rewritten.
    retained.persist(path).map_err(|error| error.error)?;
    Ok(true)
}

/// Panic output bypasses env_logger's mutex and uses a separate panic file.
pub fn install_panic(role: &'static str, directory: PathBuf) {
    if fs::create_dir_all(&directory).is_err() {
        return;
    }
    let _ = start_maintenance(&directory);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let when = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".into());
        let thread = std::thread::current();
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .map(str::to_owned)
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".into());
        let record = format!(
            "\n==== mousehop {role} panic — pid {}, unix {when}s ====\nthread '{}' panicked at {location}:\n{message}\nstack backtrace:\n{}\n",
            std::process::id(),
            thread.name().unwrap_or("<unnamed>"),
            std::backtrace::Backtrace::force_capture()
        );
        if let Ok(mut file) = DailyLog::new(&directory, "panic") {
            let _ = file.write_all(record.as_bytes());
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    fn now() -> OffsetDateTime {
        OffsetDateTime::parse("2026-01-03T12:00:00Z", &Rfc3339).unwrap()
    }
    fn idle(path: &Path) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::from(now() - time::Duration::minutes(5)))
            .unwrap();
    }

    #[test]
    fn retains_seven_dates_across_year_boundary_and_preserves_unowned_files() {
        let root = tempfile::tempdir().unwrap();
        for age in 0..10 {
            let date = now().date() - time::Duration::days(age);
            fs::write(root.path().join(format!("worker-{date}-123.log")), "record").unwrap();
        }
        for name in [
            "identity.pem",
            "config.toml",
            "notes-2020-01-01-123.log",
            "worker-2020-01-01-123.log.backup",
        ] {
            fs::write(root.path().join(name), "keep").unwrap();
        }
        cleanup(root.path(), now()).unwrap();
        for age in 0..10 {
            let date = now().date() - time::Duration::days(age);
            assert_eq!(
                root.path().join(format!("worker-{date}-123.log")).exists(),
                age < 7
            );
        }
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 11);
    }

    #[test]
    fn an_alive_writer_rotates_when_the_date_changes() {
        let root = tempfile::tempdir().unwrap();
        let previous = now().date() - time::Duration::days(1);
        let mut writer = DailyLog::at(root.path(), "core", previous).unwrap();
        writer.write_at(b"yesterday\n", previous).unwrap();
        writer.write_at(b"today\n", now().date()).unwrap();
        drop(writer);
        let pid = std::process::id();
        assert_eq!(
            fs::read_to_string(root.path().join(format!("core-{previous}-{pid}.log"))).unwrap(),
            "yesterday\n"
        );
        assert_eq!(
            fs::read_to_string(root.path().join(format!("core-{}-{pid}.log", now().date())))
                .unwrap(),
            "today\n"
        );
    }

    #[test]
    fn compacts_legacy_records_by_timestamp_and_keeps_multiline_panic_details() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("service.log");
        let mut contents =
            "[2025-12-20T12:00:00Z INFO worker] old\nold continuation\n".repeat(4096);
        let recent = "[2026-01-02T23:59:59Z ERROR worker] new\nnew continuation\n";
        contents.push_str(recent);
        fs::write(&path, contents).unwrap();
        idle(&path);
        let panic = root.path().join("panic.log");
        let retained_panic = format!(
            "==== mousehop daemon panic — pid 7, unix {}s ====\nthread main panicked\nstack backtrace:\n  0: frame\n",
            now().unix_timestamp()
        );
        fs::write(&panic, format!("==== mousehop daemon panic — pid 7, unix 100000s ====\nold backtrace\n{retained_panic}")).unwrap();
        idle(&panic);
        cleanup(root.path(), now()).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), recent);
        assert_eq!(fs::read_to_string(panic).unwrap(), retained_panic);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn defers_legacy_files_still_being_written() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.log");
        let original = "[2025-12-20T12:00:00Z INFO agent] old\n";
        fs::write(&path, original).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::from(now()))
            .unwrap();
        cleanup(root.path(), now()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        idle(&path);
        cleanup(root.path(), now()).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn multiple_process_style_cleanup_handles_do_not_damage_recent_records() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("core.log");
        let recent = "[2026-01-03T11:59:59Z INFO core] keep\n";
        fs::write(
            &path,
            format!("[2025-12-01T00:00:00Z INFO core] remove\n{recent}"),
        )
        .unwrap();
        idle(&path);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let directory = root.path();
                scope.spawn(move || cleanup(directory, now()).unwrap());
            }
        });
        assert_eq!(fs::read_to_string(path).unwrap(), recent);
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_a_log_named_symlink() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "unrelated data").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("core-2020-01-01-1.log"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("core.log")).unwrap();
        cleanup(root.path(), now()).unwrap();
        assert_eq!(
            fs::read_to_string(outside.path()).unwrap(),
            "unrelated data"
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn cleans_only_abandoned_retention_temporary_files() {
        let root = tempfile::tempdir().unwrap();
        let abandoned = format!(
            ".mousehop-log-retention-{}-123-random.tmp",
            now().unix_timestamp() - 7200
        );
        let active = format!(
            ".mousehop-log-retention-{}-123-random.tmp",
            now().unix_timestamp()
        );
        for name in [&abandoned, &active, &"other.tmp".to_owned()] {
            fs::write(root.path().join(name), "data").unwrap();
        }
        cleanup(root.path(), now()).unwrap();
        assert!(!root.path().join(abandoned).exists());
        assert!(root.path().join(active).exists());
        assert!(root.path().join("other.tmp").exists());
    }
}

//! Background desktop application. No widget toolkit is linked into this process.
use crate::{
    heartbeat::Heartbeat,
    paste_control::{Action, PasteControl},
    single_instance,
};
use input_capture::native_paste::NativePasteBridge;
use mousehop_ipc::{FilePastePhase, FrontendEvent, FrontendRequest, RuntimeState};
use std::{
    collections::HashSet,
    io,
    process::{Child, Command},
    sync::mpsc,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(crate) enum TrayCmd {
    TogglePresent,
    ToggleSharing,
    Logs,
    Quit,
}
#[cfg(target_os = "linux")]
use crate::linux_tray::Tray;
#[cfg(target_os = "macos")]
use crate::macos_tray::Tray;
#[cfg(windows)]
use crate::windows_tray::Tray;
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
struct Tray;
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
impl Tray {
    fn new() -> io::Result<Self> {
        Ok(Self)
    }
    fn poll(&self) -> Option<TrayCmd> {
        None
    }
    fn set_state(&self, _: Option<RuntimeState>) {}
}

/// Serialize command and heartbeat writes on one negotiated IPC connection.
#[derive(Clone)]
struct AgentWriter(std::sync::Arc<std::sync::Mutex<mousehop_ipc::FrontendRequestWriter>>);
impl AgentWriter {
    fn request(&self, request: FrontendRequest) -> io::Result<()> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("Agent IPC writer lock poisoned"))?
            .request(request)
    }
}

pub(crate) fn sibling(name: &str) -> io::Result<std::path::PathBuf> {
    let executable = std::env::current_exe()?;
    let parent = executable
        .parent()
        .ok_or_else(|| io::Error::other("Executable directory missing"))?;
    let path = parent.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Missing {}; install or extract the complete Mousehop package",
                path.display()
            ),
        ));
    }
    Ok(path)
}

fn open_ui(arguments: &[String]) -> io::Result<Child> {
    Command::new(sibling("mousehop-ui")?)
        .args(arguments)
        .spawn()
}

fn spawn_core() -> io::Result<Child> {
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    if args
        .last()
        .is_some_and(|arg| arg == "agent" || arg == "settings")
    {
        args.pop();
    }
    Command::new(std::env::current_exe()?)
        .args(args)
        .arg("daemon")
        .spawn()
}

struct CoreChild(Option<Child>);
impl Drop for CoreChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            if let Ok((_, mut writer)) = mousehop_ipc::connect_once() {
                let _ = writer.request(FrontendRequest::Quit);
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Child handles alone do not terminate GUI processes when the Agent exits.
#[derive(Default)]
struct UiChildren(Vec<Child>);
impl Drop for UiChildren {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            self.0
                .retain_mut(|child| child.try_wait().ok().flatten().is_none());
            if self.0.is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(crate) fn run(show_settings: bool) -> Result<(), Box<dyn std::error::Error>> {
    // run_session drops Core, GUI children and the single-instance guard before
    // the replacement Agent is launched; otherwise it just wakes the old Agent.
    if run_session(show_settings)? {
        let mut args: Vec<_> = std::env::args_os().skip(1).collect();
        if args
            .last()
            .is_some_and(|arg| arg == "agent" || arg == "settings")
        {
            args.pop();
        }
        Command::new(std::env::current_exe()?)
            .args(args)
            .arg("settings")
            .spawn()?;
    }
    Ok(())
}

fn run_session(show_settings: bool) -> Result<bool, Box<dyn std::error::Error>> {
    let (present_tx, present_rx) = mpsc::channel();
    single_instance::set_present_callback(move || {
        let _ = present_tx.send(());
    });
    let _guard = match single_instance::acquire() {
        Ok(guard) => guard,
        Err(single_instance::AcquireError::AlreadyRunning) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    #[cfg(windows)]
    let installed = mousehop_ipc::windows::service_mode();
    #[cfg(not(windows))]
    let installed = false;
    #[cfg(windows)]
    if installed {
        mousehop_core::windows_service::ensure_running()?;
    }
    let mut core = CoreChild(None);
    if !installed
        && mousehop_ipc::connect_once()
            .err()
            .is_some_and(|error| error.is_endpoint_missing())
    {
        core.0 = Some(spawn_core()?);
    }
    let tray = Tray::new()?;
    tray.set_state(None);
    crate::signals::install();
    let mut backoff = Duration::from_millis(500);
    let mut restart_after = Instant::now();
    #[cfg(target_os = "macos")]
    let show_settings = show_settings
        || !mousehop_core::macos_tcc_probe::is_accessibility_granted()
        || !mousehop_core::macos_tcc_probe::is_input_monitoring_granted();
    let mut settings_pending = show_settings;
    let mut ui_children = UiChildren::default();
    let mut shown_transfers = HashSet::new();
    let mut shown_pairing = HashSet::new();
    let mut wanted_sharing = true;
    loop {
        let deadline = Instant::now() + Duration::from_secs(30);
        let (mut reader, writer) = loop {
            match mousehop_ipc::connect_once() {
                Ok(connection) => break connection,
                Err(error) if Instant::now() >= deadline => return Err(error.into()),
                Err(_) => {
                    let _ = tray.poll();
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        };

        let writer = AgentWriter(std::sync::Arc::new(std::sync::Mutex::new(writer)));
        // Native menu tracking can own this main thread. Keep draining IPC
        // independently while it is open; this mailbox contains UI/control
        // notifications, never the keyboard/pointer event stream.
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("mousehop-agent-ipc".into())
            .spawn(move || {
                while let Some(event) = reader.next_event() {
                    if tx.send(event).is_err() {
                        return;
                    }
                }
            })?;
        let mut native: Option<NativePasteBridge> = None;
        let mut paste = PasteControl::default();
        let mut negotiated = false;
        let mut attached = false;
        let mut forwarding = false;
        let mut sharing = true;
        let mut desktop = true;
        let mut heartbeat: Option<Heartbeat> = None;
        let connected_at = Instant::now();
        let mut suppressed = Vec::<String>::new();
        loop {
            if crate::signals::stopping() {
                writer.request(FrontendRequest::DesktopAgentDisconnected)?;
                writer.request(FrontendRequest::Quit)?;
                return Ok(false);
            }
            while present_rx.try_recv().is_ok() {
                settings_pending = true;
            }
            while let Some(command) = tray.poll() {
                match command {
                    TrayCmd::TogglePresent => settings_pending = true,
                    TrayCmd::ToggleSharing => {
                        wanted_sharing = !sharing;
                        writer.request(FrontendRequest::SetSharing(wanted_sharing))?;
                    }
                    TrayCmd::Logs => open_logs(),
                    TrayCmd::Quit => {
                        if let Some(native) = native.as_mut() {
                            native.disarm();
                        }
                        writer.request(FrontendRequest::DesktopAgentDisconnected)?;
                        writer.request(FrontendRequest::Quit)?;
                        return Ok(false);
                    }
                }
            }
            if settings_pending {
                match open_ui(&[]) {
                    Ok(child) => ui_children.0.push(child),
                    Err(error) => log::error!("Open settings: {error}"),
                }
                settings_pending = false;
            }
            ui_children
                .0
                .retain_mut(|child| child.try_wait().ok().flatten().is_none());
            if heartbeat.as_ref().is_some_and(Heartbeat::failed) {
                break;
            }
            match rx.try_recv() {
                Ok(Ok(event)) => match event {
                    FrontendEvent::ProtocolVersion(version) => {
                        if version != mousehop_ipc::LOCAL_PROTOCOL_VERSION {
                            return Err("Mousehop components have different IPC versions; update the complete package".into());
                        }
                        if !negotiated {
                            negotiated = true;
                            writer.request(FrontendRequest::DesktopAgentConnected {
                                sharing: wanted_sharing,
                            })?;
                            let heartbeat_writer = writer.clone();
                            heartbeat = Some(Heartbeat::start(move || {
                                heartbeat_writer.request(FrontendRequest::DesktopAgentHeartbeat)
                            })?);
                        }
                    }
                    FrontendEvent::Restart => {
                        if let Some(native) = native.as_mut() {
                            native.disarm();
                        }
                        writer.request(FrontendRequest::DesktopAgentDisconnected)?;
                        writer.request(FrontendRequest::Quit)?;
                        return Ok(true);
                    }
                    FrontendEvent::OpenSettings => settings_pending = true,
                    FrontendEvent::ConnectionAttempt { fingerprint } => {
                        settings_pending |=
                            shown_pairing.len() < 128 && shown_pairing.insert(fingerprint);
                    }
                    FrontendEvent::AgentAttached(state) => {
                        attached = true;
                        sharing = state == RuntimeState::Running;
                        tray.set_state(Some(state));
                    }
                    FrontendEvent::RuntimeState(state) if attached => {
                        tray.set_state(Some(state));
                        sharing = matches!(state, RuntimeState::Running);
                        wanted_sharing = sharing;
                        if !sharing {
                            if let Some(native) = native.as_mut() {
                                native.disarm();
                            }
                            paste.reset();
                        }
                        if matches!(state, RuntimeState::Stopped) {
                            return Ok(false);
                        }
                    }
                    FrontendEvent::DesktopAvailable(available) => {
                        desktop = available;
                        if !desktop {
                            if let Some(native) = native.as_mut() {
                                native.disarm();
                            }
                            paste.reset();
                        }
                    }
                    FrontendEvent::InputForwarding(value) => {
                        forwarding = value;
                        if let Some(native) = native.as_ref() {
                            native.set_forwarding(value);
                        }
                    }
                    FrontendEvent::Error(message) => log::error!("Core: {message}"),
                    FrontendEvent::FilePasteAvailable(copy) => {
                        match paste.offer(copy.map(|copy| copy.id), sharing && desktop) {
                            Action::Prepare(id) => {
                                writer.request(FrontendRequest::PrepareNativeFilePaste { id })?
                            }
                            Action::Disarm => {
                                if let Some(native) = native.as_mut() {
                                    native.disarm();
                                }
                            }
                            Action::None => {}
                        }
                    }
                    FrontendEvent::PrepareNativeFilePaste {
                        id,
                        clipboard_version,
                    } if sharing && desktop && paste.accept_preparation(id) => {
                        let result = (|| {
                            if native.is_none() {
                                native = Some(NativePasteBridge::new()?);
                            }
                            let bridge = native.as_mut().expect("native bridge initialized");
                            bridge.set_forwarding(forwarding);
                            bridge.arm(id, clipboard_version)
                        })();
                        let (version, error) = match result {
                            Ok(version) => {
                                paste.armed(id);
                                (version, None)
                            }
                            Err(error) => (None, Some(error.to_string())),
                        };
                        writer.request(FrontendRequest::NativeFilePastePrepared {
                            id,
                            clipboard_version: version,
                            error,
                        })?;
                    }
                    FrontendEvent::FilePasteProgress(progress) => {
                        let finished = paste.progress(
                            progress.id,
                            matches!(
                                progress.phase,
                                FilePastePhase::Receiving
                                    | FilePastePhase::Saving
                                    | FilePastePhase::WaitingForConflict
                            ),
                        );
                        if shown_transfers.insert(progress.id) {
                            let args = [
                                "transfer".into(),
                                mousehop_ipc::encode_transfer_id(&progress.id),
                            ];
                            match open_ui(&args) {
                                Ok(child) => ui_children.0.push(child),
                                Err(error) => log::error!("Transfer UI: {error}"),
                            }
                        }
                        if finished {
                            if let Some(native) = native.as_mut() {
                                native.disarm();
                            }
                        }
                    }
                    FrontendEvent::SuppressedAppsUpdated(apps) => suppressed = apps,
                    FrontendEvent::ConfigureAutostart(enabled) => {
                        let error = crate::autostart::set(enabled)
                            .err()
                            .map(|error| error.to_string());
                        let (enabled, managed) = crate::autostart::status();
                        writer.request(FrontendRequest::AutostartReport {
                            enabled,
                            managed,
                            error,
                        })?;
                    }
                    FrontendEvent::ConfigureAutostartStatus => {
                        let (enabled, managed) = crate::autostart::status();
                        writer.request(FrontendRequest::AutostartReport {
                            enabled,
                            managed,
                            error: None,
                        })?;
                    }
                    FrontendEvent::RequestRunningApps => {
                        let mut apps = input_capture::frontmost_app::list_running_apps();
                        for identifier in &suppressed {
                            if !apps.iter().any(|app| app.identifier == *identifier) {
                                if let Some(app) =
                                    input_capture::frontmost_app::lookup_app_metadata(identifier)
                                {
                                    apps.push(app);
                                }
                            }
                        }
                        writer.request(FrontendRequest::RunningAppsReport(apps))?;
                    }
                    _ => {}
                },
                Ok(Err(error)) => {
                    log::warn!("Agent IPC: {error}");
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if !negotiated && connected_at.elapsed() > Duration::from_secs(5) {
                return Err("Core did not complete IPC version negotiation".into());
            }
            if let Some(intent) = native.as_ref().and_then(NativePasteBridge::next_intent) {
                if sharing && desktop && paste.offered == Some(intent.id) {
                    writer.request(match intent.error {
                        Some(message) => FrontendRequest::NativeFilePasteFailed {
                            id: intent.id,
                            message,
                        },
                        None => FrontendRequest::PasteFiles {
                            id: intent.id,
                            destination: intent.destination,
                        },
                    })?;
                }
            }
            if let Some(child) = core.0.as_mut() {
                if let Some(status) = child.try_wait()? {
                    core.0 = None;
                    if status.success() {
                        return Ok(false);
                    }
                    restart_after = Instant::now() + backoff;
                    if connected_at.elapsed() >= Duration::from_secs(10) {
                        backoff = Duration::from_millis(500);
                    } else {
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(heartbeat);
        if let Some(native) = native.as_mut() {
            native.disarm();
        }
        tray.set_state(None);
        if !installed {
            if let Some(child) = core.0.as_mut() {
                if let Some(status) = child.try_wait()? {
                    core.0 = None;
                    if status.success() {
                        return Ok(false);
                    }
                    restart_after = Instant::now() + backoff;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
            // A transient pipe loss can occur before a live Core exits. Reattach
            // to it rather than killing it or launching a duplicate listener.
            if core.0.is_none()
                && mousehop_ipc::connect_once()
                    .err()
                    .is_some_and(|error| error.is_endpoint_missing())
            {
                while Instant::now() < restart_after {
                    let _ = tray.poll();
                    std::thread::sleep(Duration::from_millis(25));
                }
                core.0 = Some(spawn_core()?);
            }
        }
    }
}

fn open_logs() {
    #[cfg(windows)]
    let path = if mousehop_ipc::windows::service_mode() {
        mousehop_core::windows_service::data_dir().ok()
    } else {
        std::env::var_os("LOCALAPPDATA").map(|p| std::path::PathBuf::from(p).join("Mousehop/logs"))
    };
    #[cfg(target_os = "macos")]
    let path =
        std::env::var_os("HOME").map(|p| std::path::PathBuf::from(p).join("Library/Logs/Mousehop"));
    #[cfg(not(any(windows, target_os = "macos")))]
    let path = std::env::var_os("XDG_STATE_HOME")
        .map(|p| std::path::PathBuf::from(p).join("mousehop"))
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|p| std::path::PathBuf::from(p).join(".local/state/mousehop"))
        });
    if let Some(path) = path {
        #[cfg(windows)]
        let _ = Command::new("explorer.exe").arg(path).spawn();
        #[cfg(target_os = "macos")]
        let _ = Command::new("open").arg(path).spawn();
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = Command::new("xdg-open").arg(path).spawn();
    }
}

pub(crate) fn state_label(state: Option<RuntimeState>) -> &'static str {
    match state {
        Some(RuntimeState::Running) => "Mousehop: running",
        Some(RuntimeState::Paused) => "Mousehop: sharing paused",
        Some(RuntimeState::Stopped) => "Mousehop: stopped",
        None => "Mousehop: connecting / unavailable",
    }
}

#[cfg(all(test, unix))]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn shutdown_reaps_an_unresponsive_gui() {
        // A GUI stuck in a toolkit loop must not outlive Quit or block Restart.
        let child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let pid = child.id();
        let started = Instant::now();
        drop(UiChildren(vec![child]));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn shutdown_reaps_an_already_closed_gui_without_the_grace_delay() {
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        child.wait().unwrap();
        let started = Instant::now();
        drop(UiChildren(vec![child]));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}

#[cfg(all(test, windows))]
mod windows_lifecycle_tests {
    use super::*;
    use std::os::windows::{io::FromRawHandle, process::CommandExt};
    use windows::Win32::{
        Foundation::HANDLE,
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    #[test]
    fn shutdown_terminates_an_unresponsive_windows_gui() {
        let child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 60",
            ])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .spawn()
            .unwrap();
        // Keep a separate query handle after UiChildren reaps its Child handle.
        let process =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, child.id()) }.unwrap();
        let _handle = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(process.0) };
        drop(UiChildren(vec![child]));
        let mut status = 259; // STILL_ACTIVE
        unsafe { GetExitCodeProcess(HANDLE(process.0), &mut status) }.unwrap();
        assert_ne!(status, 259, "Settings must not outlive the Agent");
    }
}

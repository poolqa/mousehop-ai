//! SCM supervisor -> persistent SYSTEM session network daemon -> input helper.
//! Desktop transitions replace only the helper; authenticated network sessions
//! survive. No user token or user-writable executable is ever elevated.
use mousehop_ipc::windows::{Handle, SecurityDescriptor, require_system, wide};
use std::{
    ffi::c_void,
    io,
    mem::size_of,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE, LUID, WAIT_OBJECT_0},
        Security::{
            AdjustTokenPrivileges, DuplicateTokenEx, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
            SE_PRIVILEGE_ENABLED, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS,
            TOKEN_PRIVILEGES, TokenPrimary, TokenSessionId,
        },
        System::{
            Com::CoTaskMemFree,
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId},
            Services::*,
            StationsAndDesktops::{
                CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
                GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
            },
            Threading::{
                CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateEventW, CreateProcessAsUserW,
                GetCurrentProcess, GetCurrentProcessId, GetExitCodeProcess, OpenEventW,
                OpenProcessToken, PROCESS_INFORMATION, ResetEvent, ResumeThread, STARTUPINFOW,
                SYNCHRONIZATION_ACCESS_RIGHTS, SetEvent, TerminateProcess, WaitForSingleObject,
            },
        },
        UI::Shell::{FOLDERID_ProgramData, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
    },
    core::{PCWSTR, PWSTR, w},
};

static STOP: AtomicBool = AtomicBool::new(false);
static STATUS: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
const TICK: Duration = Duration::from_millis(250);

pub fn worker_mode() -> bool {
    std::env::args().any(|arg| arg == "windows-worker")
}

pub fn data_dir() -> io::Result<PathBuf> {
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DEFAULT, None)?;
        let value = path.to_string().map_err(io::Error::other);
        CoTaskMemFree(Some(path.0.cast()));
        Ok(PathBuf::from(value?).join("Mousehop"))
    }
}

struct ServiceHandle(SC_HANDLE);
impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

/// Recover a stopped service when Settings is opened after Task Manager kills.
/// This runs before GTK/Tokio startup, never on their event loops.
pub fn ensure_running() -> io::Result<()> {
    unsafe {
        let manager = ServiceHandle(OpenSCManagerW(None, None, SC_MANAGER_CONNECT)?);
        let service = ServiceHandle(OpenServiceW(
            manager.0,
            w!("Mousehop"),
            SERVICE_START | SERVICE_QUERY_STATUS,
        )?);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut requested_start = false;
        loop {
            let mut status = SERVICE_STATUS::default();
            QueryServiceStatus(service.0, &mut status)?;
            if status.dwCurrentState == SERVICE_RUNNING {
                return Ok(());
            }
            if status.dwCurrentState == SERVICE_STOPPED && !requested_start {
                if let Err(e) = StartServiceW(service.0, None) {
                    if e.code()
                        != windows::Win32::Foundation::ERROR_SERVICE_ALREADY_RUNNING.to_hresult()
                    {
                        return Err(e.into());
                    }
                }
                requested_start = true;
            } else if status.dwCurrentState == SERVICE_STOPPED && requested_start {
                return Err(io::Error::other(format!(
                    "Mousehop service stopped during startup (Windows error {}). See C:\\ProgramData\\Mousehop\\service.log.",
                    status.dwWin32ExitCode
                )));
            }
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Mousehop service did not start within 30 seconds. See C:\\ProgramData\\Mousehop\\service.log.",
                ));
            }
            std::thread::sleep(TICK);
        }
    }
}

pub fn show_startup_error(message: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(wide(message).as_ptr()),
            w!("Mousehop"),
            MB_OK | MB_ICONERROR,
        );
    }
}

pub fn dispatch() -> io::Result<()> {
    require_system()?;
    let mut name = wide("Mousehop");
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe {
        StartServiceCtrlDispatcherW(table.as_ptr())?;
    }
    Ok(())
}

fn report(state: SERVICE_STATUS_CURRENT_STATE, error: u32) {
    let raw = STATUS.load(Ordering::SeqCst);
    if raw.is_null() {
        return;
    }
    let pending = state == SERVICE_STOP_PENDING || state == SERVICE_START_PENDING;
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: error,
        dwCheckPoint: u32::from(pending),
        dwWaitHint: if pending { 15000 } else { 0 },
        ..Default::default()
    };
    unsafe {
        if let Err(e) = SetServiceStatus(SERVICE_STATUS_HANDLE(raw), &status) {
            log::error!("Service status: {e}");
        }
    }
}

unsafe extern "system" fn control(code: u32) {
    match code {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOP.store(true, Ordering::SeqCst);
            report(SERVICE_STOP_PENDING, 0);
        }
        SERVICE_CONTROL_INTERROGATE => report(
            if STOP.load(Ordering::SeqCst) {
                SERVICE_STOP_PENDING
            } else {
                SERVICE_RUNNING
            },
            0,
        ),
        _ => {}
    }
}

unsafe extern "system" fn service_main(_: u32, _: *mut PWSTR) {
    let status = match unsafe { RegisterServiceCtrlHandlerW(w!("Mousehop"), Some(control)) } {
        Ok(status) => status,
        Err(e) => {
            log::error!("Register service handler: {e}");
            return;
        }
    };
    STATUS.store(status.0, Ordering::SeqCst);
    report(SERVICE_START_PENDING, 0);
    let result = supervise();
    if let Err(ref e) = result {
        log::error!("Windows service: {e}");
    }
    report(SERVICE_STOPPED, if result.is_ok() { 0 } else { 1 });
    STATUS.store(std::ptr::null_mut(), Ordering::SeqCst);
}

fn token() -> io::Result<Handle> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut token)?;
    }
    Ok(Handle(token))
}

fn enable_privilege(token: &Handle, name: &str) -> io::Result<()> {
    unsafe {
        let mut luid = LUID::default();
        LookupPrivilegeValueW(None, PCWSTR(wide(name).as_ptr()), &mut luid)?;
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        AdjustTokenPrivileges(token.0, false, Some(&privileges), 0, None, None)?;
        let error = GetLastError();
        if error.0 != 0 {
            return Err(io::Error::from_raw_os_error(error.0 as i32));
        }
    }
    Ok(())
}

fn session_token(session: u32) -> io::Result<Handle> {
    let original = token()?;
    let mut duplicate = HANDLE::default();
    unsafe {
        DuplicateTokenEx(
            original.0,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut duplicate,
        )?;
        let duplicate = Handle(duplicate);
        SetTokenInformation(
            duplicate.0,
            TokenSessionId,
            (&session as *const u32).cast(),
            size_of::<u32>() as u32,
        )?;
        Ok(duplicate)
    }
}

fn session_id() -> io::Result<u32> {
    let mut session = 0;
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &mut session)?;
    }
    Ok(session)
}

fn event_name(kind: &str, session: u32) -> String {
    format!(r"Global\Mousehop.{kind}.{session}")
}

fn new_event(name: &str) -> io::Result<Handle> {
    let descriptor = SecurityDescriptor::new("D:P(A;;GA;;;SY)(A;;GA;;;BA)")?;
    unsafe {
        let event = Handle(CreateEventW(
            Some(&descriptor.attributes()),
            true,
            false,
            PCWSTR(wide(name).as_ptr()),
        )?);
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Mousehop stop event already exists; refusing an untrusted or duplicate instance",
            ));
        }
        Ok(event)
    }
}

fn open_event(name: &str) -> io::Result<Handle> {
    unsafe {
        Ok(Handle(OpenEventW(
            SYNCHRONIZATION_ACCESS_RIGHTS(0x00100000),
            false,
            PCWSTR(wide(name).as_ptr()),
        )?))
    }
}

fn signalled(handle: &Handle) -> bool {
    unsafe { WaitForSingleObject(handle.0, 0) == WAIT_OBJECT_0 }
}

struct Child {
    process: Handle,
    // Closing the job also kills descendants if the broker crashes.
    _job: Handle,
}
impl Child {
    fn stopped(&self) -> bool {
        signalled(&self.process)
    }
    fn stop(&self, event: &Handle, timeout_ms: u32) {
        unsafe {
            let _ = SetEvent(event.0);
            if WaitForSingleObject(self.process.0, timeout_ms) != WAIT_OBJECT_0 {
                log::warn!("Worker did not finish graceful shutdown; terminating its job");
                let _ = TerminateProcess(self.process.0, 1);
                let _ = WaitForSingleObject(self.process.0, 1000);
            }
        }
    }
}

/// Only constant role arguments and administrator-owned paths reach this helper.
fn spawn(token: &Handle, desktop: &str, arguments: &str) -> io::Result<Child> {
    let exe = std::env::current_exe()?;
    let exe = exe
        .to_str()
        .ok_or_else(|| io::Error::other("Non-Unicode executable path"))?;
    let application = wide(exe);
    let mut command = wide(&format!("\"{exe}\" {arguments}"));
    let mut desktop = wide(&format!("winsta0\\{desktop}"));
    let directory = wide(
        std::path::Path::new(exe)
            .parent()
            .unwrap()
            .to_str()
            .unwrap(),
    );
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    unsafe {
        let job = Handle(CreateJobObjectW(None, None)?);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of_val(&limits) as u32,
        )?;
        let mut process = PROCESS_INFORMATION::default();
        CreateProcessAsUserW(
            Some(token.0),
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_SUSPENDED | CREATE_NO_WINDOW,
            None,
            PCWSTR(directory.as_ptr()),
            &startup,
            &mut process,
        )?;
        let child = Handle(process.hProcess);
        let thread = Handle(process.hThread);
        if let Err(e) = AssignProcessToJobObject(job.0, child.0) {
            let _ = TerminateProcess(child.0, 1);
            return Err(e.into());
        }
        if ResumeThread(thread.0) == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok(Child {
            process: child,
            _job: job,
        })
    }
}

fn supervise() -> io::Result<()> {
    let own_token = token()?;
    for privilege in [
        "SeTcbPrivilege",
        "SeAssignPrimaryTokenPrivilege",
        "SeIncreaseQuotaPrivilege",
    ] {
        enable_privilege(&own_token, privilege)?;
    }
    let mut running: Option<(u32, Handle, Child)> = None;
    report(SERVICE_RUNNING, 0);
    while !STOP.load(Ordering::SeqCst) {
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if running
            .as_ref()
            .is_some_and(|(id, _, child)| *id != session || child.stopped())
        {
            if let Some((_, event, child)) = running.take() {
                child.stop(&event, 10000);
            }
        }
        if running.is_none() && session != u32::MAX {
            let result = (|| {
                let event = new_event(&event_name("SessionStop", session))?;
                let token = session_token(session)?;
                let child = spawn(&token, "default", "windows-session")?;
                Ok::<_, io::Error>((session, event, child))
            })();
            match result {
                Ok(child) => {
                    log::info!("Started console-session broker for session {session}");
                    running = Some(child);
                }
                Err(e) => {
                    log::error!("Starting console-session broker: {e}");
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
        std::thread::sleep(TICK);
    }
    if let Some((_, event, child)) = running.take() {
        child.stop(&event, 10000);
    }
    Ok(())
}

fn input_desktop() -> io::Result<String> {
    unsafe {
        let desktop = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS)?;
        let mut buffer = [0u16; 256];
        let result = GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(buffer.as_mut_ptr().cast()),
            size_of_val(&buffer) as u32,
            None,
        );
        let _ = CloseDesktop(desktop);
        result?;
        let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        Ok(String::from_utf16_lossy(&buffer[..len]))
    }
}

fn desktop_supervisor(shutdown: std::sync::Arc<AtomicBool>) -> io::Result<()> {
    require_system()?;
    let session = session_id()?;
    if session == 0 {
        return Err(io::Error::other(
            "Desktop broker must run in a console session",
        ));
    }
    let stop = open_event(&event_name("SessionStop", session))?;
    let worker_stop = new_event(&event_name("WorkerStop", session))?;
    let token = token()?;
    let state = data_dir()?;
    let arguments = format!(
        "--config \"{}\" --cert-path \"{}\" windows-worker",
        state.join("config.toml").display(),
        state.join("identity.pem").display()
    );
    let mut running: Option<(String, Child)> = None;
    while !signalled(&stop) && !shutdown.load(Ordering::SeqCst) {
        let desktop = input_desktop().ok();
        // Fail closed while Windows has no accessible input desktop.
        if running
            .as_ref()
            .is_some_and(|(name, child)| Some(name) != desktop.as_ref() || child.stopped())
        {
            if let Some((_, child)) = running.take() {
                if let Some(link) = mousehop_ipc::desktop::current() {
                    link.suspend();
                }
                if child.stopped() {
                    let mut code = 0;
                    unsafe {
                        let _ = GetExitCodeProcess(child.process.0, &mut code);
                    }
                    log::warn!("Input worker exited with code {code:#x}; restarting");
                } else {
                    log::info!("Input desktop changed to {desktop:?}; replacing input worker");
                }
                child.stop(&worker_stop, 4000);
            }
        }
        if let (None, Some(desktop)) = (&running, desktop) {
            // The requested support is limited to Windows' standard interactive
            // and secure desktops. Do not enter arbitrary third-party desktops.
            if desktop.eq_ignore_ascii_case("default") || desktop.eq_ignore_ascii_case("winlogon") {
                unsafe {
                    ResetEvent(worker_stop.0)?;
                }
                match spawn(&token, &desktop, &arguments) {
                    Ok(child) => {
                        log::info!(
                            "Started desktop input helper on {desktop}; network daemon retained"
                        );
                        running = Some((desktop, child));
                    }
                    Err(e) => {
                        log::error!("Starting desktop worker: {e}");
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        }
        std::thread::sleep(TICK);
    }
    if let Some((_, child)) = running {
        child.stop(&worker_stop, 4000);
    }
    Ok(())
}

pub async fn worker_shutdown() -> io::Result<()> {
    let stop = open_event(&event_name("WorkerStop", session_id()?))?;
    while !signalled(&stop) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// Relaunch only the settings UI with consent; the service owns the daemon.
pub fn elevate_settings() -> io::Result<bool> {
    use windows::Win32::{
        Security::{GetTokenInformation, TOKEN_ELEVATION, TokenElevation},
        UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    };
    let token = {
        let mut handle = HANDLE::default();
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                windows::Win32::Security::TOKEN_QUERY,
                &mut handle,
            )?;
        }
        Handle(handle)
    };
    let mut elevation = TOKEN_ELEVATION::default();
    let mut length = 0;
    unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            size_of_val(&elevation) as u32,
            &mut length,
        )?;
        if elevation.TokenIsElevated != 0 {
            return Ok(false);
        }
        let exe = std::env::current_exe()?;
        let exe = wide(
            exe.to_str()
                .ok_or_else(|| io::Error::other("Non-Unicode executable path"))?,
        );
        let result = ShellExecuteW(
            None,
            w!("runas"),
            PCWSTR(exe.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
        if result.0 as usize <= 32 {
            return Err(io::Error::other(
                "Administrator consent is required to configure the Mousehop service",
            ));
        }
    }
    Ok(true)
}

/// Each worker starts on one desktop and never switches in place. Clipboard
/// content must not cross into or out of the login/UAC desktop.
pub fn secure_desktop() -> bool {
    if let Some(link) = mousehop_ipc::desktop::current() {
        return link.secure();
    }
    use std::sync::OnceLock;
    use windows::Win32::System::{
        StationsAndDesktops::GetThreadDesktop, Threading::GetCurrentThreadId,
    };
    static SECURE: OnceLock<bool> = OnceLock::new();
    if !worker_mode() {
        return false;
    }
    *SECURE.get_or_init(|| unsafe {
        let Ok(desktop) = GetThreadDesktop(GetCurrentThreadId()) else {
            return true;
        };
        let mut buffer = [0u16; 256];
        if GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(buffer.as_mut_ptr().cast()),
            size_of_val(&buffer) as u32,
            None,
        )
        .is_err()
        {
            return true;
        }
        let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        !String::from_utf16_lossy(&buffer[..len]).eq_ignore_ascii_case("default")
    })
}

/// Owns the network for the entire console session, independently of desktops.
pub async fn run_session(config: crate::config::Config) -> io::Result<()> {
    require_system()?;
    let session = session_id()?;
    let link = mousehop_ipc::desktop::initialize();
    let mut pipe_task =
        tokio::task::spawn_local(mousehop_ipc::desktop::serve(session, link.clone()));
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let flag = shutdown.clone();
    let manager = std::thread::spawn(move || desktop_supervisor(flag));
    // Geometry must be available before the first inbound handover.
    let wait_ready = async {
        while !link.ready() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(10), wait_ready).await;
    log::info!("Starting persistent console-session network daemon");
    let result = async {
        let mut service = crate::service::Service::new(config)
            .await
            .map_err(io::Error::other)?;
        tokio::select! {
            result = service.run() => result.map_err(io::Error::other),
            result = &mut pipe_task => Err(io::Error::other(format!("desktop pipe server stopped: {result:?}"))),
        }
    }
    .await;
    shutdown.store(true, Ordering::SeqCst);
    let manager_result = tokio::task::spawn_blocking(move || manager.join())
        .await
        .map_err(io::Error::other)?
        .map_err(|_| io::Error::other("desktop supervisor panicked"))?;
    pipe_task.abort();
    result.and(manager_result)
}

pub async fn session_shutdown() -> io::Result<()> {
    let stop = open_event(&event_name("SessionStop", session_id()?))?;
    while !signalled(&stop) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// Desktop-bound OS input only. No DTLS listener, peer dialer or frontend IPC.
pub async fn desktop_worker(mut config: crate::config::Config) -> io::Result<()> {
    use futures::StreamExt;
    use input_capture::{CaptureEvent, HostInputState, InputCapture, clipboard::ClipboardMonitor};
    use input_emulation::InputEmulation;
    use input_event::Event;
    use mousehop_ipc::desktop::{self, Capture, Input, Request, Response};
    use tokio::{net::windows::named_pipe::ClientOptions, sync::mpsc};
    require_system()?;
    let session = session_id()?;
    let mut pipe = loop {
        match ClientOptions::new().open(desktop::pipe_name(session)) {
            Ok(pipe) => break pipe,
            Err(error) if matches!(error.raw_os_error(), Some(2 | 231)) => {
                tokio::time::sleep(TICK).await
            }
            Err(error) => return Err(error),
        }
    };
    mousehop_ipc::windows::verify_server(&pipe)?;
    let secure = secure_desktop();
    let mut capture = InputCapture::new(Some(input_capture::Backend::Windows))
        .await
        .map_err(io::Error::other)?;
    let mut emulation = InputEmulation::new(Some(input_emulation::Backend::Windows))
        .await
        .map_err(io::Error::other)?;
    let suppression: input_capture::clipboard::SuppressionList = Default::default();
    let update_suppression = |config: &crate::config::Config| {
        let host = mousehop_ipc::HostKind::current();
        *suppression.lock().unwrap() = config
            .clipboard_suppression()
            .host()
            .iter()
            .map(|s| host.make_ident(s.clone()))
            .collect();
    };
    update_suppression(&config);
    let mut monitor = if secure {
        None
    } else {
        ClipboardMonitor::with_suppression(suppression.clone()).ok()
    };
    desktop::write_frame(
        &mut pipe,
        &Response::Ready {
            secure,
            bounds: emulation.display_bounds(),
        },
    )
    .await?;
    let (mut reader, mut writer) = tokio::io::split(pipe);
    let (tx, mut outgoing) = mpsc::channel(1024);
    let (incoming_tx, mut incoming) = mpsc::channel(1024);
    let read_task = tokio::task::spawn_local(async move {
        loop {
            let request = desktop::read_frame::<_, Request>(&mut reader).await?;
            if incoming_tx.send(request).await.is_err() {
                return Ok::<(), io::Error>(());
            }
        }
    });
    let write_task = tokio::task::spawn_local(async move {
        while let Some(response) = outgoing.recv().await {
            desktop::write_frame(&mut writer, &response).await?;
        }
        Ok::<(), io::Error>(())
    });
    let mut geometry = tokio::time::interval(Duration::from_secs(2));
    let result = async {
        loop {
            tokio::select! {
                request = incoming.recv() => {
                    let Some(request) = request else { break; };
                    match request {
                        Request::CaptureCreate(edge) => if let Some(pos) = helper_position(edge) { capture.create(u64::from(edge),pos).await.map_err(io::Error::other)?; },
                        Request::CaptureDestroy(edge) => { capture.destroy(u64::from(edge)).await.map_err(io::Error::other)?; },
                        Request::CrossingModifier(edge,modifier) => if let Some(pos) = helper_position(edge) { capture.set_crossing_modifier(pos,modifier).await.map_err(io::Error::other)?; },
                        Request::Release(id) => {
                            let ok = capture.release_no_host_warp().await.is_ok();
                            tx.send(Response::Complete {id,ok}).await.map_err(io::Error::other)?;
                        },
                        Request::Input {handle,input} => {
                            emulation.create(handle).await;
                            emulation.consume(input.into_event(),handle).await.map_err(io::Error::other)?;
                        },
                        Request::Destroy(handle) => emulation.destroy(handle).await,
                        Request::ReleaseKeys(keys) => emulation.release_previous_desktop_keys(&keys).await,
                        Request::Warp {id,x,y} => {
                            let ok = emulation.warp_cursor(x,y).await.is_ok();
                            tx.send(Response::Complete {id,ok}).await.map_err(io::Error::other)?;
                        },
                        Request::Clipboard(clipboard) => if !secure {
                            let event = clipboard.into_event();
                            if let Some(monitor) = &monitor { monitor.update_last_event(&event); }
                            emulation.consume(Event::Clipboard(event),0).await.map_err(io::Error::other)?;
                        },
                    }
                },
                event = capture.next() => {
                    let Some(event) = event else { return Err(io::Error::other("desktop capture ended")); };
                    let (handle,event) = event.map_err(io::Error::other)?;
                    let event = match event {
                        CaptureEvent::Begin {..} => Capture::Begin,
                        CaptureEvent::Input(event) => { let Some(input) = Input::from_event(&event) else { continue; }; Capture::Input(input) },
                        CaptureEvent::AutoRelease => Capture::AutoRelease,
                        CaptureEvent::HostInputState(state) => Capture::Locked(state == HostInputState::Locked),
                    };
                    tx.send(Response::Capture {edge:handle as u8,event}).await.map_err(io::Error::other)?;
                },
                event = helper_clipboard(&mut monitor) => {
                    tx.send(Response::Clipboard(desktop::Clipboard::from_event(&event))).await.map_err(io::Error::other)?;
                },
                _ = geometry.tick() => { tx.send(Response::Geometry(emulation.display_bounds())).await.map_err(io::Error::other)?; },
                _ = config.changed() => {
                    update_suppression(&config);
                },
                _ = worker_shutdown() => break,
            }
        }
        Ok::<(),io::Error>(())
    }.await;
    // Disconnect both pipe directions before OS teardown. A stuck hook can only
    // delay this disposable helper, never the network daemon's heartbeat loop.
    read_task.abort();
    write_task.abort();
    emulation.terminate().await;
    let _ = capture.terminate().await;
    log::info!("Desktop input helper exited normally (secure={secure})");
    result
}

fn helper_position(edge: u8) -> Option<input_capture::Position> {
    use input_capture::Position;
    match edge {
        0 => Some(Position::Left),
        1 => Some(Position::Right),
        2 => Some(Position::Top),
        3 => Some(Position::Bottom),
        _ => None,
    }
}
async fn helper_clipboard(
    monitor: &mut Option<input_capture::clipboard::ClipboardMonitor>,
) -> input_event::ClipboardEvent {
    if let Some(monitor) = monitor {
        loop {
            if let Some(input_capture::CaptureEvent::Input(input_event::Event::Clipboard(event))) =
                monitor.recv().await
            {
                return event;
            }
        }
    }
    std::future::pending().await
}

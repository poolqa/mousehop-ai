//! Read Explorer's file formats in a process with the console user's primary
//! token so Explorer's OLE file formats are read in the user's process.
use crate::clipboard::windows_snapshot::{FileSnapshot, MAX_FILES, MAX_SNAPSHOT_BYTES};
use mousehop_ipc::windows::{Handle, process_sid, wide};
use std::{
    io::{self, Write},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{
            ERROR_BROKEN_PIPE, GENERIC_READ, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT,
            HANDLE_FLAGS, SetHandleInformation, WAIT_OBJECT_0,
        },
        Security::{
            GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_ELEVATION_TYPE, TOKEN_LINKED_TOKEN,
            TokenElevationType, TokenElevationTypeFull, TokenLinkedToken,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            ReadFile,
        },
        System::{
            Com::{DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL},
            DataExchange::{CountClipboardFormats, GetClipboardSequenceNumber},
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            Ole::{OleGetClipboard, OleInitialize, OleUninitialize, ReleaseStgMedium},
            Pipes::{CreatePipe, PeekNamedPipe},
            RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken},
            Threading::{
                CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessAsUserW,
                DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
                GetCurrentProcessId, GetExitCodeProcess, InitializeProcThreadAttributeList,
                LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
                TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
            },
        },
        UI::Shell::{DragQueryFileW, HDROP},
    },
    core::{PCWSTR, PWSTR, w},
};

static LAST_SNAPSHOT: Mutex<Option<FileSnapshot>> = Mutex::new(None);

pub(crate) fn read_as_desktop_user() -> io::Result<Vec<PathBuf>> {
    if !mousehop_ipc::windows::service_mode()
        || process_sid(unsafe { GetCurrentProcess() })? != "S-1-5-18"
    {
        return Ok(Vec::new());
    }
    let sequence = unsafe { GetClipboardSequenceNumber() };
    if let Some(snapshot) = LAST_SNAPSHOT.lock().unwrap().as_ref() {
        if snapshot.sequence == sequence {
            return Ok(snapshot.files.clone());
        }
    }
    let snapshot = launch_reader()?;
    if snapshot.sequence != sequence || unsafe { GetClipboardSequenceNumber() } != sequence {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "clipboard changed during user read; retrying",
        ));
    }
    if let Some(error) = &snapshot.error {
        return Err(io::Error::other(error.clone()));
    }
    log::debug!(
        "Desktop user clipboard snapshot: sequence={sequence}, files={}",
        snapshot.files.len()
    );
    let files = snapshot.files.clone();
    *LAST_SNAPSHOT.lock().unwrap() = Some(snapshot);
    Ok(files)
}

pub(crate) fn write_snapshot() -> io::Result<()> {
    if process_sid(unsafe { GetCurrentProcess() })? == "S-1-5-18" {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "clipboard reader requires a desktop user primary token",
        ));
    }
    let before = unsafe { GetClipboardSequenceNumber() };
    let result = ole_file_list();
    let sequence = unsafe { GetClipboardSequenceNumber() };
    let (files, error) = if before != sequence {
        (
            Vec::new(),
            Some("clipboard changed while reading files; retrying".into()),
        )
    } else {
        match result {
            Ok(files) => (files, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        }
    };
    let bytes = FileSnapshot {
        sequence,
        files,
        error,
    }
    .encode()?;
    io::stdout().lock().write_all(&bytes)
}

struct OleApartment;
impl Drop for OleApartment {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}
struct Medium(STGMEDIUM);
impl Drop for Medium {
    fn drop(&mut self) {
        unsafe { ReleaseStgMedium(&mut self.0) };
    }
}

fn ole_file_list() -> io::Result<Vec<PathBuf>> {
    unsafe {
        if CountClipboardFormats() == 0 {
            return Ok(Vec::new());
        }
        OleInitialize(None)?;
        let _apartment = OleApartment;
        let object = OleGetClipboard()?;
        let format = FORMATETC {
            cfFormat: 15,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
            ..Default::default()
        };
        if object.QueryGetData(&format).is_err() {
            return Ok(Vec::new());
        }
        let medium = Medium(object.GetData(&format)?);
        if medium.0.tymed != TYMED_HGLOBAL.0 as u32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "FileDrop did not return global memory",
            ));
        }
        let drop = HDROP(medium.0.u.hGlobal.0);
        let count = DragQueryFileW(drop, u32::MAX, None);
        if count as usize > MAX_FILES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "FileDrop exceeds file count limit",
            ));
        }
        let mut files = Vec::with_capacity(count as usize);
        let mut total_units = 0usize;
        for index in 0..count {
            let length = DragQueryFileW(drop, index, None) as usize;
            total_units += length;
            if length == 0 || length > 32768 || total_units > MAX_SNAPSHOT_BYTES / 2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "FileDrop path exceeds limits",
                ));
            }
            let mut path = vec![0u16; length + 1];
            if DragQueryFileW(drop, index, Some(&mut path)) as usize != length {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "FileDrop path was truncated",
                ));
            }
            files.push(std::ffi::OsString::from_wide(&path[..length]).into());
        }
        Ok(files)
    }
}

struct Attributes {
    _storage: Vec<usize>,
    list: LPPROC_THREAD_ATTRIBUTE_LIST,
}
impl Attributes {
    fn new(handles: &[HANDLE]) -> io::Result<Self> {
        unsafe {
            let mut size = 0;
            let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
            if size == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
            let list = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast());
            InitializeProcThreadAttributeList(Some(list), 1, None, &mut size)?;
            let attributes = Self {
                _storage: storage,
                list,
            };
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(handles.as_ptr().cast()),
                size_of_val(handles),
                None,
                None,
            )?;
            Ok(attributes)
        }
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.list) };
    }
}

fn user_token(session: u32) -> io::Result<Handle> {
    unsafe {
        let mut raw = HANDLE::default();
        WTSQueryUserToken(session, &mut raw)?;
        let token = Handle(raw);
        let mut elevation = TOKEN_ELEVATION_TYPE::default();
        let mut size = 0;
        GetTokenInformation(
            token.0,
            TokenElevationType,
            Some((&mut elevation as *mut TOKEN_ELEVATION_TYPE).cast()),
            size_of_val(&elevation) as u32,
            &mut size,
        )?;
        if elevation == TokenElevationTypeFull {
            let mut linked = TOKEN_LINKED_TOKEN::default();
            GetTokenInformation(
                token.0,
                TokenLinkedToken,
                Some((&mut linked as *mut TOKEN_LINKED_TOKEN).cast()),
                size_of_val(&linked) as u32,
                &mut size,
            )?;
            Ok(Handle(linked.LinkedToken))
        } else {
            Ok(token)
        }
    }
}

fn launch_reader() -> io::Result<FileSnapshot> {
    unsafe {
        let mut session = 0;
        ProcessIdToSessionId(GetCurrentProcessId(), &mut session)?;
        let token = user_token(session)?;
        let inheritable = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            bInheritHandle: true.into(),
            ..Default::default()
        };
        let mut read = HANDLE::default();
        let mut write = HANDLE::default();
        CreatePipe(&mut read, &mut write, Some(&inheritable), 0)?;
        let read = Handle(read);
        let write = Handle(write);
        SetHandleInformation(read.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;
        let null = Handle(CreateFileW(
            w!("NUL"),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            Some(&inheritable),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )?);
        // Only stdout and NUL are inherited; never pass service/token/pipe handles.
        let inherited = [write.0, null.0];
        let attributes = Attributes::new(&inherited)?;
        let mut desktop = wide(r"winsta0\default");
        let startup = STARTUPINFOEXW {
            StartupInfo: windows::Win32::System::Threading::STARTUPINFOW {
                cb: size_of::<STARTUPINFOEXW>() as u32,
                lpDesktop: PWSTR(desktop.as_mut_ptr()),
                dwFlags: STARTF_USESTDHANDLES,
                hStdOutput: write.0,
                hStdError: null.0,
                hStdInput: null.0,
                ..Default::default()
            },
            lpAttributeList: attributes.list,
        };
        let exe = std::env::current_exe()?;
        let application: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut command = wide(&format!("\"{}\" windows-clipboard-read", exe.display()));
        let directory: Vec<u16> = exe
            .parent()
            .ok_or_else(|| io::Error::other("clipboard reader executable has no directory"))?
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
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
            true,
            CREATE_NO_WINDOW | CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT,
            None,
            PCWSTR(directory.as_ptr()),
            &startup.StartupInfo,
            &mut process,
        )?;
        let process_handle = Handle(process.hProcess);
        let thread = Handle(process.hThread);
        if let Err(error) = AssignProcessToJobObject(job.0, process_handle.0) {
            let _ = TerminateProcess(process_handle.0, 1);
            return Err(error.into());
        }
        if ResumeThread(thread.0) == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        drop(write);
        let started = Instant::now();
        let mut bytes = Vec::new();
        loop {
            let mut available = 0;
            match PeekNamedPipe(read.0, None, 0, None, Some(&mut available), None) {
                Ok(()) => {}
                Err(error) if error.code() == ERROR_BROKEN_PIPE.to_hresult() => break,
                Err(error) => return Err(error.into()),
            }
            if available > 0 {
                let mut buffer = [0u8; 8192];
                let mut received = 0;
                let length = (available as usize).min(buffer.len());
                ReadFile(
                    read.0,
                    Some(&mut buffer[..length]),
                    Some(&mut received),
                    None,
                )?;
                if bytes.len() + received as usize > MAX_SNAPSHOT_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "user clipboard response exceeds byte limit",
                    ));
                }
                bytes.extend_from_slice(&buffer[..received as usize]);
            } else if WaitForSingleObject(process_handle.0, 0) == WAIT_OBJECT_0 {
                break;
            }
            if started.elapsed() > Duration::from_secs(5) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "desktop user clipboard reader timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Closing stdout can precede process exit by a few instructions.
        if WaitForSingleObject(process_handle.0, 1000) != WAIT_OBJECT_0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "desktop user clipboard reader did not exit",
            ));
        }
        let mut code = 0;
        GetExitCodeProcess(process_handle.0, &mut code)?;
        if code != 0 {
            return Err(io::Error::other(format!(
                "desktop user clipboard reader exit code {code}"
            )));
        }
        FileSnapshot::decode(&bytes)
    }
}

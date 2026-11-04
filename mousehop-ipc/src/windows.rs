//! Windows control transport with authenticated desktop clients and elevated
//! machine configuration; portable instances are isolated by the caller's SID.
use std::{io, mem::size_of, os::windows::io::AsRawHandle, sync::OnceLock};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
            },
            GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
            SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
    core::{PCWSTR, PWSTR},
};

/// Each read/write owns an OVERLAPPED event. Duplicating a synchronous pipe
/// handle serializes writes behind the reader's pending ReadFile, freezing GTK.
pub(crate) struct PipeStream(pub std::fs::File);

impl PipeStream {
    pub fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }

    fn transfer(&self, read: Option<&mut [u8]>, write: Option<&[u8]>) -> io::Result<usize> {
        use windows::Win32::{
            Foundation::{ERROR_BROKEN_PIPE, ERROR_IO_PENDING},
            Storage::FileSystem::{ReadFile, WriteFile},
            System::{
                IO::{GetOverlappedResult, OVERLAPPED},
                Threading::CreateEventW,
            },
        };
        unsafe {
            let event = Handle(CreateEventW(None, true, false, None)?);
            let mut overlapped = OVERLAPPED {
                hEvent: event.0,
                ..Default::default()
            };
            let handle = HANDLE(self.0.as_raw_handle());
            let is_read = read.is_some();
            let result = if is_read {
                ReadFile(handle, read, None, Some(&mut overlapped))
            } else {
                WriteFile(handle, write, None, Some(&mut overlapped))
            };
            if let Err(error) = result {
                if is_read && error.code() == ERROR_BROKEN_PIPE.to_hresult() {
                    return Ok(0);
                }
                if error.code() != ERROR_IO_PENDING.to_hresult() {
                    return Err(error.into());
                }
            }
            // Wait until the kernel no longer owns the stack OVERLAPPED/buffer.
            let mut transferred = 0;
            match GetOverlappedResult(handle, &overlapped, &mut transferred, true) {
                Ok(()) => Ok(transferred as usize),
                Err(e) if is_read && e.code() == ERROR_BROKEN_PIPE.to_hresult() => Ok(0),
                Err(e) => Err(e.into()),
            }
        }
    }
}

impl io::Read for PipeStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.transfer(Some(buffer), None)
    }
}

impl io::Write for PipeStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.transfer(None, Some(buffer))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct Handle(pub HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

pub fn service_mode() -> bool {
    static MODE: OnceLock<bool> = OnceLock::new();
    *MODE.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("service-mode")))
            .is_some_and(|p| p.is_file())
    })
}

pub fn process_sid(process: HANDLE) -> io::Result<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token)?;
        let token = Handle(token);
        token_sid(token.0)
    }
}

fn token_sid(token: HANDLE) -> io::Result<String> {
    unsafe {
        let mut needed = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        // usize storage keeps TOKEN_USER aligned.
        let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )?;
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut sid = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut sid)?;
        let value = sid.to_string().map_err(io::Error::other);
        LocalFree(Some(HLOCAL(sid.0.cast())));
        value
    }
}

pub fn require_system() -> io::Result<()> {
    if process_sid(unsafe { GetCurrentProcess() })? != "S-1-5-18" {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "This command is reserved for the LocalSystem Windows service",
        ));
    }
    Ok(())
}

pub fn pipe_name() -> io::Result<String> {
    if service_mode() {
        Ok(r"\\.\pipe\Mousehop.Service.v2".into())
    } else {
        Ok(format!(
            r"\\.\pipe\Mousehop.User.{}",
            process_sid(unsafe { GetCurrentProcess() })?
        ))
    }
}

pub struct SecurityDescriptor(PSECURITY_DESCRIPTOR);
impl SecurityDescriptor {
    pub fn new(sddl: &str) -> io::Result<Self> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide(sddl).as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )?;
        }
        Ok(Self(descriptor))
    }
    /// Internal desktop pipes must authenticate as SYSTEM regardless of the
    /// creating token's default owner (which can be Administrators).
    pub fn system_only() -> io::Result<Self> {
        Self::new("O:SYD:P(A;;GA;;;SY)")
    }

    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        }
    }
}
impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

pub fn server(first: bool) -> io::Result<NamedPipeServer> {
    let sddl = if service_mode() {
        require_system()?;
        // Interactive users may connect, but the accept path verifies the
        // active console session before exposing events or accepting requests.
        "O:SYD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00120083;;;IU)".to_owned()
    } else {
        format!(
            "O:{0}D:P(A;;GA;;;SY)(A;;GA;;;{0})",
            process_sid(unsafe { GetCurrentProcess() })?
        )
    };
    let descriptor = SecurityDescriptor::new(&sddl)?;
    let mut attributes = descriptor.attributes();
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                pipe_name()?,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    }
}

// Read/write data, read attributes, READ_CONTROL and SYNCHRONIZE. Excludes
// FILE_CREATE_PIPE_INSTANCE (also FILE_APPEND_DATA), unlike GENERIC_WRITE.
const CLIENT_ACCESS: u32 = 0x0012_0083;

pub(crate) fn open_client() -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let pipe = std::fs::OpenOptions::new()
        .access_mode(CLIENT_ACCESS)
        .custom_flags(0x4000_0000) // FILE_FLAG_OVERLAPPED
        .security_qos_flags(0x0001_0000) // SECURITY_IDENTIFICATION
        .open(pipe_name()?)?;
    verify_server(&pipe)?;
    Ok(pipe)
}

/// Verify the connected pipe's explicit owner without opening a SYSTEM token.
/// Interactive clients cannot create service pipe instances or alter its owner.
pub fn verify_server(pipe: &impl AsRawHandle) -> io::Result<()> {
    let expected = if service_mode() {
        "S-1-5-18".to_owned()
    } else {
        process_sid(unsafe { GetCurrentProcess() })?
    };
    verify_pipe_owner(pipe, &expected)
}

fn verify_pipe_owner(pipe: &impl AsRawHandle, expected: &str) -> io::Result<()> {
    unsafe {
        let mut owner = PSID::default();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let result = GetSecurityInfo(
            HANDLE(pipe.as_raw_handle()),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut descriptor),
        );
        if result.0 != 0 {
            return Err(io::Error::from_raw_os_error(result.0 as i32));
        }
        let result = (|| {
            let mut sid = PWSTR::null();
            ConvertSidToStringSidW(owner, &mut sid)?;
            let value = sid.to_string().map_err(io::Error::other);
            LocalFree(Some(HLOCAL(sid.0.cast())));
            let actual = value?;
            if actual != expected {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("Unexpected Mousehop pipe owner: expected {expected}, got {actual}"),
                ));
            }
            Ok(())
        })();
        LocalFree(Some(HLOCAL(descriptor.0)));
        result
    }
}

/// This is checked per connection; granting pipe access does not grant configuration rights.
pub fn client_is_admin(pipe: &impl AsRawHandle) -> io::Result<bool> {
    use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
    unsafe {
        let mut pid = 0;
        GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle()), &mut pid)?;
        let process = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?);
        let privileged = process_elevated(process.0)?;
        if !privileged {
            use windows::Win32::System::RemoteDesktop::{
                ProcessIdToSessionId, WTSGetActiveConsoleSessionId,
            };
            let mut session = 0;
            ProcessIdToSessionId(pid, &mut session)?;
            let mut token = HANDLE::default();
            windows::Win32::System::RemoteDesktop::WTSQueryUserToken(
                WTSGetActiveConsoleSessionId(),
                &mut token,
            )?;
            let token = Handle(token);
            if session != WTSGetActiveConsoleSessionId()
                || process_sid(process.0)? != token_sid(token.0)?
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Only the active console session may attach",
                ));
            }
        }
        Ok(privileged)
    }
}
fn process_elevated(process: HANDLE) -> io::Result<bool> {
    use windows::Win32::Security::{TOKEN_ELEVATION, TokenElevation};
    use windows::Win32::System::Threading::OpenProcessToken;
    unsafe {
        let mut raw = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut raw)?;
        let token = Handle(raw);
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0;
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )?;
        Ok(elevation.TokenIsElevated != 0)
    }
}
pub fn is_elevated() -> io::Result<bool> {
    process_elevated(unsafe { GetCurrentProcess() })
}

/// Start a short-lived elevated runtime for one validated machine mutation.
/// Arguments contain hex-encoded JSON, never a shell command or a user-supplied path.
pub fn route_admin_request(request: &crate::FrontendRequest) -> io::Result<bool> {
    if !service_mode() || !request.requires_admin() || is_elevated()? {
        return Ok(false);
    }
    use windows::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_HIDE};
    let json = serde_json::to_vec(request).map_err(io::Error::other)?;
    let encoded: String = json.iter().map(|byte| format!("{byte:02x}")).collect();
    let executable = std::env::current_exe()?.with_file_name("mousehop.exe");
    let executable = wide(
        executable
            .to_str()
            .ok_or_else(|| io::Error::other("Non-Unicode executable path"))?,
    );
    let arguments = wide(&format!("admin-request {encoded}"));
    let result = unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("runas"),
            PCWSTR(executable.as_ptr()),
            PCWSTR(arguments.as_ptr()),
            None,
            SW_HIDE,
        )
    };
    if result.0 as usize <= 32 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Administrator consent is required for this machine setting",
        ));
    }
    Ok(true)
}
pub fn execute_admin_request() -> io::Result<()> {
    if !service_mode() || !is_elevated()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Elevated installed runtime required",
        ));
    }
    let value = std::env::args()
        .nth(2)
        .ok_or_else(|| io::Error::other("Missing administrator request"))?;
    if value.len() > 65536
        || value.len() % 2 != 0
        || !value.is_ascii()
        || std::env::args().len() != 3
    {
        return Err(io::Error::other("Invalid administrator request"));
    }
    let json: Result<Vec<_>, _> = (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16))
        .collect();
    let request: crate::FrontendRequest =
        serde_json::from_slice(&json.map_err(io::Error::other)?).map_err(io::Error::other)?;
    if !request.requires_admin() {
        return Err(io::Error::other("Not an administrator operation"));
    }
    let (_, mut writer) = crate::connect().map_err(io::Error::other)?;
    writer.request(request)
}

#[cfg(test)]
mod security_tests {
    use super::*;
    #[test]
    fn internal_pipe_has_an_explicit_system_owner() {
        use windows::Win32::Security::GetSecurityDescriptorOwner;
        let descriptor = SecurityDescriptor::system_only().unwrap();
        unsafe {
            let mut owner = PSID::default();
            let mut defaulted = false.into();
            GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut defaulted).unwrap();
            assert!(
                !owner.0.is_null(),
                "Do not inherit the creator's default owner"
            );
            let mut sid = PWSTR::null();
            ConvertSidToStringSidW(owner, &mut sid).unwrap();
            let value = sid.to_string().unwrap();
            LocalFree(Some(HLOCAL(sid.0.cast())));
            assert_eq!(value, "S-1-5-18");
        }
    }

    #[tokio::test]
    #[ignore = "requires a Windows LocalSystem test process"]
    async fn desktop_worker_authenticates_the_actual_internal_pipe() {
        use std::time::Duration;
        use tokio::net::windows::named_pipe::ClientOptions;
        require_system().unwrap();
        // Reserve a test-only pipe name, separate from real console sessions.
        let session = u32::MAX;
        let server = crate::desktop::server(session, true).unwrap();
        let client = ClientOptions::new()
            .open(crate::desktop::pipe_name(session))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), server.connect())
            .await
            .unwrap()
            .unwrap();
        verify_server(&client).unwrap();
        verify_pipe_owner(&client, "S-1-5-18").unwrap();
        assert_eq!(
            verify_pipe_owner(&client, "S-1-5-19").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}

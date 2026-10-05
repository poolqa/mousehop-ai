//! Windows control transport. Installed services accept only elevated administrators
//! and SYSTEM; portable instances are isolated by the caller's SID.
use std::{io, mem::size_of, os::windows::io::AsRawHandle, sync::OnceLock};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        System::{
            Pipes::GetNamedPipeServerProcessId,
            Threading::{
                GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            },
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
        let mut needed = 0;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut needed);
        // usize storage keeps TOKEN_USER aligned.
        let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        GetTokenInformation(
            token.0,
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
        Ok(r"\\.\pipe\Mousehop.Service.v1".into())
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
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)".to_owned()
    } else {
        format!(
            "D:P(A;;GA;;;SY)(A;;GA;;;{})",
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

/// Reject a pipe squatted by another account before sending configuration data.
pub fn verify_server(pipe: &impl AsRawHandle) -> io::Result<()> {
    unsafe {
        let mut pid = 0;
        GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid)?;
        let process = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?);
        let expected = if service_mode() {
            "S-1-5-18".to_owned()
        } else {
            process_sid(GetCurrentProcess())?
        };
        if process_sid(process.0)? != expected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Unexpected Mousehop pipe server identity",
            ));
        }
        Ok(())
    }
}

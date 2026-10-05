use mousehop_installer::{same_executable, table_port};
use std::{
    collections::BTreeSet,
    fs, io,
    mem::{size_of, size_of_val},
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_ALREADY_EXISTS, ERROR_NO_MORE_FILES, ERROR_SERVICE_DOES_NOT_EXIST,
            ERROR_SERVICE_NOT_ACTIVE, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0,
        },
        NetworkManagement::{IpHelper::*, WindowsFirewall::*},
        Networking::WinSock::{AF_INET, AF_INET6},
        Security::{Authorization::*, *},
        Storage::FileSystem::{CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoTaskMemFree, CoUninitialize,
            },
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            Services::*,
            SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE},
            Threading::{
                GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_FORMAT,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
                QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
            },
        },
        UI::{
            Shell::{
                FOLDERID_ProgramData, FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
            },
            WindowsAndMessaging::{
                IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW,
            },
        },
    },
    core::{BSTR, GUID, PCWSTR, PWSTR, w},
};

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
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
struct Descriptor(PSECURITY_DESCRIPTOR);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}
struct ComApartment;
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn path_wide(value: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.as_os_str().encode_wide().chain(Some(0)).collect()
}
fn error_code(error: &windows::core::Error) -> u32 {
    error.code().0 as u32 & 0xffff
}
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

struct Layout {
    install: PathBuf,
    data: PathBuf,
    exe: PathBuf,
    image: String,
}
impl Layout {
    fn new() -> io::Result<Self> {
        fn known(id: &GUID) -> io::Result<PathBuf> {
            unsafe {
                let path = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)?;
                let value = path.to_string().map_err(io::Error::other);
                CoTaskMemFree(Some(path.0.cast()));
                Ok(PathBuf::from(value?))
            }
        }
        let install = known(&FOLDERID_ProgramFiles)?.join("Mousehop");
        let data = known(&FOLDERID_ProgramData)?.join("Mousehop");
        let exe = install.join("mousehop.exe");
        let image = format!("\"{}\" windows-service", exe.display());
        Ok(Self {
            install,
            data,
            exe,
            image,
        })
    }
}

fn require_admin() -> io::Result<()> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let token = Handle(token);
        let mut elevation = TOKEN_ELEVATION::default();
        let mut needed = 0;
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            size_of_val(&elevation) as u32,
            &mut needed,
        )?;
        if elevation.TokenIsElevated == 0 {
            return Err(invalid("Administrator consent is required"));
        }
    }
    Ok(())
}

fn descriptor(sddl: &str) -> io::Result<Descriptor> {
    let mut raw = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(wide(sddl).as_ptr()),
            SDDL_REVISION_1,
            &mut raw,
            None,
        )?;
    }
    Ok(Descriptor(raw))
}
fn trusted_sid(sid: PSID) -> bool {
    unsafe {
        IsWellKnownSid(sid, WinLocalSystemSid).as_bool()
            || IsWellKnownSid(sid, WinBuiltinAdministratorsSid).as_bool()
    }
}

/// Validate before modifying a tree: never recurse through a junction while elevated.
fn trusted_tree(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(invalid(format!(
            "Reparse point not allowed: {}",
            path.display()
        )));
    }
    unsafe {
        let mut owner = PSID::default();
        let mut acl = std::ptr::null_mut();
        let mut raw = PSECURITY_DESCRIPTOR::default();
        GetNamedSecurityInfoW(
            PCWSTR(path_wide(path).as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut acl),
            None,
            &mut raw,
        )
        .ok()?;
        let _descriptor = Descriptor(raw);
        if !trusted_sid(owner) {
            return Err(invalid(format!("Untrusted owner: {}", path.display())));
        }
        if acl.is_null() {
            return Err(invalid(format!("Unrestricted access: {}", path.display())));
        }
        for index in 0..u32::from((*acl).AceCount) {
            let mut raw = std::ptr::null_mut();
            GetAce(acl, index, &mut raw)?;
            let header = &*raw.cast::<ACE_HEADER>();
            // Fail closed for unfamiliar allow-ACE formats rather than treating
            // an object-specific or callback ACE as an ordinary file ACE.
            if header.AceType == ACCESS_ALLOWED_ACE_TYPE as u8 {
                let ace = &*raw.cast::<ACCESS_ALLOWED_ACE>();
                let sid = PSID((&ace.SidStart as *const u32).cast_mut().cast());
                // Write, append, attributes, delete-child, delete, WRITE_DAC,
                // WRITE_OWNER, GENERIC_WRITE or GENERIC_ALL. Read is allowed.
                if ace.Mask & 0x500d_0156 != 0 && !trusted_sid(sid) {
                    return Err(invalid(format!(
                        "Non-administrator write access: {}",
                        path.display()
                    )));
                }
            } else if header.AceType != ACCESS_DENIED_ACE_TYPE as u8 {
                return Err(invalid(format!("Unsupported file ACL: {}", path.display())));
            }
        }
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            trusted_tree(&entry?.path())?;
        }
    }
    Ok(())
}

fn protect_directory(path: &Path, public_read: bool) -> io::Result<()> {
    trusted_tree(path)?;
    let sddl = if public_read {
        "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;BU)"
    } else {
        "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
    };
    let security = descriptor(sddl)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0.0,
        bInheritHandle: false.into(),
    };
    unsafe {
        if let Err(e) = CreateDirectoryW(PCWSTR(path_wide(path).as_ptr()), Some(&attributes)) {
            if error_code(&e) != ERROR_ALREADY_EXISTS.0 {
                return Err(e.into());
            }
            // Recheck if another process created the path since the first check.
            trusted_tree(path)?;
            let mut owner = PSID::default();
            GetSecurityDescriptorOwner(security.0, &mut owner, &mut false.into())?;
            let mut acl = std::ptr::null_mut();
            GetSecurityDescriptorDacl(security.0, &mut false.into(), &mut acl, &mut false.into())?;
            SetNamedSecurityInfoW(
                PCWSTR(path_wide(path).as_ptr()),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                Some(owner),
                None,
                Some(acl),
                None,
            )
            .ok()?;
        }
    }
    println!("Protected directory: {}", path.display());
    Ok(())
}

/// Pin the existing private key's DACL before allowing inherited Users read
/// access on the shared directory. Protected children do not inherit that grant.
fn protect_identity(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    trusted_tree(path)?;
    if !metadata.is_file() {
        return Err(invalid(format!(
            "Identity is not a regular file: {}",
            path.display()
        )));
    }
    let security = descriptor("D:P(A;;FA;;;SY)(A;;FA;;;BA)")?;
    unsafe {
        let mut acl = std::ptr::null_mut();
        GetSecurityDescriptorDacl(security.0, &mut false.into(), &mut acl, &mut false.into())?;
        SetNamedSecurityInfoW(
            PCWSTR(path_wide(path).as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(acl),
            None,
        )
        .ok()?;
    }
    println!(
        "Private identity remains SYSTEM/administrator-only: {}",
        path.display()
    );
    Ok(())
}

fn manager() -> io::Result<ServiceHandle> {
    unsafe {
        Ok(ServiceHandle(OpenSCManagerW(
            None,
            None,
            SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE,
        )?))
    }
}
fn open_service(manager: &ServiceHandle) -> io::Result<Option<ServiceHandle>> {
    unsafe {
        match OpenServiceW(manager.0, w!("Mousehop"), SERVICE_ALL_ACCESS) {
            Ok(handle) => Ok(Some(ServiceHandle(handle))),
            Err(e) if error_code(&e) == ERROR_SERVICE_DOES_NOT_EXIST.0 => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
fn verify_service(service: &ServiceHandle, layout: &Layout) -> io::Result<()> {
    unsafe {
        let mut needed = 0;
        let _ = QueryServiceConfigW(service.0, None, 0, &mut needed);
        let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        QueryServiceConfigW(
            service.0,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )?;
        let config = &*buffer.as_ptr().cast::<QUERY_SERVICE_CONFIGW>();
        let image = config
            .lpBinaryPathName
            .to_string()
            .map_err(io::Error::other)?;
        let account = config
            .lpServiceStartName
            .to_string()
            .map_err(io::Error::other)?;
        if !image.eq_ignore_ascii_case(&layout.image)
            || !account.eq_ignore_ascii_case("LocalSystem")
        {
            return Err(invalid(format!(
                "Unrelated Mousehop service: {image} ({account}); refusing to modify it"
            )));
        }
    }
    Ok(())
}
fn status(service: &ServiceHandle) -> io::Result<SERVICE_STATUS_PROCESS> {
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0;
    unsafe {
        let bytes = std::slice::from_raw_parts_mut(
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast::<u8>(),
            size_of_val(&status),
        );
        QueryServiceStatusEx(service.0, SC_STATUS_PROCESS_INFO, Some(bytes), &mut needed)?;
    }
    Ok(status)
}
fn wait_service(service: &ServiceHandle, expected: SERVICE_STATUS_CURRENT_STATE) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let state = status(service)?;
        if state.dwCurrentState == expected {
            return Ok(());
        }
        if expected == SERVICE_RUNNING && state.dwCurrentState == SERVICE_STOPPED {
            return Err(invalid(format!(
                "Service stopped during startup (Windows error {}); see ProgramData\\Mousehop\\service.log",
                state.dwWin32ExitCode
            )));
        }
        if Instant::now() >= deadline {
            return Err(invalid("Timed out waiting for Mousehop service state"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
fn stop_service(manager: &ServiceHandle, layout: &Layout) -> io::Result<()> {
    if let Some(service) = open_service(manager)? {
        verify_service(&service, layout)?;
        let state = status(&service)?.dwCurrentState;
        if state != SERVICE_STOPPED {
            println!("Stopping installed Mousehop service");
            if state != SERVICE_STOP_PENDING {
                let mut stopped = SERVICE_STATUS::default();
                if let Err(e) =
                    unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut stopped) }
                {
                    if error_code(&e) != ERROR_SERVICE_NOT_ACTIVE.0 {
                        return Err(e.into());
                    }
                }
            }
            wait_service(&service, SERVICE_STOPPED)?;
        }
    }
    Ok(())
}

struct Process {
    pid: u32,
    path: PathBuf,
    handle: Handle,
}
fn process(pid: u32) -> io::Result<Process> {
    unsafe {
        let handle = Handle(OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )?);
        let mut path = vec![0u16; 32768];
        let mut length = path.len() as u32;
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_FORMAT(0),
            PWSTR(path.as_mut_ptr()),
            &mut length,
        )?;
        let path = PathBuf::from(String::from_utf16_lossy(&path[..length as usize]));
        Ok(Process { pid, path, handle })
    }
}
fn executable_key(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_owned())
        .to_string_lossy()
        .into_owned()
}
fn terminate(process: &Process) -> io::Result<()> {
    println!(
        "Stopping Mousehop PID {}: {}",
        process.pid,
        process.path.display()
    );
    unsafe {
        if WaitForSingleObject(process.handle.0, 0) == WAIT_OBJECT_0 {
            return Ok(());
        }
        TerminateProcess(process.handle.0, 0)?;
        if WaitForSingleObject(process.handle.0, 5000) != WAIT_OBJECT_0 {
            return Err(invalid(format!("PID {} did not exit", process.pid)));
        }
    }
    Ok(())
}
fn stop_installed_processes(layout: &Layout) -> io::Result<()> {
    let expected = executable_key(&layout.exe);
    unsafe {
        let snapshot = Handle(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?);
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        match Process32FirstW(snapshot.0, &mut entry) {
            Ok(()) => {}
            Err(e) if error_code(&e) == ERROR_NO_MORE_FILES.0 => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        loop {
            let len = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..len])
                .eq_ignore_ascii_case("mousehop.exe")
            {
                if let Ok(process) = process(entry.th32ProcessID) {
                    if same_executable(&executable_key(&process.path), &expected) {
                        terminate(&process)?;
                    }
                }
            }
            match Process32NextW(snapshot.0, &mut entry) {
                Ok(()) => {}
                Err(e) if error_code(&e) == ERROR_NO_MORE_FILES.0 => break,
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

fn service_port(layout: &Layout) -> io::Result<u16> {
    match fs::read_to_string(layout.data.join("config.toml")) {
        Ok(text) => {
            let table: toml::Table = toml::from_str(&text).map_err(io::Error::other)?;
            match table.get("port") {
                None => Ok(4252),
                Some(value) => value
                    .as_integer()
                    .and_then(|p| u16::try_from(p).ok())
                    .ok_or_else(|| {
                        invalid("Invalid service port in ProgramData\\Mousehop\\config.toml")
                    }),
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(4252),
        Err(e) => Err(e),
    }
}

/// IP Helper snapshots detect actual listeners, independently of process names.
fn port_owners(port: u16) -> io::Result<BTreeSet<u32>> {
    let mut owners = BTreeSet::new();
    if port == 0 {
        return Ok(owners);
    }
    for family in [AF_INET.0 as u32, AF_INET6.0 as u32] {
        for tcp in [true, false] {
            let query = |data, size| unsafe {
                if tcp {
                    GetExtendedTcpTable(data, size, false, family, TCP_TABLE_OWNER_PID_LISTENER, 0)
                } else {
                    GetExtendedUdpTable(data, size, false, family, UDP_TABLE_OWNER_PID, 0)
                }
            };
            let mut needed = 0;
            let _ = query(None, &mut needed);
            for attempt in 0..3 {
                let mut buffer = vec![0u64; (needed.max(128) as usize).div_ceil(size_of::<u64>())];
                let code = query(Some(buffer.as_mut_ptr().cast()), &mut needed);
                if code == 122 && attempt < 2 {
                    continue;
                }
                if code != 0 {
                    return Err(io::Error::from_raw_os_error(code as i32));
                }
                unsafe {
                    macro_rules! rows {
                        ($table:ty) => {{
                            let table = &*buffer.as_ptr().cast::<$table>();
                            for i in 0..table.dwNumEntries as usize {
                                let row = &*table.table.as_ptr().add(i);
                                if table_port(row.dwLocalPort) == port {
                                    owners.insert(row.dwOwningPid);
                                }
                            }
                        }};
                    }
                    match (tcp, family == AF_INET.0 as u32) {
                        (true, true) => rows!(MIB_TCPTABLE_OWNER_PID),
                        (true, false) => rows!(MIB_TCP6TABLE_OWNER_PID),
                        (false, true) => rows!(MIB_UDPTABLE_OWNER_PID),
                        (false, false) => rows!(MIB_UDP6TABLE_OWNER_PID),
                    }
                }
                break;
            }
        }
    }
    Ok(owners)
}
fn check_port(layout: &Layout, silent: bool) -> io::Result<()> {
    let port = service_port(layout)?;
    for pid in port_owners(port)? {
        let process = process(pid).map_err(|e| {
            invalid(format!(
                "Service port {port} is occupied by PID {pid}; cannot inspect it: {e}"
            ))
        })?;
        let is_mousehop = process
            .path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("mousehop.exe"));
        if !is_mousehop || silent {
            return Err(invalid(format!(
                "Service port {port} is occupied by PID {pid}: {}. Close this listener and retry.",
                process.path.display()
            )));
        }
        let prompt = format!(
            "Mousehop PID {pid} is using service port {port}:\n{}\n\nStop this input-sharing process so the Windows service can use the port?",
            process.path.display()
        );
        let answer = unsafe {
            MessageBoxW(
                None,
                PCWSTR(wide(&prompt).as_ptr()),
                w!("Mousehop installation"),
                MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST,
            )
        };
        if answer != IDYES {
            return Err(invalid(
                "Installation cancelled; the portable Mousehop process was left running",
            ));
        }
        terminate(&process)?;
    }
    if !port_owners(port)?.is_empty() {
        return Err(invalid(format!(
            "Service port {port} is still occupied; close the listener and retry"
        )));
    }
    Ok(())
}

fn firewall(layout: &Layout, install: bool, allow_public: bool) -> io::Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let _apartment = ComApartment;
        let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)?;
        let profiles = NET_FW_PROFILE2_DOMAIN.0
            | NET_FW_PROFILE2_PRIVATE.0
            | if allow_public {
                NET_FW_PROFILE2_PUBLIC.0
            } else {
                0
            };
        if install {
            let active = policy.CurrentProfileTypes()?;
            println!(
                "Firewall active profiles: {active:#x}; Mousehop allowed profiles: {profiles:#x} (Domain=1, Private=2, Public=4)"
            );
            if active & !profiles != 0 {
                println!(
                    "WARNING: Mousehop inbound rules do not cover every active network profile. For a trusted Public LAN, enable the installer Public network option."
                );
            }
            for profile in [
                NET_FW_PROFILE2_DOMAIN,
                NET_FW_PROFILE2_PRIVATE,
                NET_FW_PROFILE2_PUBLIC,
            ] {
                if active & profile.0 != 0 && policy.get_BlockAllInboundTraffic(profile)?.as_bool()
                {
                    println!(
                        "WARNING: Profile {} blocks all inbound connections, including allow rules. Review Windows Firewall policy with your administrator.",
                        profile.0
                    );
                }
            }
            if policy.LocalPolicyModifyState()? != NET_FW_MODIFY_STATE_OK {
                println!(
                    "WARNING: Windows policy may prevent local firewall rules from taking effect. Review the effective firewall policy with your administrator."
                );
            }
        }
        let rules = policy.Rules()?;
        for (name, protocol) in [
            ("Mousehop Service (TCP)", NET_FW_IP_PROTOCOL_TCP),
            ("Mousehop Service (UDP)", NET_FW_IP_PROTOCOL_UDP),
        ] {
            let name = BSTR::from(name);
            // Remove current rules and rules created by the earlier PowerShell
            // installer (whose COM Name is its DisplayName).
            if let Err(e) = rules.Remove(&name) {
                if error_code(&e) != 2 {
                    return Err(e.into());
                }
            }
            if install {
                let rule: INetFwRule = CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER)?;
                rule.SetName(&name)?;
                rule.SetGrouping(&BSTR::from("Mousehop"))?;
                rule.SetApplicationName(&BSTR::from(layout.exe.to_string_lossy().as_ref()))?;
                rule.SetProtocol(protocol.0)?;
                rule.SetDirection(NET_FW_RULE_DIR_IN)?;
                rule.SetProfiles(profiles)?;
                rule.SetRemoteAddresses(&BSTR::from("LocalSubnet"))?;
                rule.SetAction(NET_FW_ACTION_ALLOW)?;
                rule.SetEnabled(true.into())?;
                rules.Add(&rule)?;
            }
        }
    }
    println!(
        "{} Mousehop executable/LocalSubnet firewall rules (Public network access: {})",
        if install { "Created" } else { "Removed" },
        allow_public
    );
    Ok(())
}
fn install_service(manager: &ServiceHandle, layout: &Layout, allow_public: bool) -> io::Result<()> {
    let service = match open_service(manager)? {
        Some(service) => {
            verify_service(&service, layout)?;
            unsafe {
                ChangeServiceConfigW(
                    service.0,
                    ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )?;
            }
            service
        }
        None => unsafe {
            ServiceHandle(CreateServiceW(
                manager.0,
                w!("Mousehop"),
                w!("Mousehop"),
                SERVICE_ALL_ACCESS,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                PCWSTR(wide(&layout.image).as_ptr()),
                None,
                None,
                None,
                w!("LocalSystem"),
                None,
            )?)
        },
    };
    unsafe {
        let mut description =
            wide("Mousehop console-session input sharing, including login, lock and UAC desktops.");
        let description = SERVICE_DESCRIPTIONW {
            lpDescription: PWSTR(description.as_mut_ptr()),
        };
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DESCRIPTION,
            Some((&description as *const SERVICE_DESCRIPTIONW).cast()),
        )?;
        let mut actions = [
            SC_ACTION {
                Type: SC_ACTION_RESTART,
                Delay: 5000,
            },
            SC_ACTION {
                Type: SC_ACTION_RESTART,
                Delay: 15000,
            },
            SC_ACTION {
                Type: SC_ACTION_RESTART,
                Delay: 60000,
            },
        ];
        let failure = SERVICE_FAILURE_ACTIONSW {
            dwResetPeriod: 86400,
            cActions: actions.len() as u32,
            lpsaActions: actions.as_mut_ptr(),
            ..Default::default()
        };
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some((&failure as *const SERVICE_FAILURE_ACTIONSW).cast()),
        )?;
        let flag = SERVICE_FAILURE_ACTIONS_FLAG {
            fFailureActionsOnNonCrashFailures: true.into(),
        };
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
            Some((&flag as *const SERVICE_FAILURE_ACTIONS_FLAG).cast()),
        )?;
    }
    firewall(layout, true, allow_public)?;
    unsafe {
        StartServiceW(service.0, None)?;
    }
    wait_service(&service, SERVICE_RUNNING)?;
    println!("Mousehop service is running as LocalSystem with automatic startup");
    Ok(())
}

pub fn run() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let action = args
        .next()
        .ok_or_else(|| invalid("Usage: mousehop-installer Prepare|Install|Uninstall [--silent]"))?;
    let remaining: Vec<_> = args.collect();
    if remaining
        .iter()
        .any(|arg| arg != "--silent" && !(action == "Install" && arg == "--allow-public"))
    {
        return Err(invalid("Unknown installer option"));
    }
    let silent = remaining.iter().any(|arg| arg == "--silent");
    let allow_public = remaining.iter().any(|arg| arg == "--allow-public");
    require_admin()?;
    let layout = Layout::new()?;
    let manager = manager()?;
    println!("Native Mousehop installer: {action} (no PowerShell)");
    match action.as_str() {
        "Prepare" => {
            trusted_tree(&layout.install)?;
            trusted_tree(&layout.data)?;
            stop_service(&manager, &layout)?;
            stop_installed_processes(&layout)?;
            check_port(&layout, silent)?;
            protect_directory(&layout.install, true)?;
            protect_identity(&layout.data.join("identity.pem"))?;
            protect_directory(&layout.data, true)?;
        }
        "Install" => {
            trusted_tree(&layout.install)?;
            trusted_tree(&layout.data)?;
            if !layout.exe.is_file() {
                return Err(invalid("Missing installed mousehop.exe"));
            }
            install_service(&manager, &layout, allow_public)?;
        }
        "Uninstall" => {
            trusted_tree(&layout.install)?;
            stop_service(&manager, &layout)?;
            stop_installed_processes(&layout)?;
            if let Some(service) = open_service(&manager)? {
                unsafe {
                    DeleteService(service.0)?;
                }
            }
            firewall(&layout, false, false)?;
            println!(
                "Machine pairing/configuration preserved in {}",
                layout.data.display()
            );
        }
        _ => {
            return Err(invalid(
                "Unknown action; expected Prepare, Install or Uninstall",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_grants_users_read_without_exposing_the_existing_identity() {
        if require_admin().is_err() {
            eprintln!("ACL upgrade regression requires an elevated Windows test process");
            return;
        }
        struct TemporaryDirectory(PathBuf);
        impl Drop for TemporaryDirectory {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        fn users_rights(path: &Path) -> u32 {
            unsafe {
                let mut acl = std::ptr::null_mut();
                let mut raw = PSECURITY_DESCRIPTOR::default();
                GetNamedSecurityInfoW(
                    PCWSTR(path_wide(path).as_ptr()),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    None,
                    None,
                    Some(&mut acl),
                    None,
                    &mut raw,
                )
                .ok()
                .unwrap();
                let _descriptor = Descriptor(raw);
                let mut users = [0u32; 17];
                let mut length = size_of_val(&users) as u32;
                let sid = PSID(users.as_mut_ptr().cast());
                CreateWellKnownSid(WinBuiltinUsersSid, None, Some(sid), &mut length).unwrap();
                let mut trustee = TRUSTEE_W::default();
                BuildTrusteeWithSidW(&mut trustee, Some(sid));
                let mut rights = 0;
                GetEffectiveRightsFromAclW(acl, &trustee, &mut rights)
                    .ok()
                    .unwrap();
                rights
            }
        }
        let name = format!(
            "mousehop-acl-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = TemporaryDirectory(std::env::temp_dir().join(name));
        protect_directory(&root.0, false).unwrap();
        let config = root.0.join("config.toml");
        let identity = root.0.join("identity.pem");
        fs::write(&config, "port = 4252").unwrap();
        fs::write(&identity, "existing secret").unwrap();
        assert_eq!(users_rights(&config), 0);
        protect_identity(&identity).unwrap();
        protect_directory(&root.0, true).unwrap();
        // Exercises propagation to existing files, the protected exception,
        // and inheritance by logs created after an upgrade.
        let log = root.0.join("service.log");
        fs::write(&log, "worker started").unwrap();
        for path in [&root.0, &config, &log] {
            let rights = users_rights(path);
            assert_ne!(
                rights & 1,
                0,
                "Users must be able to list/read {}",
                path.display()
            );
            assert_eq!(
                rights & 0x500d_0156,
                0,
                "Users must not modify {}",
                path.display()
            );
        }
        assert_eq!(users_rights(&identity), 0);
        assert_eq!(fs::read_to_string(&identity).unwrap(), "existing secret");
        trusted_tree(&root.0).unwrap();
        protect_identity(&identity).unwrap();
        protect_directory(&root.0, true).unwrap();
        assert_eq!(
            users_rights(&identity),
            0,
            "A second upgrade must preserve key privacy"
        );
    }

    #[test]
    fn resolves_the_current_process_image_without_a_name_only_match() {
        let process = process(std::process::id()).unwrap();
        assert!(same_executable(
            &executable_key(&process.path),
            &executable_key(&std::env::current_exe().unwrap())
        ));
    }

    #[test]
    fn finds_actual_tcp_and_udp_listeners_by_pid() {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        for port in [
            tcp.local_addr().unwrap().port(),
            udp.local_addr().unwrap().port(),
        ] {
            assert!(port_owners(port).unwrap().contains(&std::process::id()));
        }
    }
}

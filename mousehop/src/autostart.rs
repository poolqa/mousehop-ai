//! Per-user login integration. Installation-wide startup remains managed by SCM/installer.
use std::io;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
pub(crate) fn status() -> (bool, bool) {
    #[cfg(windows)]
    {
        if mousehop_ipc::windows::service_mode() {
            return (true, true);
        }
        use windows::{
            Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW},
            core::w,
        };
        let mut length = 0;
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
                w!("Mousehop"),
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut length),
            )
        };
        (result.is_ok() && length > 2, false)
    }
    #[cfg(target_os = "macos")]
    {
        (launch_agent().is_ok_and(|path| path.is_file()), false)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        (false, true)
    }
}
#[cfg(target_os = "macos")]
fn launch_agent() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("Missing HOME"))?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents/com.mousehop.Mousehop.plist"))
}
#[cfg(target_os = "macos")]
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
#[cfg(windows)]
fn quote(value: &str) -> String {
    let mut quoted = String::from("\"");
    let mut slashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        quoted.extend(std::iter::repeat_n(
            '\\',
            if ch == '"' { slashes * 2 + 1 } else { slashes },
        ));
        slashes = 0;
        quoted.push(ch);
    }
    quoted.extend(std::iter::repeat_n('\\', slashes * 2));
    quoted.push('"');
    quoted
}
pub(crate) fn set(enabled: bool) -> io::Result<()> {
    #[cfg(windows)]
    {
        if mousehop_ipc::windows::service_mode() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Login startup is managed by the installer",
            ));
        }
        use windows::{
            Win32::System::Registry::{
                HKEY, HKEY_CURRENT_USER, REG_SZ, RegCloseKey, RegCreateKeyW, RegDeleteValueW,
                RegSetValueExW,
            },
            core::w,
        };
        unsafe {
            let mut key = HKEY::default();
            RegCreateKeyW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
                &mut key,
            )
            .ok()?;
            let result = if enabled {
                let exe = std::env::current_exe()?;
                let mut args: Vec<String> = std::env::args().skip(1).collect();
                if args
                    .last()
                    .is_some_and(|arg| arg == "agent" || arg == "settings")
                {
                    args.pop();
                }
                let mut command = quote(&exe.to_string_lossy());
                for arg in args {
                    command.push(' ');
                    command.push_str(&quote(&arg));
                }
                command.push_str(" agent");
                let value: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
                let bytes =
                    std::slice::from_raw_parts(value.as_ptr().cast::<u8>(), value.len() * 2);
                RegSetValueExW(key, w!("Mousehop"), None, REG_SZ, Some(bytes)).ok()
            } else {
                let result = RegDeleteValueW(key, w!("Mousehop"));
                if result == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
                    Ok(())
                } else {
                    result.ok()
                }
            };
            let _ = RegCloseKey(key);
            result?;
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let path = launch_agent()?;
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        if enabled {
            std::fs::create_dir_all(path.parent().expect("LaunchAgents directory"))?;
            let exe = std::env::current_exe()?;
            let mut arguments: Vec<String> = std::env::args().skip(1).collect();
            if arguments
                .last()
                .is_some_and(|arg| arg == "agent" || arg == "settings")
            {
                arguments.pop();
            }
            let args: String = std::iter::once(exe.to_string_lossy().into_owned())
                .chain(arguments)
                .chain(std::iter::once("agent".into()))
                .map(|arg| format!("<string>{}</string>", xml(&arg)))
                .collect();
            std::fs::write(
                &path,
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>com.mousehop.Mousehop</string><key>ProgramArguments</key><array>{args}</array><key>RunAtLoad</key><true/></dict></plist>"
                ),
            )?;
            // RunAtLoad is sufficient at next login. Do not bootstrap a duplicate
            // while the user is changing this setting in a running Agent.
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            // No KeepAlive is registered, so Exit remains authoritative.
            let _ = domain;
        }
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = enabled;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Manage login startup through your desktop session",
        ))
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    #[test]
    fn plist_arguments_escape_executable_and_profile_paths() {
        assert_eq!(
            xml("/Applications/A&B <test> \"app\"/mousehop"),
            "/Applications/A&amp;B &lt;test&gt; &quot;app&quot;/mousehop"
        );
    }
}
#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn startup_arguments_cannot_escape_quoted_paths() {
        assert_eq!(
            quote(r"C:\Program Files\Mousehop\mousehop.exe"),
            r#""C:\Program Files\Mousehop\mousehop.exe""#
        );
        assert_eq!(quote("a\"b\\"), "\"a\\\"b\\\\\"");
    }
}

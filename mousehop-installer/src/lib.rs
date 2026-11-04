//! Platform-independent decisions used by the native Windows installer.

/// QueryFullProcessImageNameW and canonicalize may differ in verbatim prefixes.
/// Compare full paths, never process names or directory-prefix matches.
pub fn same_executable(actual: &str, installed: &str) -> bool {
    fn normalize(path: &str) -> String {
        let path = path.strip_prefix(r"\\?\").unwrap_or(path);
        path.replace('/', "\\").to_lowercase()
    }
    normalize(actual) == normalize(installed)
}

/// IP Helper stores a network-order u16 port in the low half of a DWORD.
pub fn table_port(raw: u32) -> u16 {
    u16::from_be(raw as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_installed_executable_is_selected() {
        let installed = r"C:\Program Files\Mousehop\mousehop.exe";
        assert!(same_executable(
            r"\\?\c:\PROGRAM FILES\Mousehop\mousehop.exe",
            installed
        ));
        assert!(!same_executable(
            r"C:\Users\admin\Downloads\mousehop.exe",
            installed
        ));
        assert!(!same_executable(
            r"C:\Program Files\Mousehop-old\mousehop.exe",
            installed
        ));
        assert!(!same_executable(
            r"C:\Program Files\Mousehop\other.exe",
            installed
        ));
    }

    #[test]
    fn ipv4_and_ipv6_port_values_use_network_byte_order() {
        assert_eq!(table_port(u16::to_be(4252) as u32), 4252);
        assert_eq!(table_port(u16::to_be(65535) as u32), 65535);
        assert_eq!(table_port(0), 0);
    }
}

/// Refuse to start a service using a partial or mixed-version installation.
pub fn verify_payload(directory: &std::path::Path, manifest: &str) -> std::io::Result<()> {
    use sha2::{Digest, Sha256};
    use std::{
        collections::HashSet,
        io::{self, Read},
    };
    let required = ["mousehop.exe", "mousehop-ui.exe", "mousehop-cli.exe"];
    let mut seen = HashSet::new();
    for line in manifest.lines() {
        let (expected, name) = line
            .split_once("  ")
            .ok_or_else(|| io::Error::other("Invalid payload manifest"))?;
        if expected.len() != 64
            || !expected.bytes().all(|c| c.is_ascii_hexdigit())
            || name.is_empty()
            || name.contains(['/', '\\', ':'])
            || !(required.contains(&name) || name.to_ascii_lowercase().ends_with(".dll"))
            || !seen.insert(name.to_ascii_lowercase())
        {
            return Err(io::Error::other("Invalid payload manifest entry"));
        }
        let mut file = std::fs::File::open(directory.join(name))
            .map_err(|e| io::Error::new(e.kind(), format!("Installed file {name}: {e}")))?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        if !format!("{:x}", hash.finalize()).eq_ignore_ascii_case(expected) {
            return Err(io::Error::other(format!(
                "Installed file {name} differs from this installer; refusing to start a mixed-version service"
            )));
        }
    }
    if required.iter().any(|name| !seen.contains(*name)) {
        return Err(io::Error::other(
            "Payload manifest is missing a required executable",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod payload_tests {
    use super::*;
    use sha2::{Digest, Sha256};
    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn payload() -> (Directory, String) {
        let directory = Directory(std::env::temp_dir().join(format!(
                "mousehop-payload-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        std::fs::create_dir(&directory.0).unwrap();
        let mut manifest = String::new();
        for name in [
            "mousehop.exe",
            "mousehop-ui.exe",
            "mousehop-cli.exe",
            "gtk-4-1.dll",
        ] {
            std::fs::write(directory.0.join(name), name).unwrap();
            manifest.push_str(&format!("{:x}  {name}\n", Sha256::digest(name.as_bytes())));
        }
        (directory, manifest)
    }
    #[test]
    fn upgrade_rejects_a_settings_executable_left_from_an_older_build() {
        let (directory, manifest) = payload();
        verify_payload(&directory.0, &manifest).unwrap();
        std::fs::write(directory.0.join("mousehop-ui.exe"), "old build").unwrap();
        assert!(
            verify_payload(&directory.0, &manifest)
                .unwrap_err()
                .to_string()
                .contains("mousehop-ui.exe")
        );
    }
    #[test]
    fn upgrade_rejects_missing_runtime_libraries_and_incomplete_manifests() {
        let (directory, manifest) = payload();
        std::fs::remove_file(directory.0.join("gtk-4-1.dll")).unwrap();
        assert!(verify_payload(&directory.0, &manifest).is_err());
        assert!(verify_payload(&directory.0, "").is_err());
        assert!(
            verify_payload(
                &directory.0,
                &format!("{}  ../mousehop.exe", "0".repeat(64))
            )
            .is_err()
        );
    }
}

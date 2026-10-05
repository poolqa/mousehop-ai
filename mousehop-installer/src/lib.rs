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

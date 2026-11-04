//! Keep the local half of a key gesture balanced across pointer ownership changes.
use std::collections::HashSet;

#[derive(Default)]
pub(crate) struct HostKeyRouting {
    local_down: HashSet<u32>,
}
impl HostKeyRouting {
    /// Whether this physical transition must also reach local Windows.
    pub fn pass_to_host(&mut self, key: u32, state: u8, captured: bool) -> bool {
        if state == 0 {
            let started_locally = self.local_down.remove(&key);
            !captured || started_locally
        } else if !captured {
            self.local_down.insert(key);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_event::scancode::Linux::*;
    #[test]
    fn modifiers_pressed_before_crossing_are_released_on_both_machines() {
        for key in [
            KeyLeftShift,
            KeyRightShift,
            KeyLeftCtrl,
            KeyRightCtrl,
            KeyLeftAlt,
            KeyRightalt,
            KeyLeftMeta,
            KeyRightmeta,
        ] {
            let mut route = HostKeyRouting::default();
            assert!(route.pass_to_host(key as u32, 1, false));
            assert!(!route.pass_to_host(key as u32, 1, true));
            assert!(route.pass_to_host(key as u32, 0, true));
            // A later remote-only press must not inherit the old local pairing.
            assert!(!route.pass_to_host(key as u32, 1, true));
            assert!(!route.pass_to_host(key as u32, 0, true));
        }
    }
    #[test]
    fn remote_shortcuts_do_not_leak_into_local_apps() {
        let mut route = HostKeyRouting::default();
        for key in [KeyLeftCtrl, KeyA] {
            assert!(!route.pass_to_host(key as u32, 1, true));
            assert!(!route.pass_to_host(key as u32, 0, true));
        }
        assert!(route.pass_to_host(KeyLeftCtrl as u32, 0, false));
    }
}

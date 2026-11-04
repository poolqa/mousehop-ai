//! Track successfully injected modifiers independently of the network wrapper.
//! A rejected key-up remains owed until a later input can submit it successfully.
use crate::{EmulationHandle, modifiers::modifier_mask};
use std::collections::{BTreeSet, HashMap};

#[derive(Default)]
pub(crate) struct ModifierRecovery {
    held: HashMap<u32, EmulationHandle>,
    pending: BTreeSet<u32>,
}
impl ModifierRecovery {
    pub fn record(&mut self, handle: EmulationHandle, key: u32, state: u8, submitted: bool) {
        if modifier_mask(key).is_none() {
            return;
        }
        match (state, submitted) {
            (1, true) => {
                self.held.insert(key, handle);
                self.pending.remove(&key);
            }
            (0, true) => {
                self.held.remove(&key);
                self.pending.remove(&key);
            }
            (0, false) => {
                self.pending.insert(key);
            }
            _ => {}
        }
    }
    pub fn snapshot(&mut self, handle: EmulationHandle, mask: u32) {
        for (&key, &owner) in &self.held {
            if owner == handle && modifier_mask(key).is_some_and(|group| group & mask == 0) {
                self.pending.insert(key);
            }
        }
    }
    pub fn release(&mut self, handle: EmulationHandle) {
        self.pending.extend(
            self.held
                .iter()
                .filter_map(|(&key, &owner)| (owner == handle).then_some(key)),
        );
    }
    pub fn release_all(&mut self) {
        self.pending.extend(self.held.keys().copied());
    }
    pub fn retry(&mut self, mut release: impl FnMut(u32) -> bool) {
        for key in self.pending.iter().copied().collect::<Vec<_>>() {
            if release(key) {
                self.pending.remove(&key);
                self.held.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_event::scancode::Linux::*;
    #[test]
    fn shutdown_releases_all_owned_modifiers_and_no_lock_or_character_keys() {
        let mut state = ModifierRecovery::default();
        let keys = [
            KeyLeftShift,
            KeyRightShift,
            KeyLeftCtrl,
            KeyRightCtrl,
            KeyLeftAlt,
            KeyRightalt,
            KeyLeftMeta,
            KeyRightmeta,
        ];
        for (owner, key) in keys.iter().enumerate() {
            state.record(owner as u64, *key as u32, 1, true);
        }
        state.record(1, KeyCapsLock as u32, 1, true);
        state.record(1, KeyA as u32, 1, true);
        state.release_all();
        let mut count = 0;
        state.retry(|key| {
            assert!(keys.iter().any(|known| *known as u32 == key));
            count += 1;
            true
        });
        assert_eq!(count, 8);
        assert!(state.held.is_empty());
    }
    #[test]
    fn rejected_key_up_is_retried_before_the_next_click() {
        let key = KeyLeftCtrl as u32;
        let mut state = ModifierRecovery::default();
        state.record(1, key, 1, true);
        state.record(1, key, 0, false);
        let mut calls = Vec::new();
        state.retry(|key| {
            calls.push(key);
            false
        });
        state.snapshot(1, 0);
        state.retry(|key| {
            calls.push(key);
            true
        });
        state.retry(|_| panic!("release was already delivered"));
        assert_eq!(calls, [key, key]);
    }
    #[test]
    fn snapshots_release_only_owned_modifiers_and_preserve_held_groups() {
        let mut state = ModifierRecovery::default();
        state.record(1, KeyLeftShift as u32, 1, true);
        state.record(1, KeyRightalt as u32, 1, true);
        state.record(2, KeyLeftMeta as u32, 1, true);
        state.record(1, KeyA as u32, 1, true);
        state.snapshot(1, (1 << 0) | (1 << 7));
        state.retry(|_| panic!("still held or owned by another peer"));
        state.snapshot(1, 0);
        let mut released = Vec::new();
        state.retry(|key| {
            released.push(key);
            true
        });
        assert_eq!(released, [KeyLeftShift as u32, KeyRightalt as u32]);
        state.release(2);
        state.retry(|key| {
            assert_eq!(key, KeyLeftMeta as u32);
            true
        });
    }
    #[test]
    fn a_new_press_supersedes_an_old_owners_failed_release() {
        let key = KeyLeftMeta as u32;
        let mut state = ModifierRecovery::default();
        state.record(1, key, 1, true);
        state.release(1);
        state.retry(|_| false);
        state.record(2, key, 1, true);
        state.retry(|_| panic!("must not release the new owner's held key"));
        state.release(1);
        state.retry(|_| panic!("old owner no longer holds this key"));
        state.record(2, key, 0, true);
        assert!(state.held.is_empty());
    }
}

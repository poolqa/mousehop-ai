use input_event::scancode::Linux;
use std::collections::HashSet;

/// XKB depressed/latched masks describe groups, never the physical side.
/// Do not release either side while any bit for its group remains set.
pub(crate) fn released_modifiers(keys: &HashSet<u32>, mask: u32) -> Vec<u32> {
    let mut released = keys
        .iter()
        .copied()
        .filter(|key| {
            let group = match Linux::try_from(*key) {
                Ok(Linux::KeyLeftShift | Linux::KeyRightShift) => 1 << 0,
                Ok(Linux::KeyLeftCtrl | Linux::KeyRightCtrl) => 1 << 2,
                Ok(Linux::KeyLeftAlt | Linux::KeyRightalt) => (1 << 3) | (1 << 7),
                Ok(Linux::KeyLeftMeta | Linux::KeyRightmeta) => 1 << 6,
                _ => return false,
            };
            mask & group == 0
        })
        .collect::<Vec<_>>();
    released.sort_unstable();
    released
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_releases_both_commands_but_preserves_characters_and_caps_lock() {
        let keys = [
            Linux::KeyLeftMeta,
            Linux::KeyRightmeta,
            Linux::KeyA,
            Linux::KeyCapsLock,
        ]
        .map(|key| key as u32)
        .into_iter()
        .collect();
        assert_eq!(
            released_modifiers(&keys, 0),
            vec![Linux::KeyLeftMeta as u32, Linux::KeyRightmeta as u32]
        );
        assert!(released_modifiers(&keys, 1 << 6).is_empty());
    }
    #[test]
    fn held_groups_and_altgr_are_preserved() {
        let keys = [
            Linux::KeyLeftShift,
            Linux::KeyRightShift,
            Linux::KeyRightalt,
            Linux::KeyLeftCtrl,
        ]
        .map(|key| key as u32)
        .into_iter()
        .collect();
        assert_eq!(
            released_modifiers(&keys, (1 << 0) | (1 << 7)),
            vec![Linux::KeyLeftCtrl as u32]
        );
    }
}

#[cfg(all(test, any(target_os = "macos", windows)))]
mod pipeline_tests {
    use super::*;
    use crate::{Emulation, EmulationError, EmulationHandle, InputEmulation};
    use input_event::{Event, KeyboardEvent};
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    struct Recorder(Arc<Mutex<Vec<Event>>>);
    #[async_trait::async_trait]
    impl Emulation for Recorder {
        async fn consume(
            &mut self,
            event: Event,
            _: EmulationHandle,
        ) -> Result<(), EmulationError> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
        async fn create(&mut self, _: EmulationHandle) {}
        async fn destroy(&mut self, _: EmulationHandle) {}
        async fn terminate(&mut self) {}
    }
    fn key(key: Linux, state: u8) -> Event {
        Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key: key as u32,
            state,
        })
    }
    fn snapshot(depressed: u32) -> Event {
        Event::Keyboard(KeyboardEvent::Modifiers {
            depressed,
            latched: 0,
            locked: 0,
            group: 0,
        })
    }
    #[tokio::test]
    async fn missing_command_up_is_repaired_without_suppressing_the_next_press() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut emulation = InputEmulation {
            emulation: Box::new(Recorder(events.clone())),
            handles: HashSet::new(),
            pressed_keys: HashMap::new(),
            post_processing: HashMap::new(),
            clipboard: None,
        };
        emulation.create(1).await;
        emulation.create(2).await;
        emulation
            .consume(key(Linux::KeyLeftMeta, 1), 1)
            .await
            .unwrap();
        emulation
            .consume(key(Linux::KeyRightmeta, 1), 2)
            .await
            .unwrap();
        emulation.consume(snapshot(0), 1).await.unwrap();
        assert!(!emulation.has_pressed_keys(1));
        assert!(emulation.has_pressed_keys(2));
        emulation
            .consume(key(Linux::KeyLeftMeta, 1), 1)
            .await
            .unwrap();
        assert!(emulation.has_pressed_keys(1));
        emulation.terminate().await;
        let events = events.lock().unwrap();
        assert_eq!(
            &events[..5],
            &[
                key(Linux::KeyLeftMeta, 1),
                key(Linux::KeyRightmeta, 1),
                key(Linux::KeyLeftMeta, 0),
                snapshot(0),
                key(Linux::KeyLeftMeta, 1),
            ]
        );
        assert!(events[5..].contains(&key(Linux::KeyLeftMeta, 0)));
        assert!(events[5..].contains(&key(Linux::KeyRightmeta, 0)));
        assert!(!emulation.has_pressed_keys(1));
        assert!(!emulation.has_pressed_keys(2));
    }
}

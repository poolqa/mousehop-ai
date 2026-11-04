use input_event::scancode::Linux;
use std::collections::HashSet;

pub(crate) fn modifier_mask(key: u32) -> Option<u32> {
    match Linux::try_from(key).ok()? {
        Linux::KeyLeftShift | Linux::KeyRightShift => Some(1 << 0),
        Linux::KeyLeftCtrl | Linux::KeyRightCtrl => Some(1 << 2),
        Linux::KeyLeftAlt | Linux::KeyRightalt => Some((1 << 3) | (1 << 7)),
        Linux::KeyLeftMeta | Linux::KeyRightmeta => Some(1 << 6),
        _ => None,
    }
}

/// XKB depressed/latched masks describe groups, never the physical side.
/// Do not release either side while any bit for its group remains set.
pub(crate) fn released_modifiers(keys: &HashSet<u32>, mask: u32) -> Vec<u32> {
    let mut released = keys
        .iter()
        .copied()
        .filter(|key| modifier_mask(*key).is_some_and(|group| mask & group == 0))
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
    use input_event::{BTN_LEFT, BTN_RIGHT, Event, KeyboardEvent, PointerEvent};
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
    fn recorder() -> (InputEmulation, Arc<Mutex<Vec<Event>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            InputEmulation {
                emulation: Box::new(Recorder(events.clone())),
                handles: HashSet::new(),
                pressed_keys: HashMap::new(),
                pressed_buttons: HashMap::new(),
                post_processing: HashMap::new(),
                clipboard: None,
                #[cfg(windows)]
                desktop_generation: 0,
            },
            events,
        )
    }
    fn button(button: u32, state: u32) -> Event {
        Event::Pointer(PointerEvent::Button {
            time: 0,
            button,
            state,
        })
    }

    struct RejectNext {
        reject: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait::async_trait]
    impl Emulation for RejectNext {
        async fn consume(&mut self, _: Event, _: EmulationHandle) -> Result<(), EmulationError> {
            if self.reject.swap(false, std::sync::atomic::Ordering::SeqCst) {
                Err(EmulationError::EndOfStream)
            } else {
                Ok(())
            }
        }
        async fn create(&mut self, _: EmulationHandle) {}
        async fn destroy(&mut self, _: EmulationHandle) {}
        async fn terminate(&mut self) {}
    }
    #[tokio::test]
    async fn failed_key_transitions_and_cleanup_retain_the_last_confirmed_state() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (mut emulation, _) = recorder();
        let reject = Arc::new(AtomicBool::new(true));
        emulation.emulation = Box::new(RejectNext {
            reject: reject.clone(),
        });
        emulation.create(1).await;
        assert!(
            emulation
                .consume(key(Linux::KeyLeftCtrl, 1), 1)
                .await
                .is_err()
        );
        assert!(!emulation.has_pressed_keys(1));
        emulation
            .consume(key(Linux::KeyLeftCtrl, 1), 1)
            .await
            .unwrap();
        reject.store(true, Ordering::SeqCst);
        assert!(
            emulation
                .consume(key(Linux::KeyLeftCtrl, 0), 1)
                .await
                .is_err()
        );
        assert!(emulation.has_pressed_keys(1));
        reject.store(true, Ordering::SeqCst);
        assert!(emulation.release_keys(1).await.is_err());
        assert!(emulation.has_pressed_keys(1));
        emulation.release_keys(1).await.unwrap();
        assert!(!emulation.has_pressed_keys(1));
    }
    #[tokio::test]
    async fn disconnect_releases_only_that_peers_held_buttons() {
        let (mut emulation, events) = recorder();
        emulation.create(1).await;
        emulation.create(2).await;
        emulation.consume(button(BTN_LEFT, 1), 1).await.unwrap();
        emulation.consume(button(BTN_RIGHT, 1), 2).await.unwrap();
        events.lock().unwrap().clear();
        emulation.destroy(1).await;
        assert!(events.lock().unwrap().contains(&button(BTN_LEFT, 0)));
        assert!(!events.lock().unwrap().contains(&button(BTN_RIGHT, 0)));
        events.lock().unwrap().clear();
        emulation.terminate().await;
        assert!(events.lock().unwrap().contains(&button(BTN_RIGHT, 0)));
        assert!(!events.lock().unwrap().contains(&button(BTN_LEFT, 0)));
    }
    #[tokio::test]
    async fn double_click_passes_all_transitions_without_extra_release_on_disconnect() {
        let (mut emulation, events) = recorder();
        emulation.create(1).await;
        let clicks = [
            button(BTN_LEFT, 1),
            button(BTN_LEFT, 0),
            button(BTN_LEFT, 1),
            button(BTN_LEFT, 0),
        ];
        for event in &clicks {
            emulation.consume(event.clone(), 1).await.unwrap();
        }
        emulation.destroy(1).await;
        let events = events.lock().unwrap();
        let buttons: Vec<_> = events
            .iter()
            .filter(|event| matches!(event, Event::Pointer(PointerEvent::Button { .. })))
            .cloned()
            .collect();
        assert_eq!(buttons, clicks);
    }
    #[tokio::test]
    async fn missing_command_up_is_repaired_without_suppressing_the_next_press() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut emulation = InputEmulation {
            emulation: Box::new(Recorder(events.clone())),
            handles: HashSet::new(),
            pressed_keys: HashMap::new(),
            pressed_buttons: HashMap::new(),
            post_processing: HashMap::new(),
            clipboard: None,
            #[cfg(windows)]
            desktop_generation: 0,
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

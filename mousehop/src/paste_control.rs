//! Native-paste ownership across repeated Sync events and desktop transitions.
type Id = [u8; 16];
#[derive(Default)]
pub(crate) struct PasteControl {
    pub offered: Option<Id>,
    preparing: Option<Id>,
    armed: Option<Id>,
    active: Option<Id>,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Prepare(Id),
    Disarm,
    None,
}
impl PasteControl {
    pub fn offer(&mut self, id: Option<Id>, available: bool) -> Action {
        self.offered = id;
        if let Some(id) = id.filter(|_| available) {
            if self.armed != Some(id) && self.preparing != Some(id) {
                self.preparing = Some(id);
                return Action::Prepare(id);
            }
        } else if self.active.is_none() {
            self.reset();
            return Action::Disarm;
        }
        Action::None
    }
    pub fn accept_preparation(&mut self, id: Id) -> bool {
        if self.offered != Some(id) || self.preparing != Some(id) {
            return false;
        }
        self.preparing = None;
        true
    }
    pub fn armed(&mut self, id: Id) {
        self.armed = Some(id);
    }
    pub fn progress(&mut self, id: Id, running: bool) -> bool {
        if running {
            self.active = Some(id);
            return false;
        }
        if self.active == Some(id) || self.armed == Some(id) {
            self.reset();
            return true;
        }
        false
    }
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_sync_cannot_prepare_two_clipboard_markers() {
        let mut control = PasteControl::default();
        let id = [1; 16];
        assert_eq!(control.offer(Some(id), true), Action::Prepare(id));
        assert_eq!(control.offer(Some(id), true), Action::None);
        assert!(control.accept_preparation(id));
        control.armed(id);
        assert!(!control.accept_preparation(id));
        assert_eq!(control.offer(Some(id), true), Action::None);
    }
    #[test]
    fn consuming_an_offer_keeps_the_marker_until_transfer_finishes() {
        let mut control = PasteControl::default();
        let id = [2; 16];
        control.offer(Some(id), true);
        control.accept_preparation(id);
        control.armed(id);
        assert!(!control.progress(id, true));
        assert_eq!(control.offer(None, true), Action::None);
        assert_eq!(control.armed, Some(id));
        assert!(control.progress(id, false));
        assert_eq!(control.armed, None);
    }
    #[test]
    fn desktop_replacement_rejects_late_preparation_and_completion() {
        let mut control = PasteControl::default();
        let old = [3; 16];
        let new = [4; 16];
        control.offer(Some(old), true);
        control.reset();
        assert!(!control.accept_preparation(old));
        assert_eq!(control.offer(Some(new), true), Action::Prepare(new));
        assert!(!control.progress(old, false));
        assert_eq!(control.offered, Some(new));
    }
}

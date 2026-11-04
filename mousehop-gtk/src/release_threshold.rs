//! Correlate ordered threshold echoes while keeping the user's newest edit on screen.
use std::collections::VecDeque;

#[derive(Default)]
pub(crate) struct ThresholdEdit {
    confirmed: u32,
    pending: VecDeque<u32>,
}
impl ThresholdEdit {
    pub fn edit(&mut self, value: u32) {
        self.pending.push_back(value);
    }
    pub fn confirm(&mut self, value: u32) -> Option<u32> {
        if let Some(expected) = self.pending.front() {
            if *expected != value {
                // A Sync or older echo must not overwrite an outstanding edit.
                return None;
            }
            self.pending.pop_front();
        }
        self.confirmed = value;
        self.pending.is_empty().then_some(value)
    }
    pub fn cancel(&mut self) -> u32 {
        self.pending.clear();
        self.confirmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_sync_cannot_restore_thirty_pixels_over_a_new_edit() {
        let mut edit = ThresholdEdit::default();
        assert_eq!(edit.confirm(30), Some(30));
        edit.edit(80);
        assert_eq!(edit.confirm(30), None);
        assert_eq!(edit.confirm(80), Some(80));
    }
    #[test]
    fn rapid_drag_and_return_to_the_old_value_preserve_the_latest_edit() {
        let mut edit = ThresholdEdit::default();
        edit.confirm(30);
        for value in [40, 70, 30] {
            edit.edit(value);
        }
        assert_eq!(edit.confirm(30), None);
        assert_eq!(edit.confirm(40), None);
        assert_eq!(edit.confirm(40), None); // A redundant Sync.
        assert_eq!(edit.confirm(70), None);
        assert_eq!(edit.confirm(30), Some(30));
    }
    #[test]
    fn zero_disables_release_and_external_updates_work_after_confirmation() {
        let mut edit = ThresholdEdit::default();
        edit.confirm(30);
        edit.edit(0);
        assert_eq!(edit.confirm(30), None);
        assert_eq!(edit.confirm(0), Some(0));
        assert_eq!(edit.confirm(90), Some(90));
    }
    #[test]
    fn failed_send_or_reconnect_clears_pending_edits_and_accepts_core_state() {
        let mut edit = ThresholdEdit::default();
        edit.confirm(30);
        edit.edit(50);
        edit.edit(80);
        assert_eq!(edit.confirm(50), None);
        assert_eq!(edit.cancel(), 50);
        assert_eq!(edit.confirm(30), Some(30));
        edit.edit(70);
        assert_eq!(edit.confirm(70), Some(70));
    }
}

//! Pure Windows pointer policy, also exercised without Win32 hooks in tests.
use crate::{Position, display_edge};
use input_event::display::DisplayLayout;

pub(crate) fn observe_injected_motion(injected: bool, tag: usize, movement: bool) -> bool {
    injected && movement && tag == input_event::WINDOWS_INPUT_EVENT_TAG
}

/// Win32 can report an already-clamped pixel, never an out-of-screen point.
/// Reaching the exposed contour while moving outward is enough to cross.
pub(crate) fn touched_barrier(
    layout: &DisplayLayout,
    previous: (i32, i32),
    current: (i32, i32),
) -> Option<Position> {
    if !layout.rectangles().any(|(_, rect)| rect.contains(previous))
        || !layout.rectangles().any(|(_, rect)| rect.contains(current))
    {
        return None;
    }
    [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ]
    .into_iter()
    .find(|&position| {
        let outward = match position {
            Position::Left => current.0 < previous.0,
            Position::Right => current.0 > previous.0,
            Position::Top => current.1 < previous.1,
            Position::Bottom => current.1 > previous.1,
        };
        outward && layout.project_point(display_edge(position), current) == Some(current)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mac_controlled_windows_returns_at_the_rightmost_clamped_pixel() {
        let layout = DisplayLayout::new([(0, 0, 1920, 1080)]);
        assert_eq!(
            touched_barrier(&layout, (1910, 400), (1919, 400)),
            Some(Position::Right)
        );
        assert_eq!(touched_barrier(&layout, (1919, 400), (1910, 400)), None);
        assert_eq!(touched_barrier(&layout, (1919, 400), (1919, 400)), None);
    }
    #[test]
    fn internal_monitor_seams_and_gaps_do_not_trigger_an_outer_edge() {
        let layout = DisplayLayout::new([(-1920, 0, 1920, 1080), (0, 0, 1920, 1080)]);
        assert_eq!(touched_barrier(&layout, (-10, 400), (-1, 400)), None);
        assert_eq!(touched_barrier(&layout, (-1, 400), (0, 400)), None);
        assert_eq!(
            touched_barrier(&layout, (-1900, 400), (-1920, 400)),
            Some(Position::Left)
        );
    }
    #[test]
    fn only_mousehop_relative_motion_can_signal_a_return_without_being_forwarded() {
        assert!(observe_injected_motion(
            true,
            input_event::WINDOWS_INPUT_EVENT_TAG,
            true
        ));
        assert!(!observe_injected_motion(
            true,
            input_event::WINDOWS_POINTER_WARP_TAG,
            true
        ));
        assert!(!observe_injected_motion(true, 0, true));
        assert!(!observe_injected_motion(
            true,
            input_event::WINDOWS_INPUT_EVENT_TAG,
            false
        ));
    }
}

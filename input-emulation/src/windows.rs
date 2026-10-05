use super::error::{EmulationError, WindowsEmulationCreationError};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    scancode,
};

use async_trait::async_trait;
use std::cell::Cell;
use std::ffi::c_void;
use std::ops::BitOrAssign;
use std::time::Duration;
use tokio::task::AbortHandle;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT_0, KEYEVENTF_EXTENDEDKEY, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SPI_GETKEYBOARDDELAY, SPI_GETKEYBOARDSPEED, SYSTEM_PARAMETERS_INFO_ACTION,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW, XBUTTON1, XBUTTON2,
};

use super::{Emulation, EmulationHandle};

/// Fallback initial key-repeat delay used only when the keyboard delay
/// can't be read from the OS (see [`read_key_repeat_settings`]).
const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(500);
/// Fallback key-repeat interval used only when the keyboard speed can't
/// be read from the OS (see [`read_key_repeat_settings`]).
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(32);

/// Reads this machine's keyboard repeat settings and returns them as
/// `(initial_delay, repeat_interval)`.
///
/// `SendInput`-injected keystrokes don't auto-repeat on Windows, so this
/// sink synthesizes the repeat stream itself. Reading the host's own
/// settings (Control Panel → Keyboard) makes forwarded keys feel like
/// typing directly on this machine instead of a hardcoded rate.
///
/// * `SPI_GETKEYBOARDDELAY` returns 0..=3, where each step is ~250 ms,
///   so the delay is `(value + 1) * 250` ms.
/// * `SPI_GETKEYBOARDSPEED` returns 0..=31, mapping roughly linearly
///   from ~2.5 repeats/sec (0) to ~30 repeats/sec (31).
fn read_key_repeat_settings() -> (Duration, Duration) {
    let delay = read_spi_u32(SPI_GETKEYBOARDDELAY)
        .map(|v| Duration::from_millis(u64::from(v.min(3) + 1) * 250))
        .unwrap_or(DEFAULT_REPEAT_DELAY);
    let interval = read_spi_u32(SPI_GETKEYBOARDSPEED)
        .map(|v| {
            let cps = 2.5 + f64::from(v.min(31)) * (30.0 - 2.5) / 31.0;
            Duration::from_millis((1000.0 / cps) as u64)
        })
        .unwrap_or(DEFAULT_REPEAT_INTERVAL);
    (delay, interval)
}

/// Reads a `SystemParametersInfoW` action that yields a single `u32`,
/// returning `None` if the call fails.
fn read_spi_u32(action: SYSTEM_PARAMETERS_INFO_ACTION) -> Option<u32> {
    let mut value: u32 = 0;
    unsafe {
        SystemParametersInfoW(
            action,
            0,
            Some(&mut value as *mut u32 as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
    }
    Some(value)
}

pub(crate) struct WindowsEmulation {
    repeat_task: Option<AbortHandle>,
    /// Cached virtual-screen origin, refreshed on each
    /// `display_bounds()` call — which the emulation proxy invokes
    /// at backend creation and then every 2s. `warp_cursor` reads
    /// the origin from here so it pairs with the size the warp
    /// target was scaled against, instead of a fresher origin that
    /// could mix two arrangements while displays are being
    /// rearranged.
    virtual_screen_origin: Cell<Option<(i32, i32)>>,
    remote: Option<std::sync::Arc<mousehop_ipc::desktop::Link>>,
}

impl WindowsEmulation {
    pub(crate) fn new() -> Result<Self, WindowsEmulationCreationError> {
        Ok(Self {
            repeat_task: None,
            virtual_screen_origin: Cell::new(None),
            remote: mousehop_ipc::desktop::current(),
        })
    }
}

#[async_trait]
impl Emulation for WindowsEmulation {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        if let Some(remote) = &self.remote {
            if let Some(input) = mousehop_ipc::desktop::Input::from_event(&event) {
                remote.send(mousehop_ipc::desktop::Request::Input { handle, input });
            }
            return Ok(());
        }
        match event {
            Event::Pointer(pointer_event) => match pointer_event {
                PointerEvent::Motion { time: _, dx, dy } => {
                    rel_mouse(dx as i32, dy as i32);
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => mouse_button(button, state),
                PointerEvent::Axis { axis, value, .. } => scroll(axis, value as i32),
                PointerEvent::AxisDiscrete120 { axis, value } => scroll(axis, value),
            },
            Event::Keyboard(keyboard_event) => match keyboard_event {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    let submitted = key_event(key, state);
                    if !submitted || key == scancode::Linux::KeyCapsLock as u32 {
                        // Lock keys are discrete toggles. Never synthesize
                        // repeats: delayed release delivery must not flip the
                        // Windows lock state more than once.
                        self.kill_repeat_task();
                    } else if is_modifier_key(key) {
                        // Modifier keys never repeat on a physical keyboard.
                    } else {
                        match state {
                            // pressed
                            0 => self.kill_repeat_task(),
                            1 => self.spawn_repeat_task(key).await,
                            _ => {}
                        }
                    }
                }
                KeyboardEvent::Modifiers { .. } => {}
            },
            Event::Clipboard(_) => {
                // Clipboard injection is handled by the cross-
                // platform `ClipboardEmulation` sink.
            }
        }
        // FIXME
        Ok(())
    }

    async fn create(&mut self, _handle: EmulationHandle) {}

    async fn destroy(&mut self, handle: EmulationHandle) {
        if let Some(remote) = &self.remote {
            remote.send(mousehop_ipc::desktop::Request::Destroy(handle));
        }
        self.kill_repeat_task();
    }

    async fn terminate(&mut self) {
        self.kill_repeat_task();
    }

    fn display_bounds(&mut self) -> Option<(u32, u32)> {
        if let Some(remote) = &self.remote {
            return remote.bounds();
        }
        // Virtual-screen metrics cover the union of every monitor
        // attached to the system, matching the host-side capture
        // model that uses the union of all displays. Also the sole
        // refresh point of `virtual_screen_origin` — see the field
        // docs.
        let (w, h) = unsafe {
            (
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        if w <= 0 || h <= 0 {
            return None;
        }
        self.virtual_screen_origin
            .set(Some(virtual_screen_origin()));
        Some((w as u32, h as u32))
    }

    fn supports_edge_warp(&self) -> bool {
        true
    }

    async fn warp_cursor(&mut self, x: i32, y: i32) -> Result<(), EmulationError> {
        if let Some(remote) = &self.remote {
            let _ = remote
                .complete(|id| mousehop_ipc::desktop::Request::Warp { id, x, y })
                .await;
            return Ok(());
        }
        // Cached at the last display_bounds() poll; the live query
        // is only a fallback for a warp that somehow arrives before
        // the proxy's creation-time bounds read.
        let origin = self
            .virtual_screen_origin
            .get()
            .unwrap_or_else(virtual_screen_origin);
        let (screen_x, screen_y) = union_to_screen(origin, x, y);
        let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
        if width > 0 && height > 0 {
            send_mouse_input(MOUSEINPUT {
                dx: absolute_coordinate(screen_x, origin.0, width),
                dy: absolute_coordinate(screen_y, origin.1, height),
                dwFlags: MOUSEEVENTF_MOVE
                    | windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_ABSOLUTE
                    | windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_VIRTUALDESK,
                dwExtraInfo: input_event::WINDOWS_INPUT_EVENT_TAG,
                ..Default::default()
            });
        }
        Ok(())
    }
}

/// Top-left corner of the virtual screen in absolute screen
/// coordinates. Zero on the common layout, but negative on any setup
/// with a display left of or above the primary — the primary always
/// starts at (0, 0), so the union extends into negative space.
fn virtual_screen_origin() -> (i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    }
}

/// Convert a union-relative warp target into absolute screen
/// coordinates.
///
/// Warp targets arrive union-relative — the `ProtoEvent::CursorPos`
/// handler scales the peer's normalized fraction against
/// `display_bounds()`, which reports only the *size* of the display
/// union — while `SetCursorPos` consumes absolute screen coordinates.
/// Reapplying the origin is a no-op whenever the primary display is
/// the top-left one, and is the difference between landing on the
/// intended monitor and the wrong one when it isn't.
fn union_to_screen(origin: (i32, i32), x: i32, y: i32) -> (i32, i32) {
    (origin.0.saturating_add(x), origin.1.saturating_add(y))
}

impl WindowsEmulation {
    async fn spawn_repeat_task(&mut self, key: u32) {
        // there can only be one repeating key and it's
        // always the last to be pressed
        self.kill_repeat_task();
        // Use the host's own keyboard repeat settings so forwarded keys
        // feel identical to typing directly on this machine.
        let (repeat_delay, repeat_interval) = read_key_repeat_settings();
        let repeat_task = tokio::task::spawn_local(async move {
            tokio::time::sleep(repeat_delay).await;
            loop {
                if !key_event(key, 1) {
                    break;
                }
                tokio::time::sleep(repeat_interval).await;
            }
        });
        self.repeat_task = Some(repeat_task.abort_handle());
    }
    fn kill_repeat_task(&mut self) {
        if let Some(task) = self.repeat_task.take() {
            task.abort();
        }
    }
}

thread_local! {
    static INPUT_BLOCKED: Cell<bool> = const { Cell::new(false) };
}

fn submit_input_once(submit: impl FnOnce() -> u32) -> bool {
    let submitted = submit() == 1;
    INPUT_BLOCKED.with(|blocked| {
        let was_blocked = blocked.replace(!submitted);
        if !submitted && !was_blocked {
            log::warn!(
                "SendInput rejected an event; skipping it to keep heartbeat and desktop shutdown responsive"
            );
        } else if submitted && was_blocked {
            log::info!("SendInput recovered; input injection resumed");
        }
    });
    submitted
}

fn send_input_safe(input: INPUT) -> bool {
    // Desktop switches can reject injection indefinitely. Never retry on the
    // daemon's runtime thread: it also services DTLS heartbeats and WorkerStop.
    submit_input_once(|| unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) })
}

fn send_mouse_input(mi: MOUSEINPUT) {
    send_input_safe(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    });
}

fn send_keyboard_input(ki: KEYBDINPUT) -> bool {
    send_input_safe(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    })
}
fn rel_mouse(dx: i32, dy: i32) {
    let mi = MOUSEINPUT {
        dx,
        dy,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE,
        time: 0,
        dwExtraInfo: input_event::WINDOWS_INPUT_EVENT_TAG,
    };
    send_mouse_input(mi);
}

fn mouse_button(button: u32, state: u32) {
    let dw_flags = match state {
        0 => match button {
            BTN_LEFT => MOUSEEVENTF_LEFTUP,
            BTN_RIGHT => MOUSEEVENTF_RIGHTUP,
            BTN_MIDDLE => MOUSEEVENTF_MIDDLEUP,
            BTN_BACK => MOUSEEVENTF_XUP,
            BTN_FORWARD => MOUSEEVENTF_XUP,
            _ => return,
        },
        1 => match button {
            BTN_LEFT => MOUSEEVENTF_LEFTDOWN,
            BTN_RIGHT => MOUSEEVENTF_RIGHTDOWN,
            BTN_MIDDLE => MOUSEEVENTF_MIDDLEDOWN,
            BTN_BACK => MOUSEEVENTF_XDOWN,
            BTN_FORWARD => MOUSEEVENTF_XDOWN,
            _ => return,
        },
        _ => return,
    };
    let mouse_data = match button {
        BTN_BACK => XBUTTON1 as u32,
        BTN_FORWARD => XBUTTON2 as u32,
        _ => 0,
    };
    let mi = MOUSEINPUT {
        dx: 0,
        dy: 0, // no movement
        mouseData: mouse_data,
        dwFlags: dw_flags,
        time: 0,
        dwExtraInfo: input_event::WINDOWS_INPUT_EVENT_TAG,
    };
    send_mouse_input(mi);
}

fn scroll(axis: u8, value: i32) {
    let event_type = match axis {
        0 => MOUSEEVENTF_WHEEL,
        1 => MOUSEEVENTF_HWHEEL,
        _ => return,
    };
    let mi = MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: -value as u32,
        dwFlags: event_type,
        time: 0,
        dwExtraInfo: input_event::WINDOWS_INPUT_EVENT_TAG,
    };
    send_mouse_input(mi);
}

fn is_modifier_key(key: u32) -> bool {
    matches!(
        scancode::Linux::try_from(key),
        Ok(scancode::Linux::KeyLeftShift
            | scancode::Linux::KeyRightShift
            | scancode::Linux::KeyLeftCtrl
            | scancode::Linux::KeyRightCtrl
            | scancode::Linux::KeyLeftAlt
            | scancode::Linux::KeyRightalt
            | scancode::Linux::KeyLeftMeta
            | scancode::Linux::KeyRightmeta)
    )
}

fn key_event(key: u32, state: u8) -> bool {
    let scancode = match linux_keycode_to_windows_scancode(key) {
        Some(code) => code,
        None => return false,
    };
    let extended = scancode > 0xff;
    let scancode = scancode & 0xff;
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags.bitor_assign(KEYEVENTF_EXTENDEDKEY);
    }
    if state == 0 {
        flags.bitor_assign(KEYEVENTF_KEYUP);
    }
    let ki = KEYBDINPUT {
        wVk: Default::default(),
        wScan: scancode,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: input_event::WINDOWS_INPUT_EVENT_TAG,
    };
    send_keyboard_input(ki)
}

fn linux_keycode_to_windows_scancode(linux_keycode: u32) -> Option<u16> {
    let linux_scancode = match scancode::Linux::try_from(linux_keycode) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("unknown keycode: {linux_keycode}");
            return None;
        }
    };
    log::trace!("linux code: {linux_scancode:?}");
    let windows_scancode = match scancode::Windows::try_from(linux_scancode) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("failed to translate linux code into windows scancode: {linux_scancode:?}");
            return None;
        }
    };
    log::trace!("windows code: {windows_scancode:?}");
    Some(windows_scancode as u16)
}

// Windows maps the 16-bit absolute coordinate to pixel cells, not endpoints.
fn absolute_coordinate(value: i32, origin: i32, extent: i32) -> i32 {
    let pixel = (i64::from(value) - i64::from(origin)).clamp(0, i64::from(extent) - 1);
    ((pixel * 65536 + 32768) / i64::from(extent)).clamp(0, 65535) as i32
}

#[cfg(test)]
mod tests {
    use super::{INPUT_BLOCKED, absolute_coordinate, submit_input_once, union_to_screen};
    use std::cell::Cell;

    #[test]
    fn absolute_warp_targets_pixel_centers_including_negative_origin() {
        for extent in [1080, 1920, 2560, 3840] {
            for pixel in 0..extent {
                let normalized = absolute_coordinate(pixel - 1920, -1920, extent);
                assert_eq!(
                    i64::from(normalized) * i64::from(extent) / 65536,
                    i64::from(pixel)
                );
            }
        }
    }

    #[test]
    fn rejected_input_returns_without_retry_and_later_input_recovers() {
        INPUT_BLOCKED.with(|blocked| blocked.set(false));
        let mut calls = 0;
        assert!(!submit_input_once(|| {
            calls += 1;
            0
        }));
        assert_eq!(calls, 1);
        assert!(INPUT_BLOCKED.with(Cell::get));
        assert!(submit_input_once(|| 1));
        assert!(!INPUT_BLOCKED.with(Cell::get));
    }

    #[test]
    fn union_to_screen_is_identity_when_the_primary_is_top_left() {
        assert_eq!(union_to_screen((0, 0), 0, 0), (0, 0));
        assert_eq!(union_to_screen((0, 0), 1919, 1079), (1919, 1079));
    }

    #[test]
    fn union_to_screen_reapplies_a_negative_origin() {
        // A 1920x1200 display left of and slightly above a 1920x1080
        // primary: the union's top-left sits at (-1920, -120), so a
        // union-relative left-edge entry belongs there, not at x = 0.
        let origin = (-1920, -120);
        assert_eq!(union_to_screen(origin, 0, 0), (-1920, -120));
        // The primary's own top-left, reached from union coordinates.
        assert_eq!(union_to_screen(origin, 1920, 120), (0, 0));
        // The far corner of the union: the primary's bottom-right.
        assert_eq!(union_to_screen(origin, 3839, 1199), (1919, 1079));
    }

    #[test]
    fn union_to_screen_saturates_rather_than_overflowing() {
        assert_eq!(
            union_to_screen((i32::MAX, i32::MIN), 1, -1),
            (i32::MAX, i32::MIN)
        );
    }
}

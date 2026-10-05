use std::{
    fmt::{self, Display},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};

pub mod display;
pub mod error;
pub mod scancode;

#[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
mod libei;

// FIXME
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_BACK: u32 = 0x113;
pub const BTN_FORWARD: u32 = 0x114;

/// `CGEventField::EVENT_SOURCE_USER_DATA` marker used by Mousehop's
/// macOS keep-awake pulse. The capture event tap lets tagged events
/// reach WindowServer so they reset screen-saver idle, but never
/// forwards them to a peer. The hexadecimal bytes spell `MOUSEHOP`.
pub const MACOS_KEEP_AWAKE_EVENT_TAG: i64 = 0x4d4f_5553_4548_4f50;

/// Identifies Mousehop's own Windows SendInput events in low-level hooks.
pub const WINDOWS_INPUT_EVENT_TAG: usize = 0x4d48_4f50;

/// Modifier family that may gate an outgoing edge crossing. Either physical
/// side satisfies the requirement; platform UIs label these as Control,
/// Alt/Option, Shift, and Super/Command/Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CrossingModifier {
    #[default]
    Control,
    Alt,
    Shift,
    Super,
}

#[derive(Debug, PartialEq, Clone, Copy, Serialize, Deserialize)]
pub enum PointerEvent {
    /// relative motion event
    Motion { time: u32, dx: f64, dy: f64 },
    /// mouse button event
    Button { time: u32, button: u32, state: u32 },
    /// axis event, scroll event for touchpads.
    ///
    /// `momentum` is `true` for the source OS's synthesised momentum-coast
    /// deltas (macOS keeps emitting these after the finger lifts). A sink that
    /// doesn't replay OS momentum for injected scroll (everything but a macOS
    /// sink) drops them, so a forwarded macOS coast doesn't pin a cohort app's
    /// gap-inference kinetic scroll. Always `false` off macOS sources.
    Axis {
        time: u32,
        axis: u8,
        value: f64,
        momentum: bool,
    },
    /// discrete axis event, scroll event for mice - 120 = one scroll tick
    AxisDiscrete120 { axis: u8, value: i32 },
}

#[derive(Debug, PartialEq, Clone, Copy, Serialize, Deserialize)]
pub enum KeyboardEvent {
    /// a key press / release event
    Key { time: u32, key: u32, state: u8 },
    /// modifiers changed state
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
}

#[derive(Debug, PartialEq, Clone)]
pub enum ClipboardEvent {
    /// text content from clipboard
    Text(String),
    /// Clipboard bitmap in row-major, un-premultiplied RGBA8 form. The bulk
    /// protocol uses PNG on the wire and decodes into this platform-neutral
    /// representation only after validating its dimensions.
    ImageRgba {
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    },
    /// Platform clipboard file list. Local capture uses `owned_root = None`;
    /// received file trees set it to the private staging directory so the
    /// clipboard sink can retain and later reclaim the downloaded files.
    Files {
        paths: Vec<PathBuf>,
        owned_root: Option<ClipboardFileLease>,
    },
}

/// Shared lifetime token for a received clipboard file tree. The staging
/// directory is reclaimed only after the clipboard sink, forwarding tasks,
/// and all event clones have released it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardFileLease(Arc<ClipboardFileLeaseInner>);

#[derive(Debug, PartialEq, Eq)]
struct ClipboardFileLeaseInner(PathBuf);

impl ClipboardFileLease {
    pub fn new(path: PathBuf) -> Self {
        Self(Arc::new(ClipboardFileLeaseInner(path)))
    }

    pub fn path(&self) -> &Path {
        &self.0.0
    }
}

impl Drop for ClipboardFileLeaseInner {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::debug!(
                    "failed to remove clipboard staging directory {:?}: {error}",
                    self.0
                );
            }
        }
    }
}

#[derive(PartialEq, Debug, Clone)]
pub enum Event {
    /// pointer event (motion / button / axis)
    Pointer(PointerEvent),
    /// keyboard events (key / modifiers)
    Keyboard(KeyboardEvent),
    /// clipboard events (cross-peer clipboard sync)
    Clipboard(ClipboardEvent),
}

impl Display for PointerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PointerEvent::Motion { time: _, dx, dy } => write!(f, "motion({dx},{dy})"),
            PointerEvent::Button {
                time: _,
                button,
                state,
            } => {
                let str = match *button {
                    BTN_LEFT => Some("left"),
                    BTN_RIGHT => Some("right"),
                    BTN_MIDDLE => Some("middle"),
                    BTN_FORWARD => Some("forward"),
                    BTN_BACK => Some("back"),
                    _ => None,
                };
                if let Some(button) = str {
                    write!(f, "button({button}, {state})")
                } else {
                    write!(f, "button({button}, {state}")
                }
            }
            PointerEvent::Axis {
                axis,
                value,
                momentum,
                ..
            } => write!(
                f,
                "scroll({axis}, {value}{})",
                if *momentum { ", momentum" } else { "" }
            ),
            PointerEvent::AxisDiscrete120 { axis, value } => {
                write!(f, "scroll-120 ({axis}, {value})")
            }
        }
    }
}

impl Display for KeyboardEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyboardEvent::Key {
                time: _,
                key,
                state,
            } => {
                let scan = scancode::Linux::try_from(*key);
                if let Ok(scan) = scan {
                    write!(f, "key({scan:?}, {state})")
                } else {
                    write!(f, "key({key}, {state})")
                }
            }
            KeyboardEvent::Modifiers {
                depressed: mods_depressed,
                latched: mods_latched,
                locked: mods_locked,
                group,
            } => write!(
                f,
                "modifiers({mods_depressed},{mods_latched},{mods_locked},{group})"
            ),
        }
    }
}

impl Display for ClipboardEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClipboardEvent::Text(text) => {
                write!(f, "clipboard(text: {}b)", text.len())
            }
            ClipboardEvent::ImageRgba {
                width,
                height,
                bytes,
            } => write!(f, "clipboard(image: {width}x{height}, {}b)", bytes.len()),
            ClipboardEvent::Files { paths, .. } => {
                write!(f, "clipboard(files: {} item(s))", paths.len())
            }
        }
    }
}

impl Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Pointer(p) => write!(f, "{p}"),
            Event::Keyboard(k) => write!(f, "{k}"),
            Event::Clipboard(c) => write!(f, "{c}"),
        }
    }
}

//! User-session interception of file-manager paste commands. Clipboard markers
//! enable the native Paste command but are deliberately unreadable: they must
//! never be mistaken for downloaded files if interception is unavailable.
use std::{
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

pub const MARKER_TYPE: &str = "org.mousehop.pending-file-paste.v1";

#[derive(Clone)]
pub(crate) struct Armed {
    id: [u8; 16],
    version: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct PasteIntent {
    pub id: [u8; 16],
    pub destination: PathBuf,
    pub error: Option<String>,
}

#[derive(Clone, Default)]
pub(crate) struct Context {
    pub updated: Option<Instant>,
    pub window: usize,
    pub folder: Option<PathBuf>,
    pub editing: bool,
    pub paste_rect: Option<(f64, f64, f64, f64)>,
    pub menu_paste_rects: Vec<(f64, f64, f64, f64)>,
    pub paste_focused: bool,
    pub hovered_folder: Option<PathBuf>,
    pub pointer: Option<(f64, f64)>,
    pub hover_uncertain: bool,
}

impl Context {
    pub fn fresh(&self) -> bool {
        self.updated
            .is_some_and(|time| time.elapsed() < Duration::from_millis(250))
            && self.window != 0
    }
    pub fn paste_at(&self, x: f64, y: f64) -> bool {
        self.fresh()
            && self
                .paste_rect
                .iter()
                .chain(self.menu_paste_rects.iter())
                .any(|&(left, top, width, height)| {
                    x >= left && y >= top && x < left + width && y < top + height
                })
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Confirmation {
    Shortcut,
    ContextShortcut,
    MenuClick(f64, f64),
    MenuKey,
}

#[cfg(test)]
fn cancel_native_menu() -> bool {
    true
}
#[cfg(not(test))]
fn cancel_native_menu() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::cancel_menu()
    }
    #[cfg(windows)]
    {
        windows::cancel_menu()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

pub(crate) struct Shared {
    armed: Mutex<Option<Armed>>,
    context: Mutex<Context>,
    stop: AtomicBool,
    forwarding: AtomicBool,
    intents: std::sync::mpsc::Sender<PasteIntent>,
    requests: Mutex<Vec<(PasteIntent, Context, Confirmation)>>,
}

impl Shared {
    pub fn accepts_input(&self, injected_by_mousehop: bool) -> bool {
        !self.forwarding.load(Ordering::Relaxed) || injected_by_mousehop
    }

    pub fn pending(&self) -> bool {
        self.armed.lock().unwrap().is_some()
    }
    pub fn has_work(&self) -> bool {
        self.pending() || !self.requests.lock().unwrap().is_empty()
    }
    pub fn confirm(&self, current: &Context) {
        for (mut intent, expected, confirmation) in
            std::mem::take(&mut *self.requests.lock().unwrap())
        {
            if current.window != expected.window
                || current.folder != expected.folder
                || !current.fresh()
                || intent.destination.as_os_str().is_empty()
            {
                intent.error = Some("The paste destination could not be confirmed. Open the destination folder and paste again.".into());
            }
            let menu_visible = current.paste_rect.is_some()
                || !current.menu_paste_rects.is_empty()
                || current.paste_focused;
            let menu_valid = match confirmation {
                Confirmation::Shortcut => !current.editing,
                Confirmation::ContextShortcut => menu_visible,
                Confirmation::MenuClick(x, y) => current.paste_at(x, y),
                Confirmation::MenuKey => current.paste_focused,
            };
            if !menu_valid {
                intent.error = Some(
                    "The Paste command is no longer active. Paste again in the destination folder."
                        .into(),
                );
            }
            if intent.error.is_none() && menu_visible && !cancel_native_menu() {
                intent.error = Some("The native Paste menu could not be closed safely.".into());
            }
            let _ = self.intents.send(intent);
        }
    }
    pub fn trigger(
        &self,
        context: &Context,
        destination: Option<PathBuf>,
        confirmation: Confirmation,
    ) -> bool {
        if !context.fresh() {
            return false;
        }
        let mut armed = self.armed.lock().unwrap();
        let Some(copy) = armed.as_ref() else {
            return false;
        };
        if copy.version != crate::clipboard::native_clipboard_version() {
            return false;
        }
        let destination = destination
            .or_else(|| context.folder.clone())
            .unwrap_or_default();
        let id = copy.id;
        self.requests.lock().unwrap().push((
            PasteIntent {
                id,
                destination,
                error: None,
            },
            context.clone(),
            confirmation,
        ));
        armed.take();
        true
    }
}

pub struct NativePasteBridge {
    shared: Arc<Shared>,
    intents: std::sync::mpsc::Receiver<PasteIntent>,
    marker: Option<(tempfile::TempDir, arboard::Clipboard, [u8; 16])>,
    last_own_version: Option<u64>,
}

impl NativePasteBridge {
    pub fn new() -> io::Result<Self> {
        let (tx, intents) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            armed: Mutex::new(None),
            context: Mutex::new(Context::default()),
            stop: AtomicBool::new(false),
            forwarding: AtomicBool::new(false),
            intents: tx,
            requests: Mutex::new(Vec::new()),
        });
        #[cfg(target_os = "macos")]
        if let Err(error) = macos::start(shared.clone()) {
            shared.stop.store(true, Ordering::Relaxed);
            return Err(error);
        }
        #[cfg(windows)]
        if let Err(error) = windows::start(shared.clone()) {
            shared.stop.store(true, Ordering::Relaxed);
            return Err(error);
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Native file paste is supported in Finder and Explorer",
        ));
        #[cfg(any(windows, target_os = "macos"))]
        Ok(Self {
            shared,
            intents,
            marker: None,
            last_own_version: None,
        })
    }

    pub fn arm(&mut self, id: [u8; 16], expected_version: Option<u64>) -> io::Result<Option<u64>> {
        let current = crate::clipboard::native_clipboard_version();
        if current != expected_version && current != self.last_own_version {
            return Err(io::Error::other(
                "Clipboard changed before native paste could be prepared",
            ));
        }
        let root = tempfile::Builder::new()
            .prefix(".mousehop-native-paste-")
            .tempdir()?;
        let path = root
            .path()
            .join(format!("Mousehop-remote-{}", encode_id(id)));
        std::fs::write(
            &path,
            b"Mousehop pending remote files. This is not a transferred file.\n",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))?;
        }
        #[cfg(windows)]
        windows::protect_marker(&path)?;
        let mut clipboard = arboard::Clipboard::new().map_err(io::Error::other)?;
        clipboard
            .set()
            .file_list(&[path])
            .map_err(io::Error::other)?;
        if let Err(error) = publish_id(id) {
            let _ = clipboard.clear();
            return Err(error);
        }
        let version = crate::clipboard::native_clipboard_version();
        if marker_id() != Some(id) || crate::clipboard::native_clipboard_version() != version {
            return Err(io::Error::other(
                "Clipboard changed during native paste preparation",
            ));
        }
        self.last_own_version = version;
        self.marker = Some((root, clipboard, id));
        *self.shared.armed.lock().unwrap() = Some(Armed { id, version });
        Ok(version)
    }

    /// Physical input captured for a peer must not trigger a local paste.
    /// Tagged input emulated on this receiving machine is still eligible.
    pub fn set_forwarding(&self, forwarding: bool) {
        self.shared.forwarding.store(forwarding, Ordering::Relaxed);
    }

    pub fn next_intent(&self) -> Option<PasteIntent> {
        self.intents.try_recv().ok()
    }

    pub fn disarm(&mut self) {
        self.shared.armed.lock().unwrap().take();
        if let Some((_, mut clipboard, id)) = self.marker.take() {
            if marker_id() == Some(id)
                && crate::clipboard::native_clipboard_version() == self.last_own_version
            {
                let _ = clipboard.clear();
                self.last_own_version = crate::clipboard::native_clipboard_version();
            }
        }
    }
}

impl Drop for NativePasteBridge {
    fn drop(&mut self) {
        self.disarm();
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

pub fn marker_id() -> Option<[u8; 16]> {
    #[cfg(target_os = "macos")]
    {
        macos::marker_id()
    }
    #[cfg(windows)]
    {
        windows::marker_id()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

fn publish_id(id: [u8; 16]) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::publish_id(id)
    }
    #[cfg(windows)]
    {
        windows::publish_id(id)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = id;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Native paste unsupported",
        ))
    }
}

pub fn is_marker_path(path: &std::path::Path) -> bool {
    path.parent()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".mousehop-native-paste-"))
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.strip_prefix("Mousehop-remote-")
                    .and_then(decode_id)
                    .is_some()
            })
}

pub(crate) fn encode_id(id: [u8; 16]) -> String {
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}
pub(crate) fn decode_id(value: &str) -> Option<[u8; 16]> {
    if value.len() != 32 || !value.is_ascii() {
        return None;
    }
    let mut id = [0; 16];
    for (index, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(id)
}

pub(crate) fn is_paste_title(title: &str) -> bool {
    let title = title.trim().trim_end_matches('…').replace('&', "");
    let mut title = title.trim();
    if let Some(start) = title.rfind('(') {
        if let Some(shortcut) = title[start + 1..].strip_suffix(')') {
            if shortcut == "Ctrl+V"
                || shortcut.len() == 1 && shortcut.as_bytes()[0].is_ascii_alphabetic()
            {
                title = title[..start].trim_end();
            }
        }
    }
    matches!(
        title,
        "Paste" | "Paste Item" | "Paste Items" | "貼上" | "貼上項目" | "粘贴" | "粘贴项目"
    ) || title.strip_prefix("Paste ").is_some_and(|rest| {
        rest.ends_with(" Items")
            && rest
                .split_whitespace()
                .next()
                .is_some_and(|count| count.parse::<u32>().is_ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> (Shared, std::sync::mpsc::Receiver<PasteIntent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            Shared {
                armed: Mutex::new(Some(Armed {
                    id: [7; 16],
                    version: crate::clipboard::native_clipboard_version(),
                })),
                context: Mutex::new(Context::default()),
                stop: AtomicBool::new(false),
                forwarding: AtomicBool::new(false),
                intents: tx,
                requests: Mutex::new(Vec::new()),
            },
            rx,
        )
    }

    #[test]
    fn input_forwarded_to_a_peer_cannot_paste_into_the_hosts_stale_finder_window() {
        let (shared, _) = shared();
        assert!(shared.accepts_input(false));
        shared.forwarding.store(true, Ordering::Relaxed);
        assert!(!shared.accepts_input(false));
        assert!(
            shared.accepts_input(true),
            "emulated input belongs to this receiving machine"
        );
    }

    #[test]
    fn native_paste_is_one_shot_and_destination_is_confirmed_after_interception() {
        let (shared, rx) = shared();
        let target = tempfile::tempdir().unwrap();
        let context = Context {
            window: 1,
            updated: Some(Instant::now()),
            folder: Some(target.path().into()),
            ..Context::default()
        };
        assert!(shared.trigger(&context, None, Confirmation::Shortcut));
        assert!(!shared.trigger(&context, None, Confirmation::Shortcut));
        assert!(
            rx.try_recv().is_err(),
            "interception alone must not start downloading"
        );
        shared.confirm(&context);
        let intent = rx.try_recv().unwrap();
        assert_eq!(intent.id, [7; 16]);
        assert_eq!(intent.destination, target.path());
        assert!(intent.error.is_none());
    }

    #[test]
    fn changing_the_file_manager_window_rejects_a_captured_paste_destination() {
        let (shared, rx) = shared();
        let target = tempfile::tempdir().unwrap();
        let mut context = Context {
            window: 1,
            updated: Some(Instant::now()),
            folder: Some(target.path().into()),
            ..Context::default()
        };
        assert!(shared.trigger(&context, None, Confirmation::Shortcut));
        context.window = 2;
        shared.confirm(&context);
        assert!(rx.try_recv().unwrap().error.is_some());
    }

    #[test]
    fn a_stale_menu_rectangle_cannot_start_a_transfer_after_the_menu_closes() {
        let (shared, rx) = shared();
        let target = tempfile::tempdir().unwrap();
        let mut context = Context {
            window: 1,
            updated: Some(Instant::now()),
            folder: Some(target.path().into()),
            menu_paste_rects: vec![(10.0, 20.0, 100.0, 30.0)],
            ..Context::default()
        };
        assert!(shared.trigger(&context, None, Confirmation::MenuClick(15.0, 25.0)));
        context.menu_paste_rects.clear();
        shared.confirm(&context);
        assert!(rx.try_recv().unwrap().error.is_some());
    }

    #[test]
    fn a_live_menu_selection_requires_a_matching_enabled_paste_rectangle() {
        let (shared, rx) = shared();
        let target = tempfile::tempdir().unwrap();
        let context = Context {
            window: 1,
            updated: Some(Instant::now()),
            folder: Some(target.path().into()),
            menu_paste_rects: vec![(10.0, 20.0, 100.0, 30.0)],
            ..Context::default()
        };
        assert!(context.paste_at(15.0, 25.0));
        assert!(!context.paste_at(115.0, 25.0));
        assert!(shared.trigger(&context, None, Confirmation::MenuClick(15.0, 25.0)));
        shared.confirm(&context);
        assert!(rx.try_recv().unwrap().error.is_none());
    }

    #[test]
    fn stale_context_and_a_new_clipboard_copy_cannot_start_a_native_paste() {
        let (shared, rx) = shared();
        let mut context = Context {
            window: 1,
            updated: Some(Instant::now() - Duration::from_secs(1)),
            ..Context::default()
        };
        assert!(!shared.trigger(&context, None, Confirmation::Shortcut));
        context.updated = Some(Instant::now());
        shared.armed.lock().unwrap().as_mut().unwrap().version = Some(u64::MAX);
        assert!(!shared.trigger(&context, None, Confirmation::Shortcut));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn native_paste_does_not_match_other_menu_commands() {
        for title in [
            "Paste",
            "&Paste",
            "貼上(P)",
            "Paste (Ctrl+V)",
            "Paste Item",
            "Paste 3 Items",
            "貼上項目",
            "粘贴",
        ] {
            assert!(is_paste_title(title));
        }
        for title in [
            "Copy",
            "Paste Shortcut",
            "Paste and Match Style",
            "貼上並符合樣式",
            "Paste Special…",
        ] {
            assert!(!is_paste_title(title));
        }
    }
    #[test]
    fn marker_ids_and_paths_are_bounded() {
        let id = [17; 16];
        assert_eq!(decode_id(&encode_id(id)), Some(id));
        assert!(decode_id("not-an-offer").is_none());
        assert!(is_marker_path(std::path::Path::new(&format!(
            "/tmp/.mousehop-native-paste-test/Mousehop-remote-{}",
            encode_id(id)
        ))));
        assert!(!is_marker_path(std::path::Path::new(
            "/tmp/user-folder/Mousehop-remote-hello"
        )));
    }
}

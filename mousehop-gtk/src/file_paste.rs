use crate::{
    i18n::{text, tr},
    window::Window,
};
use adw::prelude::*;
use gtk::glib;
use input_capture::native_paste::NativePasteBridge;
use mousehop_ipc::{FilePasteOffer, FilePastePhase, FilePasteProgress, FrontendRequest};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

#[derive(Clone)]
pub(crate) struct FilePastePanel {
    pub widget: gtk::Box,
    label: gtk::Label,
    offer: Rc<RefCell<Option<FilePasteOffer>>>,
    active: Rc<Cell<Option<[u8; 16]>>>,
    armed: Rc<Cell<Option<[u8; 16]>>>,
    native: Rc<RefCell<Option<NativePasteBridge>>>,
    popup: Rc<RefCell<Option<ProgressWindow>>>,
    window: glib::WeakRef<Window>,
    forwarding: Rc<Cell<bool>>,
}

impl FilePastePanel {
    pub fn new(window: &Window) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let title = gtk::Label::new(Some(tr("Remote file paste")));
        title.set_xalign(0.0);
        title.add_css_class("heading");
        widget.append(&title);
        let label = gtk::Label::new(Some(tr(
            "Copy files on another computer, then use Paste in Finder or Explorer.",
        )));
        label.set_xalign(0.0);
        label.set_wrap(true);
        widget.append(&label);
        let panel = Self {
            widget,
            label,
            offer: Rc::default(),
            active: Rc::new(Cell::new(None)),
            armed: Rc::new(Cell::new(None)),
            native: Rc::default(),
            popup: Rc::default(),
            window: window.downgrade(),
            forwarding: Rc::new(Cell::new(false)),
        };
        let poll = panel.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(25), move || {
            let Some(window) = poll.window.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let intent = poll
                .native
                .borrow()
                .as_ref()
                .and_then(NativePasteBridge::next_intent);
            if let Some(intent) = intent {
                if poll
                    .offer
                    .borrow()
                    .as_ref()
                    .is_some_and(|offer| offer.id == intent.id)
                {
                    if let Some(message) = intent.error {
                        window.request(FrontendRequest::NativeFilePasteFailed {
                            id: intent.id,
                            message,
                        });
                    } else {
                        window.request(FrontendRequest::PasteFiles {
                            id: intent.id,
                            destination: intent.destination,
                        });
                    }
                }
            }
            glib::ControlFlow::Continue
        });
        panel
    }

    pub fn set_forwarding(&self, forwarding: bool) {
        self.forwarding.set(forwarding);
        if let Some(native) = self.native.borrow().as_ref() {
            native.set_forwarding(forwarding);
        }
    }

    pub fn available(&self, offer: Option<FilePasteOffer>) {
        if let Some(copy) = &offer {
            self.label.set_text(&text(
                "Ready from {source}: {count} items ({size}). Use Paste in Finder or Explorer.",
                &[
                    ("source", copy.source.clone()),
                    ("count", copy.item_count.to_string()),
                    ("size", glib::format_size(copy.total_bytes).to_string()),
                ],
            ));
        } else if self.active.get().is_none() {
            self.label.set_text(tr(
                "Copy files on another computer, then use Paste in Finder or Explorer.",
            ));
            if let Some(native) = self.native.borrow_mut().as_mut() {
                native.disarm();
            }
            self.armed.set(None);
        }
        self.offer.replace(offer.clone());
        if let Some(copy) = offer {
            if self.armed.get() != Some(copy.id) {
                if let Some(window) = self.window.upgrade() {
                    window.request(FrontendRequest::PrepareNativeFilePaste { id: copy.id });
                }
            }
        }
    }

    pub fn prepare_native(&self, id: [u8; 16], version: Option<u64>) {
        if self
            .offer
            .borrow()
            .as_ref()
            .is_none_or(|offer| offer.id != id)
        {
            return;
        }
        let result: std::io::Result<Option<u64>> = (|| {
            let mut native = self.native.borrow_mut();
            if native.is_none() {
                *native = Some(NativePasteBridge::new()?);
            }
            native
                .as_ref()
                .expect("native bridge")
                .set_forwarding(self.forwarding.get());
            native.as_mut().expect("native bridge").arm(id, version)
        })();
        let (clipboard_version, error) = match result {
            Ok(version) => {
                self.armed.set(Some(id));
                (version, None)
            }
            Err(error) => {
                log::warn!("native file paste preparation failed: {error}");
                (None, Some(error.to_string()))
            }
        };
        if let Some(window) = self.window.upgrade() {
            window.request(FrontendRequest::NativeFilePastePrepared {
                id,
                clipboard_version,
                error,
            });
        }
    }

    pub fn update(&self, update: FilePasteProgress) {
        let running = matches!(
            update.phase,
            FilePastePhase::Receiving | FilePastePhase::Saving
        );
        self.active.set(running.then_some(update.id));
        if !running {
            if let Some(native) = self.native.borrow_mut().as_mut() {
                native.disarm();
            }
            self.armed.set(None);
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let mut popup = self.popup.borrow_mut();
        if popup.is_none() {
            *popup = Some(ProgressWindow::new(&window, self.active.clone()));
        }
        popup.as_ref().expect("progress window").update(update);
    }
}

struct ProgressWindow {
    window: adw::Window,
    label: gtk::Label,
    progress: gtk::ProgressBar,
    action: gtk::Button,
    current: Cell<Option<[u8; 16]>>,
    dismissed: Rc<Cell<bool>>,
    auto_close_generation: Rc<Cell<u64>>,
}
impl ProgressWindow {
    fn new(parent: &Window, active: Rc<Cell<Option<[u8; 16]>>>) -> Self {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
        content.set_margin_top(24);
        content.set_margin_bottom(24);
        content.set_margin_start(24);
        content.set_margin_end(24);
        let label = gtk::Label::new(None);
        label.set_wrap(true);
        label.set_xalign(0.0);
        let progress = gtk::ProgressBar::new();
        progress.set_show_text(true);
        let action = gtk::Button::with_label(tr("Cancel"));
        content.append(&label);
        content.append(&progress);
        content.append(&action);
        // No transient parent: this window must appear on its own while the
        // settings window stays hidden in the tray/menu bar.
        let window = adw::Window::builder()
            .title(tr("Copying files"))
            .default_width(420)
            .resizable(false)
            .content(&content)
            .build();
        window.set_application(parent.application().as_ref());
        #[cfg(target_os = "macos")]
        window.connect_hide(|popup| {
            if popup
                .application()
                .is_some_and(|app| !app.windows().iter().any(|window| window.is_visible()))
            {
                crate::macos_status_item::set_activation_policy(
                    crate::macos_status_item::ACTIVATION_POLICY_ACCESSORY,
                );
            }
        });
        let weak_parent = parent.downgrade();
        let weak_popup = window.downgrade();
        let active_action = active.clone();
        action.connect_clicked(move |_| {
            if let (Some(parent), Some(id)) = (weak_parent.upgrade(), active_action.get()) {
                parent.request(FrontendRequest::CancelFilePaste { id });
            } else if let Some(popup) = weak_popup.upgrade() {
                popup.close();
            }
        });
        let dismissed = Rc::new(Cell::new(false));
        let closing = dismissed.clone();
        let weak_parent = parent.downgrade();
        window.connect_close_request(move |popup| {
            closing.set(true);
            if let (Some(parent), Some(id)) = (weak_parent.upgrade(), active.get()) {
                parent.request(FrontendRequest::CancelFilePaste { id });
            }
            popup.set_visible(false);
            glib::Propagation::Stop
        });
        Self {
            window,
            label,
            progress,
            action,
            current: Cell::new(None),
            dismissed,
            auto_close_generation: Rc::new(Cell::new(0)),
        }
    }
    fn update(&self, update: FilePasteProgress) {
        // Any subsequent update invalidates an older completion timer, including
        // when the same popup is reused for another transfer.
        let generation = self.auto_close_generation.get().wrapping_add(1);
        self.auto_close_generation.set(generation);
        let new_paste = self.current.replace(Some(update.id)) != Some(update.id);
        if new_paste {
            self.dismissed.set(false);
            log::info!(
                "File paste progress window opening: id={:02x?}, phase={:?}",
                update.id,
                update.phase
            );
        }
        let running = matches!(
            update.phase,
            FilePastePhase::Receiving | FilePastePhase::Saving
        );
        self.action
            .set_label(tr(if running { "Cancel" } else { "Close" }));
        let status = tr(match update.phase {
            FilePastePhase::Receiving => "Receiving files…",
            FilePastePhase::Saving => "Saving files…",
            FilePastePhase::Complete => "Files pasted successfully.",
            FilePastePhase::Cancelled => "File paste cancelled. Copy the files again to retry.",
            FilePastePhase::Failed => "File paste failed. Copy the files again to retry.",
        });
        self.label.set_text(
            &update
                .message
                .as_ref()
                .map(|message| format!("{status}\n{message}"))
                .unwrap_or_else(|| status.to_owned()),
        );
        let fraction = if update.phase == FilePastePhase::Complete {
            1.0
        } else if update.total == 0 {
            0.0
        } else {
            (update.received as f64 / update.total as f64).clamp(0.0, 1.0)
        };
        self.progress.set_fraction(fraction);
        self.progress.set_text(Some(&format!(
            "{:.0}% · {} / {}",
            fraction * 100.0,
            glib::format_size(update.received),
            glib::format_size(update.total)
        )));
        if !self.dismissed.get() && (new_paste || !self.window.is_visible()) {
            self.window.present();
            #[cfg(target_os = "macos")]
            crate::macos_status_item::activate_file_paste();
            #[cfg(windows)]
            raise_windows_progress(&self.window);
        }
        if update.phase == FilePastePhase::Complete && !self.dismissed.get() {
            let weak_popup = self.window.downgrade();
            let current = self.auto_close_generation.clone();
            let dismissed = self.dismissed.clone();
            glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
                if current.get() == generation && !dismissed.get() {
                    if let Some(popup) = weak_popup.upgrade() {
                        popup.close();
                    }
                }
            });
        }
    }
}

#[cfg(windows)]
fn raise_windows_progress(window: &adw::Window) {
    use glib::translate::ToGlibPtr;
    use windows::{
        Win32::{
            Foundation::HWND,
            System::LibraryLoader::{GetModuleHandleW, GetProcAddress},
            UI::WindowsAndMessaging::{
                HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
                SWP_SHOWWINDOW, SetForegroundWindow, SetWindowPos,
            },
        },
        core::{s, w},
    };
    let Some(surface) = window.surface() else {
        return;
    };
    // GTK is already loaded. Resolve its backend entry point rather than
    // introducing another GTK DLL or relying on a window title/handle search.
    unsafe {
        let module =
            GetModuleHandleW(w!("gtk-4-1.dll")).or_else(|_| GetModuleHandleW(w!("libgtk-4-1.dll")));
        let handle = module
            .ok()
            .and_then(|module| GetProcAddress(module, s!("gdk_win32_surface_get_handle")));
        let Some(handle) = handle else {
            log::warn!("Cannot resolve the progress window's Win32 handle");
            return;
        };
        let get_handle: unsafe extern "C" fn(*mut gtk::gdk::ffi::GdkSurface) -> HWND =
            std::mem::transmute(handle);
        let hwnd = get_handle(surface.to_glib_none().0);
        if hwnd.0.is_null() {
            return;
        }
        // Windows may refuse a background application's focus request. Briefly
        // raise this user-requested popup anyway; do not leave it always on top.
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW;
        if let Err(error) = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags) {
            log::warn!("Cannot raise the file paste progress window: {error}");
        }
        if !SetForegroundWindow(hwnd).as_bool() {
            log::debug!("Windows kept keyboard focus in the current application");
        }
        let weak = window.downgrade();
        glib::timeout_add_local_once(std::time::Duration::from_secs(1), move || {
            // Retain no native handle after its GTK owner has been destroyed.
            if weak.upgrade().is_some_and(|window| window.is_visible()) {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_NOTOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        });
    }
}

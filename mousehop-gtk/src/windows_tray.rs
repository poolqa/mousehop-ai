//! Windows notification-area ("system tray") icon — keeps the daemon
//! visible and reachable when the preferences window is closed.
//!
//! Mirrors the role of [`crate::linux_tray`] and
//! [`crate::macos_status_item`]: holds a [`gio::ApplicationHoldGuard`]
//! so the GtkApplication survives its last window closing, exposes
//! Open / Quit, and toggles the window on left-click. Before this,
//! Windows was the one platform with neither a tray nor a menu-bar
//! item, so closing the window quit the process and took input sharing
//! with it — the app had to be left on screen to keep working.
//!
//! Threading: on Windows [`crate::run`] drives GTK on a dedicated
//! "gtk" thread, so the hidden window created here belongs to that
//! thread and its [`wndproc`] is invoked by GDK's Win32 message pump.
//! Callbacks are therefore already on the GTK thread — but they arrive
//! *inside* a `DispatchMessage`, and for the context menu inside
//! `TrackPopupMenu`'s own nested modal loop. Re-entering GTK from
//! there invites reentrancy bugs, so commands are pushed through an
//! `async_channel` and applied by a `glib::spawn_future_local` task
//! once the pump unwinds — the same shape `linux_tray` uses to get off
//! ksni's thread.
//!
//! The icon is owned by a hidden *top-level* window rather than the
//! cheaper `HWND_MESSAGE`-parented message-only window, because
//! message-only windows do not receive broadcasts and we need two of
//! them: `TaskbarCreated` (Explorer restarted and rebuilt the
//! notification area — re-add the icon or it is gone for good) and
//! `WM_SETTINGCHANGE` (the user flipped light/dark — re-tint the
//! glyph).

use std::cell::RefCell;
use std::time::{Duration, Instant};

use adw::prelude::*;
use async_channel::Sender;
use gtk::{gio, glib};

use windows::Win32::Foundation::{E_FAIL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, GetCursorPos, GetSystemMetrics, HICON, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW,
    MF_SEPARATOR, MF_STRING, PostMessageW, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON,
    SM_CYSMICON, SetForegroundWindow, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, WM_APP, WM_DESTROY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_SETTINGCHANGE,
    WNDCLASSW, WNDPROC, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};
use windows::core::{PCWSTR, w};

use crate::window::Window;

/// Private message `Shell_NotifyIcon` posts back to us for mouse
/// activity on the icon. Any value in `[WM_APP, 0xBFFF]` is ours to
/// pick.
const WM_MOUSEHOP_TRAY: u32 = WM_APP + 1;

/// Menu command ids. Both non-zero, because `TrackPopupMenu` with
/// `TPM_RETURNCMD` returns 0 for "dismissed without choosing".
const IDM_OPEN: usize = 1;
const IDM_QUIT: usize = 2;

/// Our icon's id within the owning window. There is only ever one, so
/// the value is arbitrary — it just has to match across add / modify /
/// delete.
const TRAY_ICON_ID: u32 = 1;

#[derive(Debug)]
enum TrayCmd {
    TogglePresent,
    Quit,
}

struct TrayState {
    hwnd: HWND,
    icon: HICON,
    tx: Sender<TrayCmd>,
    /// Runtime-allocated id of the `TaskbarCreated` broadcast, which
    /// Explorer sends when it restarts and rebuilds the notification
    /// area. 0 if registration failed, in which case an Explorer crash
    /// costs the user their icon until the next launch.
    taskbar_created: u32,
    /// Which taskbar tint the current [`TrayState::icon`] was
    /// loaded for, so a `WM_SETTINGCHANGE` storm doesn't reload
    /// the icon on every message.
    light_taskbar: bool,
    /// When the icon last toggled the window, for the double-click
    /// debounce in [`toggle_from_click`].
    last_toggle: Option<Instant>,
}

thread_local! {
    /// The tray lives on the GTK thread and every touch point —
    /// `setup`, the wndproc, the command task — runs there, so a
    /// thread-local is enough and none of this needs to be `Send`.
    static TRAY: RefCell<Option<TrayState>> = const { RefCell::new(None) };
}

/// Register the notification-area icon and arm the lifetime hold.
///
/// Returns `None` if the icon could not be created. That distinction
/// matters to the caller: hide-on-close must only be wired up when
/// there is actually a tray to restore the window from, or the close
/// button would strand a running process with no way back to it.
///
/// The returned guard MUST be kept alive for as long as the tray
/// should run — the call site stashes it in a thread-local, mirroring
/// the Linux and macOS code.
pub(crate) fn setup(app: &adw::Application, window: &Window) -> Option<gio::ApplicationHoldGuard> {
    let hold = app.hold();
    let (tx, rx) = async_channel::bounded::<TrayCmd>(8);

    if let Err(e) = install(tx) {
        log::warn!(
            "windows_tray: could not create the notification-area icon ({e}) — \
             closing the window will quit Mousehop"
        );
        return None;
    }
    log::debug!("windows_tray: notification-area icon registered");

    let app = app.clone();
    let window = window.clone();
    glib::spawn_future_local(async move {
        while let Ok(cmd) = rx.recv().await {
            match cmd {
                TrayCmd::TogglePresent => {
                    let visible = window.is_visible();
                    log::info!(
                        "windows_tray: TogglePresent — currently visible={visible}, will {}",
                        if visible { "hide" } else { "present" }
                    );
                    if visible {
                        window.set_visible(false);
                    } else {
                        window.present();
                    }
                }
                TrayCmd::Quit => {
                    log::debug!("windows_tray: quit requested via tray menu");
                    // Take the icon down before the loop stops. The
                    // shell only reaps a dead process's icon lazily —
                    // often not until the user happens to sweep the
                    // mouse over the stale one.
                    shutdown();
                    app.quit();
                }
            }
        }
    });

    Some(hold)
}

/// Create the owning window and put the icon in the notification area.
fn install(tx: Sender<TrayCmd>) -> windows::core::Result<()> {
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        register_class(instance)?;

        // Never shown: without `WS_VISIBLE` the window stays hidden,
        // and `WS_EX_TOOLWINDOW` keeps it out of the taskbar and
        // Alt-Tab even so. It exists only to own the icon and to be a
        // top-level window that broadcasts reach.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            window_class_name(),
            w!("mousehop-tray"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )?;

        let light_taskbar = taskbar_is_light();
        let icon = load_tray_icon(light_taskbar);
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        if taskbar_created == 0 {
            log::warn!(
                "windows_tray: RegisterWindowMessageW(TaskbarCreated) failed — \
                 the icon will not come back if Explorer restarts"
            );
        }

        if !add_icon(hwnd, icon) {
            // `Shell_NotifyIcon` only reports a bare FALSE and does not
            // reliably set last-error, so `from_win32` here would often
            // render as "The operation completed successfully".
            let _ = DestroyWindow(hwnd);
            if !icon.is_invalid() {
                let _ = DestroyIcon(icon);
            }
            return Err(windows::core::Error::from_hresult(E_FAIL));
        }

        // Publish the state only now that the icon exists. `add_icon`
        // SendMessages the taskbar, and while this thread waits inside
        // that call Windows dispatches incoming *sent* messages — a
        // WM_SETTINGCHANGE landing mid-add would find the state and
        // race `refresh_icon_for_theme` against this construction
        // (destroying the very `icon` the failure path above still
        // owns). With the state unpublished, such a message no-ops.
        TRAY.with(|cell| {
            *cell.borrow_mut() = Some(TrayState {
                hwnd,
                icon,
                tx,
                taskbar_created,
                light_taskbar,
                last_toggle: None,
            });
        });
        Ok(())
    }
}

fn window_class_name() -> PCWSTR {
    w!("mousehop-tray-window-class")
}

/// Register the window class, once per process.
///
/// A second `RegisterClassW` for the same name fails with
/// `ERROR_CLASS_ALREADY_EXISTS`, which is harmless and expected if the
/// GUI is ever torn down and rebuilt within one process.
unsafe fn register_class(instance: HINSTANCE) -> windows::core::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    if REGISTERED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(());
    }

    let wndproc: WNDPROC = Some(wndproc);
    let class = WNDCLASSW {
        lpfnWndProc: wndproc,
        hInstance: instance,
        lpszClassName: window_class_name(),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        REGISTERED.store(false, Ordering::SeqCst);
        return Err(windows::core::Error::from_win32());
    }
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // `TaskbarCreated`'s id is only known at runtime, so it cannot be
    // a match arm pattern — read it out (releasing the borrow before
    // any handler runs, since handlers borrow `TRAY` themselves).
    let taskbar_created = TRAY.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|tray| tray.taskbar_created)
            .filter(|&id| id != 0)
    });

    match msg {
        WM_MOUSEHOP_TRAY => {
            // Classic (pre-version-4) callback protocol, which is what
            // you get without an explicit `NIM_SETVERSION`: wParam is
            // the icon id, and lParam's low word is the mouse message.
            match lparam.0 as u32 & 0xffff {
                WM_LBUTTONUP => toggle_from_click(),
                WM_RBUTTONUP => unsafe { show_menu(hwnd) },
                _ => {}
            }
            return LRESULT(0);
        }
        WM_SETTINGCHANGE => {
            refresh_icon_for_theme();
            // Broadcast — let DefWindowProc have it too.
        }
        WM_DESTROY => {
            // Covers both the explicit `shutdown` path and any other
            // route to window destruction.
            unsafe { remove_icon(hwnd) };
            return LRESULT(0);
        }
        _ if Some(msg) == taskbar_created => {
            readd_icon();
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Handle a left-click on the icon, debounced against the system
/// double-click interval.
///
/// Double-clicking a tray icon is a common Windows habit, and it
/// arrives here as two `WM_LBUTTONUP`s — toggling on both would show
/// the window and hide it again in the same gesture, so the second is
/// swallowed. `linux_tray` debounces for the same reason, there
/// against SNI hosts that emit duplicate `Activate` signals.
fn toggle_from_click() {
    let now = Instant::now();
    let debounced = TRAY.with(|cell| {
        let mut tray = cell.borrow_mut();
        let Some(tray) = tray.as_mut() else {
            return true;
        };
        let window = Duration::from_millis(u64::from(unsafe { GetDoubleClickTime() }));
        if tray
            .last_toggle
            .is_some_and(|prev| now.duration_since(prev) < window)
        {
            return true;
        }
        tray.last_toggle = Some(now);
        false
    });
    if debounced {
        log::debug!("windows_tray: swallowed a duplicate click within the double-click window");
        return;
    }
    send(TrayCmd::TogglePresent);
}

/// Queue a command for the GTK main loop.
fn send(cmd: TrayCmd) {
    TRAY.with(|cell| {
        let Some(tray) = cell.borrow().as_ref().map(|tray| tray.tx.clone()) else {
            return;
        };
        // `try_send`, never `send_blocking`: the receiving task runs on
        // this very thread's main loop, so blocking here would
        // deadlock. Dropping a click when the queue is somehow full is
        // the right trade for a tray.
        if tray.try_send(cmd).is_err() {
            log::debug!("windows_tray: dropped a tray command (channel full or closed)");
        }
    });
}

/// Pop the Open / Quit menu at the cursor.
unsafe fn show_menu(hwnd: HWND) {
    let mut point = POINT::default();
    if let Err(e) = unsafe { GetCursorPos(&mut point) } {
        log::warn!("windows_tray: GetCursorPos failed: {e}");
        return;
    }
    let menu = match unsafe { CreatePopupMenu() } {
        Ok(menu) => menu,
        Err(e) => {
            log::warn!("windows_tray: CreatePopupMenu failed: {e}");
            return;
        }
    };

    unsafe {
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_OPEN,
            windows::core::PCWSTR(
                crate::i18n::tr("Open Mousehop")
                    .encode_utf16()
                    .chain(Some(0))
                    .collect::<Vec<_>>()
                    .as_ptr(),
            ),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_QUIT,
            windows::core::PCWSTR(
                crate::i18n::tr("Quit Mousehop")
                    .encode_utf16()
                    .chain(Some(0))
                    .collect::<Vec<_>>()
                    .as_ptr(),
            ),
        );

        // The documented dance (KB135788): a tray menu only dismisses
        // on an outside click while its owner is the foreground
        // window, and the trailing WM_NULL is what lets it close once
        // focus moves on. Skip either and the menu sticks on screen.
        let _ = SetForegroundWindow(hwnd);
        // With TPM_RETURNCMD the return value is the chosen command id
        // rather than a success flag — 0 means the user dismissed it.
        let choice = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);

        match choice.0 as usize {
            IDM_OPEN => send(TrayCmd::TogglePresent),
            IDM_QUIT => send(TrayCmd::Quit),
            _ => {}
        }
    }
}

/// Fill in the parts of `NOTIFYICONDATAW` every message shares.
fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    }
}

unsafe fn add_icon(hwnd: HWND, icon: HICON) -> bool {
    let mut data = icon_data(hwnd);
    data.uFlags = NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_MOUSEHOP_TRAY;
    // A failed resource load leaves a null HICON; claiming NIF_ICON
    // with a null handle risks the whole NIM_ADD being rejected,
    // while adding without it still claims a (blank) clickable slot —
    // which beats having no way back to a hidden window.
    if !icon.is_invalid() {
        data.uFlags |= NIF_ICON;
        data.hIcon = icon;
    }
    write_tip(&mut data.szTip, "Mousehop");
    unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool()
}

unsafe fn modify_icon(hwnd: HWND, icon: HICON) -> bool {
    let mut data = icon_data(hwnd);
    data.uFlags = NIF_ICON;
    data.hIcon = icon;
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool()
}

unsafe fn remove_icon(hwnd: HWND) {
    use windows::Win32::UI::Shell::NIM_DELETE;
    let data = icon_data(hwnd);
    if !unsafe { Shell_NotifyIconW(NIM_DELETE, &data) }.as_bool() {
        log::debug!("windows_tray: NIM_DELETE reported failure (icon likely already gone)");
    }
}

/// Copy a tooltip into `szTip`'s fixed UTF-16 buffer, truncating rather
/// than overflowing. The buffer arrives zeroed, and stopping one short
/// of the end guarantees the null terminator survives.
fn write_tip(dst: &mut [u16; 128], text: &str) {
    let limit = dst.len() - 1;
    for (slot, unit) in dst.iter_mut().zip(text.encode_utf16().take(limit)) {
        *slot = unit;
    }
}

/// Explorer restarted: the notification area it rebuilt has no memory
/// of our icon, so add it again.
fn readd_icon() {
    let Some((hwnd, icon)) =
        TRAY.with(|cell| cell.borrow().as_ref().map(|tray| (tray.hwnd, tray.icon)))
    else {
        return;
    };
    if unsafe { add_icon(hwnd, icon) } {
        log::info!("windows_tray: notification area was rebuilt — icon re-added");
    } else {
        log::warn!("windows_tray: failed to re-add the icon after an Explorer restart");
    }
}

/// Re-tint the glyph when the user flips between light and dark.
///
/// Decide, load, and swap under a single borrow, *before* the
/// `Shell_NotifyIcon` call. A theme switch broadcasts several
/// `WM_SETTINGCHANGE`s, and while `modify_icon`'s SendMessage to the
/// taskbar blocks, Windows dispatches the next one of the burst
/// straight into this function re-entrantly — a swap *after* the
/// modify would let both calls claim the same stale icon and destroy
/// it twice. Swapping first means the re-entrant call sees the
/// updated tint and bails, and each icon handle has exactly one
/// owner at all times. (Loading these resources does not pump messages;
/// only SendMessage-style calls do.)
fn refresh_icon_for_theme() {
    let light_taskbar = taskbar_is_light();
    let Some((hwnd, icon, stale_icon)) = TRAY.with(|cell| {
        let mut tray = cell.borrow_mut();
        let tray = tray.as_mut()?;
        // WM_SETTINGCHANGE fires for far more than the colour scheme;
        // only reload when the taskbar actually flipped.
        if tray.light_taskbar == light_taskbar {
            return None;
        }
        let icon = load_tray_icon(light_taskbar);
        if icon.is_invalid() {
            // Keep the old (wrongly tinted, but visible) glyph; the
            // stale flag makes the next WM_SETTINGCHANGE retry.
            log::warn!("windows_tray: reload for theme change failed — keeping the old glyph");
            return None;
        }
        let stale_icon = std::mem::replace(&mut tray.icon, icon);
        tray.light_taskbar = light_taskbar;
        Some((tray.hwnd, icon, stale_icon))
    }) else {
        return;
    };

    if !unsafe { modify_icon(hwnd, icon) } {
        log::warn!("windows_tray: NIM_MODIFY failed after a theme change");
    }
    // The shell copies the icon on add/modify, so the swapped-out
    // handle is ours to free even if it is still on screen.
    if !stale_icon.is_invalid() {
        let _ = unsafe { DestroyIcon(stale_icon) };
    }
}

/// Take the icon down and release everything it owns.
fn shutdown() {
    // Take the state out *before* destroying the window: DestroyWindow
    // sends WM_DESTROY synchronously, and that handler would otherwise
    // re-enter a live `RefCell` borrow.
    let Some(tray) = TRAY.with(|cell| cell.borrow_mut().take()) else {
        return;
    };
    unsafe {
        // WM_DESTROY is what actually issues the NIM_DELETE.
        let _ = DestroyWindow(tray.hwnd);
        if !tray.icon.is_invalid() {
            let _ = DestroyIcon(tray.icon);
        }
    }
}

/// Whether the taskbar is currently light.
///
/// Windows exposes no API for "what colour is the notification area",
/// so this reads the value every shell app reads. Absent — older
/// builds, some managed profiles — means the shipped default, which is
/// a dark taskbar.
fn taskbar_is_light() -> bool {
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::from_mut(&mut value).cast()),
            Some(&mut size),
        )
    };
    status.0 == 0 && value != 0
}

/// Load a DPI-sized, pre-rendered glyph from the executable's resources.
/// No GdkPixbuf SVG loader or external icon file is needed on Windows.
fn load_tray_icon(light_taskbar: bool) -> HICON {
    // IDs match mousehop/assets/windows/mousehop.rc. LoadImageW without
    // LR_SHARED gives each load an owned handle that DestroyIcon can release.
    let resource_id = if light_taskbar { 2 } else { 3 };
    let width = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
    let height = unsafe { GetSystemMetrics(SM_CYSMICON) }.max(16);
    match load_icon_resource(resource_id, width, height) {
        Ok(icon) => icon,
        Err(e) => {
            log::warn!("windows_tray: load tray icon resource {resource_id}: {e}");
            // The colored app icon remains visible on either taskbar theme.
            load_icon_resource(1, width, height).unwrap_or_else(|e| {
                log::warn!("windows_tray: load fallback app icon: {e}");
                HICON::default()
            })
        }
    }
}

fn load_icon_resource(id: u16, width: i32, height: i32) -> windows::core::Result<HICON> {
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let image = LoadImageW(
            Some(instance),
            PCWSTR(id as usize as *const u16),
            IMAGE_ICON,
            width,
            height,
            LR_DEFAULTCOLOR,
        )?;
        Ok(HICON(image.0))
    }
}

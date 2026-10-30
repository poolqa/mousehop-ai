use super::*;
#[cfg(not(test))]
use ::windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
};
use ::windows::{
    Win32::{
        Foundation::{HANDLE, HINSTANCE, HLOCAL, HWND, LPARAM, LRESULT, LocalFree, POINT, WPARAM},
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
                SE_FILE_OBJECT, SetNamedSecurityInfoW,
            },
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PSECURITY_DESCRIPTOR,
        },
        System::{
            Com::{
                CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
                CoUninitialize, IServiceProvider,
            },
            DataExchange::{
                CloseClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
                SetClipboardData,
            },
            LibraryLoader::GetModuleHandleW,
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
            Variant::VARIANT,
        },
        UI::{
            Accessibility::{
                CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement,
                UIA_ButtonControlTypeId, UIA_DataItemControlTypeId, UIA_DocumentControlTypeId,
                UIA_EditControlTypeId, UIA_ListItemControlTypeId, UIA_MenuItemControlTypeId,
                UIA_TreeItemControlTypeId,
            },
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RWIN,
                VK_SHIFT,
            },
            Shell::{
                IFolderView, IShellBrowser, IShellItem, IShellWindows, IWebBrowser2,
                SID_STopLevelBrowser, SIGDN_FILESYSPATH, ShellWindows,
            },
            WindowsAndMessaging::*,
        },
    },
    core::{Interface, PCWSTR, w},
};
#[cfg(not(test))]
use std::mem::size_of;
use std::{cell::RefCell, path::Path, ptr};

struct ClipboardOpen;
impl Drop for ClipboardOpen {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}
fn open_clipboard() -> io::Result<ClipboardOpen> {
    unsafe {
        OpenClipboard(None)?;
    }
    Ok(ClipboardOpen)
}
fn format() -> u32 {
    unsafe { RegisterClipboardFormatW(PCWSTR(mousehop_ipc::windows::wide(MARKER_TYPE).as_ptr())) }
}

pub(super) fn marker_id() -> Option<[u8; 16]> {
    let _open = open_clipboard().ok()?;
    unsafe {
        let handle = GetClipboardData(format()).ok()?;
        let handle = ::windows::Win32::Foundation::HGLOBAL(handle.0);
        if GlobalSize(handle) < 16 {
            return None;
        }
        let memory = GlobalLock(handle);
        if memory.is_null() {
            return None;
        }
        let mut id = [0; 16];
        ptr::copy_nonoverlapping(memory.cast::<u8>(), id.as_mut_ptr(), 16);
        let _ = GlobalUnlock(handle);
        Some(id)
    }
}
pub(super) fn publish_id(id: [u8; 16]) -> io::Result<()> {
    let _open = open_clipboard()?;
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, 16)?;
        let target = GlobalLock(memory);
        if target.is_null() {
            let _ = ::windows::Win32::Foundation::GlobalFree(Some(memory));
            return Err(io::Error::last_os_error());
        }
        ptr::copy_nonoverlapping(id.as_ptr(), target.cast::<u8>(), 16);
        let _ = GlobalUnlock(memory);
        if let Err(error) = SetClipboardData(format(), Some(HANDLE(memory.0))) {
            let _ = ::windows::Win32::Foundation::GlobalFree(Some(memory));
            return Err(error.into());
        }
    }
    Ok(())
}

pub(super) fn protect_marker(path: &Path) -> io::Result<()> {
    unsafe {
        // Deny file-content reads to its owner while retaining metadata and
        // deletion rights. Explorer may enable Paste but cannot copy this
        // marker as though it were a received file if the hook is unavailable.
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:P(D;;0x1;;;OW)(A;;FA;;;OW)(A;;FA;;;SY)"),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
        let mut present = ::windows::core::BOOL::default();
        let mut defaulted = ::windows::core::BOOL::default();
        let mut dacl = ptr::null_mut();
        let result = GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
            .and_then(|_| {
                SetNamedSecurityInfoW(
                    PCWSTR(mousehop_ipc::windows::wide(&path.to_string_lossy()).as_ptr()),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    None,
                    None,
                    Some(dacl),
                    None,
                )
                .ok()
            });
        LocalFree(Some(HLOCAL(descriptor.0)));
        result?;
    }
    Ok(())
}

fn class(hwnd: HWND) -> String {
    let mut name = [0u16; 128];
    let count = unsafe { GetClassNameW(hwnd, &mut name) };
    String::from_utf16_lossy(&name[..count.max(0) as usize])
}
fn foreground_explorer() -> Option<HWND> {
    let hwnd = unsafe { GetForegroundWindow() };
    if matches!(
        class(hwnd).as_str(),
        "CabinetWClass" | "Progman" | "WorkerW"
    ) {
        return Some(hwnd);
    }
    let owner = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
    matches!(
        class(owner).as_str(),
        "CabinetWClass" | "Progman" | "WorkerW"
    )
    .then_some(owner)
}

fn explorer_folder(hwnd: HWND, shell: &IShellWindows) -> Option<PathBuf> {
    unsafe {
        if matches!(class(hwnd).as_str(), "Progman" | "WorkerW") {
            let path = ::windows::Win32::UI::Shell::SHGetKnownFolderPath(
                &::windows::Win32::UI::Shell::FOLDERID_Desktop,
                ::windows::Win32::UI::Shell::KF_FLAG_DEFAULT,
                None,
            )
            .ok()?;
            let folder = PathBuf::from(path.to_string().ok()?);
            CoTaskMemFree(Some(path.0.cast()));
            return folder.is_dir().then_some(folder);
        }
        if class(hwnd) != "CabinetWClass" {
            return None;
        }
        for index in 0..shell.Count().ok()? {
            let browser: IWebBrowser2 = shell.Item(&VARIANT::from(index)).ok()?.cast().ok()?;
            if browser.HWND().ok()?.0 as usize != hwnd.0 as usize {
                continue;
            }
            // QueryActiveShellView resolves the active Explorer tab rather
            // than choosing the first tab with a matching top-level HWND.
            let provider: IServiceProvider = browser.cast().ok()?;
            let browser: IShellBrowser = provider.QueryService(&SID_STopLevelBrowser).ok()?;
            let view: IFolderView = browser.QueryActiveShellView().ok()?.cast().ok()?;
            let folder: IShellItem = view.GetFolder().ok()?;
            let path = folder.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let folder = PathBuf::from(path.to_string().ok()?);
            CoTaskMemFree(Some(path.0.cast()));
            return folder
                .is_absolute()
                .then_some(folder)
                .filter(|folder| folder.is_dir());
        }
        None
    }
}

fn menu_rect(
    automation: &IUIAutomation,
    original: &IUIAutomationElement,
) -> Option<(f64, f64, f64, f64)> {
    unsafe {
        let walker = automation.ControlViewWalker().ok()?;
        let mut element = original.clone();
        for _ in 0..6 {
            let control = element.CurrentControlType().ok()?;
            if (control == UIA_MenuItemControlTypeId || control == UIA_ButtonControlTypeId)
                && is_paste_title(&element.CurrentName().ok()?.to_string())
            {
                if !element.CurrentIsEnabled().ok()?.as_bool() {
                    return None;
                }
                let rect = element.CurrentBoundingRectangle().ok()?;
                return Some((
                    rect.left as f64,
                    rect.top as f64,
                    (rect.right - rect.left) as f64,
                    (rect.bottom - rect.top) as f64,
                ));
            }
            element = walker.GetParentElement(&element).ok()?;
        }
        None
    }
}
fn menu_paste_rects(automation: &IUIAutomation, hwnd: HWND) -> Vec<(f64, f64, f64, f64)> {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        use ::windows::Win32::UI::Accessibility::{
            PropertyConditionFlags_MatchSubstring, UIA_NamePropertyId,
        };
        let Ok(mut condition) = automation.CreatePropertyConditionEx(
            UIA_NamePropertyId,
            &VARIANT::from("Paste"),
            PropertyConditionFlags_MatchSubstring,
        ) else {
            return Vec::new();
        };
        for name in ["貼上", "粘贴"] {
            if let Ok(other) = automation.CreatePropertyConditionEx(
                UIA_NamePropertyId,
                &VARIANT::from(name),
                PropertyConditionFlags_MatchSubstring,
            ) {
                if let Ok(combined) = automation.CreateOrCondition(&condition, &other) {
                    condition = combined;
                }
            }
        }
        let mut output = Vec::new();
        let mut window = GetTopWindow(None).unwrap_or_default();
        for _ in 0..128 {
            if window.0.is_null() {
                break;
            }
            let name = class(window);
            let mut owner_pid = 0;
            GetWindowThreadProcessId(window, Some(&mut owner_pid));
            if owner_pid == pid
                && IsWindowVisible(window).as_bool()
                && (name == "#32768" || name.contains("Popup") || name.contains("Xaml"))
            {
                if let Ok(element) = automation.ElementFromHandle(window) {
                    if let Ok(items) = element.FindAll(
                        ::windows::Win32::UI::Accessibility::TreeScope_Subtree,
                        &condition,
                    ) {
                        for index in 0..items.Length().unwrap_or(0).min(128) {
                            if let Ok(item) = items.GetElement(index) {
                                if let Some(rect) = menu_rect(automation, &item) {
                                    output.push(rect);
                                }
                            }
                        }
                    }
                }
            }
            window = GetWindow(window, GW_HWNDNEXT).unwrap_or_default();
        }
        output
    }
}

fn context(automation: &IUIAutomation, shell: &IShellWindows) -> Context {
    unsafe {
        let sampled = Instant::now();
        let Some(hwnd) = foreground_explorer() else {
            return Context::default();
        };
        let folder = explorer_folder(hwnd, shell);
        let focused = automation.GetFocusedElement().ok();
        let editing = focused
            .as_ref()
            .and_then(|element| element.CurrentControlType().ok())
            .is_some_and(|kind| kind == UIA_EditControlTypeId || kind == UIA_DocumentControlTypeId);
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        let hit = automation.ElementFromPoint(point).ok();
        let hovered_folder = hit
            .as_ref()
            .and_then(|element| element.CurrentName().ok())
            .map(|name| name.to_string())
            .filter(|name| !name.contains(['/', '\\']) && name != "." && name != "..")
            .and_then(|name| folder.as_ref().map(|folder| folder.join(name)))
            .filter(|path| path.is_dir());
        let hover_uncertain = hovered_folder.is_none()
            && hit
                .as_ref()
                .and_then(|element| element.CurrentControlType().ok())
                .is_some_and(|kind| {
                    kind == UIA_ListItemControlTypeId
                        || kind == UIA_DataItemControlTypeId
                        || kind == UIA_TreeItemControlTypeId
                });
        if foreground_explorer() != Some(hwnd) {
            return Context::default();
        }
        Context {
            updated: Some(sampled),
            window: hwnd.0 as usize,
            folder,
            editing,
            menu_paste_rects: menu_paste_rects(automation, hwnd),
            paste_rect: hit
                .as_ref()
                .and_then(|element| menu_rect(automation, element)),
            paste_focused: focused
                .as_ref()
                .and_then(|element| menu_rect(automation, element))
                .is_some(),
            hovered_folder,
            pointer: Some((point.x as f64, point.y as f64)),
            hover_uncertain,
        }
    }
}

thread_local! {
    static HOOK: RefCell<Option<Arc<Shared>>> = const { RefCell::new(None) };
    static CONSUMED: RefCell<(Option<u32>, bool, Option<PathBuf>)> = const { RefCell::new((None, false, None)) };
}
fn down(key: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(key.0 as i32) < 0 }
}
#[cfg(not(test))]
pub(super) fn cancel_menu() -> bool {
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_ESCAPE,
                    dwExtraInfo: input_event::WINDOWS_INPUT_EVENT_TAG,
                    ..Default::default()
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_ESCAPE,
                    dwFlags: KEYEVENTF_KEYUP,
                    dwExtraInfo: input_event::WINDOWS_INPUT_EVENT_TAG,
                    ..Default::default()
                },
            },
        },
    ];
    unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) == inputs.len() as u32 }
}

unsafe extern "system" fn keyboard(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let key = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let message = wparam.0 as u32;
    let consumed = CONSUMED.with_borrow_mut(|state| {
        if matches!(message, WM_KEYUP | WM_SYSKEYUP) && state.0 == Some(key.vkCode) {
            state.0 = None;
            return true;
        }
        if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) && state.0 == Some(key.vkCode) {
            return true;
        }
        if !matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) {
            return false;
        }
        if key.vkCode == VK_ESCAPE.0 as u32 {
            state.2 = None;
        }
        HOOK.with_borrow(|shared| {
            let Some(shared) = shared else {
                return false;
            };
            if !shared.accepts_input(key.dwExtraInfo == input_event::WINDOWS_INPUT_EVENT_TAG) {
                return false;
            }
            if !shared.pending() {
                return false;
            }
            let context = shared.context.lock().unwrap().clone();
            if context.window
                != foreground_explorer()
                    .map(|window| window.0 as usize)
                    .unwrap_or(0)
            {
                return false;
            }
            let ctrl_v = key.vkCode == 0x56
                && down(VK_CONTROL)
                && !down(VK_SHIFT)
                && !down(VK_MENU)
                && !down(VK_LWIN)
                && !down(VK_RWIN);
            let shift_insert =
                key.vkCode == 0x2D && down(VK_SHIFT) && !down(VK_CONTROL) && !down(VK_MENU);
            let menu_enter = key.vkCode == 0x0D && context.paste_focused;
            let context_shortcut = state.2.is_some() || !context.menu_paste_rects.is_empty();
            let confirmation = if menu_enter {
                Confirmation::MenuKey
            } else if context_shortcut {
                Confirmation::ContextShortcut
            } else {
                Confirmation::Shortcut
            };
            let destination = if menu_enter || context_shortcut {
                state.2.take()
            } else {
                None
            };
            if ((ctrl_v || shift_insert) && !context.editing || menu_enter)
                && shared.trigger(&context, destination, confirmation)
            {
                state.0 = Some(key.vkCode);
                return true;
            }
            false
        })
    });
    if consumed {
        LRESULT(1)
    } else {
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }
}
unsafe extern "system" fn mouse(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let point = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) }.pt;
    let message = wparam.0 as u32;
    let consumed = CONSUMED.with_borrow_mut(|state| {
        if message == WM_LBUTTONUP && state.1 {
            state.1 = false;
            return true;
        }
        HOOK.with_borrow(|shared| {
            let Some(shared) = shared else {
                return false;
            };
            let input = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            if !shared.accepts_input(input.dwExtraInfo == input_event::WINDOWS_INPUT_EVENT_TAG) {
                return false;
            }
            if !shared.pending() {
                return false;
            }
            let context = shared.context.lock().unwrap().clone();
            if context.window
                != foreground_explorer()
                    .map(|window| window.0 as usize)
                    .unwrap_or(0)
            {
                return false;
            }
            if message == WM_RBUTTONDOWN {
                state.2 = if !context.hover_uncertain
                    && context.pointer.is_some_and(|(x, y)| {
                        (x - point.x as f64).abs() <= 2.0 && (y - point.y as f64).abs() <= 2.0
                    }) {
                    context.hovered_folder.clone().or(context.folder.clone())
                } else {
                    Some(PathBuf::new())
                };
            }
            if message == WM_LBUTTONDOWN
                && context.paste_at(point.x as f64, point.y as f64)
                && shared.trigger(
                    &context,
                    state.2.take(),
                    Confirmation::MenuClick(point.x as f64, point.y as f64),
                )
            {
                state.1 = true;
                return true;
            }
            if message == WM_LBUTTONDOWN {
                state.2 = None;
            }
            false
        })
    });
    if consumed {
        LRESULT(1)
    } else {
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }
}

pub(super) fn start(shared: Arc<Shared>) -> io::Result<()> {
    let worker = shared.clone();
    let (ready, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || unsafe {
        let initialized = CoInitializeEx(None, COINIT_MULTITHREADED).ok();
        if let Err(error) = initialized {
            let _ = ready.send(Err(io::Error::other(error)));
            return;
        }
        let setup = (|| -> ::windows::core::Result<(IUIAutomation, IShellWindows)> {
            let automation: IUIAutomation = CoCreateInstance(&CUIAutomation8, None, CLSCTX_ALL)?;
            if let Ok(settings) = automation.cast::<IUIAutomation2>() {
                let _ = settings.SetConnectionTimeout(100);
                let _ = settings.SetTransactionTimeout(100);
            }
            Ok((
                automation,
                CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)?,
            ))
        })();
        let Ok((automation, shell)) = setup else {
            let _ = ready.send(Err(io::Error::other(
                "Cannot start Explorer accessibility monitor",
            )));
            CoUninitialize();
            return;
        };
        let _ = ready.send(Ok(()));
        while !worker.stop.load(Ordering::Relaxed) {
            if worker.has_work() {
                let context = context(&automation, &shell);
                worker.confirm(&context);
                *worker.context.lock().unwrap() = context;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        drop((automation, shell));
        CoUninitialize();
    });
    rx.recv_timeout(Duration::from_secs(3))
        .map_err(io::Error::other)??;
    let (ready, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || unsafe {
        HOOK.with_borrow_mut(|slot| *slot = Some(shared.clone()));
        let result = (|| -> ::windows::core::Result<(HHOOK, HHOOK)> {
            let module: HINSTANCE = GetModuleHandleW(None)?.into();
            let keyboard_hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), Some(module), 0)?;
            match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), Some(module), 0) {
                Ok(mouse_hook) => Ok((keyboard_hook, mouse_hook)),
                Err(error) => {
                    let _ = UnhookWindowsHookEx(keyboard_hook);
                    Err(error)
                }
            }
        })();
        let Ok((keyboard_hook, mouse_hook)) = result else {
            let _ = ready.send(Err(io::Error::other(
                "Cannot install native Explorer paste hooks",
            )));
            return;
        };
        let _ = ready.send(Ok(()));
        let mut message = MSG::default();
        while !shared.stop.load(Ordering::Relaxed) {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            std::thread::sleep(Duration::from_millis(3));
        }
        let _ = UnhookWindowsHookEx(keyboard_hook);
        let _ = UnhookWindowsHookEx(mouse_hook);
        HOOK.with_borrow_mut(|slot| *slot = None);
    });
    rx.recv_timeout(Duration::from_secs(3))
        .map_err(io::Error::other)?
}

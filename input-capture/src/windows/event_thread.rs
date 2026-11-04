use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::io;
use std::ptr::addr_of_mut;

use std::default::Default;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS,
    EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::core::{PCWSTR, w};

use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DispatchMessageW, EDD_GET_DEVICE_INTERFACE_NAME,
    GUI_INMENUMODE, GUI_INMOVESIZE, GUI_POPUPMENUMODE, GUI_SYSTEMMENUMODE, GUITHREADINFO,
    GetGUIThreadInfo, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, KillTimer, LLKHF_EXTENDED, MSG,
    MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, RegisterClassW, SetTimer,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WINDOW_STYLE, WM_DISPLAYCHANGE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WM_USER, WM_WTSSESSION_CHANGE,
    WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WNDPROC, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, CrossingModifier, Event, KeyboardEvent,
    PointerEvent,
    scancode::{self, Linux},
};

use super::{CaptureEvent, Position, display_util};

pub(crate) struct EventThread {
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    thread: Option<thread::JoinHandle<()>>,
    thread_id: u32,
    stopping: Arc<AtomicBool>,
}

impl EventThread {
    pub(crate) fn new(event_tx: Sender<(Position, CaptureEvent)>) -> Self {
        let request_buffer = Default::default();
        let stopping = Arc::new(AtomicBool::new(false));
        let (thread, thread_id) = start(event_tx, Arc::clone(&request_buffer), stopping.clone());
        Self {
            request_buffer,
            thread: Some(thread),
            thread_id,
            stopping,
        }
    }

    pub(crate) async fn release_capture(&self) -> bool {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Err(error) = self.client_update(ClientUpdate::Release(tx)) {
            log::warn!("Cannot release Windows capture: {error}");
            return false;
        }
        matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), rx).await,
            Ok(Ok(()))
        )
    }

    pub(crate) fn create(&self, pos: Position) -> io::Result<()> {
        self.client_update(ClientUpdate::Create(pos))
    }

    pub(crate) fn destroy(&self, pos: Position) -> io::Result<()> {
        self.client_update(ClientUpdate::Destroy(pos))
    }

    pub(crate) fn set_crossing_modifier(
        &self,
        pos: Position,
        modifier: Option<CrossingModifier>,
    ) -> io::Result<()> {
        self.client_update(ClientUpdate::SetCrossingModifier(pos, modifier))
    }

    fn exit(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Err(error) = self.signal(RequestType::Exit) {
            log::debug!("Windows capture thread already stopped or unavailable: {error}");
        }
    }

    fn client_update(&self, request: ClientUpdate) -> io::Result<()> {
        {
            let mut requests = self.request_buffer.lock().unwrap();
            requests.push(request);
        }
        if let Err(error) = self.signal(RequestType::ClientUpdate) {
            // Drop outstanding release senders too; their callers must not
            // wait for acknowledgements from a thread which cannot be reached.
            self.request_buffer.lock().unwrap().clear();
            return Err(error);
        }
        Ok(())
    }

    fn signal(&self, event_type: RequestType) -> io::Result<()> {
        if self
            .thread
            .as_ref()
            .is_none_or(thread::JoinHandle::is_finished)
        {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Windows capture event thread has exited",
            ));
        }
        let id = self.thread_id;
        unsafe { PostThreadMessageW(id, WM_USER, WPARAM(event_type as usize), LPARAM(0)) }
            .map_err(io::Error::from)
    }
}

impl Drop for EventThread {
    fn drop(&mut self) {
        self.exit();
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                log::error!("Windows capture event thread panicked");
            }
        }
    }
}

enum RequestType {
    ClientUpdate = 0,
    Exit = 2,
}

enum ClientUpdate {
    Create(Position),
    Destroy(Position),
    SetCrossingModifier(Position, Option<CrossingModifier>),
    Release(tokio::sync::oneshot::Sender<()>),
}

/// `SetTimer` id for the low-level-hook watchdog.
const HOOK_WATCHDOG_TIMER: usize = 1;
/// How often the watchdog checks that the hooks are still live.
const HOOK_WATCHDOG_INTERVAL_MS: u32 = 3000;

fn try_send_event(
    pos: Position,
    event: CaptureEvent,
) -> Result<(), TrySendError<(Position, CaptureEvent)>> {
    EVENT_TX.with_borrow_mut(|tx| tx.as_mut().unwrap().try_send((pos, event)))
}

thread_local! {
    static HOST_KEY_ROUTING: RefCell<crate::windows_keyboard::HostKeyRouting> = RefCell::new(Default::default());
    /// all configured clients
    static CLIENTS: RefCell<HashSet<Position>> = RefCell::new(HashSet::new());
    /// currently active client
    static ACTIVE_CLIENT: Cell<Option<Position>> = const { Cell::new(None) };
    /// input event channel
    static EVENT_TX: RefCell<Option<Sender<(Position, CaptureEvent)>>> = const { RefCell::new(None) };
    /// position of barrier entry
    static ENTRY_POINT: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    /// previous mouse position
    static PREV_POS: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
    /// displays and generation counter
    static DISPLAYS: RefCell<(Vec<RECT>, i32)> = const { RefCell::new((Vec::new(), 0)) };
    /// True while the host's session is locked. Set/cleared from the
    /// `WM_WTSSESSION_CHANGE` window message. While true, barrier
    /// crossings are suppressed and any active capture is released —
    /// matches macOS's lock-screen suppression and what Wayland does
    /// for free on locked Linux. Without this, low-level mouse hooks
    /// would happily forward motion to the peer while the lock screen
    /// consumes keyboard events, leaving a half-broken state.
    static HOST_LOCKED: Cell<bool> = const { Cell::new(false) };
    static DESKTOP_SECURE: Cell<bool> = const { Cell::new(false) };
    /// Momentary modifiers observed by the global low-level keyboard hook,
    /// including presses that happened before an edge activated capture.
    static HOST_MODIFIERS: RefCell<HashSet<Linux>> = RefCell::new(HashSet::new());
    /// Optional per-edge preflight. An absent entry preserves immediate edge
    /// capture without adding any modifier check.
    static CROSSING_MODIFIERS: RefCell<HashMap<Position, CrossingModifier>> = RefCell::new(HashMap::new());
    /// Handles to the installed low-level hooks. Kept so the
    /// watchdog can unhook the stale handle before re-installing.
    static MOUSE_HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
    static KBD_HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
}

/// Set true by either hook procedure whenever it runs; swapped back
/// to false by the watchdog timer. While a capture is active, a
/// full watchdog interval with this still false means the OS has
/// silently removed the hooks (it does this when a hook proc
/// exceeds `LowLevelHooksTimeout`) and they must be re-installed.
static HOOK_FIRED: AtomicBool = AtomicBool::new(false);

fn get_msg() -> Option<MSG> {
    unsafe {
        let mut msg = std::mem::zeroed();
        let ret = GetMessageW(addr_of_mut!(msg), None, 0, 0);
        match ret.0 {
            0 => None,
            x if x > 0 => Some(msg),
            _ => {
                log::error!(
                    "Windows capture message loop failed: {}",
                    io::Error::last_os_error()
                );
                None
            }
        }
    }
}

fn start(
    event_tx: Sender<(Position, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    stopping: Arc<AtomicBool>,
) -> (thread::JoinHandle<()>, u32) {
    /* condition variable to wait for thead id */
    let thread_id = Arc::new((Condvar::new(), Mutex::new(None)));
    let thread_id_ = Arc::clone(&thread_id);

    let msg_thread =
        thread::spawn(|| start_routine(thread_id_, event_tx, request_buffer, stopping));

    /* wait for thread to set its id */
    let (cond, thread_id) = &*thread_id;
    let mut thread_id = thread_id.lock().unwrap();
    while (*thread_id).is_none() {
        thread_id = cond.wait(thread_id).expect("channel closed");
    }
    (msg_thread, thread_id.expect("thread id"))
}

fn start_routine(
    ready: Arc<(Condvar, Mutex<Option<u32>>)>,
    event_tx: Sender<(Position, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    stopping: Arc<AtomicBool>,
) {
    publish_ready(&ready);
    // A Winlogon helper starts after the lock notification was already sent.
    // Initialize from its assigned desktop rather than waiting for another lock.
    DESKTOP_SECURE.set(desktop_is_secure());
    HOST_LOCKED.set(DESKTOP_SECURE.get());
    EVENT_TX.replace(Some(event_tx));

    let window_proc: WNDPROC = Some(window_proc);

    /* register the low-level input hooks. The capture thread is
     * useless without them, so a failure here is fatal to capture:
     * log loudly and bail rather than spinning a dead message loop
     * that silently captures nothing. (The previous `.unwrap()`
     * panicked this spawned thread, which the joiner swallowed —
     * same silent outcome, no diagnostic.) */
    if let Err(e) = install_hooks() {
        log::error!("failed to install low-level input hooks: {e:?} — capture disabled");
        return;
    }

    let instance = unsafe { GetModuleHandleW(None).unwrap() };
    let instance = instance.into();
    let window_class: WNDCLASSW = WNDCLASSW {
        lpfnWndProc: window_proc,
        hInstance: instance,
        lpszClassName: w!("mousehop-message-window-class"),
        ..Default::default()
    };

    static WINDOW_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
    if WINDOW_CLASS_REGISTERED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        /* register window class if not yet done so */
        unsafe {
            let ret = RegisterClassW(&window_class);
            if ret == 0 {
                panic!("RegisterClassW");
            }
        }
    }

    /* window is used to receive WM_DISPLAYCHANGE and
     * WM_WTSSESSION_CHANGE messages. Keep the HWND so we can register
     * for session notifications and unregister on exit. */
    let msg_window = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("mousehop-message-window-class"),
            w!("mousehop-msg-window"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
        .expect("CreateWindowExW")
    };

    /* register for WM_WTSSESSION_CHANGE notifications so we can
     * detect lock/unlock and suppress crossings while locked. Failure
     * is logged but non-fatal — the rest of the capture still works,
     * we just lose the lock-screen suppression. */
    unsafe {
        if let Err(e) = WTSRegisterSessionNotification(msg_window, NOTIFY_FOR_THIS_SESSION) {
            log::warn!(
                "WTSRegisterSessionNotification failed: {e:?} — host-lock suppression disabled"
            );
        }
    }

    /* arm the hook watchdog. Windows silently removes a low-level
     * hook whose proc exceeds LowLevelHooksTimeout; the try_send
     * hot path makes that unlikely but not impossible, and a
     * removed hook would leave this thread alive but capturing
     * nothing. The WM_TIMER tick detects that and re-installs. */
    unsafe {
        if SetTimer(
            Some(msg_window),
            HOOK_WATCHDOG_TIMER,
            HOOK_WATCHDOG_INTERVAL_MS,
            None,
        ) == 0
        {
            log::warn!("SetTimer for hook watchdog failed — hook auto-recovery disabled");
        }
    }

    /* run message loop. mouse / keybrd procs don't actually return
     * a message, so `get_msg() == None` ends the loop; an Exit-typed
     * thread message breaks out from inside the body. */
    while !stopping.load(Ordering::SeqCst) {
        let Some(msg) = get_msg() else {
            break;
        };
        if msg.hwnd.0.is_null() {
            /* messages sent via PostThreadMessage */
            match msg.wParam.0 {
                x if x == RequestType::Exit as usize => break,
                x if x == RequestType::ClientUpdate as usize => {
                    let requests = {
                        let mut res = vec![];
                        let mut requests = request_buffer.lock().unwrap();
                        for request in requests.drain(..) {
                            res.push(request);
                        }
                        res
                    };

                    for request in requests {
                        update_clients(request)
                    }
                }
                _ => {}
            }
        } else {
            /* other messages for window_procs */
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /* tear down the watchdog timer, session-notification and hooks
     * before the window/thread goes away. */
    unsafe {
        let _ = KillTimer(Some(msg_window), HOOK_WATCHDOG_TIMER);
        let _ = WTSUnRegisterSessionNotification(msg_window);
    }
    uninstall_hooks();
}

fn publish_ready(ready: &(Condvar, Mutex<Option<u32>>)) {
    // PostThreadMessage fails with ERROR_INVALID_THREAD_ID before the target
    // has a message queue, even when the thread itself is already running.
    let mut msg = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
    }
    let (condition, id) = ready;
    *id.lock().unwrap() = Some(unsafe { GetCurrentThreadId() });
    condition.notify_one();
}

/// Install the `WH_MOUSE_LL` / `WH_KEYBOARD_LL` hooks and stash
/// their handles. Must run on the message-loop thread — low-level
/// hooks deliver callbacks to the thread that installed them.
fn install_hooks() -> windows::core::Result<()> {
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0)? };
    let kbd = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(kybrd_proc), None, 0)? };
    MOUSE_HOOK.set(Some(mouse));
    KBD_HOOK.set(Some(kbd));
    Ok(())
}

/// Remove the installed hooks, if any. Unhooking a handle the OS
/// has already removed is a harmless no-op.
fn uninstall_hooks() {
    if let Some(h) = MOUSE_HOOK.take() {
        unsafe {
            let _ = UnhookWindowsHookEx(h);
        }
    }
    if let Some(h) = KBD_HOOK.take() {
        unsafe {
            let _ = UnhookWindowsHookEx(h);
        }
    }
}

/// Watchdog tick (`WM_TIMER`). When a capture is active but no hook
/// callback has fired for a whole interval, the hooks have almost
/// certainly been removed by the OS — re-install them. When no
/// capture is active, silence is expected, so this only resets the
/// flag and waits.
fn hook_watchdog_tick() {
    if ACTIVE_CLIENT.get().is_none() {
        HOOK_FIRED.store(false, Ordering::Relaxed);
        return;
    }
    if !HOOK_FIRED.swap(false, Ordering::Relaxed) {
        log::warn!(
            "no input-hook callbacks during active capture — hooks presumed removed by the OS; re-installing"
        );
        uninstall_hooks();
        match install_hooks() {
            Ok(()) => log::warn!("re-installed low-level input hooks"),
            Err(e) => log::error!("failed to re-install low-level input hooks: {e:?}"),
        }
    }
}

fn check_client_activation(wparam: WPARAM, lparam: LPARAM, returning_injected_input: bool) -> bool {
    if wparam.0 != WM_MOUSEMOVE as usize {
        return ACTIVE_CLIENT.get().is_some();
    }
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    let curr_pos = (mouse_low_level.pt.x, mouse_low_level.pt.y);
    let prev_pos = PREV_POS.get().unwrap_or(curr_pos);
    PREV_POS.replace(Some(curr_pos));

    /* next event is the first actual event */
    let ret = ACTIVE_CLIENT.get().is_some();

    /* client already active, no need to check */
    if ACTIVE_CLIENT.get().is_some() {
        return ret;
    }

    /* host session locked — don't let the cursor leave the lock
     * screen. The lock screen consumes keyboard before our hook sees
     * it; allowing a cross would put the mouse on the peer with no
     * keyboard, a confusing half-broken state. */
    if HOST_LOCKED.get() {
        return ret;
    }

    /* check if a client was activated */
    let entered = DISPLAYS.with_borrow_mut(|(displays, generation)| {
        update_display_regions(displays, generation);
        display_util::entered_barrier(prev_pos, curr_pos, displays)
    });

    let Some(pos) = entered else {
        return ret;
    };

    /* check if a client is registered for the barrier */
    if !CLIENTS.with_borrow(|clients| clients.contains(&pos)) {
        return ret;
    }

    if !returning_injected_input
        && CROSSING_MODIFIERS.with_borrow(|modifiers| {
            modifiers
                .get(&pos)
                .is_some_and(|modifier| !host_modifier_held(*modifier))
        })
    {
        return ret;
    }

    // A local click/drag or shell popup must retain pointer ownership until
    // its gesture ends. Splitting down/up across machines leaves Windows in
    // a drag/menu state and can swallow subsequent clicks.
    if host_interaction_active() {
        return ret;
    }

    /* update active client and entry point */
    ACTIVE_CLIENT.replace(Some(pos));
    let entry_point = DISPLAYS.with_borrow(|(displays, _)| {
        display_util::clamp_to_display_bounds(displays, prev_pos, curr_pos)
    });
    ENTRY_POINT.replace(entry_point);

    /* notify main thread. This runs inside the WH_MOUSE_LL hook
     * proc, so it must not block: a blocked low-level hook freezes
     * input system-wide and the OS then removes the hook outright.
     * try_send it instead. With the deep channel a drop here is
     * essentially impossible; if it ever happens it means ~1024
     * events are already backlogged, in which case blocking would
     * be far worse than a logged drop. */
    log::debug!("ENTERED @ {prev_pos:?} -> {curr_pos:?}");
    let active = ACTIVE_CLIENT.get().expect("active client");
    let normalized_cursor = DISPLAYS.with_borrow(|(displays, _)| {
        crate::normalize_cursor_in_layout(&display_util::layout(displays), entry_point)
    });
    if let Err(e) = try_send_event(
        active,
        CaptureEvent::Begin {
            cursor: Some(entry_point),
            normalized_cursor,
        },
    ) {
        log::error!("forwarding channel full — dropped capture-begin event: {e}");
        // Core cannot release a capture whose Begin it never received.
        ACTIVE_CLIENT.take();
        return ret;
    }
    for event in host_modifier_snapshot() {
        if let Err(e) = try_send_event(active, event) {
            log::error!("forwarding channel full — dropped modifier snapshot: {e}");
        }
    }

    ret
}

fn host_interaction_active() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
    };
    if [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
        .iter()
        .any(|button| unsafe { GetAsyncKeyState(button.0 as i32) < 0 })
    {
        return true;
    }
    let mut gui = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetGUIThreadInfo(0, &mut gui) }.is_err() {
        // Foreground transitions can make this snapshot unavailable. Defer
        // this crossing rather than stealing a menu we cannot inspect.
        return true;
    }
    gui.flags.0 & (GUI_INMENUMODE.0 | GUI_POPUPMENUMODE.0 | GUI_SYSTEMMENUMODE.0 | GUI_INMOVESIZE.0)
        != 0
        || !gui.hwndCapture.0.is_null()
}

unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode < 0 {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    let mouse = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
    if mouse.flags & windows::Win32::UI::WindowsAndMessaging::LLMHF_INJECTED != 0
        || mouse.dwExtraInfo == input_event::WINDOWS_INPUT_EVENT_TAG
    {
        // Observe our relative injection for a native return-edge Begin, but
        // never capture/forward the injected input itself (no feedback loop).
        // Absolute landing/warps and other programs' injection remain ignored.
        if wparam.0 == WM_MOUSEMOVE as usize {
            if crate::windows_pointer::observe_injected_motion(true, mouse.dwExtraInfo, true) {
                HOOK_FIRED.store(true, Ordering::Relaxed);
                check_client_activation(wparam, lparam, true);
            } else {
                PREV_POS.set(Some((mouse.pt.x, mouse.pt.y)));
            }
        }
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    HOOK_FIRED.store(true, Ordering::Relaxed);
    let active = check_client_activation(wparam, lparam, false);

    /* no client was active */
    if !active {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    /* get active client if any */
    let Some(pos) = ACTIVE_CLIENT.get() else {
        return LRESULT(1);
    };

    /* convert to mousehop event */
    let Some(pointer_event) = to_mouse_event(wparam, lparam) else {
        return LRESULT(1);
    };

    if matches!(pointer_event, PointerEvent::Button { .. }) {
        send_host_modifier_state(pos);
    }

    /* notify mainthread (drop events if sending too fast) */
    if let Err(e) = try_send_event(pos, CaptureEvent::Input(Event::Pointer(pointer_event))) {
        log::warn!("e: {e}");
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn kybrd_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode < 0 {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    let key = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    if key
        .flags
        .contains(windows::Win32::UI::WindowsAndMessaging::LLKHF_INJECTED)
        || key.dwExtraInfo == input_event::WINDOWS_INPUT_EVENT_TAG
    {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    HOOK_FIRED.store(true, Ordering::Relaxed);
    /* convert to key event */
    let Some(key_event) = to_key_event(wparam, lparam) else {
        return CallNextHookEx(None, ncode, wparam, lparam);
    };
    let modifier_changed = update_host_modifier_state(&key_event);
    let active = ACTIVE_CLIENT.get();
    let pass_to_host = if let KeyboardEvent::Key { key, state, .. } = key_event {
        HOST_KEY_ROUTING
            .with_borrow_mut(|routing| routing.pass_to_host(key, state, active.is_some()))
    } else {
        active.is_none()
    };

    /* get active client if any */
    let Some(client) = active else {
        return CallNextHookEx(None, ncode, wparam, lparam);
    };

    if let Err(e) = try_send_event(client, CaptureEvent::Input(Event::Keyboard(key_event))) {
        log::warn!("e: {e}");
    }

    if modifier_changed {
        send_host_modifier_state(client);
    }
    // A modifier pressed before crossing already reached Windows. Its release
    // must reach both Windows and the peer, even while new keys are captured.
    if pass_to_host {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn window_proc(
    _hwnd: HWND,
    uint: u32,
    wparam: WPARAM,
    _lparam: LPARAM,
) -> LRESULT {
    if uint == WM_DISPLAYCHANGE {
        log::debug!("display resolution changed");
        DISPLAY_RESOLUTION_GENERATION.fetch_add(1, Ordering::Release);
    } else if uint == WM_TIMER {
        if wparam.0 == HOOK_WATCHDOG_TIMER {
            hook_watchdog_tick();
        }
    } else if uint == WM_WTSSESSION_CHANGE {
        match wparam.0 as u32 {
            WTS_SESSION_LOCK => {
                HOST_LOCKED.set(true);
                HOST_MODIFIERS.with_borrow_mut(HashSet::clear);
                if let Some(pos) = ACTIVE_CLIENT.take() {
                    log::info!("host session locked mid-capture; releasing");
                    let _ = try_send_event(pos, CaptureEvent::AutoRelease);
                } else {
                    log::info!("host session locked");
                }
            }
            WTS_SESSION_UNLOCK => {
                // The unlock notification can precede Default activation.
                HOST_LOCKED.set(DESKTOP_SECURE.get());
                log::info!("host session unlocked");
            }
            _ => {}
        }
    }
    LRESULT(1)
}

fn update_host_modifier_state(event: &KeyboardEvent) -> bool {
    let KeyboardEvent::Key { key, state, .. } = event else {
        return false;
    };
    let Ok(key) = Linux::try_from(*key) else {
        return false;
    };
    if !matches!(
        key,
        Linux::KeyLeftShift
            | Linux::KeyRightShift
            | Linux::KeyLeftCtrl
            | Linux::KeyRightCtrl
            | Linux::KeyLeftAlt
            | Linux::KeyRightalt
            | Linux::KeyLeftMeta
            | Linux::KeyRightmeta
    ) {
        return false;
    }
    HOST_MODIFIERS.with_borrow_mut(|pressed| {
        if *state == 0 {
            pressed.remove(&key);
        } else {
            pressed.insert(key);
        }
    });
    true
}

fn send_host_modifier_state(client: Position) {
    // The final event is the aggregate mask, not another set of key-downs.
    if let Some(snapshot) = host_modifier_snapshot().pop() {
        if let Err(error) = try_send_event(client, snapshot) {
            log::warn!("Cannot forward modifier snapshot: {error}");
        }
    }
}

fn host_modifier_snapshot() -> Vec<CaptureEvent> {
    const SHIFT_MASK: u32 = 1 << 0;
    const CONTROL_MASK: u32 = 1 << 2;
    const ALT_MASK: u32 = 1 << 3;
    const SUPER_MASK: u32 = 1 << 6;
    const ORDERED_MODIFIERS: [Linux; 8] = [
        Linux::KeyLeftShift,
        Linux::KeyRightShift,
        Linux::KeyLeftCtrl,
        Linux::KeyRightCtrl,
        Linux::KeyLeftAlt,
        Linux::KeyRightalt,
        Linux::KeyLeftMeta,
        Linux::KeyRightmeta,
    ];

    HOST_MODIFIERS.with_borrow(|pressed| {
        let mut events = ORDERED_MODIFIERS
            .into_iter()
            .filter(|key| pressed.contains(key))
            .map(|key| {
                CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: key as u32,
                    state: 1,
                }))
            })
            .collect::<Vec<_>>();
        let mut depressed = 0;
        for key in pressed {
            depressed |= match key {
                Linux::KeyLeftShift | Linux::KeyRightShift => SHIFT_MASK,
                Linux::KeyLeftCtrl | Linux::KeyRightCtrl => CONTROL_MASK,
                Linux::KeyLeftAlt | Linux::KeyRightalt => ALT_MASK,
                Linux::KeyLeftMeta | Linux::KeyRightmeta => SUPER_MASK,
                _ => 0,
            };
        }
        events.push(CaptureEvent::Input(Event::Keyboard(
            KeyboardEvent::Modifiers {
                depressed,
                latched: 0,
                locked: 0,
                group: 0,
            },
        )));
        events
    })
}

fn host_modifier_held(modifier: CrossingModifier) -> bool {
    HOST_MODIFIERS.with_borrow(|pressed| match modifier {
        CrossingModifier::Control => {
            pressed.contains(&Linux::KeyLeftCtrl) || pressed.contains(&Linux::KeyRightCtrl)
        }
        CrossingModifier::Alt => {
            pressed.contains(&Linux::KeyLeftAlt) || pressed.contains(&Linux::KeyRightalt)
        }
        CrossingModifier::Shift => {
            pressed.contains(&Linux::KeyLeftShift) || pressed.contains(&Linux::KeyRightShift)
        }
        CrossingModifier::Super => {
            pressed.contains(&Linux::KeyLeftMeta) || pressed.contains(&Linux::KeyRightmeta)
        }
    })
}

static DISPLAY_RESOLUTION_GENERATION: AtomicI32 = AtomicI32::new(1);

fn update_display_regions(displays: &mut Vec<RECT>, generation: &mut i32) {
    let global_generation = DISPLAY_RESOLUTION_GENERATION.load(Ordering::Acquire);
    if *generation != global_generation {
        enumerate_displays(displays);
        log::debug!("displays: {displays:?}");
        *generation = global_generation;
    }
}

fn enumerate_displays(display_rects: &mut Vec<RECT>) {
    display_rects.clear();
    unsafe {
        let mut devices = vec![];
        for i in 0.. {
            let mut device: DISPLAY_DEVICEW = std::mem::zeroed();
            device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            let ret = EnumDisplayDevicesW(None, i, &mut device, EDD_GET_DEVICE_INTERFACE_NAME);
            if ret == FALSE {
                break;
            }
            if device
                .StateFlags
                .contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)
            {
                devices.push(device.DeviceName);
            }
        }
        for device in devices {
            let mut dev_mode: DEVMODEW = std::mem::zeroed();
            dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            let ret = EnumDisplaySettingsW(
                PCWSTR::from_raw(&device as *const _),
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
            );
            if ret == FALSE {
                log::warn!("no display mode");
            }

            let pos = dev_mode.Anonymous1.Anonymous2.dmPosition;
            let (x, y) = (pos.x, pos.y);
            let (width, height) = (dev_mode.dmPelsWidth, dev_mode.dmPelsHeight);

            display_rects.push(RECT {
                left: x,
                right: x + width as i32,
                top: y,
                bottom: y + height as i32,
            });
        }
    }
}

fn update_clients(request: ClientUpdate) {
    match request {
        ClientUpdate::Release(completion) => {
            ACTIVE_CLIENT.take();
            PREV_POS.set(None);
            let _ = completion.send(());
        }
        ClientUpdate::Create(pos) => {
            CLIENTS.with_borrow_mut(|clients| clients.insert(pos));
        }
        ClientUpdate::Destroy(pos) => {
            if let Some(active_pos) = ACTIVE_CLIENT.get() {
                if pos == active_pos {
                    let _ = ACTIVE_CLIENT.take();
                }
            }
            CLIENTS.with_borrow_mut(|clients| clients.remove(&pos));
            CROSSING_MODIFIERS.with_borrow_mut(|modifiers| modifiers.remove(&pos));
        }
        ClientUpdate::SetCrossingModifier(pos, modifier) => {
            CROSSING_MODIFIERS.with_borrow_mut(|modifiers| {
                if let Some(modifier) = modifier {
                    modifiers.insert(pos, modifier);
                } else {
                    modifiers.remove(&pos);
                }
            });
        }
    }
}

fn to_key_event(wparam: WPARAM, lparam: LPARAM) -> Option<KeyboardEvent> {
    let kybrdllhookstruct: KBDLLHOOKSTRUCT = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let mut scan_code = kybrdllhookstruct.scanCode;
    log::trace!("scan_code: {scan_code}");
    if kybrdllhookstruct.flags.contains(LLKHF_EXTENDED) {
        scan_code |= 0xE000;
    }
    let Ok(win_scan_code) = scancode::Windows::try_from(scan_code) else {
        log::warn!("failed to translate to windows scancode: {scan_code}");
        return None;
    };
    log::trace!("windows_scan: {win_scan_code:?}");
    let Ok(linux_scan_code): Result<Linux, ()> = win_scan_code.try_into() else {
        log::warn!("failed to translate into linux scancode: {win_scan_code:?}");
        return None;
    };
    log::trace!("windows_scan: {linux_scan_code:?}");
    let scan_code = linux_scan_code as u32;
    match wparam {
        WPARAM(p) if p == WM_KEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_KEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        WPARAM(p) if p == WM_SYSKEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_SYSKEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        _ => None,
    }
}

fn to_mouse_event(wparam: WPARAM, lparam: LPARAM) -> Option<PointerEvent> {
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    match wparam {
        WPARAM(p) if p == WM_LBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 1,
        }),
        WPARAM(p) if p == WM_MBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 1,
        }),
        WPARAM(p) if p == WM_RBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 1,
        }),
        WPARAM(p) if p == WM_LBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 0,
        }),
        WPARAM(p) if p == WM_RBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MOUSEMOVE as usize => {
            let (x, y) = (mouse_low_level.pt.x, mouse_low_level.pt.y);
            let (ex, ey) = ENTRY_POINT.get();
            let (dx, dy) = (x - ex, y - ey);
            let (dx, dy) = (dx as f64, dy as f64);
            Some(PointerEvent::Motion { time: 0, dx, dy })
        }
        WPARAM(p) if p == WM_MOUSEWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 0,
            value: -(mouse_low_level.mouseData as i32 >> 16),
        }),
        WPARAM(p) if p == WM_XBUTTONDOWN as usize || p == WM_XBUTTONUP as usize => {
            let hb = mouse_low_level.mouseData >> 16;
            let button = match hb {
                1 => BTN_BACK,
                2 => BTN_FORWARD,
                _ => {
                    log::warn!("unknown mouse button");
                    return None;
                }
            };
            Some(PointerEvent::Button {
                time: 0,
                button,
                state: if p == WM_XBUTTONDOWN as usize { 1 } else { 0 },
            })
        }
        WPARAM(p) if p == WM_MOUSEHWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 1, // Horizontal
            value: mouse_low_level.mouseData as i32 >> 16,
        }),
        w => {
            log::warn!("unknown mouse event: {w:?}");
            None
        }
    }
}

fn desktop_is_secure() -> bool {
    use windows::Win32::System::StationsAndDesktops::{
        GetThreadDesktop, GetUserObjectInformationW, UOI_NAME,
    };
    unsafe {
        let Ok(desktop) = GetThreadDesktop(GetCurrentThreadId()) else {
            return true;
        };
        let mut name = [0u16; 256];
        if GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            std::mem::size_of_val(&name) as u32,
            None,
        )
        .is_err()
        {
            return true;
        }
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        !String::from_utf16_lossy(&name[..len]).eq_ignore_ascii_case("default")
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ready_thread_already_has_a_message_queue() {
        let ready = Arc::new((Condvar::new(), Mutex::new(None)));
        let thread_ready = ready.clone();
        let worker = thread::spawn(move || {
            publish_ready(&thread_ready);
            let message = get_msg().expect("posted exit message");
            assert_eq!(message.wParam.0, RequestType::Exit as usize);
        });
        let (condition, id) = &*ready;
        let (id, timeout) = condition
            .wait_timeout_while(id.lock().unwrap(), Duration::from_secs(2), |id| {
                id.is_none()
            })
            .unwrap();
        assert!(!timeout.timed_out(), "message queue readiness timed out");
        unsafe {
            PostThreadMessageW(
                id.expect("ready thread"),
                WM_USER,
                WPARAM(RequestType::Exit as usize),
                LPARAM(0),
            )
        }
        .expect("ready thread must accept a message immediately");
        worker.join().unwrap();
    }

    #[test]
    fn updates_and_drop_after_thread_exit_do_not_panic() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            sender.send(unsafe { GetCurrentThreadId() }).unwrap();
        });
        let id = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        while !worker.is_finished() {
            thread::yield_now();
        }
        let event_thread = EventThread {
            request_buffer: Default::default(),
            thread: Some(worker),
            thread_id: id,
            stopping: Arc::new(AtomicBool::new(false)),
        };
        assert!(event_thread.create(Position::Left).is_err());
        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        assert!(
            event_thread
                .client_update(ClientUpdate::Release(sender))
                .is_err()
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        assert!(event_thread.request_buffer.lock().unwrap().is_empty());
        drop(event_thread);
    }
}

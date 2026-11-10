//! Windows stealth keyboard interception via a WH_KEYBOARD_LL low-level hook.
//!
//! # What this is (and why)
//!
//! This is the Windows counterpart of `keyboard_tap.rs` (macOS CGEventTap). It
//! exposes the SAME napi surface — a `StealthKeyboardTap` class with
//! `start`/`stop`/`update_overlay_bounds`/`is_active` plus a free
//! `is_accessibility_granted()` — so `StealthKeyboardManager` (JS) and the
//! renderer's `stealth-key-captured` contract are byte-for-byte identical
//! across platforms. The renderer never learns which OS produced a keystroke.
//!
//! On macOS an NSPanel can become the keyboard-receiving ("key") window WITHOUT
//! activating the app, so clicking the overlay input types with no focus shift.
//! Windows has no equivalent: a window either is foreground (steals focus →
//! the meeting app fires blur) or it receives no `WM_KEYDOWN` at all. The only
//! way to type into the overlay without stealing focus is to siphon keystrokes
//! at the OS input layer and inject the characters into the renderer — exactly
//! what the macOS tap does. `SetWindowsHookEx(WH_KEYBOARD_LL)` is that layer.
//!
//! # Activation model (identical to macOS)
//!
//! Opt-in per session, driven entirely from JS (`StealthKeyboardManager`):
//!   1. User presses Ctrl+Shift+Space (globalShortcut) OR clicks the overlay
//!      input once a session is live → JS calls `start(callback)`.
//!   2. Each captured key fires `callback` with `{keyCode, chars, flags,
//!      isKeyDown}`. Plain typing keys are SWALLOWED (return 1 from the hook
//!      proc) so the foreground meeting app never receives them.
//!   3. JS calls `stop()` on Esc / Enter-submit / 10s idle / hotkey-again.
//!
//! Swallowing is unconditional while engaged, mirroring the macOS design note:
//! pass-through mode would just be a keylogger. The hook is therefore
//! short-lived — engaged only while the user is actively typing into Natively.
//!
//! # keyCode contract with the renderer
//!
//! `NativelyInterface.tsx` hardcodes macOS HID virtual keycodes (53=Esc,
//! 36=Return, 76=NumpadEnter, 51=Backspace) and expects Tab (48) + arrows
//! (123-126) + F-keys + any system-modifier combo to be PASSED THROUGH (never
//! delivered). We translate Windows VK codes to those mac HID codes and apply
//! the same pass-through filter, so the JS switch statement works unchanged.
//!
//! # Threading
//!
//! `WH_KEYBOARD_LL` is a global hook whose callback runs on the thread that
//! installed it, and that thread MUST pump a message loop or the callback
//! never fires. We spawn a dedicated worker thread, install the hook there,
//! and block on `GetMessageW`. `stop()` posts `WM_QUIT` to the worker via
//! `PostThreadMessageW`, the loop exits, we unhook, and the thread joins.
//!
//! The LowLevelKeyboardProc has NO user-data parameter (unlike CGEventTap), so
//! the shared state is reached through a process-global `Mutex<Option<...>>`.
//! Only one tap is ever active at a time (single overlay), so a global is the
//! natural fit.

#![cfg(target_os = "windows")]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::app_chord::{
    app_chords_from_inputs, match_app_chord, AppChord, AppChordInput, MOD_ALT, MOD_CTRL, MOD_SHIFT,
};
use crate::edit_shortcut::{windows_edit_key, FLAG_CTRL as EDIT_FLAG_CTRL};
use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use once_cell::sync::Lazy;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, GetKeyboardLayout, SendInput, ToUnicodeEx, INPUT, INPUT_0, INPUT_KEYBOARD,
    KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CAPITAL,
    VK_CONTROL, VK_LWIN, VK_MENU, VK_NUMLOCK, VK_RWIN, VK_SHIFT,
};
// HKL (keyboard layout handle) lives under TextServices, not KeyboardAndMouse.
use windows::Win32::UI::TextServices::HKL;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetAncestor, GetForegroundWindow, GetMessageW,
    GetWindowThreadProcessId, KillTimer, PeekMessageW, PostThreadMessageW, SetTimer,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WindowFromPoint,
    EVENT_SYSTEM_FOREGROUND, GA_ROOT, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
    LLKHF_LOWER_IL_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WINEVENT_OUTOFCONTEXT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_QUIT,
    WM_RBUTTONDOWN, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WM_USER, WM_XBUTTONDOWN,
};

// ─── napi objects shared with the macOS module's JS surface ──────────────────
//
// These mirror keyboard_tap.rs EXACTLY (same field names/types) so the emitted
// TypeScript and the JS callers are identical. On Windows the keyboard_tap
// module is cfg'd out, so defining them here creates no duplicate-symbol clash.

/// Overlay bounds accepted for API parity with macOS. Windows does not wire an
/// outside-click stop (neither does the shipped macOS build — its bounds
/// provider is never registered), so these are accepted and ignored.
#[napi(object)]
pub struct OverlayBoundsInput {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Event payload delivered to the JS callback. Flat, matching macOS.
#[napi(object)]
pub struct CapturedKey {
    /// macOS HID virtual keycode (53=Esc, 36=Return, 76=NumpadEnter,
    /// 51=Backspace). Translated from the Windows VK so the renderer's
    /// hardcoded switch works unchanged. 0 for ordinary printable keys.
    pub key_code: u32,
    /// The character(s) this key produces under the active layout + modifiers
    /// (via ToUnicodeEx). Empty for non-printable keys.
    pub chars: String,
    /// Modifier bitmask using the SAME bit layout the macOS tap emits
    /// (cmd=1<<20, opt=1<<19, ctrl=1<<18, shift=1<<17, capsLock=1<<16) so the
    /// renderer can decode identically. We only ever set shift/caps here
    /// because modifier combos are passed through (see the filter below).
    pub flags: u32,
    /// True for keyDown, false for keyUp. The renderer only acts on keyDown.
    pub is_key_down: bool,
    /// True when the user has left Natively and stealth must stop: a click on
    /// another process's window (mouse hook) or a foreground switch such as
    /// Alt+Tab (WinEvent hook). StealthKeyboardManager turns this into stop().
    /// Named for the macOS field it mirrors; on Windows it covers both triggers.
    pub is_outside_mouse_down: bool,
    /// Non-empty ⟹ this event is the app's OWN global shortcut firing, swallowed
    /// by the hook so it can never leak into the foreground app. The value is
    /// the KeybindManager action id; StealthKeyboardManager dispatches it
    /// instead of treating the event as typed text. Empty for ordinary keys.
    pub app_chord_id: String,
}

// macOS CGEventFlags bits the renderer decodes. We reuse the SHIFT/CAPS bits.
const MAC_FLAG_SHIFT: u32 = 1 << 17;
const MAC_FLAG_CAPS: u32 = 1 << 16;

const VK_LSHIFT_: u32 = 0xA0;
const VK_RSHIFT_: u32 = 0xA1;
const VK_LCONTROL_: u32 = 0xA2;
const VK_RCONTROL_: u32 = 0xA3;
const VK_LMENU_: u32 = 0xA4;
const VK_RMENU_: u32 = 0xA5;
const LLKHF_EXTENDED_MASK: u32 = 0x01;
const CTRL_DEFER_MS: u32 = 45;
const CTRL_DEFER_TIMER_ID: usize = 0x4E41_5443; // "NATC"
const PENDING_CTRL_VALID: u64 = 1 << 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BufferedCtrl {
    vk: u32,
    scan: u32,
    flags: u32,
    tracked_vk: u32,
}

impl BufferedCtrl {
    fn pack(self) -> u64 {
        PENDING_CTRL_VALID
            | u64::from(self.vk & 0xff)
            | (u64::from(self.scan & 0xffff) << 8)
            | (u64::from(self.flags & 0xff) << 24)
            | (u64::from(self.tracked_vk & 0xff) << 32)
    }

    fn unpack(value: u64) -> Option<Self> {
        if value & PENDING_CTRL_VALID == 0 {
            return None;
        }
        Some(Self {
            vk: (value & 0xff) as u32,
            scan: ((value >> 8) & 0xffff) as u32,
            flags: ((value >> 24) & 0xff) as u32,
            tracked_vk: ((value >> 32) & 0xff) as u32,
        })
    }
}

fn is_control_vk(vk: u32) -> bool {
    matches!(vk, 0x11 | VK_LCONTROL_ | VK_RCONTROL_)
}

fn is_modifier_vk(vk: u32) -> bool {
    matches!(
        vk,
        0x10 | 0x11
            | 0x12
            | VK_LSHIFT_
            | VK_RSHIFT_
            | VK_LCONTROL_
            | VK_RCONTROL_
            | VK_LMENU_
            | VK_RMENU_
            | 0x5B
            | 0x5C
    )
}

/// Lock-free VK bitmap shared by the hook thread and lifecycle reset paths.
/// SeqCst gives pressed and latch transitions one total order across teardown,
/// focus changes, and hook callbacks.
struct VkBitmap {
    words: [AtomicU64; 4],
}

impl VkBitmap {
    fn new() -> Self {
        Self {
            words: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Set/clear a VK and return its previous state.
    fn transition(&self, vk: u32, down: bool) -> bool {
        if vk >= 256 {
            return false;
        }
        let word = (vk / 64) as usize;
        let mask = 1u64 << (vk % 64);
        let previous = if down {
            self.words[word].fetch_or(mask, Ordering::SeqCst)
        } else {
            self.words[word].fetch_and(!mask, Ordering::SeqCst)
        };
        previous & mask != 0
    }

    fn contains(&self, vk: u32) -> bool {
        if vk >= 256 {
            return false;
        }
        let word = (vk / 64) as usize;
        let mask = 1u64 << (vk % 64);
        self.words[word].load(Ordering::SeqCst) & mask != 0
    }

    fn clear(&self) {
        for word in &self.words {
            word.store(0, Ordering::SeqCst);
        }
    }
}

/// Normalize generic modifier VKs to physical left/right keys. The low-level
/// hook supplies the extended flag for Ctrl/Alt and distinct scan codes for
/// Shift, so releasing one side cannot clear the other side's held state.
fn state_vk(vk: u32, scan: u32, flags: u32) -> u32 {
    match vk {
        0x10 => {
            if scan == 0x36 {
                VK_RSHIFT_
            } else {
                VK_LSHIFT_
            }
        }
        0x11 => {
            if flags & LLKHF_EXTENDED_MASK != 0 {
                VK_RCONTROL_
            } else {
                VK_LCONTROL_
            }
        }
        0x12 => {
            if flags & LLKHF_EXTENDED_MASK != 0 {
                VK_RMENU_
            } else {
                VK_LMENU_
            }
        }
        _ => vk,
    }
}

// ─── Shared hook state ───────────────────────────────────────────────────────

struct HookState {
    /// True while the hook is installed and swallowing keys.
    active: AtomicBool,
    /// Worker thread id, for PostThreadMessageW(WM_QUIT) from stop().
    worker_thread_id: AtomicU32,
    /// Monotonic session counter. `start()` bumps it; each worker captures the
    /// value at spawn. Cleanup only touches the shared state if its captured id
    /// still matches — so a stale worker (e.g. one detached on a start() timeout)
    /// can never clear ACTIVE_HOOK / active out from under a newer session.
    /// (The old Arc::ptr_eq guard was a no-op: every worker clones the same Arc.)
    session_id: AtomicU64,
    /// CapsLock / NumLock toggle state, seeded on the JS main thread at start()
    /// (which pumps input so GetKeyState is accurate) and flipped by the hook on
    /// each lock-key press. GetKeyState on the worker thread is unreliable for
    /// toggles because that thread pumps no keyboard input.
    caps_on: AtomicBool,
    num_on: AtomicBool,
    /// Physical key state derived only from ordered hook transitions. This is
    /// deliberately independent of GetAsyncKeyState snapshots.
    pressed: VkBitmap,
    /// Threadsafe callback into V8. Set on start(), cleared on stop().
    callback: Mutex<Option<Arc<ThreadsafeFunction<CapturedKey>>>>,
    /// The app's supported Ctrl-based global shortcuts the hook swallows
    /// and self-dispatches. Populated at start() from the JS-supplied table;
    /// empty ⟹ the feature is inert and every chord passes through exactly as
    /// before. See app_chord.rs for why and the scope.
    app_chords: Mutex<Vec<AppChord>>,
    /// Completing-key VKs whose initial DOWN was intercepted. Every repeat and
    /// the final UP stay suppressed even after the modifier is released.
    latched: VkBitmap,
    /// One physical Control-down held for the short chord-disambiguation
    /// window. Packed into one atomic so the hook, timer message, focus hook,
    /// and teardown paths observe an indivisible event without allocation.
    pending_ctrl: AtomicU64,
    /// Actual identifier returned by SetTimer. With a null HWND Windows may
    /// replace the requested ID, so WM_TIMER and KillTimer must use this value.
    ctrl_timer_id: AtomicU64,
    /// Physical left/right Control keys whose DOWN was discarded as part of an
    /// intercepted chord. Their repeats and final UP must also be swallowed.
    ctrl_latched: VkBitmap,
    /// Shortcut-guard mode. When true the hook does NOT swallow ordinary typing
    /// (everything passes straight through to the foreground app) and installs
    /// neither the mouse nor the foreground-change hook — its ONLY job is to
    /// swallow + self-dispatch the app's own chords so they cannot leak even
    /// while stealth typing is OFF. When false (the default) the hook is the
    /// full stealth-typing tap: it swallows typing into the overlay and arms the
    /// outside-click / Alt+Tab auto-stop hooks. See StealthKeyboardManager.
    shortcut_only: AtomicBool,
}

impl HookState {
    fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            worker_thread_id: AtomicU32::new(0),
            session_id: AtomicU64::new(0),
            caps_on: AtomicBool::new(false),
            num_on: AtomicBool::new(false),
            pressed: VkBitmap::new(),
            callback: Mutex::new(None),
            app_chords: Mutex::new(Vec::new()),
            latched: VkBitmap::new(),
            pending_ctrl: AtomicU64::new(0),
            ctrl_timer_id: AtomicU64::new(0),
            ctrl_latched: VkBitmap::new(),
            shortcut_only: AtomicBool::new(false),
        }
    }

    fn ctrl_held(&self) -> bool {
        self.pressed.contains(VK_LCONTROL_) || self.pressed.contains(VK_RCONTROL_)
    }

    fn alt_held(&self) -> bool {
        self.pressed.contains(VK_LMENU_) || self.pressed.contains(VK_RMENU_)
    }

    fn shift_held(&self) -> bool {
        self.pressed.contains(VK_LSHIFT_) || self.pressed.contains(VK_RSHIFT_)
    }

    fn win_held(&self) -> bool {
        self.pressed.contains(VK_LWIN.0 as u32) || self.pressed.contains(VK_RWIN.0 as u32)
    }

    fn has_app_chords(&self) -> bool {
        !self
            .app_chords
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty()
    }

    fn reset_key_tracking(&self) {
        self.pressed.clear();
        self.latched.clear();
        self.pending_ctrl.store(0, Ordering::SeqCst);
        self.ctrl_timer_id.store(0, Ordering::SeqCst);
        self.ctrl_latched.clear();
    }

    fn buffer_ctrl(&self, event: BufferedCtrl) {
        self.pending_ctrl.store(event.pack(), Ordering::SeqCst);
    }

    fn pending_ctrl(&self) -> Option<BufferedCtrl> {
        BufferedCtrl::unpack(self.pending_ctrl.load(Ordering::SeqCst))
    }

    fn take_pending_ctrl(&self) -> Option<BufferedCtrl> {
        BufferedCtrl::unpack(self.pending_ctrl.swap(0, Ordering::SeqCst))
    }

    fn cancel_ctrl_timer(&self) {
        let timer_id = self.ctrl_timer_id.swap(0, Ordering::SeqCst) as usize;
        if timer_id != 0 {
            unsafe {
                let _ = KillTimer(HWND::default(), timer_id);
            }
        }
    }

    fn discard_pending_ctrl_for_chord(&self) -> Option<BufferedCtrl> {
        let pending = self.take_pending_ctrl()?;
        self.cancel_ctrl_timer();
        self.ctrl_latched.transition(pending.tracked_vk, true);
        Some(pending)
    }

    fn consume_ctrl_latched_transition(&self, tracked_vk: u32, is_down: bool, is_up: bool) -> bool {
        if is_down {
            return self.ctrl_latched.contains(tracked_vk);
        }
        is_up && self.ctrl_latched.transition(tracked_vk, false)
    }

    /// Repeats remain latched. The first physical UP clears the latch while
    /// still being consumed, so the next DOWN begins a fresh press cycle.
    fn consume_latched_transition(&self, vk: u32, is_down: bool, is_up: bool) -> bool {
        if is_down {
            return self.latched.contains(vk);
        }
        is_up && self.latched.transition(vk, false)
    }
}

/// The single in-flight hook's state, reachable from the C hook proc (which
/// has no user-data pointer). `None` when no tap is engaged. Set by the worker
/// thread just before installing the hook, cleared after it unhooks.
static ACTIVE_HOOK: Lazy<Mutex<Option<Arc<HookState>>>> = Lazy::new(|| Mutex::new(None));

fn injected_key_input(vk: u32, scan: u32, ll_flags: u32, key_up: bool) -> INPUT {
    let mut flags = if ll_flags & LLKHF_EXTENDED_MASK != 0 {
        KEYEVENTF_EXTENDEDKEY.0
    } else {
        0
    };
    if key_up {
        flags |= KEYEVENTF_KEYUP.0;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk as u16),
                wScan: scan as u16,
                dwFlags: KEYBD_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Re-inject a deferred Control-down after chord disambiguation. SendInput marks
/// the event LLKHF_INJECTED, so the hook's injection bypass passes it directly
/// downstream and cannot feed it back into this state machine.
unsafe fn replay_ctrl_down(event: BufferedCtrl) -> bool {
    let input = injected_key_input(event.vk, event.scan, event.flags, false);
    SendInput(&[input], std::mem::size_of::<INPUT>() as i32) == 1
}

unsafe fn replay_ctrl_tap(state: &HookState) -> Option<bool> {
    let event = state.take_pending_ctrl()?;
    state.cancel_ctrl_timer();
    let inputs = [
        injected_key_input(event.vk, event.scan, event.flags, false),
        injected_key_input(event.vk, event.scan, event.flags, true),
    ];
    Some(SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) == 2)
}

/// Replay Control and the ordinary completing key in one serialized input
/// batch. Passing the original key after a standalone SendInput(Control) would
/// not guarantee ordering inside the foreground queue.
unsafe fn replay_ctrl_with_key_down(state: &HookState, key: &KBDLLHOOKSTRUCT) -> Option<bool> {
    let event = state.take_pending_ctrl()?;
    state.cancel_ctrl_timer();
    let inputs = [
        injected_key_input(event.vk, event.scan, event.flags, false),
        injected_key_input(key.vkCode, key.scanCode, key.flags.0, false),
    ];
    Some(SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) == 2)
}

/// Resolve a pending Control-down as ordinary input. Safe to call repeatedly;
/// only the atomic swap winner replays an event.
unsafe fn flush_pending_ctrl(state: &HookState) -> bool {
    let Some(event) = state.take_pending_ctrl() else {
        return false;
    };
    state.cancel_ctrl_timer();
    replay_ctrl_down(event)
}

/// Hold an initial physical Control-down for one short timer interval. The
/// original hook event is always swallowed; if SetTimer fails, replay it
/// immediately so ordinary Control use is never lost.
unsafe fn defer_ctrl_down(state: &HookState, event: BufferedCtrl) {
    // Preserve a rare second Control-side transition instead of overwriting it.
    let _ = flush_pending_ctrl(state);
    state.buffer_ctrl(event);
    let timer_id = SetTimer(HWND::default(), CTRL_DEFER_TIMER_ID, CTRL_DEFER_MS, None);
    if timer_id == 0 {
        let _ = flush_pending_ctrl(state);
    } else {
        state.ctrl_timer_id.store(timer_id as u64, Ordering::SeqCst);
    }
}

// ─── The low-level keyboard hook procedure ───────────────────────────────────
//
// Runs on the worker thread's message loop for every key event, system-wide.
// Must return promptly (Windows drops slow LL hooks after LowLevelHooksTimeout,
// ~300ms). We do no blocking work: the tsfn.call is non-blocking (queues onto
// V8 and returns).
unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // A panic crossing this `extern "system"` boundary aborts the process (since
    // Rust 1.81) — a hard kill from the OS input thread. Contain it and pass the
    // event through, mirroring keyboard_tap.rs. `pass_through` never swallows, so
    // on any internal failure the keystroke still reaches the foreground app.
    // NB: do NOT log in the Err arm. eprintln! panics if the stderr write fails
    // (a console-less packaged GUI process is exactly that case), and this arm
    // runs on the extern "system" hook thread — a panic here crosses the FFI
    // boundary and aborts the process, reintroducing the very abort the
    // catch_unwind exists to prevent. Just pass the event through.
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        keyboard_hook_inner(code, wparam, lparam)
    })) {
        Ok(r) => r,
        Err(_) => CallNextHookEx(HHOOK::default(), code, wparam, lparam),
    }
}

unsafe fn keyboard_hook_inner(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let pass = || CallNextHookEx(HHOOK::default(), code, wparam, lparam);

    // HC_ACTION == 0. Negative codes must be passed straight through per the
    // Win32 contract; codes other than HC_ACTION carry no actionable struct.
    if code != 0 {
        return pass();
    }

    // Snapshot the shared state (clone the Arc under a brief lock, then drop the
    // lock before doing any work / calling into V8) so stop() on another thread
    // can't deadlock against us.
    let state = {
        let guard = match ACTIVE_HOOK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.as_ref().map(Arc::clone)
    };
    let Some(state) = state else {
        return pass();
    };
    if !state.active.load(Ordering::Acquire) {
        return pass();
    }

    let msg = wparam.0 as u32;
    let is_key_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let is_key_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
    if !is_key_down && !is_key_up {
        return pass();
    }

    let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);

    // Never consume or re-dispatch synthetic input. Besides respecting assistive
    // technology and automation, this prevents a future SendInput-based feature
    // from feeding its own events back through the N-API callback indefinitely.
    // LOWER_IL_INJECTED is normally accompanied by INJECTED; test both explicitly
    // so the policy remains correct even for unusual third-party injectors.
    if (kb.flags.0 & (LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0)) != 0 {
        return pass();
    }

    let vk = kb.vkCode;
    let scan = kb.scanCode;
    let tracked_vk = state_vk(vk, scan, kb.flags.0);
    let was_down = state.pressed.transition(tracked_vk, is_key_down);

    // Keep our toggle state current. The lock keys are passed through below, but
    // we must observe their DOWN transition first — the worker thread's own
    // GetKeyState is unreliable for toggles (it pumps no keyboard input). A
    // single press flips the lock.
    if is_key_down && !was_down {
        if vk == VK_CAPITAL.0 as u32 {
            state.caps_on.fetch_xor(true, Ordering::AcqRel);
        } else if vk == VK_NUMLOCK.0 as u32 {
            state.num_on.fetch_xor(true, Ordering::AcqRel);
        }
    }

    // ── APP-CHORD key-UP swallow ──
    // If we swallowed this key's DOWN as one of the app's own shortcuts (see the
    // swallow below), swallow its UP too so the foreground app never receives
    // half a sequence. Keyed by VK alone because the modifiers may already be
    // released by the time the up arrives. Runs before the pass-through filter:
    // the completing key (Enter, a digit, a letter) would otherwise take the
    // deliver-to-overlay path on its own up.
    if state.consume_latched_transition(vk, is_key_down, is_key_up) {
        return LRESULT(1);
    }

    // Defer Control briefly so an app chord can consume the complete modifier
    // stream instead of leaking a bare Ctrl transition to the focused app.
    if is_control_vk(vk) {
        if state.consume_ctrl_latched_transition(tracked_vk, is_key_down, is_key_up) {
            return LRESULT(1);
        }

        if is_key_down && !was_down && state.has_app_chords() {
            defer_ctrl_down(
                &state,
                BufferedCtrl {
                    vk,
                    scan,
                    flags: kb.flags.0,
                    tracked_vk,
                },
            );
            return LRESULT(1);
        }

        // Hide typematic repeats while unresolved. If Control is released
        // without a chord, replay DOWN+UP as one ordered batch and swallow the
        // original UP. A standalone SendInput(DOWN) followed by passing this
        // already-in-flight hook event does not provide a queue-order guarantee.
        if state.pending_ctrl().is_some() {
            if is_key_down {
                return LRESULT(1);
            }
            if is_key_up {
                return match replay_ctrl_tap(&state) {
                    Some(true) => LRESULT(1),
                    // SendInput can be rejected by UIPI for a higher-integrity
                    // foreground process. Preserve the physical release as the
                    // least surprising best-effort fallback.
                    Some(false) | None => pass(),
                };
            }
        }
    }

    // ── PASS-THROUGH FILTER (mirrors keyboard_tap.rs R3/R4) ──
    // Win combos and F-keys/nav/modifiers/lock/media keys go back to the OS so
    // system shortcuts keep working. Ctrl/Alt combos also pass through EXCEPT
    // AltGr: Windows reports AltGr as Ctrl+Alt (or right-Alt), and on EU layouts
    // AltGr produces real text (@ { } \ € ~ …). Passing those through would both
    // fail to type them into the overlay AND leak them into the foreground
    // meeting app — the exact stealth leak this module prevents. So for AltGr we
    // resolve the character first and only pass through if it yields none (i.e.
    // it was a genuine Ctrl+Alt shortcut, which produces no text).
    let ctrl = state.ctrl_held();
    let alt = state.alt_held();
    // AltGr == Ctrl+Alt. On layouts that define AltGr, Windows injects a
    // synthetic Left-Ctrl alongside right-Alt, so (ctrl && alt) already catches
    // every real AltGr. Do NOT also test VK_RMENU: on US/UK layouts right-Alt is
    // a plain Alt (no synthetic Ctrl), and flagging it as AltGr would fabricate
    // a Ctrl in the key-state and split right- vs left-Alt shortcut handling.
    let altgr = ctrl && alt;
    if state.win_held() {
        return pass();
    }

    // ── APP-CHORD SWALLOW (defence against the RegisterHotKey-drop leak) ──
    // If this key-down completes one of the app's OWN registered shortcuts,
    // swallow it and dispatch the action ourselves rather than letting it fall
    // through to the foreground app. The OS silently drops RegisterHotKey
    // registrations on sleep/wake, display/workspace change, etc.; during the
    // recovery window the chord would otherwise leak its completing character
    // into the focused field (a newline from Ctrl+Enter, a digit from Ctrl+1…).
    // Win combos already returned above. Alt is accepted by match_app_chord only
    // for the arrow-key horizontal-scroll binds, so AltGr text still falls
    // through: an Alt chord with a printable completing key never matches.
    if is_key_down && ctrl {
        let mods = MOD_CTRL
            | if state.shift_held() { MOD_SHIFT } else { 0 }
            | if alt { MOD_ALT } else { 0 };
        let matched = {
            let chords = state.app_chords.lock().unwrap_or_else(|p| p.into_inner());
            match_app_chord(chords.as_slice(), vk, mods).map(|s| s.to_string())
        };
        if let Some(id) = matched {
            // Deliver on the SAME threadsafe callback as ordinary keys, tagged
            // with the action id so StealthKeyboardManager dispatches it instead
            // of typing it into the overlay.
            if let Some(swallowed) = deliver_and_swallow(
                &state,
                vk,
                CapturedKey {
                    key_code: 0,
                    chars: String::new(),
                    flags: 0,
                    is_key_down: true,
                    is_outside_mouse_down: false,
                    app_chord_id: id,
                },
            ) {
                // Delivery succeeded, so this is our chord. Discard the held
                // Control DOWN and latch that physical side through its UP.
                let _ = state.discard_pending_ctrl_for_chord();
                return swallowed;
            }
            // No live callback ⟹ nowhere to dispatch. Fall through so the OS /
            // RegisterHotKey can still handle it — never swallow a key with
            // nowhere to go (same rule as the plain-typing path below).
        }
    }

    // A real non-modifier that did not complete an app chord resolves Control
    // as ordinary input immediately, before this event reaches the foreground.
    // Modifier transitions may still be part of Ctrl+Shift/Alt chords, so the
    // 45ms timer remains authoritative for those.
    if state.pending_ctrl().is_some() && !is_modifier_vk(vk) {
        if is_key_down {
            return match replay_ctrl_with_key_down(&state, kb) {
                // The injected key-down bypasses this state machine, so swallow
                // the original event to avoid duplicate input.
                Some(true) => LRESULT(1),
                // UIPI may reject injection into an elevated foreground app.
                // Pass the physical key as a best-effort fallback.
                Some(false) | None => pass(),
            };
        }
        if is_key_up {
            let _ = flush_pending_ctrl(&state);
        }
    }

    // Shortcut-guard mode: the app-chord swallow above is the ENTIRE job. Every
    // other key — ordinary typing included — passes straight through to the
    // foreground app; we are not capturing text here. (Full stealth-typing mode
    // falls past this and swallows typing into the overlay below.)
    if state.shortcut_only.load(Ordering::Acquire) {
        return pass();
    }

    // ── EDITING SHORTCUTS (Ctrl+V/A/C/X) ──
    // The pass-through below sends every Ctrl combo to the foreground app, which
    // made Ctrl+V paste into the meeting app instead of the overlay being typed
    // in. Keep just these four: deliver them tagged with the Ctrl flag (the
    // renderer runs paste/select-all/copy/cut) and swallow the down and its up,
    // like an app chord. App chords matched above still win. Ctrl alone: not
    // with Alt, so AltGr text is untouched, and not with Shift, so Ctrl+Shift
    // shortcuts still reach the meeting app. Ctrl+Backspace / Ctrl+Delete come
    // as Backspace (mac keycode 51) with the Ctrl flag, so the renderer deletes
    // the last word (windows_edit_key has the rules).
    if is_key_down {
        if let Some((key_code, chars)) = windows_edit_key(vk, ctrl, alt, state.shift_held()) {
            if let Some(swallowed) = deliver_and_swallow(
                &state,
                vk,
                CapturedKey {
                    key_code,
                    chars: chars.to_string(),
                    flags: EDIT_FLAG_CTRL,
                    is_key_down: true,
                    is_outside_mouse_down: false,
                    app_chord_id: String::new(),
                },
            ) {
                return swallowed;
            }
            // Nowhere to deliver: let the OS have it rather than lose it.
        }
    }

    if (ctrl || alt) && !altgr {
        return pass();
    }
    if is_passthrough_vk(vk) {
        return pass();
    }

    // Translate to the macOS HID keycode the renderer expects, or 0 for keys
    // whose identity the renderer reads from `chars` instead of `keyCode`.
    let key_code = vk_to_mac_keycode(vk);

    let shift = state.shift_held();
    let caps = state.caps_on.load(Ordering::Acquire);
    let num = state.num_on.load(Ordering::Acquire);

    // Resolve characters using the FOREGROUND app's keyboard layout (the layout
    // the user actually selected), not this worker thread's default.
    let hkl = foreground_keyboard_layout();
    let chars = if key_code == 0 {
        unicode_for_key(vk, scan, hkl, shift, caps, num, altgr)
    } else {
        // Enter/Backspace/Esc: the renderer keys off key_code, not chars.
        String::new()
    };

    // An AltGr combo that produced no character is a genuine Ctrl+Alt shortcut —
    // let the OS have it.
    if altgr && key_code == 0 && chars.is_empty() {
        return pass();
    }

    let mut flags = 0u32;
    if shift {
        flags |= MAC_FLAG_SHIFT;
    }
    if caps {
        flags |= MAC_FLAG_CAPS;
    }

    // Send BOTH keydown and keyup (renderer filters on isKeyDown), matching the
    // macOS tap. If delivery could not even be attempted (no live callback),
    // DON'T swallow — a swallowed key with nowhere to go is lost input.
    let delivered = send_payload(
        &state,
        CapturedKey {
            key_code,
            chars,
            flags,
            is_key_down,
            is_outside_mouse_down: false,
            app_chord_id: String::new(),
        },
    );
    if !delivered {
        return pass();
    }

    // Swallow: the foreground meeting app never sees this keystroke.
    LRESULT(1)
}

/// The keyboard layout of the foreground window's thread — the layout the user
/// actually has selected. Falls back to the calling thread's layout (NULL HKL)
/// when there is no foreground window.
unsafe fn foreground_keyboard_layout() -> HKL {
    let hwnd = GetForegroundWindow();
    let tid = if hwnd.0 != 0 {
        GetWindowThreadProcessId(hwnd, None)
    } else {
        0
    };
    GetKeyboardLayout(tid)
}

// ─── The low-level MOUSE hook procedure ──────────────────────────────────────
//
// Runs alongside the keyboard hook while stealth typing is engaged. Its ONLY
// job is to detect a click that lands OUTSIDE every Natively window and stop
// the session — the Windows equivalent of the macOS tap's isOutsideMouseDown.
// Without it, because the keyboard hook swallows keys process-wide, a user who
// clicks back into their meeting app could not type there until Esc / 10s idle.
//
// Clicks are NEVER swallowed (always CallNextHookEx) — the click itself must
// reach whatever the user clicked. We only classify inside-vs-outside.
//
// DPI-free by design: rather than compare cursor coordinates (physical pixels)
// against the overlay's DIP bounds, we ask which window is under the cursor and
// whether it belongs to OUR process. Clicking any Natively window (overlay,
// pill, toggle, settings, model selector) keeps the session; clicking any other
// process's window — or empty desktop — stops it.
unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // catch_unwind: a panic here would abort the process from the input thread.
    // On panic, pass the click through (never swallow).
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        mouse_hook_inner(code, wparam, lparam)
    })) {
        Ok(r) => r,
        // No eprintln! here — see keyboard_hook_proc: logging in the panic arm
        // can itself panic across the FFI boundary and abort.
        Err(_) => CallNextHookEx(HHOOK::default(), code, wparam, lparam),
    }
}

unsafe fn mouse_hook_inner(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code != 0 {
        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
    }

    // Only button-DOWN events matter; ignore moves / wheels / button-ups.
    let msg = wparam.0 as u32;
    let is_button_down = matches!(
        msg,
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
    );
    if !is_button_down {
        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
    }

    let state = {
        let guard = match ACTIVE_HOOK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.as_ref().map(Arc::clone)
    };
    let Some(state) = state else {
        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
    };
    if !state.active.load(Ordering::Acquire) {
        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
    }

    // Screen-space click point from the hook struct.
    let pt: POINT = (*(lparam.0 as *const MSLLHOOKSTRUCT)).pt;
    let outside = !window_belongs_to_us(WindowFromPoint(pt));

    if outside {
        send_payload(
            &state,
            CapturedKey {
                key_code: 0,
                chars: String::new(),
                flags: 0,
                is_key_down: false,
                is_outside_mouse_down: true,
                app_chord_id: String::new(),
            },
        );
    }

    // Never swallow the click.
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

/// True if `hwnd` (or its root top-level window) belongs to THIS process — i.e.
/// it is one of Natively's windows.
///
/// The GA_ROOT walk matters: WindowFromPoint can return a Chromium child HWND
/// (Chrome_RenderWidgetHostHWND) whose owning process differs from the main
/// process in some Electron versions. The root BrowserWindow HWND is always
/// owned by the main process where this native module runs. Without the walk,
/// clicking the overlay input could read as "not ours" and stop stealth on the
/// very click that engages typing.
///
/// SAFETY: pure Win32 queries on a possibly-stale HWND; both calls are
/// null/invalid tolerant.
unsafe fn window_belongs_to_us(hwnd: HWND) -> bool {
    // HWND(0) = no window (empty desktop, or a destroyed window).
    if hwnd.0 == 0 {
        return false;
    }
    let root = GetAncestor(hwnd, GA_ROOT);
    let target = if root.0 != 0 { root } else { hwnd };
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(target, Some(&mut pid as *mut u32));
    pid != 0 && pid == GetCurrentProcessId()
}

// ─── Foreground-change hook (Alt+Tab and friends) ────────────────────────────
//
// The mouse hook only sees CLICKS. The user can also leave via the keyboard —
// Alt+Tab, Win+Tab, Win+D — and those combos are deliberately passed through by
// the keyboard hook's system-modifier filter, so no click ever happens. Without
// this, stealth would stay engaged after Alt+Tab and keep swallowing keystrokes
// system-wide: the user would type into their newly focused app and the text
// would silently land in Natively's chatbox instead.
//
// macOS gets this for free: when another app activates, the nonactivating panel
// resigns key and typing goes to the new app. This WinEvent hook is the
// equivalent — any foreground change to a window that is not ours stops the
// session.
//
// WINEVENT_OUTOFCONTEXT delivers the callback on the installing thread via its
// message queue, which the worker already pumps (GetMessageW/DispatchMessageW).
unsafe extern "system" fn foreground_event_proc(
    hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    thread: u32,
    time: u32,
) {
    // catch_unwind: a panic here would abort the process. On panic, do nothing
    // (the WinEvent callback has no return value / no swallow semantics).
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        foreground_event_inner(hook, event, hwnd, id_object, id_child, thread, time)
    }));
}

#[allow(clippy::too_many_arguments)]
unsafe fn foreground_event_inner(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND {
        return;
    }
    // hwnd == 0 is a transient foreground state (desktop switch, a window
    // closing before the next activates). It is NOT the user leaving for another
    // app, and window_belongs_to_us(0) is false — so without this guard we'd
    // spuriously stop the session on those transitions.
    if hwnd.0 == 0 {
        let state = ACTIVE_HOOK
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(Arc::clone);
        if let Some(state) = state {
            let _ = flush_pending_ctrl(&state);
            state.reset_key_tracking();
        }
        return;
    }
    let state = {
        let guard = match ACTIVE_HOOK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.as_ref().map(Arc::clone)
    };
    let Some(state) = state else { return };
    if !state.active.load(Ordering::Acquire) {
        return;
    }
    // The overlay is WS_EX_NOACTIVATE so it never becomes foreground; any
    // foreground change to a non-Natively window is therefore a real app switch.
    // Still check ownership explicitly so activating a Natively window (e.g. the
    // launcher) doesn't end the session.
    if window_belongs_to_us(hwnd) {
        return;
    }
    // The foreground left Natively. Any missing key-up during the transition
    // must not leave a modifier or suppression latch stuck for the next app.
    let _ = flush_pending_ctrl(&state);
    state.reset_key_tracking();
    if state.shortcut_only.load(Ordering::Acquire) {
        return;
    }
    send_payload(
        &state,
        CapturedKey {
            key_code: 0,
            chars: String::new(),
            flags: 0,
            is_key_down: false,
            // Reuse the existing stop signal: StealthKeyboardManager already
            // calls stop() on isOutsideMouseDown, and reusing it keeps the
            // captured-key payload byte-identical across platforms (no JS or
            // macOS change needed for a Windows-only trigger).
            is_outside_mouse_down: true,
            app_chord_id: String::new(),
        },
    );
}

/// Deliver a captured event to V8. Returns true if a live callback was called,
/// false if there was none — the caller uses that to decide whether swallowing
/// the key is safe (never swallow a key that had nowhere to go).
fn send_payload(state: &HookState, payload: CapturedKey) -> bool {
    // Clone the tsfn Arc under the lock, drop the lock, then call — same
    // deadlock-avoidance pattern as the macOS module.
    let tsfn = {
        let guard = match state.callback.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.as_ref().map(Arc::clone)
    };
    match tsfn {
        Some(tsfn) => {
            tsfn.call(Ok(payload), ThreadsafeFunctionCallMode::NonBlocking);
            true
        }
        None => false,
    }
}

/// Deliver a key-down the hook keeps from the foreground app (an app chord or an
/// editing key), then swallow it and remember to swallow its key-up. `None` when
/// there was no live callback: the caller passes the key on, since a swallowed
/// key with nowhere to go is lost input.
fn deliver_and_swallow(state: &HookState, vk: u32, payload: CapturedKey) -> Option<LRESULT> {
    if !send_payload(state, payload) {
        return None;
    }
    state.latched.transition(vk, true);
    Some(LRESULT(1))
}

/// VK codes we pass through untouched, matching the macOS whitelist intent.
///
/// # The modifier keys themselves MUST be here
///
/// This is the Windows translation of the macOS tap's `if event_type == 12 {
/// return event }` — flagsChanged, i.e. a modifier pressed or released on its
/// own. On Windows there is no separate event type: pressing Alt arrives as an
/// ordinary WM_KEYDOWN with vkCode = VK_MENU.
///
/// Modifier transitions are recorded internally before this filter. The
/// modifier events themselves must still pass through to Windows; otherwise
/// the OS never receives the Ctrl/Alt/Shift/Win transition even though our
/// state machine recorded it. Without the modifier VKs listed here, they were
/// SWALLOWED — the OS never saw the modifier, so no combination could ever
/// form. That broke every shortcut while stealth typing was engaged: the app's
/// own Alt+H screenshot bind, and Windows' own Win-key shortcuts too.
///
/// Also passed through: lock keys (swallowing CapsLock would break the toggle),
/// media / volume / browser keys, and navigation keys — matching the macOS
/// rationale for arrows and Tab ("navigation, not text"). Esc/Enter/Backspace
/// are deliberately NOT here: those are delivered to the renderer with a
/// translated keyCode.
fn is_passthrough_vk(vk: u32) -> bool {
    // Modifiers, incl. the left/right-specific codes an LL hook can report.
    const VK_SHIFT_: u32 = 0x10;
    const VK_CONTROL_: u32 = 0x11;
    const VK_MENU_: u32 = 0x12; // Alt
    const VK_PAUSE_: u32 = 0x13;
    const VK_CAPITAL_: u32 = 0x14;
    const VK_LWIN_: u32 = 0x5B;
    const VK_RWIN_: u32 = 0x5C;
    const VK_APPS_: u32 = 0x5D; // context-menu key
    const VK_LSHIFT_: u32 = 0xA0;
    const VK_RMENU_: u32 = 0xA5; // 0xA0..=0xA5 = L/R Shift, Control, Alt
    const VK_NUMLOCK_: u32 = 0x90;
    const VK_SCROLL_: u32 = 0x91;
    // Navigation / editing cluster.
    const VK_TAB: u32 = 0x09;
    const VK_PRIOR: u32 = 0x21; // PageUp
    const VK_NEXT: u32 = 0x22; // PageDown
    const VK_END: u32 = 0x23;
    const VK_HOME: u32 = 0x24;
    const VK_LEFT: u32 = 0x25;
    const VK_UP: u32 = 0x26;
    const VK_RIGHT: u32 = 0x27;
    const VK_DOWN: u32 = 0x28;
    const VK_SNAPSHOT: u32 = 0x2C; // PrintScreen
    const VK_INSERT: u32 = 0x2D;
    const VK_DELETE: u32 = 0x2E;
    // Function keys.
    const VK_F1: u32 = 0x70;
    const VK_F24: u32 = 0x87;
    // Browser / media / volume / launch keys.
    const VK_BROWSER_BACK: u32 = 0xA6;
    const VK_LAUNCH_APP2: u32 = 0xB7;

    matches!(
        vk,
        VK_SHIFT_
            | VK_CONTROL_
            | VK_MENU_
            | VK_PAUSE_
            | VK_CAPITAL_
            | VK_LWIN_
            | VK_RWIN_
            | VK_APPS_
            | VK_NUMLOCK_
            | VK_SCROLL_
            | VK_TAB
            | VK_PRIOR
            | VK_NEXT
            | VK_END
            | VK_HOME
            | VK_LEFT
            | VK_UP
            | VK_RIGHT
            | VK_DOWN
            | VK_SNAPSHOT
            | VK_INSERT
            | VK_DELETE
    ) || (VK_LSHIFT_..=VK_RMENU_).contains(&vk)
        || (VK_F1..=VK_F24).contains(&vk)
        || (VK_BROWSER_BACK..=VK_LAUNCH_APP2).contains(&vk)
}

/// Map a Windows VK to the macOS HID keycode the renderer switch expects.
/// Returns 0 for keys the renderer identifies via `chars` (ordinary text).
fn vk_to_mac_keycode(vk: u32) -> u32 {
    const VK_BACK: u32 = 0x08;
    const VK_RETURN: u32 = 0x0D;
    const VK_ESCAPE: u32 = 0x1B;
    match vk {
        VK_ESCAPE => 53,
        VK_RETURN => 36, // Enter (numpad Enter also reports VK_RETURN via LL hook)
        VK_BACK => 51,   // Backspace
        _ => 0,
    }
}

/// Resolve the character(s) a key produces, given the foreground layout and the
/// caller-supplied modifier/toggle state.
///
/// We build the 256-byte key-state array from state the caller already computed
/// (via ordered hook transitions and our own maintained toggle flags)
/// rather than GetKeyboardState(), because a low-level hook runs outside the
/// foreground input queue and GetKeyboardState there is unreliable.
///
/// `hkl` is the FOREGROUND window's layout so the character matches what the
/// user's active app would type (a US-default worker thread would mistranslate
/// every non-US layout). `ctrl_alt` set means AltGr — Shift+AltGr and AltGr
/// characters need Ctrl+Alt in the state array to resolve.
///
/// `ToUnicodeEx` is called with wFlags bit 2 (0x4) set — "do not change the
/// kernel keyboard state" (Windows 10 1607+) — so probing does not corrupt the
/// foreground app's dead-key composition.
#[allow(clippy::too_many_arguments)]
fn unicode_for_key(
    vk: u32,
    scan: u32,
    hkl: HKL,
    shift: bool,
    caps: bool,
    num: bool,
    ctrl_alt: bool,
) -> String {
    let mut key_state = [0u8; 256];
    if shift {
        key_state[VK_SHIFT.0 as usize] = 0x80;
    }
    if caps {
        key_state[VK_CAPITAL.0 as usize] = 0x01;
    }
    // NumLock must be set for the numpad-digit VKs to resolve to digits (with it
    // clear, ToUnicodeEx returns 0 for VK_NUMPAD0-9 and they'd vanish).
    if num {
        key_state[VK_NUMLOCK.0 as usize] = 0x01;
    }
    // AltGr = Ctrl+Alt in the state array.
    if ctrl_alt {
        key_state[VK_CONTROL.0 as usize] = 0x80;
        key_state[VK_MENU.0 as usize] = 0x80;
    }

    let mut buf = [0u16; 8];
    // wFlags = 0x4 → do not alter kernel keyboard state (preserve dead keys).
    let n = unsafe { ToUnicodeEx(vk, scan, &key_state, &mut buf, 0x4, hkl) };
    if n == 0 {
        // No translation for this key in this state. Nothing was written to the
        // buffer, so its contents are stale and must not be read.
        return String::new();
    }
    if n < 0 {
        // DEAD KEY (accent / diacritic). ToUnicodeEx has, where the layout
        // allows, written the SPACING form of the accent to the buffer — e.g.
        // U+00B4 ACUTE ACCENT rather than U+0301 COMBINING ACUTE ACCENT. Only
        // buf[0] is meaningful; the rest of the buffer is stale.
        //
        // Returning "" here loses the key outright. The caller only hands an
        // empty result back to the OS on the AltGr path, so a PLAIN dead key
        // would fall through to `LRESULT(1)` and be swallowed — reaching
        // neither the overlay nor the foreground app. That silently kills every
        // accent key on German T1, AZERTY, Spanish, Nordic and US-International.
        //
        // "If possible" in the Win32 docs is load-bearing: a layout may write
        // nothing at all, in which case there is genuinely no character.
        //
        // NOT dead-key COMPOSITION. wFlags=0x4 deliberately stops the kernel
        // accumulating the dead key (composing would corrupt what every other
        // app on the machine sees), so an acute accent followed by `e` yields
        // two characters, not one precomposed glyph. Visible and correctable
        // beats silently eaten. Real composition must be built and validated on
        // Windows.
        return match buf.first() {
            None | Some(&0) => String::new(),
            Some(&unit) => String::from_utf16_lossy(&[unit]),
        };
    }
    let n = (n as usize).min(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

// ─── Worker thread: installs the hooks and pumps messages ────────────────────

/// Owns the three installed hooks and the shared-state teardown. Its Drop runs
/// on EVERY worker exit — a normal loop break OR a panic unwinding the worker —
/// so a global keyboard hook can never be left installed swallowing the whole
/// system's keystrokes with no recovery (the failure mode a bare
/// unhook-at-the-bottom had if anything above it panicked).
struct HookGuard {
    state: Arc<HookState>,
    session_id: u64,
    kb: HHOOK,
    mouse: Option<HHOOK>,
    fg: HWINEVENTHOOK,
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        // Teardown can race the 45ms disambiguation window. Resolve a pending
        // physical Control-down on its owning message-loop thread before the
        // hook disappears; otherwise stop() would silently eat that transition.
        unsafe {
            let _ = flush_pending_ctrl(&self.state);
        }
        self.state.cancel_ctrl_timer();
        self.state.reset_key_tracking();
        unsafe {
            let _ = UnhookWindowsHookEx(self.kb);
            if let Some(m) = self.mouse {
                let _ = UnhookWindowsHookEx(m);
            }
            if self.fg.0 != 0 {
                let _ = UnhookWinEvent(self.fg);
            }
        }
        // Only touch the shared state if we are STILL the current session. A
        // worker detached on a start() timeout must never clear ACTIVE_HOOK /
        // active out from under a newer session that reused the same Arc.
        if self.state.session_id.load(Ordering::Acquire) == self.session_id {
            let mut guard = ACTIVE_HOOK.lock().unwrap_or_else(|p| p.into_inner());
            *guard = None;
            self.state.worker_thread_id.store(0, Ordering::Release);
            self.state.active.store(false, Ordering::Release);
        }
    }
}

fn hook_worker(state: Arc<HookState>, session_id: u64, ready_tx: mpsc::Sender<bool>) {
    // Publish this thread's id so stop() can PostThreadMessageW(WM_QUIT).
    let tid = unsafe { GetCurrentThreadId() };
    state.worker_thread_id.store(tid, Ordering::Release);

    // Force this thread's message queue to exist FIRST — before installing the
    // hooks, not just before signalling ready. A low-level hook does not create
    // a queue, and thread spawn doesn't either; the queue is created lazily on
    // the first queue call. If it doesn't exist yet, a PostThreadMessageW(WM_QUIT)
    // from stop()/the start() timeout path fails with ERROR_INVALID_THREAD_ID and
    // is lost, and this worker blocks in GetMessageW forever — leaking the thread
    // and both global hooks (and, on the happy path, hanging the main thread on
    // join). Creating the queue here (right after the tid is published) means any
    // WM_QUIT posted once tid is known is delivered whenever GetMessageW runs,
    // even if SetWindowsHookExW below is slow. PeekMessageW(PM_NOREMOVE) is the
    // documented way to force queue creation (MSDN PostThreadMessage remarks).
    let mut msg = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut msg, HWND::default(), WM_USER, WM_USER, PM_NOREMOVE);
    }

    // Register the shared state BEFORE installing the hook so the very first
    // callback (which can fire immediately) can find it.
    {
        let mut guard = match ACTIVE_HOOK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *guard = Some(state.clone());
    }

    // hmod: the module containing the hook proc. For a WH_KEYBOARD_LL hook the
    // proc lives in this module; passing our own module handle is the documented
    // form (GetModuleHandle(NULL) = the running .exe). HMODULE → HINSTANCE via
    // the From impl. Fall back to NULL on the unlikely GetModuleHandleW error.
    let hmod: HINSTANCE = unsafe { GetModuleHandleW(PCWSTR::null()) }
        .map(HINSTANCE::from)
        .unwrap_or_default();

    let hook = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), hmod, 0) }
    {
        Ok(h) => h,
        Err(e) => {
            // Most likely cause in the wild: security software / corporate EDR
            // blocking global keyboard hooks. Report the failure to start() so
            // it returns false and JS never shows an "engaged" state that would
            // silently capture nothing. No guard exists yet, so clean up by hand
            // (session-scoped so we don't disturb a newer session).
            eprintln!("[keyboard_hook_windows] SetWindowsHookExW failed: {e:?}");
            if state.session_id.load(Ordering::Acquire) == session_id {
                state.active.store(false, Ordering::Release);
                state.worker_thread_id.store(0, Ordering::Release);
                *ACTIVE_HOOK.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
            let _ = ready_tx.send(false);
            return;
        }
    };

    // From here the guard owns cleanup on ALL exit paths (normal + panic). Seed
    // it with the keyboard hook, then install the two NON-FATAL auxiliary hooks
    // into it, so a panic during those installs still unhooks the keyboard.
    let mut guard = HookGuard {
        state: state.clone(),
        session_id,
        kb: hook,
        mouse: None,
        fg: HWINEVENTHOOK::default(),
    };

    // The auto-stop hooks (outside-click, Alt+Tab) exist to end a stealth-TYPING
    // session when the user leaves. Shortcut-guard mode has no session to end —
    // it stays armed as long as the app runs — so it installs NEITHER, avoiding
    // two extra system-wide hooks (less surface, less for EDR to notice).
    if !state.shortcut_only.load(Ordering::Acquire) {
        // Low-level MOUSE hook for outside-click stop. If it fails, keyboard
        // stealth still works; the user just loses click-away auto-stop (Esc /
        // idle / hotkey still disengage).
        guard.mouse = match unsafe {
            SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), hmod, 0)
        } {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("[keyboard_hook_windows] WH_MOUSE_LL install failed (outside-click stop disabled): {e:?}");
                None
            }
        };

        // Foreground-change hook: stops the session on Alt+Tab / Win+Tab / any
        // app switch without a click. NON-FATAL. hmod must be NULL for an
        // out-of-context WinEvent hook.
    }

    // Always observe foreground changes. Full stealth mode uses them to stop
    // capture; shortcut-only mode uses them solely as an OS-desync boundary
    // that clears pressed/latch state without disabling the guard.
    guard.fg = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            HMODULE::default(),
            Some(foreground_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if guard.fg.0 == 0 {
        eprintln!("[keyboard_hook_windows] SetWinEventHook failed (focus reset / Alt+Tab auto-stop disabled)");
    }

    // The queue was already forced at the top of this function; the hooks are
    // installed — start() may report success.
    let _ = ready_tx.send(true);

    // Message pump: REQUIRED for the LL hooks to fire. Blocks until stop() posts
    // WM_QUIT (GetMessageW returns 0) or an error (-1).
    loop {
        let r = unsafe { GetMessageW(&mut msg, HWND::default(), 0, 0) };
        if r.0 <= 0 {
            break;
        }
        if msg.message == WM_TIMER
            && state.ctrl_timer_id.load(Ordering::SeqCst) == msg.wParam.0 as u64
        {
            unsafe {
                let _ = flush_pending_ctrl(&state);
            }
            continue;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if !state.active.load(Ordering::Acquire) {
            break;
        }
    }
    // `guard` drops here (or on unwind): unhooks all three and, if still our
    // session, clears ACTIVE_HOOK + worker_thread_id + active.
}

// ─── Public N-API: identical surface to the macOS StealthKeyboardTap ─────────

/// Windows needs no OS permission for a WH_KEYBOARD_LL hook (unlike macOS
/// Accessibility). Always true so the JS permission flow no-ops.
#[napi]
pub fn is_accessibility_granted() -> bool {
    true
}

/// True when the active keyboard layout is a CJK IME (Chinese / Japanese /
/// Korean), i.e. one where text is composed rather than typed key-for-key.
///
/// # Why the stealth hook is unusable for these users
///
/// IME composition happens ABOVE the raw keyboard layer, in IMM32/TSF. Our
/// WH_KEYBOARD_LL hook swallows the keystroke (returns 1) before it ever
/// reaches that layer, and the text we substitute comes from `ToUnicodeEx`,
/// which resolves the layout but performs no composition. So with an IME
/// active, engaging the hook means the candidate window never appears and the
/// user can only enter raw Latin letters.
///
/// macOS has exactly this problem with its CGEventTap and solves it by probing
/// the enabled input sources and declining to auto-engage (see
/// electron/services/ImeDetector.ts). This is the Windows probe for the same
/// decision. JS uses it to report stealth typing unavailable, which routes
/// these users through the existing no-hook fallback: the overlay stays a
/// normal focusable window and typing works through real DOM focus, at the cost
/// of the click stealing foreground focus. Composition that works beats silent,
/// unexplained mojibake.
///
/// Reads the layout of the FOREGROUND thread (the app the user is actually
/// typing in), falling back to this process's layout when that is unavailable.
#[napi]
pub fn is_ime_keyboard_active() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        let thread_id = if hwnd.0 != 0 {
            GetWindowThreadProcessId(hwnd, None)
        } else {
            0
        };
        // idthread = 0 means "the calling thread's layout".
        let hkl = GetKeyboardLayout(thread_id);
        // The low word of an HKL is the LANGID; its low 10 bits are the
        // PRIMARYLANGID. LANG_CHINESE = 0x04, LANG_JAPANESE = 0x11,
        // LANG_KOREAN = 0x12.
        let primary_lang = (hkl.0 as usize as u16) & 0x03FF;
        const LANG_CHINESE: u16 = 0x04;
        const LANG_JAPANESE: u16 = 0x11;
        const LANG_KOREAN: u16 = 0x12;
        matches!(primary_lang, LANG_CHINESE | LANG_JAPANESE | LANG_KOREAN)
    }
}

#[napi]
pub struct StealthKeyboardTap {
    state: Arc<HookState>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

#[napi]
impl StealthKeyboardTap {
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            state: Arc::new(HookState::new()),
            worker: Mutex::new(None),
        }
    }

    /// Engage the hook. Every plain-text keystroke fires `callback` and is
    /// swallowed from the foreground app. `app_chords` is the app's OWN global
    /// supported shortcuts the hook should swallow + self-dispatch
    /// so they can never leak into the foreground app while a `RegisterHotKey`
    /// registration is temporarily dropped; pass `[]` to disable that (fully
    /// inert — every chord passes through as before). `overlay_bounds` is
    /// accepted for cross-platform parity and ignored (no outside-click stop on
    /// Windows, matching the shipped macOS build). Idempotent while active.
    #[napi]
    pub fn start(
        &self,
        callback: ThreadsafeFunction<CapturedKey>,
        app_chords: Vec<AppChordInput>,
        // When true, engage SHORTCUT-GUARD mode: swallow only the app's own
        // chords and pass all other keys (typing included) through; no
        // outside-click / Alt+Tab auto-stop. When false, the full stealth-typing
        // tap. Defaults matter: JS passes false for the existing behaviour.
        shortcut_only: bool,
        _overlay_bounds: Option<OverlayBoundsInput>,
    ) -> Result<bool> {
        if self.state.active.load(Ordering::Acquire) {
            return Ok(true);
        }

        // Publish the mode + chord table BEFORE the worker installs the hook, so
        // the very first keystroke already sees them. Also clear any stale
        // swallowed-up tracking from a prior session.
        self.state
            .shortcut_only
            .store(shortcut_only, Ordering::Release);
        *self
            .state
            .app_chords
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = app_chords_from_inputs(app_chords);
        self.state.reset_key_tracking();

        // Join any prior worker still winding down before publishing active,
        // so its cleanup store(false) can't race our store(true).
        let prev = self.worker.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(h) = prev {
            let _ = h.join();
        }

        self.state.active.store(true, Ordering::Release);
        *self
            .state
            .callback
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(callback));

        // New session id. The worker captures it and only tears down shared
        // state if it is still current — so a worker detached on a timeout can't
        // clobber a newer session. fetch_add returns the previous value; +1 is
        // this session's id.
        let session_id = self
            .state
            .session_id
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);

        // Seed the CapsLock / NumLock toggle state HERE, on the JS main thread,
        // which pumps input so GetKeyState reports the true toggle. The worker
        // thread cannot read this reliably (it pumps no keyboard input); the
        // hook then keeps it current by flipping on each lock-key press.
        self.state.caps_on.store(
            (unsafe { GetKeyState(VK_CAPITAL.0 as i32) } & 0x0001) != 0,
            Ordering::Release,
        );
        self.state.num_on.store(
            (unsafe { GetKeyState(VK_NUMLOCK.0 as i32) } & 0x0001) != 0,
            Ordering::Release,
        );

        // Confirm the hook actually installs before reporting success. Without
        // this, start() returns true the moment the thread spawns, so a hook
        // blocked by EDR would leave JS showing "stealth engaged" while keys go
        // nowhere. The worker sends true once SetWindowsHookExW succeeds (before
        // its blocking message loop) or false on failure.
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        let state = self.state.clone();
        let handle = thread::Builder::new()
            .name("input-hook".into())
            .spawn(move || hook_worker(state, session_id, ready_tx))
            .map_err(|e| {
                self.state.active.store(false, Ordering::Release);
                *self
                    .state
                    .callback
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = None;
                Error::new(
                    Status::GenericFailure,
                    format!("failed to spawn keyboard-hook worker: {e}"),
                )
            })?;

        // SetWindowsHookExW is fast; 2s is a generous ceiling. On success keep
        // the worker (stop() joins it later). On failure/timeout roll back so
        // no false "engaged" state persists.
        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(true) => {
                *self.worker.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
                Ok(true)
            }
            Ok(false) => {
                // Worker reported an install failure and has already returned.
                self.state.active.store(false, Ordering::Release);
                let _ = handle.join();
                *self
                    .state
                    .callback
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = None;
                Ok(false)
            }
            Err(_) => {
                // Timeout or the worker died before signalling. Don't block on
                // join(); best-effort ask it to quit and detach the handle.
                self.state.active.store(false, Ordering::Release);
                let tid = self.state.worker_thread_id.load(Ordering::Acquire);
                if tid != 0 {
                    unsafe {
                        let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
                    }
                }
                drop(handle);
                *self
                    .state
                    .callback
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = None;
                Ok(false)
            }
        }
    }

    /// No-op on Windows (accepted for API parity — see `start`).
    #[napi]
    pub fn update_overlay_bounds(&self, _overlay_bounds: Option<OverlayBoundsInput>) {}

    /// Disengage the hook. Safe to call when inactive. After this returns the
    /// next keystroke reaches the foreground app normally.
    #[napi]
    pub fn stop(&self) {
        let was_active = self.state.active.swap(false, Ordering::AcqRel);
        if !was_active {
            return;
        }
        // Wake the worker's GetMessageW loop so it unhooks and exits.
        let tid = self.state.worker_thread_id.load(Ordering::Acquire);
        if tid != 0 {
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        // Drop the JS callback so V8 can GC it.
        *self
            .state
            .callback
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;

        // Wait for the worker to finish unhooking (so a fast start() after this
        // installs cleanly and the WM_KEYBOARD_LL hook is provably removed).
        let handle = self.worker.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(h) = handle {
            let join_start = std::time::Instant::now();
            if let Err(e) = h.join() {
                eprintln!("[keyboard_hook_windows] worker panicked during cleanup: {e:?}");
            }
            let ms = join_start.elapsed().as_millis();
            if ms > 100 {
                eprintln!("[keyboard_hook_windows] stop() join took {ms}ms");
            }
        }
    }

    #[napi(getter)]
    pub fn is_active(&self) -> bool {
        self.state.active.load(Ordering::Acquire)
    }
}

impl Default for StealthKeyboardTap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{state_vk, BufferedCtrl, HookState, VK_LCONTROL_, VK_RCONTROL_};

    #[test]
    fn generic_control_is_normalized_by_extended_flag() {
        assert_eq!(state_vk(0x11, 0x1d, 0), VK_LCONTROL_);
        assert_eq!(state_vk(0x11, 0x1d, 0x01), VK_RCONTROL_);
    }

    #[test]
    fn modifier_state_follows_ordered_transitions() {
        let state = HookState::new();
        assert!(!state.ctrl_held());
        assert!(!state.pressed.transition(VK_LCONTROL_, true));
        assert!(state.ctrl_held());
        assert!(state.pressed.transition(VK_LCONTROL_, false));
        assert!(!state.ctrl_held());
    }

    #[test]
    fn latch_consumes_repeats_and_exactly_one_release() {
        let state = HookState::new();
        const VK_B: u32 = 0x42;
        assert!(!state.consume_latched_transition(VK_B, true, false));
        assert!(!state.latched.transition(VK_B, true));
        assert!(state.consume_latched_transition(VK_B, true, false));
        assert!(state.consume_latched_transition(VK_B, false, true));
        assert!(!state.consume_latched_transition(VK_B, false, true));
    }

    #[test]
    fn buffered_control_round_trips_without_allocation() {
        let event = BufferedCtrl {
            vk: 0x11,
            scan: 0x1d,
            flags: 0x01,
            tracked_vk: VK_RCONTROL_,
        };
        assert_eq!(BufferedCtrl::unpack(event.pack()), Some(event));
        assert_eq!(BufferedCtrl::unpack(0), None);
    }

    #[test]
    fn intercepted_chord_latches_control_repeats_and_release() {
        let state = HookState::new();
        state.buffer_ctrl(BufferedCtrl {
            vk: 0x11,
            scan: 0x1d,
            flags: 0,
            tracked_vk: VK_LCONTROL_,
        });
        assert!(state.pending_ctrl().is_some());
        assert!(state.discard_pending_ctrl_for_chord().is_some());
        assert!(state.pending_ctrl().is_none());
        assert!(state.consume_ctrl_latched_transition(VK_LCONTROL_, true, false));
        assert!(state.consume_ctrl_latched_transition(VK_LCONTROL_, false, true));
        assert!(!state.consume_ctrl_latched_transition(VK_LCONTROL_, false, true));
    }

    #[test]
    fn lifecycle_reset_clears_pressed_and_latched_state() {
        let state = HookState::new();
        state.pressed.transition(VK_LCONTROL_, true);
        state.latched.transition(0x42, true);
        state.buffer_ctrl(BufferedCtrl {
            vk: 0x11,
            scan: 0x1d,
            flags: 0,
            tracked_vk: VK_LCONTROL_,
        });
        state.ctrl_latched.transition(VK_LCONTROL_, true);
        state.reset_key_tracking();
        assert!(!state.ctrl_held());
        assert!(!state.consume_latched_transition(0x42, true, false));
        assert!(state.pending_ctrl().is_none());
        assert!(!state.consume_ctrl_latched_transition(VK_LCONTROL_, true, false));
    }
}

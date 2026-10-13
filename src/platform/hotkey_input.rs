//! Windows physical-input capture for modifier taps and middle/side mouse shortcuts.
//! The callbacks never consume keyboard input or touch GPUI. Only configured
//! middle/side-button down/up pairs (and side buttons during recording) are claimed.

use super::{
    is_hook_hotkey, modifier_name, platform_input, Code, HotKey, Modifiers, DOUBLE_PRESS_WINDOW,
};
use global_hotkey::GlobalHotKeyManager;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED,
    LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN,
    WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_QUIT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

const MAX_TAP_HOLD: Duration = DOUBLE_PRESS_WINDOW;
const MAX_EVENTS: usize = 256;

#[derive(Debug, Clone, Copy)]
pub(super) enum InputEvent {
    Press(HotKey, Instant),
    ModifierTap(Code),
    Key(Code, Modifiers, Instant),
    Interrupted,
}

#[derive(Clone, Copy)]
struct Tap {
    vk: u32,
    at: Instant,
    foreground: usize,
}

/// Only modifier metadata is logged; ordinary key codes/text are not collected.
#[derive(Clone, Copy)]
struct ModifierTrace {
    raw_vk: u32,
    physical_vk: u32,
    pressed: bool,
    flags: u32,
    held_keys: usize,
    mouse_down: u8,
    tap_active: bool,
    waiting_second: bool,
}

struct InputState {
    down: [bool; 256],
    mouse_down: u8,
    claimed: [bool; 3],
    tap: Option<Tap>,
    previous_tap: Option<Tap>,
    bindings: Vec<HotKey>,
    recording: bool,
    record_special: bool,
    foreground: usize,
    events: VecDeque<InputEvent>,
    modifier_traces: VecDeque<ModifierTrace>,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            down: [false; 256],
            mouse_down: 0,
            claimed: [false; 3],
            tap: None,
            previous_tap: None,
            bindings: Vec::new(),
            recording: false,
            record_special: true,
            foreground: 0,
            events: VecDeque::new(),
            modifier_traces: VecDeque::new(),
        }
    }
}

/// Low-level keyboard sources may report aggregate or side-specific VKs.
/// Keep one physical-key identity for both forms and for both press/release.
fn physical_key(vk: u32, scan_code: u32, flags: u32) -> u32 {
    match vk {
        0x10 => {
            if scan_code & 0xff == 0x36 {
                0xa1
            } else {
                0xa0
            }
        }
        0x11 => {
            if flags & LLKHF_EXTENDED != 0 {
                0xa3
            } else {
                0xa2
            }
        }
        0x12 => {
            if flags & LLKHF_EXTENDED != 0 {
                0xa5
            } else {
                0xa4
            }
        }
        _ => vk,
    }
}

fn modifier(vk: u32) -> Option<(Code, Modifiers)> {
    match vk {
        0xa0 | 0xa1 => Some((Code::ShiftLeft, Modifiers::SHIFT)),
        0xa2 | 0xa3 => Some((Code::ControlLeft, Modifiers::CONTROL)),
        0xa4 | 0xa5 => Some((Code::AltLeft, Modifiers::ALT)),
        0x5b | 0x5c => Some((Code::MetaLeft, Modifiers::SUPER)),
        _ => None,
    }
}

/// Alt/Win may move foreground focus by opening or closing a system menu.
/// This focus change alone is not an intervening key or mouse gesture.
fn opens_system_menu(vk: u32) -> bool {
    matches!(vk, 0xa4 | 0xa5 | 0x5b | 0x5c)
}

impl InputState {
    fn enqueue(&mut self, event: InputEvent) {
        if matches!(event, InputEvent::Interrupted)
            && matches!(self.events.back(), Some(InputEvent::Interrupted))
        {
            return;
        }
        if self.events.len() >= MAX_EVENTS {
            // Never stitch together taps across an overflow or registration change.
            self.reset();
        }
        self.events.push_back(event);
    }

    fn reset(&mut self) {
        self.tap = None;
        self.previous_tap = None;
        self.events.clear();
        self.events.push_back(InputEvent::Interrupted);
        // Preserve physical downs and claimed releases across blacklist/recording changes.
    }

    fn interrupt(&mut self) {
        self.tap = None;
        self.previous_tap = None;
        self.enqueue(InputEvent::Interrupted);
    }

    fn modifiers(&self) -> Modifiers {
        [0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0x5b, 0x5c]
            .into_iter()
            .filter(|vk| self.down[*vk as usize])
            .fold(Modifiers::empty(), |mods, vk| {
                mods | modifier(vk).unwrap().1
            })
    }

    fn keyboard(&mut self, vk: u32, pressed: bool, injected: bool, at: Instant, foreground: usize) {
        self.observe_focus(foreground);
        if injected {
            // Clippi's SendInput paste, accessibility tools and remappers cannot
            // become one half of a physical double tap.
            self.interrupt();
            return;
        }
        let Some(was_down) = self.down.get(vk as usize).copied() else {
            self.interrupt();
            return;
        };
        if pressed == was_down {
            if pressed {
                // Auto-repeat is a hold, never a new tap.
                self.interrupt();
            }
            return;
        }
        self.down[vk as usize] = pressed;
        let Some((code, _)) = modifier(vk) else {
            self.interrupt();
            if pressed && self.recording {
                if let Some(code) = platform_input::code_for_vk(vk) {
                    self.enqueue(InputEvent::Key(code, self.modifiers(), at));
                }
            }
            return;
        };
        let hotkey = HotKey::new(None, code);
        let enabled = if self.recording {
            self.record_special
        } else {
            self.bindings.contains(&hotkey)
        };
        if !enabled {
            // Track held modifiers for mouse combinations and interference,
            // but never create double-tap candidates for unbound modifiers.
            self.interrupt();
            return;
        }
        if pressed {
            self.enqueue(InputEvent::Interrupted);
            let held_keys = self.down.iter().filter(|down| **down).count();
            if held_keys != 1 || self.mouse_down != 0 {
                self.interrupt();
                return;
            }
            if self.previous_tap.is_some_and(|tap| {
                tap.vk != vk
                    || (!opens_system_menu(vk) && tap.foreground != foreground)
                    || at.duration_since(tap.at) > DOUBLE_PRESS_WINDOW
            }) {
                self.previous_tap = None;
            }
            self.tap = Some(Tap { vk, at, foreground });
        } else if let Some(tap) = self.tap.take() {
            if tap.vk != vk
                || (!opens_system_menu(vk) && tap.foreground != foreground)
                || at.duration_since(tap.at) > MAX_TAP_HOLD
                || self.down.iter().any(|down| *down)
                || self.mouse_down != 0
            {
                self.interrupt();
                return;
            }
            let hotkey = HotKey::new(None, code);
            if self.previous_tap.is_some_and(|previous| {
                previous.vk == vk
                    && (opens_system_menu(vk) || previous.foreground == foreground)
                    && at.duration_since(previous.at) <= DOUBLE_PRESS_WINDOW
            }) {
                self.previous_tap = None;
                if modifier_name(code).is_some()
                    && ((self.recording && self.record_special)
                        || (!self.recording && self.bindings.contains(&hotkey)))
                {
                    self.enqueue(InputEvent::Press(hotkey, at));
                }
            } else {
                self.previous_tap = Some(Tap { vk, at, foreground });
                if self.recording && self.record_special && modifier_name(code).is_some() {
                    self.enqueue(InputEvent::ModifierTap(code));
                }
            }
        }
    }

    fn observe_focus(&mut self, foreground: usize) {
        if self.foreground != foreground {
            // Preserve only Alt/Win taps through menu-induced focus changes.
            // Typing, clicking, scrolling, holds, combinations and registration
            // changes still cancel them using the same guards as Ctrl/Shift.
            let tap = self.tap.filter(|tap| opens_system_menu(tap.vk));
            let previous = self.previous_tap.filter(|tap| opens_system_menu(tap.vk));
            let completed: Vec<_> = self
                .events
                .iter()
                .copied()
                .filter(|event| {
                    matches!(event, InputEvent::Press(hotkey, _)
                    if matches!(hotkey.key, Code::AltLeft | Code::MetaLeft))
                        || matches!(
                            event,
                            InputEvent::ModifierTap(Code::AltLeft | Code::MetaLeft)
                        )
                })
                .collect();
            self.reset();
            self.tap = tap;
            self.previous_tap = previous;
            self.events.extend(completed);
            self.foreground = foreground;
        }
    }

    fn mouse_button(
        &mut self,
        button: usize,
        pressed: bool,
        injected: bool,
        at: Instant,
        foreground: usize,
    ) -> bool {
        self.observe_focus(foreground);
        if injected || button >= 3 {
            self.interrupt();
            return false;
        }
        self.tap = None;
        self.previous_tap = None;
        let bit = if button == 2 { 4 } else { 1 << (button + 3) };
        if !pressed {
            self.mouse_down &= !bit;
            return std::mem::take(&mut self.claimed[button]);
        }
        if self.mouse_down & bit != 0 {
            return self.claimed[button];
        }
        self.mouse_down |= bit;
        let hotkey = HotKey::new(
            Some(self.modifiers()),
            match button {
                0 => Code::LaunchApp1,
                1 => Code::LaunchApp2,
                _ => Code::LaunchMail,
            },
        );
        let claimed = (self.recording && self.record_special)
            || (!self.recording && self.bindings.contains(&hotkey));
        self.claimed[button] = claimed;
        if claimed {
            self.enqueue(InputEvent::Press(hotkey, at));
        } else {
            self.enqueue(InputEvent::Interrupted);
        }
        claimed
    }

    fn other_mouse(&mut self, message: u32) {
        // Pointer motion is harmless; clicking, dragging or scrolling cancels taps.
        if message == WM_MOUSEMOVE {
            return;
        }
        match message {
            0x201 => self.mouse_down |= 1,
            0x202 => self.mouse_down &= !1,
            0x204 => self.mouse_down |= 2,
            0x205 => self.mouse_down &= !2,
            0x207 => self.mouse_down |= 4,
            0x208 => self.mouse_down &= !4,
            _ => {}
        }
        if self.recording
            && matches!(
                message,
                WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP
            )
        {
            // Primary clicks operate UI controls and are not recordable keys. The
            // native hook runs before GPUI receives the click, and a recording
            // poll may run between them. Keep the single-key candidate alive
            // until the UI commits it, while still cancelling modifier taps.
            self.tap = None;
            self.previous_tap = None;
            return;
        }
        self.interrupt();
    }
}

// The quick-paste hook may consume a key before our hook sees it. Feed those
// consumed edges back so Ctrl+Enter/Shift+typing cannot masquerade as bare taps.
static ACTIVE_INPUT: OnceLock<Mutex<Weak<Mutex<InputState>>>> = OnceLock::new();

pub(crate) fn observe_claimed_key(vk: u32, key_up: bool) {
    let Some(active) = ACTIVE_INPUT.get() else {
        return;
    };
    let state = active
        .lock()
        .expect("active hotkey input lock poisoned")
        .upgrade();
    if let Some(state) = state {
        state.lock().expect("hotkey input lock poisoned").keyboard(
            vk,
            !key_up,
            false,
            Instant::now(),
            unsafe { GetForegroundWindow() } as usize,
        );
    }
}

thread_local! {
    static HOOK_STATE: RefCell<Option<Arc<Mutex<InputState>>>> = const { RefCell::new(None) };
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0
        && matches!(
            wparam as u32,
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
        )
    {
        // SAFETY: Windows supplies KBDLLHOOKSTRUCT for a non-negative keyboard hook code.
        let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let pressed = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        HOOK_STATE.with(|slot| {
            if let Some(state) = slot.borrow().as_ref() {
                let mut state = state.lock().expect("hotkey input lock poisoned");
                let physical_vk = physical_key(event.vkCode, event.scanCode, event.flags);
                state.keyboard(
                    physical_vk,
                    pressed,
                    event.flags & LLKHF_INJECTED != 0,
                    Instant::now(),
                    unsafe { GetForegroundWindow() } as usize,
                );
                if state.recording && modifier(physical_vk).is_some() {
                    let trace = ModifierTrace {
                        raw_vk: event.vkCode,
                        physical_vk,
                        pressed,
                        flags: event.flags,
                        held_keys: state.down.iter().filter(|down| **down).count(),
                        mouse_down: state.mouse_down,
                        tap_active: state.tap.is_some(),
                        waiting_second: state.previous_tap.is_some(),
                    };
                    if state.modifier_traces.len() >= 32 {
                        state.modifier_traces.pop_front();
                    }
                    state.modifier_traces.push_back(trace);
                }
            }
        });
    }
    // Keyboard events always reach the OS, IME and other hooks immediately.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let mut claimed = false;
    if code >= 0 {
        // SAFETY: Windows supplies MSLLHOOKSTRUCT for a non-negative mouse hook code.
        let event = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        HOOK_STATE.with(|slot| {
            if let Some(state) = slot.borrow().as_ref() {
                let mut state = state.lock().expect("hotkey input lock poisoned");
                if matches!(
                    wparam as u32,
                    WM_XBUTTONDOWN | WM_XBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP
                ) {
                    let button = if matches!(wparam as u32, WM_MBUTTONDOWN | WM_MBUTTONUP) {
                        2
                    } else {
                        (event.mouseData >> 16).wrapping_sub(1) as usize
                    };
                    claimed = state.mouse_button(
                        button,
                        matches!(wparam as u32, WM_XBUTTONDOWN | WM_MBUTTONDOWN),
                        event.flags & LLMHF_INJECTED != 0,
                        Instant::now(),
                        unsafe { GetForegroundWindow() } as usize,
                    );
                } else {
                    state.other_mouse(wparam as u32);
                }
            }
        });
    }
    if claimed {
        1
    } else {
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }
}

struct InputMonitor {
    state: Arc<Mutex<InputState>>,
    thread_id: u32,
    worker: Option<JoinHandle<()>>,
}

impl InputMonitor {
    fn new() -> Result<Self, String> {
        let state = Arc::new(Mutex::new(InputState::default()));
        let thread_state = state.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            HOOK_STATE.with(|slot| *slot.borrow_mut() = Some(thread_state.clone()));
            let mut state = thread_state.lock().expect("hotkey input lock poisoned");
            for vk in 0..256 {
                // Skip aggregate modifier VKs: each physical side is tracked separately.
                if !matches!(vk, 0x10..=0x12 | 0x01..=0x06) {
                    state.down[vk] = unsafe { GetAsyncKeyState(vk as i32) } < 0;
                }
            }
            for (bit, vk) in [(1, 1), (2, 2), (4, 4), (8, 5), (16, 6)] {
                if unsafe { GetAsyncKeyState(vk) } < 0 {
                    state.mouse_down |= bit;
                }
            }
            drop(state);
            // SAFETY: callbacks and state live until this thread unhooks them;
            // the message queue is created before publishing the thread ID.
            let mut message: MSG = unsafe { std::mem::zeroed() };
            unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE) };
            let keyboard = unsafe {
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), std::ptr::null_mut(), 0)
            };
            let mouse = unsafe {
                SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), std::ptr::null_mut(), 0)
            };
            if keyboard.is_null() || mouse.is_null() {
                if !keyboard.is_null() {
                    unsafe { UnhookWindowsHookEx(keyboard) };
                }
                if !mouse.is_null() {
                    unsafe { UnhookWindowsHookEx(mouse) };
                }
                let _ = tx.send(Err(
                    "Unable to install physical hotkey input hooks".to_string()
                ));
                return;
            }
            let _ = tx.send(Ok(unsafe { GetCurrentThreadId() }));
            while unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) } > 0 {}
            unsafe {
                UnhookWindowsHookEx(keyboard);
                UnhookWindowsHookEx(mouse);
            }
            HOOK_STATE.with(|slot| *slot.borrow_mut() = None);
        });
        match rx.recv() {
            Ok(Ok(thread_id)) => {
                *ACTIVE_INPUT
                    .get_or_init(|| Mutex::new(Weak::new()))
                    .lock()
                    .expect("active hotkey input lock poisoned") = Arc::downgrade(&state);
                Ok(Self {
                    state,
                    thread_id,
                    worker: Some(worker),
                })
            }
            result => {
                let _ = worker.join();
                Err(result
                    .ok()
                    .and_then(Result::err)
                    .unwrap_or_else(|| "Hotkey input thread exited".to_string()))
            }
        }
    }
}

impl Drop for InputMonitor {
    fn drop(&mut self) {
        // SAFETY: this thread ID belongs to the live message pump created above.
        if self.thread_id != 0 {
            unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Routes ordinary keys to RegisterHotKey and hook-only keys to the monitor.
pub(super) struct HotkeyManager {
    os: GlobalHotKeyManager,
    monitor: Option<InputMonitor>,
}

impl HotkeyManager {
    pub fn new() -> Result<Self, String> {
        let os = GlobalHotKeyManager::new().map_err(|error| error.to_string())?;
        let monitor = match InputMonitor::new() {
            Ok(monitor) => Some(monitor),
            Err(error) => {
                log::warn!("{error}");
                None
            }
        };
        Ok(Self { os, monitor })
    }

    pub fn register(&self, hotkey: HotKey) -> Result<(), String> {
        if is_hook_hotkey(hotkey) {
            let monitor = self
                .monitor
                .as_ref()
                .ok_or_else(|| "Physical input hooks unavailable".to_string())?;
            let mut state = monitor.state.lock().expect("hotkey input lock poisoned");
            if state.bindings.contains(&hotkey) {
                return Err("Hotkey already registered".to_string());
            }
            state.reset();
            state.bindings.push(hotkey);
            Ok(())
        } else {
            self.os.register(hotkey).map_err(|error| error.to_string())
        }
    }

    pub fn unregister(&self, hotkey: HotKey) -> Result<(), String> {
        if let Some(monitor) = &self.monitor {
            let mut state = monitor.state.lock().expect("hotkey input lock poisoned");
            state.reset();
            state.bindings.retain(|binding| binding != &hotkey);
        }
        if is_hook_hotkey(hotkey) {
            Ok(())
        } else {
            self.os
                .unregister(hotkey)
                .map_err(|error| error.to_string())
        }
    }

    pub fn set_recording(&self, recording: bool) {
        if let Some(monitor) = &self.monitor {
            let mut state = monitor.state.lock().expect("hotkey input lock poisoned");
            state.reset();
            state.recording = recording;
            state.record_special = true;
        }
    }

    pub fn set_record_special(&self, enabled: bool) {
        if let Some(monitor) = &self.monitor {
            let mut state = monitor.state.lock().expect("hotkey input lock poisoned");
            state.reset();
            state.record_special = enabled;
        }
    }

    pub fn clear_pending_input(&self) {
        if let Some(monitor) = &self.monitor {
            monitor
                .state
                .lock()
                .expect("hotkey input lock poisoned")
                .reset();
        }
    }

    #[cfg(test)]
    pub fn for_test(bindings: Vec<HotKey>) -> Self {
        Self {
            os: GlobalHotKeyManager::new().unwrap(),
            monitor: Some(InputMonitor {
                state: Arc::new(Mutex::new(InputState {
                    bindings,
                    ..InputState::default()
                })),
                thread_id: 0,
                worker: None,
            }),
        }
    }

    pub fn pop_event(&self) -> Option<InputEvent> {
        let monitor = self.monitor.as_ref()?;
        let mut state = monitor.state.lock().expect("hotkey input lock poisoned");
        if monitor.thread_id != 0 {
            state.observe_focus(unsafe { GetForegroundWindow() } as usize);
        }
        let event = state.events.pop_front();
        let traces: Vec<_> = state.modifier_traces.drain(..).collect();
        drop(state);
        // File logging stays on the polling thread, never in the native hook.
        for trace in traces {
            log::info!("Modifier recording: raw_vk={:#x} physical_vk={:#x} pressed={} flags={:#x} held_keys={} mouse_buttons={:#x} tap_active={} waiting_second={}",
                trace.raw_vk, trace.physical_vk, trace.pressed, trace.flags,
                trace.held_keys, trace.mouse_down, trace.tap_active, trace.waiting_second);
        }
        event
    }

    #[cfg(test)]
    pub fn push_test_event(&self, event: InputEvent) {
        self.monitor
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .enqueue(event);
    }

    #[cfg(test)]
    pub fn primary_click_for_test(&self, right: bool) {
        let mut state = self.monitor.as_ref().unwrap().state.lock().unwrap();
        state.other_mouse(if right {
            WM_RBUTTONDOWN
        } else {
            WM_LBUTTONDOWN
        });
        state.other_mouse(if right { WM_RBUTTONUP } else { WM_LBUTTONUP });
    }

    pub fn recording_system_menu_gesture(&self) -> bool {
        let Some(monitor) = &self.monitor else {
            return false;
        };
        let state = monitor.state.lock().expect("hotkey input lock poisoned");
        state.recording
            && state.record_special
            && [state.tap, state.previous_tap]
                .into_iter()
                .flatten()
                .any(|tap| opens_system_menu(tap.vk) && tap.at.elapsed() <= DOUBLE_PRESS_WINDOW)
    }

    pub fn has_monitor(&self) -> bool {
        self.monitor.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorder() -> InputState {
        InputState {
            recording: true,
            ..InputState::default()
        }
    }

    fn edge(state: &mut InputState, vk: u32, down: bool, base: Instant, ms: u64) {
        state.keyboard(vk, down, false, base + Duration::from_millis(ms), 1);
    }

    fn tap(state: &mut InputState, vk: u32, base: Instant, ms: u64) {
        edge(state, vk, true, base, ms);
        edge(state, vk, false, base, ms + 30);
    }

    fn presses(state: &InputState) -> Vec<HotKey> {
        state
            .events
            .iter()
            .filter_map(|event| match event {
                InputEvent::Press(hotkey, _) => Some(*hotkey),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn generic_and_side_specific_native_modifier_codes_share_physical_identity() {
        for (generic, scan, flags, side) in [
            (0x10, 0x2a, 0, 0xa0),
            (0x10, 0x36, 0, 0xa1),
            (0x11, 0x1d, 0, 0xa2),
            (0x11, 0x1d, LLKHF_EXTENDED, 0xa3),
            (0x12, 0x38, 0, 0xa4),
            (0x12, 0x38, LLKHF_EXTENDED, 0xa5),
        ] {
            let mut state = recorder();
            let at = Instant::now();
            for ms in [0, 200] {
                // Some sources mix generic down with side-specific up.
                state.keyboard(
                    physical_key(generic, scan, flags),
                    true,
                    false,
                    at + Duration::from_millis(ms),
                    1,
                );
                state.keyboard(
                    physical_key(side, scan, flags),
                    false,
                    false,
                    at + Duration::from_millis(ms + 50),
                    1,
                );
            }
            assert_eq!(
                presses(&state),
                vec![HotKey::new(None, modifier(side).unwrap().0)]
            );
            assert!(!state.down.iter().any(|down| *down));
        }
    }

    #[test]
    fn unbound_modifiers_never_create_double_tap_candidates() {
        let at = Instant::now();
        for vk in [0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0x5b, 0x5c] {
            let mut state = InputState::default();
            tap(&mut state, vk, at, 0);
            assert!(state.tap.is_none() && state.previous_tap.is_none());
            tap(&mut state, vk, at, 100);
            assert!(presses(&state).is_empty());
            state
                .bindings
                .push(HotKey::new(None, modifier(vk).unwrap().0));
            tap(&mut state, vk, at, 200);
            assert!(state.previous_tap.is_some());
            tap(&mut state, vk, at, 300);
            assert_eq!(presses(&state).len(), 1);
        }
    }

    #[test]
    fn unbound_modifier_cancels_bound_taps_but_remains_available_for_mouse_combinations() {
        let at = Instant::now();
        let mut state = InputState::default();
        state.bindings.push(HotKey::new(None, Code::ShiftLeft));
        tap(&mut state, 0xa0, at, 0);
        tap(&mut state, 0xa2, at, 100);
        assert!(state.tap.is_none() && state.previous_tap.is_none());
        tap(&mut state, 0xa0, at, 200);
        assert!(presses(&state).is_empty());
        state.reset();
        let mouse = HotKey::new(Some(Modifiers::CONTROL), Code::LaunchApp1);
        state.bindings.push(mouse);
        edge(&mut state, 0xa2, true, at, 300);
        assert!(state.tap.is_none());
        assert!(state.mouse_button(0, true, false, at + Duration::from_millis(310), 1));
        assert_eq!(presses(&state), vec![mouse]);
    }

    #[test]
    fn first_standalone_modifier_tap_emits_feedback_only_during_gesture_recording() {
        for vk in [0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0x5b, 0x5c] {
            let mut state = recorder();
            let at = Instant::now();
            tap(&mut state, vk, at, 0);
            assert!(presses(&state).is_empty());
            assert!(state.events.iter().any(|event| matches!(event,
                InputEvent::ModifierTap(code) if *code == modifier(vk).unwrap().0)));
            state.reset();
            state.record_special = false;
            tap(&mut state, vk, at, 100);
            assert!(!state
                .events
                .iter()
                .any(|event| matches!(event, InputEvent::ModifierTap(_))));
            state.reset();
            state.recording = false;
            tap(&mut state, vk, at, 200);
            assert!(!state
                .events
                .iter()
                .any(|event| matches!(event, InputEvent::ModifierTap(_))));
        }
    }

    #[test]
    fn generic_modifier_combinations_and_altgr_still_cancel_taps() {
        let mut state = recorder();
        let at = Instant::now();
        let ctrl = physical_key(0x11, 0x1d, 0);
        state.keyboard(ctrl, true, false, at, 1);
        state.keyboard(0x43, true, false, at + Duration::from_millis(10), 1);
        state.keyboard(0x43, false, false, at + Duration::from_millis(20), 1);
        state.keyboard(ctrl, false, false, at + Duration::from_millis(30), 1);
        tap(&mut state, ctrl, at, 100);
        assert!(presses(&state).is_empty());
        state.reset();
        for ms in [200, 300] {
            state.keyboard(ctrl, true, false, at + Duration::from_millis(ms), 1);
            state.keyboard(
                physical_key(0x12, 0x38, LLKHF_EXTENDED),
                true,
                false,
                at + Duration::from_millis(ms + 1),
                1,
            );
            state.keyboard(ctrl, false, false, at + Duration::from_millis(ms + 20), 1);
            state.keyboard(
                physical_key(0x12, 0x38, LLKHF_EXTENDED),
                false,
                false,
                at + Duration::from_millis(ms + 21),
                1,
            );
        }
        assert!(presses(&state).is_empty());
    }

    #[test]
    fn deliberate_modifier_taps_are_not_rejected_as_short_holds() {
        for vk in [0xa0, 0xa2, 0xa4, 0x5b] {
            let mut state = recorder();
            let at = Instant::now();
            edge(&mut state, vk, true, at, 0);
            edge(&mut state, vk, false, at, 300);
            edge(&mut state, vk, true, at, 400);
            edge(&mut state, vk, false, at, 700);
            assert_eq!(presses(&state).len(), 1);
        }
    }

    #[test]
    fn each_modifier_requires_two_complete_taps_on_the_same_side() {
        for vk in [0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0x5b, 0x5c] {
            let mut state = recorder();
            let at = Instant::now();
            tap(&mut state, vk, at, 0);
            assert!(presses(&state).is_empty());
            edge(&mut state, vk, true, at, 100);
            assert!(presses(&state).is_empty());
            edge(&mut state, vk, false, at, 130);
            assert_eq!(
                presses(&state),
                vec![HotKey::new(None, modifier(vk).unwrap().0)]
            );
            // A third tap starts a fresh pair; no overlapping double triggers.
            tap(&mut state, vk, at, 200);
            assert_eq!(presses(&state).len(), 1);
        }
    }

    #[test]
    fn system_menu_focus_changes_do_not_break_alt_or_win_recording() {
        for vk in [0xa4, 0xa5, 0x5b, 0x5c] {
            let mut state = recorder();
            let at = Instant::now();
            state.keyboard(vk, true, false, at, 1);
            state.keyboard(vk, false, false, at + Duration::from_millis(30), 2);
            state.observe_focus(2);
            state.keyboard(vk, true, false, at + Duration::from_millis(100), 2);
            state.keyboard(vk, false, false, at + Duration::from_millis(130), 1);
            assert_eq!(
                presses(&state),
                vec![HotKey::new(None, modifier(vk).unwrap().0)]
            );
            // Polling after the menu closes must still receive the completed gesture.
            state.observe_focus(3);
            assert_eq!(presses(&state).len(), 1);
        }
    }

    #[test]
    fn system_menu_focus_changes_do_not_bypass_combo_or_mouse_guards() {
        for vk in [0xa4, 0xa5, 0x5b, 0x5c] {
            for mouse in [false, true] {
                let mut state = recorder();
                let at = Instant::now();
                tap(&mut state, vk, at, 0);
                state.observe_focus(2);
                if mouse {
                    state.other_mouse(0x201);
                    state.other_mouse(0x202);
                } else {
                    state.keyboard(vk, true, false, at + Duration::from_millis(100), 2);
                    state.keyboard(0x09, true, false, at + Duration::from_millis(110), 2);
                    state.keyboard(0x09, false, false, at + Duration::from_millis(120), 3);
                    state.keyboard(vk, false, false, at + Duration::from_millis(130), 3);
                }
                state.keyboard(vk, true, false, at + Duration::from_millis(200), 3);
                state.keyboard(vk, false, false, at + Duration::from_millis(230), 3);
                assert!(presses(&state).is_empty());
            }
        }
    }

    #[test]
    fn ordinary_shortcut_then_bare_modifier_is_not_a_double_tap() {
        for vk in [0xa0, 0xa2, 0xa4, 0x5b] {
            let mut state = recorder();
            let at = Instant::now();
            edge(&mut state, vk, true, at, 0);
            edge(&mut state, 0x43, true, at, 10);
            edge(&mut state, 0x43, false, at, 20);
            edge(&mut state, vk, false, at, 30);
            tap(&mut state, vk, at, 80);
            assert!(presses(&state).is_empty());
        }
    }

    #[test]
    fn second_tap_used_as_a_shortcut_cancels_first_tap() {
        let mut state = recorder();
        let at = Instant::now();
        tap(&mut state, 0xa2, at, 0);
        edge(&mut state, 0xa2, true, at, 100);
        edge(&mut state, 0x56, true, at, 110);
        edge(&mut state, 0xa2, false, at, 120);
        edge(&mut state, 0x56, false, at, 130);
        tap(&mut state, 0xa2, at, 200);
        assert!(presses(&state).is_empty());
    }

    #[test]
    fn altgr_and_overlapping_modifiers_do_not_trigger() {
        for pair in [(0xa2, 0xa5), (0xa2, 0xa0), (0xa0, 0xa1)] {
            let mut state = recorder();
            let at = Instant::now();
            for ms in [0, 100] {
                edge(&mut state, pair.0, true, at, ms);
                edge(&mut state, pair.1, true, at, ms + 1);
                edge(&mut state, pair.0, false, at, ms + 20);
                edge(&mut state, pair.1, false, at, ms + 21);
            }
            assert!(presses(&state).is_empty());
        }
    }

    #[test]
    fn alternating_left_and_right_modifiers_do_not_form_a_pair() {
        let mut state = recorder();
        let at = Instant::now();
        tap(&mut state, 0xa2, at, 0);
        tap(&mut state, 0xa3, at, 100);
        assert!(presses(&state).is_empty());
        tap(&mut state, 0xa3, at, 200);
        assert_eq!(presses(&state).len(), 1);
    }

    #[test]
    fn hold_repeat_timeout_and_injected_input_cancel_taps() {
        for interruption in 0..4 {
            let mut state = recorder();
            let at = Instant::now();
            tap(&mut state, 0xa2, at, 0);
            match interruption {
                0 => {
                    edge(&mut state, 0xa2, true, at, 100);
                    edge(&mut state, 0xa2, false, at, 800);
                }
                1 => {
                    edge(&mut state, 0xa2, true, at, 100);
                    edge(&mut state, 0xa2, true, at, 110);
                    edge(&mut state, 0xa2, false, at, 120);
                }
                2 => {
                    tap(&mut state, 0xa2, at, 800);
                }
                _ => {
                    state.keyboard(0x56, true, true, at + Duration::from_millis(80), 1);
                    tap(&mut state, 0xa2, at, 100);
                }
            }
            assert!(presses(&state).is_empty(), "interruption {interruption}");
        }
    }

    #[test]
    fn unrelated_keyboard_mouse_and_focus_changes_cancel_taps() {
        for interruption in 0..4 {
            let mut state = recorder();
            let at = Instant::now();
            tap(&mut state, 0xa0, at, 0);
            match interruption {
                0 => {
                    edge(&mut state, 0x41, true, at, 50);
                    edge(&mut state, 0x41, false, at, 60);
                }
                1 => {
                    state.other_mouse(0x20a);
                }
                2 => {
                    state.other_mouse(0x201);
                    state.other_mouse(0x202);
                }
                _ => {
                    state.observe_focus(2);
                }
            }
            tap(&mut state, 0xa0, at, 100);
            assert!(presses(&state).is_empty());
        }
    }

    #[test]
    fn modifier_cannot_be_a_tap_while_a_key_or_mouse_button_is_held() {
        for mouse in [false, true] {
            let mut state = recorder();
            let at = Instant::now();
            if mouse {
                state.other_mouse(0x201);
            } else {
                edge(&mut state, 0x41, true, at, 0);
            }
            tap(&mut state, 0xa0, at, 50);
            tap(&mut state, 0xa0, at, 150);
            assert!(presses(&state).is_empty());
        }
    }

    #[test]
    fn side_button_only_claims_configured_modifiers_and_pairs_its_release() {
        let mut state = InputState::default();
        let at = Instant::now();
        let hotkey = HotKey::new(Some(Modifiers::CONTROL), Code::LaunchApp1);
        state.bindings.push(hotkey);
        assert!(!state.mouse_button(0, true, false, at, 1));
        assert!(!state.mouse_button(0, false, false, at, 1));
        edge(&mut state, 0xa2, true, at, 1);
        assert!(state.mouse_button(0, true, false, at + Duration::from_millis(10), 1));
        assert_eq!(presses(&state), vec![hotkey]);
        // Blacklist changes during a hold must still swallow the matching release.
        state.bindings.clear();
        state.reset();
        edge(&mut state, 0xa2, false, at, 20);
        assert!(state.mouse_button(0, false, false, at + Duration::from_millis(30), 1));
        assert!(!state.mouse_button(0, true, false, at + Duration::from_millis(40), 1));
    }

    #[test]
    fn middle_button_records_press_edges_and_preserves_release_pairing() {
        let mut state = recorder();
        let at = Instant::now();
        assert!(state.mouse_button(2, true, false, at, 1));
        assert_eq!(presses(&state), vec![HotKey::new(None, Code::LaunchMail)]);
        assert_eq!(state.mouse_down, 4);
        assert!(state.mouse_button(2, true, false, at, 1));
        assert_eq!(presses(&state).len(), 1);
        state.recording = false;
        state.reset();
        assert!(state.mouse_button(2, false, false, at, 1));
        assert_eq!(state.mouse_down, 0);
        assert!(!state.mouse_button(2, true, false, at, 1));
        assert!(!state.mouse_button(2, false, false, at, 1));
        edge(&mut state, 0xa2, true, at, 10);
        let hotkey = HotKey::new(Some(Modifiers::CONTROL), Code::LaunchMail);
        state.bindings.push(hotkey);
        assert!(state.mouse_button(2, true, false, at, 1));
        assert_eq!(presses(&state), vec![hotkey]);
        assert!(state.mouse_button(2, false, false, at, 1));
    }

    #[test]
    fn side_button_repeats_and_injections_never_add_physical_presses() {
        let mut state = recorder();
        let at = Instant::now();
        assert!(state.mouse_button(1, true, false, at, 1));
        assert!(state.mouse_button(1, true, false, at, 1));
        assert!(!state.mouse_button(1, true, true, at, 1));
        assert_eq!(presses(&state).len(), 1);
        assert!(state.mouse_button(1, false, false, at, 1));
        assert!(state.mouse_button(1, true, false, at, 1));
        assert_eq!(presses(&state).len(), 2);
    }

    #[test]
    fn recording_and_registration_reset_cannot_complete_an_old_tap() {
        let mut state = recorder();
        let at = Instant::now();
        tap(&mut state, 0xa2, at, 0);
        state.reset();
        tap(&mut state, 0xa2, at, 100);
        assert!(presses(&state).is_empty());
        edge(&mut state, 0xa2, true, at, 200);
        state.reset();
        edge(&mut state, 0xa2, false, at, 230);
        assert!(presses(&state).is_empty());
    }

    #[test]
    fn paste_shortcut_recording_excludes_modifiers_and_mouse_gestures() {
        let mut state = recorder();
        state.record_special = false;
        let at = Instant::now();
        tap(&mut state, 0xa2, at, 0);
        tap(&mut state, 0xa2, at, 100);
        assert!(presses(&state).is_empty());
        assert!(!state.mouse_button(0, true, false, at, 1));
        assert!(!state.mouse_button(0, false, false, at, 1));
        edge(&mut state, 0xa2, true, at, 200);
        edge(&mut state, 0x56, true, at, 210);
        assert!(state.events.iter().any(|event| matches!(event, InputEvent::Key(Code::KeyV, mods, _) if *mods == Modifiers::CONTROL)));
    }

    #[test]
    fn recorder_primary_clicks_preserve_side_candidates_but_cancel_modifier_taps() {
        for (down, up) in [
            (WM_LBUTTONDOWN, WM_LBUTTONUP),
            (WM_RBUTTONDOWN, WM_RBUTTONUP),
        ] {
            let mut state = recorder();
            let at = Instant::now();
            state.mouse_button(0, true, false, at, 1);
            state.mouse_button(0, false, false, at, 1);
            state.events.clear();
            state.other_mouse(down);
            state.other_mouse(up);
            assert!(state.events.is_empty());
            tap(&mut state, 0xa2, at, 0);
            state.other_mouse(down);
            state.other_mouse(up);
            tap(&mut state, 0xa2, at, 100);
            assert!(presses(&state).is_empty());
            // Outside recording these clicks still cancel side double-clicks.
            state.recording = false;
            state.events.clear();
            state.other_mouse(down);
            assert!(matches!(state.events.back(), Some(InputEvent::Interrupted)));
        }
    }

    #[test]
    fn mouse_motion_preserves_taps_and_event_overflow_cancels_them() {
        let mut state = recorder();
        let at = Instant::now();
        tap(&mut state, 0xa2, at, 0);
        state.other_mouse(WM_MOUSEMOVE);
        tap(&mut state, 0xa2, at, 100);
        assert_eq!(presses(&state).len(), 1);
        tap(&mut state, 0xa2, at, 200);
        for _ in 0..MAX_EVENTS {
            state.enqueue(InputEvent::Key(Code::KeyA, Modifiers::empty(), at));
        }
        tap(&mut state, 0xa2, at, 300);
        assert!(presses(&state).is_empty());
    }
}

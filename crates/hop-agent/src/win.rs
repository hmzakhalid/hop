//! Windows input injection with `SendInput`.

use crate::lock::{LockKey, Step};
use hop_core::agent::Platform;
use hop_core::keymap::win;
use hop_core::receiver::Injector;
use hop_core::wire::{code, Monitor};
use std::mem::size_of;
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{HANDLE, LPARAM, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop, DESKTOP_READOBJECTS, HDESK, UOI_NAME,
};
use windows_sys::Win32::System::Shutdown::LockWorkStation;
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, SetPriorityClass, SetThreadPriority, HIGH_PRIORITY_CLASS, THREAD_PRIORITY_TIME_CRITICAL};
use windows_sys::Win32::UI::Accessibility::MOUSEKEYS;
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, VK_NUMLOCK, GetKeyState, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetCursorPos, GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId, SystemParametersInfoW, CURSORINFO, CURSOR_SUPPRESSED, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_MOUSEPRESENT, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SPIF_SENDCHANGE, SPI_GETMOUSEKEYS,
    SPI_SETMOUSEKEYS, XBUTTON1, XBUTTON2, MKF_AVAILABLE, MKF_MOUSEKEYSON, MKF_REPLACENUMBERS};

/// Unassigned virtual key used to keep Alt/Win releases from opening menus (AutoHotkey's choice).
pub(crate) const VK_MASK: u16 = 0xE8;

/// Marks Hop's own injected input (`dwExtraInfo`), so test hooks catch only ours.
pub const HOP_TAG: usize = 0x484F_5021;

/// Windows draws no pointer when it believes there is no mouse (a PC at a TV, driven only by
/// Hop), though injected moves still move it. With Mouse Keys on, it draws one. Hop turns Mouse
/// Keys on the first time it moves a hidden pointer, for this session only (not saved), and puts
/// the old setting back when the agent stops.
struct ShowPointer {
    /// The setting before Hop changed it.
    saved: Option<MOUSEKEYS>,
    done: bool,
    checked: std::time::Instant,
}

impl ShowPointer {
    fn new() -> Self {
        ShowPointer { saved: None, done: false, checked: std::time::Instant::now() }
    }

    /// Call as the pointer moves; it looks at most twice a second, and acts once.
    fn check(&mut self) {
        if self.done || self.checked.elapsed() < std::time::Duration::from_millis(500) {
            return;
        }
        self.checked = std::time::Instant::now();
        unsafe {
            let mut ci: CURSORINFO = std::mem::zeroed();
            ci.cbSize = size_of::<CURSORINFO>() as u32;
            let suppressed = GetCursorInfo(&mut ci) != 0 && ci.flags & CURSOR_SUPPRESSED != 0;
            if !suppressed && GetSystemMetrics(SM_MOUSEPRESENT) != 0 {
                return;
            }
            self.done = true;
            let mut was = MOUSEKEYS { cbSize: size_of::<MOUSEKEYS>() as u32, ..Default::default() };
            if SystemParametersInfoW(SPI_GETMOUSEKEYS, was.cbSize, (&mut was as *mut MOUSEKEYS).cast(), 0) == 0 || was.dwFlags & MKF_MOUSEKEYSON != 0 {
                return;
            }
            let mut on = was;
            on.dwFlags |= MKF_AVAILABLE | MKF_MOUSEKEYSON;
            // The keypad moves the pointer only in the Num Lock state that is not current, so
            // typing on the keypad is unchanged.
            if num_lock_on() {
                on.dwFlags &= !MKF_REPLACENUMBERS;
            } else {
                on.dwFlags |= MKF_REPLACENUMBERS;
            }
            if SystemParametersInfoW(SPI_SETMOUSEKEYS, on.cbSize, (&mut on as *mut MOUSEKEYS).cast(), SPIF_SENDCHANGE) != 0 {
                self.saved = Some(was);
            }
        }
    }
}

/// Num Lock as the user sees it now. This thread pumps no keyboard messages, so its own key
/// state can be stale; borrowing the foreground thread's input state gives the current one.
fn num_lock_on() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        let fg_thread = if fg.is_null() { 0 } else { GetWindowThreadProcessId(fg, std::ptr::null_mut()) };
        let me = GetCurrentThreadId();
        let attached = fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, 1) != 0;
        let on = GetKeyState(VK_NUMLOCK as i32) & 1 != 0;
        if attached {
            AttachThreadInput(me, fg_thread, 0);
        }
        on
    }
}

impl Drop for ShowPointer {
    fn drop(&mut self) {
        if let Some(mut was) = self.saved.take() {
            unsafe { SystemParametersInfoW(SPI_SETMOUSEKEYS, was.cbSize, (&mut was as *mut MOUSEKEYS).cast(), SPIF_SENDCHANGE) };
        }
    }
}

pub struct WinPlatform {
    virt: (i32, i32, i32, i32),
    pointer: ShowPointer,
    /// Windows keys Hop holds down (bit 0 left, bit 1 right).
    win_held: u8,
    lock_key: LockKey,
    /// Our own hooks catch what we inject (swallow mode, selftest): then nothing may act outside
    /// `SendInput`: no locking the PC, no Mouse Keys. The hooks live as long as the platform.
    caught_by: Option<crate::hooks::Hooks>,
    /// Service helper: switch to whichever desktop has input (normal, lock screen, UAC).
    follow: bool,
    desk: HDESK,
    desk_name: String,
    checked: std::time::Instant,
}

fn desktop_name(d: HDESK) -> Option<String> {
    let mut name = [0u16; 64];
    let mut needed = 0u32;
    let ok = unsafe { GetUserObjectInformationW(d as HANDLE, UOI_NAME, name.as_mut_ptr().cast(), (name.len() * 2) as u32, &mut needed) };
    if ok == 0 {
        return None;
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..len]))
}

impl WinPlatform {
    /// Call on the thread that will inject (it gets time-critical priority).
    pub fn new() -> Self {
        unsafe {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        }
        let mut p = Self {
            virt: (0, 0, 1, 1),
            pointer: ShowPointer::new(),
            win_held: 0,
            lock_key: LockKey::default(),
            caught_by: None,
            follow: false,
            desk: std::ptr::null_mut(),
            desk_name: String::new(),
            checked: std::time::Instant::now(),
        };
        p.refresh_virtual();
        p
    }

    /// Inject only into `hooks`, which catch it before any app sees it.
    pub fn caught_by(mut self, hooks: crate::hooks::Hooks) -> Self {
        self.caught_by = Some(hooks);
        self
    }

    /// For the service helper (SYSTEM): follows input onto the lock screen and UAC prompts.
    pub fn following() -> Self {
        let mut p = Self::new();
        p.follow = true;
        p.ensure_desktop(true);
        p
    }

    /// Move this thread onto the desktop that currently receives input, if it changed.
    fn ensure_desktop(&mut self, force: bool) {
        if !self.follow || (!force && self.checked.elapsed() < std::time::Duration::from_millis(50)) {
            return;
        }
        self.checked = std::time::Instant::now();
        unsafe {
            let d = OpenInputDesktop(0, 0, 0x1000_0000 /* GENERIC_ALL */);
            if d.is_null() {
                return;
            }
            let name = desktop_name(d).unwrap_or_default();
            if name != self.desk_name && SetThreadDesktop(d) != 0 {
                if !self.desk.is_null() {
                    CloseDesktop(self.desk);
                }
                self.desk = d;
                self.desk_name = name;
                self.refresh_virtual();
            } else {
                CloseDesktop(d);
            }
        }
    }

    fn send_following(&mut self, inputs: &[INPUT]) -> bool {
        self.ensure_desktop(false);
        if Self::send(inputs) {
            return true;
        }
        if self.follow {
            // The input desktop may have just changed (lock, UAC): switch and retry once.
            self.ensure_desktop(true);
            return Self::send(inputs);
        }
        false
    }

    fn refresh_virtual(&mut self) {
        unsafe {
            self.virt = (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
            );
        }
    }

    /// True if Windows accepted every input.
    fn send(inputs: &[INPUT]) -> bool {
        if inputs.is_empty() {
            return true;
        }
        unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) == inputs.len() as u32 }
    }

    fn mouse(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: HOP_TAG } },
        }
    }

    fn keyboard(vk: u16, scan: u16, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: HOP_TAG } },
        }
    }
}

impl Injector for WinPlatform {
    fn key(&mut self, c: u16, down: bool) -> bool {
        let bit = match c {
            win::LWIN => 1,
            win::RWIN => 2,
            _ => 0,
        };
        if !down {
            self.win_held &= !bit;
        }
        if self.caught_by.is_none() {
            match self.lock_key.step(c, down, self.win_held != 0) {
                Step::Drop => return true,
                // The mask tap keeps the Win release from opening Start afterwards.
                Step::Lock => {
                    self.mask_key();
                    if unsafe { LockWorkStation() } != 0 {
                        self.lock_key.locked();
                        return true;
                    }
                }
                Step::Send => {}
            }
        }
        let up = if down { 0 } else { KEYEVENTF_KEYUP };
        let ext = if code::is_extended(c) { KEYEVENTF_EXTENDEDKEY } else { 0 };
        let ok = self.send_following(&[Self::keyboard(0, code::low(c) as u16, KEYEVENTF_SCANCODE | ext | up)]);
        // Held only if Windows took it: a refused Win press must not make a later L lock.
        if down && ok {
            self.win_held |= bit;
        }
        ok
    }

    fn button(&mut self, b: u8, down: bool) -> bool {
        let (flags, data) = match (b, down) {
            (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (0, false) => (MOUSEEVENTF_LEFTUP, 0),
            (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (3, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as u32),
            (3, false) => (MOUSEEVENTF_XUP, XBUTTON1 as u32),
            (4, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as u32),
            (4, false) => (MOUSEEVENTF_XUP, XBUTTON2 as u32),
            _ => return true,
        };
        // No MOVE flag: a button-only input must not move the cursor.
        self.send_following(&[Self::mouse(0, 0, data, flags)])
    }

    fn move_to(&mut self, x: i32, y: i32) {
        self.ensure_desktop(false);
        let (vx, vy, vw, vh) = self.virt;
        // Aim at the pixel centre so either rounding rule Windows applies lands on (x, y).
        let norm = |p: i32, o: i32, size: i32| ((((p - o) as f64 + 0.5) * 65536.0 / size as f64) as i32).clamp(0, 65535);
        let _ = self.send_following(&[Self::mouse(
            norm(x, vx, vw),
            norm(y, vy, vh),
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )]);
        if self.caught_by.is_none() {
            self.pointer.check();
        }
    }

    fn wheel(&mut self, v: i32, h: i32) {
        let mut inputs = Vec::with_capacity(2);
        if v != 0 {
            inputs.push(Self::mouse(0, 0, v as u32, MOUSEEVENTF_WHEEL));
        }
        if h != 0 {
            inputs.push(Self::mouse(0, 0, h as u32, MOUSEEVENTF_HWHEEL));
        }
        let _ = self.send_following(&inputs);
    }

    fn mask_key(&mut self) {
        let _ = self.send_following(&[Self::keyboard(VK_MASK, 0, 0), Self::keyboard(VK_MASK, 0, KEYEVENTF_KEYUP)]);
    }
}

unsafe extern "system" fn collect(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    let out = &mut *(data as *mut Vec<Monitor>);
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(h, &mut info) != 0 {
        let (mut dx, mut dy) = (96u32, 96u32);
        let scale = if GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) >= 0 { dx * 1000 / 96 } else { 1000 };
        let r = info.rcMonitor;
        out.push(Monitor { left: r.left, top: r.top, right: r.right, bottom: r.bottom, scale_milli: scale.min(u16::MAX as u32) as u16 });
    }
    1
}

impl Platform for WinPlatform {
    fn monitors(&mut self) -> Vec<Monitor> {
        self.refresh_virtual();
        let mut v: Vec<Monitor> = Vec::new();
        unsafe {
            EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(collect), &mut v as *mut _ as LPARAM);
        }
        v.truncate(hop_core::wire::MAX_MONITORS);
        v
    }

    fn cursor_pos(&mut self) -> (i32, i32) {
        self.ensure_desktop(false);
        let mut p = POINT { x: 0, y: 0 };
        unsafe {
            GetCursorPos(&mut p);
        }
        (p.x, p.y)
    }

    fn input_blocked(&mut self) -> bool {
        unsafe {
            let d = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
            if d.is_null() {
                return true; // the secure desktop (lock screen, UAC) refuses us
            }
            let name = desktop_name(d);
            CloseDesktop(d);
            if self.follow {
                self.ensure_desktop(false);
                return false; // the service helper can follow input anywhere
            }
            name.as_deref() != Some("Default")
        }
    }
}

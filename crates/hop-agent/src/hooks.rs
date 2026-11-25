//! Low-level keyboard/mouse hooks that observe Hop's own injected input (tagged with
//! [`HOP_TAG`]), so the agent can verify what Windows actually received. Those events are
//! dropped before any application sees them. Physical input and other programs' injected input
//! always pass through untouched.
//!
//! The hook callbacks never block: they push into a bounded buffer and return at once, because
//! Windows silently removes a low-level hook that is too slow.

use crate::win::HOP_TAG;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT,
    LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYUP, WM_QUIT, WM_SYSKEYUP,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    Key { vk: u32, scan: u32, flags: u32, up: bool },
    Mouse { msg: u32, x: i32, y: i32, data: u32 },
}

const CAP: usize = 4096;
static SEEN: Mutex<VecDeque<Seen>> = Mutex::new(VecDeque::new());
static STOP_ECHO: AtomicBool = AtomicBool::new(false);

fn record(s: Seen) {
    if let Ok(mut q) = SEEN.try_lock() {
        if q.len() == CAP {
            q.pop_front();
        }
        q.push_back(s);
    }
}

unsafe extern "system" fn keyboard(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = &*(l as *const KBDLLHOOKSTRUCT);
        if k.flags & LLKHF_INJECTED != 0 && k.dwExtraInfo == HOP_TAG {
            let up = w as u32 == WM_KEYUP || w as u32 == WM_SYSKEYUP;
            record(Seen::Key { vk: k.vkCode, scan: k.scanCode, flags: k.flags, up });
            return 1;
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, w, l)
}

unsafe extern "system" fn mouse(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let m = &*(l as *const MSLLHOOKSTRUCT);
        if m.flags & LLMHF_INJECTED != 0 && m.dwExtraInfo == HOP_TAG {
            record(Seen::Mouse { msg: w as u32, x: m.pt.x, y: m.pt.y, data: m.mouseData });
            return 1;
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, w, l)
}

pub struct Hooks {
    tid: u32,
    thread: Option<JoinHandle<()>>,
    echo: Option<JoinHandle<()>>,
}

impl Hooks {
    /// Install both hooks on their own message-pumping thread. Fails (with nothing left
    /// installed) unless both hooks are in place. With `echo`, a separate thread prints what was
    /// seen and nothing is retained; without it, [`take`](Self::take) returns it.
    pub fn start(echo: bool) -> Result<Hooks, String> {
        SEEN.lock().unwrap().clear();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || unsafe {
            let module = GetModuleHandleW(std::ptr::null());
            let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), module, 0);
            let ms = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), module, 0);
            let ok = !kb.is_null() && !ms.is_null();
            if !ok {
                if !kb.is_null() {
                    UnhookWindowsHookEx(kb);
                }
                if !ms.is_null() {
                    UnhookWindowsHookEx(ms);
                }
                let _ = tx.send((GetCurrentThreadId(), false));
                return;
            }
            let _ = tx.send((GetCurrentThreadId(), true));
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {}
            UnhookWindowsHookEx(kb);
            UnhookWindowsHookEx(ms);
        });
        let (tid, ok) = rx.recv().map_err(|_| "hook thread died".to_string())?;
        if !ok {
            let _ = thread.join();
            return Err("Windows refused the input hooks".into());
        }
        let echo = echo.then(|| {
            STOP_ECHO.store(false, Ordering::SeqCst);
            std::thread::spawn(|| {
                while !STOP_ECHO.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(50));
                    for s in Self::take() {
                        println!("  windows saw {s:?}");
                    }
                }
            })
        });
        Ok(Hooks { tid, thread: Some(thread), echo })
    }

    pub fn take() -> Vec<Seen> {
        SEEN.lock().unwrap().drain(..).collect()
    }
}

impl Drop for Hooks {
    fn drop(&mut self) {
        unsafe {
            PostThreadMessageW(self.tid, WM_QUIT, 0, 0);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        STOP_ECHO.store(true, Ordering::SeqCst);
        if let Some(t) = self.echo.take() {
            let _ = t.join();
        }
    }
}

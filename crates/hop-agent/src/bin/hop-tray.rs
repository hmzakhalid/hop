//! hop-tray: the Hop icon in the Windows taskbar, started at sign-in for every user. It shows
//! whether the Hop service is running, and "Pair a Mac…" opens an administrator window with a
//! pairing code (`hop-agent pair`). It changes nothing itself, so it needs no privileges.
//!
//! `hop-tray --pair` (the Start-menu "Pair a Mac with Hop" shortcut) starts pairing at once.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!("hop-tray runs on Windows");
}

#[cfg(windows)]
#[path = "../scm.rs"]
mod scm;

#[cfg(windows)]
fn main() {
    tray::run()
}

#[cfg(windows)]
mod tray {
    use crate::scm::{self, wide};
    use std::sync::atomic::{AtomicU32, Ordering};
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Services::{SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STOPPED, SERVICE_STOP_PENDING};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, GetCursorPos, GetMessageW,
        LoadIconW, MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetTimer,
        TrackPopupMenu, TranslateMessage, MB_ICONINFORMATION, MB_OK, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG,
        SW_SHOWNORMAL, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
        WS_EX_TOOLWINDOW, WS_POPUP,
    };

    const WM_TRAY: u32 = WM_APP + 1;
    const CMD_PAIR: usize = 1;
    const CMD_ABOUT: usize = 2;
    const CMD_QUIT: usize = 3;
    const STATUS_TIMER: usize = 1;
    const STATUS_EVERY_MS: u32 = 3000;
    /// Explorer's broadcast when the taskbar comes back (after a crash or restart): add the icon again.
    static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

    pub fn run() {
        let pair_now = std::env::args().any(|a| a == "--pair");
        unsafe {
            let name = wide("Local\\HopTray");
            CreateMutexW(std::ptr::null(), 1, name.as_ptr());
            if GetLastError() == ERROR_ALREADY_EXISTS {
                // Already in the taskbar: just do what was asked.
                if pair_now {
                    pair();
                }
                return;
            }
            let class = wide("HopTray");
            let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: GetModuleHandleW(std::ptr::null()), lpszClassName: class.as_ptr(), ..std::mem::zeroed() };
            RegisterClassW(&wc);
            // A hidden top-level window, not a message-only one: only top-level windows receive
            // the TaskbarCreated broadcast that lets the icon come back after Explorer restarts.
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class.as_ptr(),
                class.as_ptr(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                wc.hInstance,
                std::ptr::null(),
            );
            TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), Ordering::Relaxed);
            set_icon(hwnd, NIM_ADD);
            SetTimer(hwnd, STATUS_TIMER, STATUS_EVERY_MS, None);
            if pair_now {
                pair();
            }
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    fn status() -> String {
        match scm::service_state() {
            Ok(Some(SERVICE_RUNNING)) => "Hop is running. On your Mac, press ⌃⌥⌘ and an arrow.".into(),
            Ok(Some(SERVICE_START_PENDING)) => "Hop is starting…".into(),
            Ok(Some(SERVICE_STOPPED | SERVICE_STOP_PENDING)) => "Hop is stopped. Reinstall Hop to start it again.".into(),
            Ok(Some(_)) => "Hop is busy…".into(),
            Ok(None) => "Hop is not installed on this PC.".into(),
            Err(e) => format!("Hop: {e}"),
        }
    }

    unsafe fn set_icon(hwnd: HWND, action: u32) {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = LoadIconW(GetModuleHandleW(std::ptr::null()), 1 as _); // resource 1 (res/hop.rc)
        let tip: Vec<u16> = status().encode_utf16().take(nid.szTip.len() - 1).collect();
        nid.szTip[..tip.len()].copy_from_slice(&tip);
        Shell_NotifyIconW(action, &nid);
    }

    /// Open an administrator window that shows a pairing code (Windows asks for permission).
    fn pair() {
        let agent = std::env::current_exe().map(|p| p.with_file_name("hop-agent.exe")).unwrap_or_default();
        let (verb, file, args) = (wide("runas"), wide(&agent.display().to_string()), wide("pair --keep-open"));
        // A declined permission prompt is an answer, not an error.
        unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), args.as_ptr(), std::ptr::null(), SW_SHOWNORMAL) };
    }

    unsafe fn show_menu(hwnd: HWND) {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(&status()).as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, CMD_PAIR, wide("Pair a Mac…").as_ptr());
        AppendMenuW(menu, MF_STRING, CMD_ABOUT, wide("About Hop").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, CMD_QUIT, wide("Hide This Icon").as_ptr());
        let mut at = POINT { x: 0, y: 0 };
        GetCursorPos(&mut at);
        // Needed so the menu closes when the user clicks elsewhere (documented Win32 quirk).
        SetForegroundWindow(hwnd);
        TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, at.x, at.y, 0, hwnd, std::ptr::null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        match msg {
            WM_TRAY if l as u32 == WM_RBUTTONUP || l as u32 == WM_LBUTTONUP => show_menu(hwnd),
            WM_TIMER => set_icon(hwnd, NIM_MODIFY),
            WM_COMMAND => match w & 0xFFFF {
                CMD_PAIR => pair(),
                CMD_ABOUT => {
                    let text = format!(
                        "Hop {} (build {})\n\nUse your Mac's keyboard and trackpad on this PC.\n\n{}\n\nThe icon comes back when you sign in again.",
                        env!("CARGO_PKG_VERSION"),
                        hop_core::BUILD,
                        status()
                    );
                    MessageBoxW(hwnd, wide(&text).as_ptr(), wide("About Hop").as_ptr(), MB_OK | MB_ICONINFORMATION);
                }
                CMD_QUIT => {
                    set_icon(hwnd, NIM_DELETE);
                    PostQuitMessage(0);
                }
                _ => {}
            },
            m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => set_icon(hwnd, NIM_ADD),
            _ => return DefWindowProcW(hwnd, msg, w, l),
        }
        0
    }
}

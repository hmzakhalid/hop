//! `hop-agent selftest`: inject real input through the same code the agent uses, catch it with
//! low-level hooks before any application sees it, and check that Windows received exactly
//! what was meant. Nothing reaches your apps and the cursor does not move.

use crate::hooks::{Hooks, Seen};
use crate::win::{WinPlatform, VK_MASK};
use hop_core::agent::Platform;
use hop_core::keymap::win;
use hop_core::wire::code;
use hop_core::receiver::Injector;
use std::time::Duration;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VSC_TO_VK_EX};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    LLKHF_EXTENDED, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

fn settle() -> Vec<Seen> {
    std::thread::sleep(Duration::from_millis(150));
    Hooks::take()
}

struct Report {
    passed: u32,
    failed: u32,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool, detail: impl FnOnce() -> String) {
        if ok {
            self.passed += 1;
            println!("  PASS  {name}");
        } else {
            self.failed += 1;
            println!("  FAIL  {name}: {}", detail());
        }
    }
}

/// Expected virtual key for a wire scan code on this PC's keyboard layout.
fn vk_for(code: u16) -> u32 {
    let sc = code::low(code) as u32 | if code::is_extended(code) { 0xE000 } else { 0 };
    unsafe { MapVirtualKeyW(sc, MAPVK_VSC_TO_VK_EX) }
}

pub fn run() -> bool {
    println!("hop-agent selftest: injecting test input and catching it before any app sees it.\n");
    let mut r = Report { passed: 0, failed: 0 };
    let mut p = match Hooks::start(false) {
        Ok(h) => WinPlatform::new().caught_by(h),
        Err(e) => {
            println!("{e}; not injecting anything.");
            return false;
        }
    };

    let monitors = p.monitors();
    println!("Monitors (physical pixels):");
    for m in &monitors {
        println!("  {}×{} at ({}, {}), scale {}%", m.right - m.left, m.bottom - m.top, m.left, m.top, m.scale_milli / 10);
    }
    for m in &monitors {
        let targets = [
            (m.left, m.top),
            (m.right - 1, m.bottom - 1),
            ((m.left + m.right) / 2, (m.top + m.bottom) / 2),
            (m.left + 1, m.bottom - 2),
        ];
        for (x, y) in targets {
            p.move_to(x, y);
            let seen = settle();
            let got = seen.iter().rev().find_map(|s| match s {
                Seen::Mouse { msg, x, y, .. } if *msg == WM_MOUSEMOVE => Some((*x, *y)),
                _ => None,
            });
            r.check(&format!("move to ({x}, {y})"), got == Some((x, y)), || format!("Windows put it at {got:?}"));
        }
    }

    let keys: &[(&str, u16)] = &[
        ("A", 0x1E),
        ("Return", 0x1C),
        ("Left Ctrl", win::LCTRL),
        ("Right Ctrl", win::RCTRL),
        ("Left Shift", win::LSHIFT),
        ("Left Alt", win::LALT),
        ("Left Win", win::LWIN),
        ("Left arrow", win::LEFT),
        ("Home", win::HOME),
        ("Tab", win::TAB),
        ("F4", win::F4),
        ("Backspace", win::BACKSPACE),
    ];
    for &(name, c) in keys {
        p.key(c, true);
        p.key(c, false);
        let seen: Vec<Seen> = settle().into_iter().filter(|s| matches!(s, Seen::Key { .. })).collect();
        let (want_scan, want_ext, want_vk) = (code::low(c) as u32, code::is_extended(c), vk_for(c));
        let ok = seen.len() >= 2
            && matches!(seen[0], Seen::Key { scan, flags, up: false, vk } if scan == want_scan && (flags & LLKHF_EXTENDED != 0) == want_ext && vk == want_vk)
            && matches!(seen[seen.len() - 1], Seen::Key { up: true, .. });
        r.check(&format!("key {name}"), ok, || format!("expected vk {want_vk:#x} scan {want_scan:#x} ext {want_ext}, got {seen:?}"));
    }
    p.mask_key();
    let seen = settle();
    let mask = VK_MASK as u32;
    r.check(&format!("menu mask key (vk{mask:X})"), seen.iter().any(|s| matches!(s, Seen::Key { vk, up: false, .. } if *vk == mask)), || format!("{seen:?}"));

    // Buttons must not move the cursor. (The test's own moves were caught, so the real cursor
    // is wherever you left it; clicks must land there.)
    let at = p.cursor_pos();
    let buttons = [
        ("left", 0u8, WM_LBUTTONDOWN, WM_LBUTTONUP),
        ("right", 1, WM_RBUTTONDOWN, WM_RBUTTONUP),
        ("middle", 2, WM_MBUTTONDOWN, WM_MBUTTONUP),
        ("back (X1)", 3, WM_XBUTTONDOWN, WM_XBUTTONUP),
    ];
    for (name, b, down, up) in buttons {
        p.button(b, true);
        p.button(b, false);
        let seen = settle();
        let msgs: Vec<u32> = seen.iter().filter_map(|s| if let Seen::Mouse { msg, .. } = s { Some(*msg) } else { None }).collect();
        let still = seen.iter().all(|s| !matches!(s, Seen::Mouse { x, y, .. } if (*x, *y) != at));
        r.check(&format!("{name} button"), msgs == vec![down, up] && still, || format!("cursor at {at:?}, got {seen:?}"));
    }

    for (name, v, h) in [("wheel up one notch", 120, 0), ("wheel down a fine step", -30, 0), ("horizontal wheel", 0, 45)] {
        p.wheel(v, h);
        let seen = settle();
        let (msg, want) = if v != 0 { (WM_MOUSEWHEEL, v) } else { (WM_MOUSEHWHEEL, h) };
        let got = seen.iter().find_map(|s| match s {
            Seen::Mouse { msg: m, data, .. } if *m == msg => Some(((*data >> 16) as u16) as i16 as i32),
            _ => None,
        });
        r.check(name, got == Some(want), || format!("got {got:?} from {seen:?}"));
    }

    println!();
    println!("{} passed, {} failed. Secure desktop now: {}", r.passed, r.failed, if p.input_blocked() { "yes (locked or UAC)" } else { "no" });
    r.failed == 0
}

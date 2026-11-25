//! Thin macOS bindings: event tap, cursor control, displays, permissions.

#![allow(non_upper_case_globals, non_snake_case)]

use hop_core::layout::Rect;
use objc2::MainThreadMarker;
use std::ffi::{c_char, c_void};

pub type CGEventRef = *mut c_void;
pub type CFMachPortRef = *mut c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFRunLoopRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

pub type TapCallback = extern "C" fn(proxy: *mut c_void, etype: u32, event: CGEventRef, user: *mut c_void) -> CGEventRef;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(tap: u32, place: u32, options: u32, mask: u64, callback: TapCallback, user: *mut c_void) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(e: CGEventRef, field: u32) -> i64;
    fn CGEventGetDoubleValueField(e: CGEventRef, field: u32) -> f64;
    fn CGEventGetFlags(e: CGEventRef) -> u64;
    fn CGEventGetLocation(e: CGEventRef) -> CGPoint;
    fn CGEventCreate(source: *mut c_void) -> CGEventRef;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(d: u32) -> CGRect;
    fn CGMainDisplayID() -> u32;
    fn CGWarpMouseCursorPosition(p: CGPoint) -> i32;
    fn CGAssociateMouseAndMouseCursorPosition(connected: i32) -> i32;
    fn CGDisplayHideCursor(d: u32) -> i32;
    fn CGDisplayShowCursor(d: u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(alloc: *const c_void, port: CFMachPortRef, order: isize) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRelease(cf: *const c_void);
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, encoding: u32) -> CFStringRef;
    fn CFDictionaryCreate(
        alloc: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        n: isize,
        kcb: *const c_void,
        vcb: *const c_void,
    ) -> CFDictionaryRef;
    static kCFRunLoopCommonModes: CFStringRef;
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

// Event types and fields (CGEventTypes.h).
pub const T_LEFT_DOWN: u32 = 1;
pub const T_LEFT_UP: u32 = 2;
pub const T_RIGHT_DOWN: u32 = 3;
pub const T_RIGHT_UP: u32 = 4;
pub const T_MOVED: u32 = 5;
pub const T_LEFT_DRAGGED: u32 = 6;
pub const T_RIGHT_DRAGGED: u32 = 7;
pub const T_KEY_DOWN: u32 = 10;
pub const T_KEY_UP: u32 = 11;
pub const T_FLAGS: u32 = 12;
pub const T_SCROLL: u32 = 22;
pub const T_OTHER_DOWN: u32 = 25;
pub const T_OTHER_UP: u32 = 26;
pub const T_OTHER_DRAGGED: u32 = 27;
pub const T_TAP_DISABLED_TIMEOUT: u32 = 0xFFFF_FFFE;
pub const T_TAP_DISABLED_USER: u32 = 0xFFFF_FFFF;

const F_BUTTON_NUMBER: u32 = 3;
const F_DELTA_X: u32 = 4;
const F_DELTA_Y: u32 = 5;
const F_AUTOREPEAT: u32 = 8;
const F_KEYCODE: u32 = 9;
const F_SCROLL_FIXED_1: u32 = 93;
const F_SCROLL_FIXED_2: u32 = 94;
const F_SCROLL_POINT_1: u32 = 96;
const F_SCROLL_POINT_2: u32 = 97;
const F_SCROLL_CONTINUOUS: u32 = 88;

const SESSION_TAP: u32 = 1; // kCGSessionEventTap
const HEAD_INSERT: u32 = 0;
const OPTION_DEFAULT: u32 = 0; // active: may drop events

pub fn event_mask() -> u64 {
    [
        T_LEFT_DOWN, T_LEFT_UP, T_RIGHT_DOWN, T_RIGHT_UP, T_MOVED, T_LEFT_DRAGGED, T_RIGHT_DRAGGED, T_KEY_DOWN,
        T_KEY_UP, T_FLAGS, T_SCROLL, T_OTHER_DOWN, T_OTHER_UP, T_OTHER_DRAGGED,
    ]
    .iter()
    .fold(0u64, |m, t| m | (1u64 << t))
}

pub struct Ev(pub CGEventRef);

impl Ev {
    pub fn int(&self, f: u32) -> i64 {
        unsafe { CGEventGetIntegerValueField(self.0, f) }
    }
    pub fn double(&self, f: u32) -> f64 {
        unsafe { CGEventGetDoubleValueField(self.0, f) }
    }
    pub fn flags(&self) -> u64 {
        unsafe { CGEventGetFlags(self.0) }
    }
    pub fn loc(&self) -> (f64, f64) {
        let p = unsafe { CGEventGetLocation(self.0) };
        (p.x, p.y)
    }
    pub fn delta(&self) -> (f64, f64) {
        (self.double(F_DELTA_X), self.double(F_DELTA_Y))
    }
    pub fn button(&self) -> u8 {
        self.int(F_BUTTON_NUMBER).clamp(0, 7) as u8
    }
    pub fn keycode(&self) -> u16 {
        self.int(F_KEYCODE) as u16
    }
    pub fn autorepeat(&self) -> bool {
        self.int(F_AUTOREPEAT) != 0
    }
    /// (vertical, horizontal, continuous): points for trackpads, lines for wheels.
    pub fn scroll(&self) -> (f64, f64, bool) {
        if self.int(F_SCROLL_CONTINUOUS) != 0 {
            (self.double(F_SCROLL_POINT_1), self.double(F_SCROLL_POINT_2), true)
        } else {
            (self.double(F_SCROLL_FIXED_1), self.double(F_SCROLL_FIXED_2), false)
        }
    }
}

/// Create the active session-level tap. `None` means macOS refused (no Accessibility grant).
pub fn create_tap(cb: TapCallback, user: *mut c_void) -> Option<CFMachPortRef> {
    let t = unsafe { CGEventTapCreate(SESSION_TAP, HEAD_INSERT, OPTION_DEFAULT, event_mask(), cb, user) };
    (!t.is_null()).then_some(t)
}

pub fn enable_tap(tap: CFMachPortRef) {
    unsafe { CGEventTapEnable(tap, true) }
}

/// Attach the tap to this thread's run loop (the main loop then services it).
pub fn attach_tap(tap: CFMachPortRef) {
    unsafe {
        let src = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
        CFRunLoopAddSource(CFRunLoopGetCurrent(), src, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
    }
}

/// How this process shows itself.
pub enum AppKind {
    /// No Dock icon (the menu-bar app, `hop run`, the badge).
    Background,
    /// A Dock icon and a window of its own (`hop view`).
    Window,
}

/// Start AppKit for this process. Call once, on the main thread, before any window, badge or
/// menu.
pub fn start_app(kind: AppKind) -> MainThreadMarker {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let mtm = MainThreadMarker::new().expect("AppKit starts on the main thread");
    let policy = match kind {
        AppKind::Background => NSApplicationActivationPolicy::Accessory,
        AppKind::Window => NSApplicationActivationPolicy::Regular,
    };
    NSApplication::sharedApplication(mtm).setActivationPolicy(policy);
    mtm
}

/// Run AppKit's main loop (the event tap, the badge, windows) until the process exits.
pub fn run_app() -> ! {
    let mtm = MainThreadMarker::new().expect("AppKit runs on the main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).run();
    std::process::exit(0)
}

/// Whether we may tap input, without prompting.
pub fn accessibility_granted() -> bool {
    unsafe { AXIsProcessTrustedWithOptions(std::ptr::null()) != 0 }
}

/// Run `f` on the main thread, soon. Callable from any thread.
pub fn on_main(f: impl FnOnce() + Send + 'static) {
    extern "C" {
        static _dispatch_main_q: c_void;
        fn dispatch_async_f(queue: *const c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    }
    extern "C" fn trampoline(ctx: *mut c_void) {
        let f = unsafe { Box::from_raw(ctx as *mut Box<dyn FnOnce() + Send>) };
        f();
    }
    let boxed: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(f));
    unsafe { dispatch_async_f(&_dispatch_main_q, Box::into_raw(boxed) as *mut c_void, trampoline) }
}

/// Whether this binary runs from inside an app bundle (`….app/Contents/MacOS/`).
pub fn in_app_bundle() -> bool {
    std::env::current_exe().is_ok_and(|p| p.parent().is_some_and(|d| d.ends_with("Contents/MacOS")))
}

/// Whether we may tap input, asking for it (the system prompt) if not yet granted.
pub fn ask_for_accessibility() -> bool {
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let vals = [kCFBooleanTrue];
        let d = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            vals.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks as *const _,
            &kCFTypeDictionaryValueCallBacks as *const _,
        );
        let ok = AXIsProcessTrustedWithOptions(d) != 0;
        CFRelease(d);
        ok
    }
}

pub fn secure_input() -> bool {
    unsafe { IsSecureEventInputEnabled() != 0 }
}

pub fn cursor_location() -> (f64, f64) {
    unsafe {
        let e = CGEventCreate(std::ptr::null_mut());
        if e.is_null() {
            return (0.0, 0.0);
        }
        let p = CGEventGetLocation(e);
        CFRelease(e);
        (p.x, p.y)
    }
}

pub fn displays() -> Vec<Rect> {
    let mut ids = [0u32; 16];
    let mut n = 0u32;
    unsafe {
        if CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut n) != 0 {
            return Vec::new();
        }
    }
    ids[..n as usize]
        .iter()
        .map(|&d| {
            let b = unsafe { CGDisplayBounds(d) };
            Rect::new(b.origin.x, b.origin.y, b.origin.x + b.size.width, b.origin.y + b.size.height)
        })
        .collect()
}

/// Let a background process hide the cursor. Private CoreGraphics call (used the same way by
/// Synergy/Deskflow); looked up at run time so a missing symbol just means a visible cursor.
pub fn allow_background_cursor_hiding() {
    type DefaultConn = unsafe extern "C" fn() -> i32;
    type SetProp = unsafe extern "C" fn(i32, i32, CFStringRef, *const c_void) -> i32;
    unsafe {
        let conn = libc::dlsym(libc::RTLD_DEFAULT, c"_CGSDefaultConnection".as_ptr());
        let set = libc::dlsym(libc::RTLD_DEFAULT, c"CGSSetConnectionProperty".as_ptr());
        if conn.is_null() || set.is_null() {
            return;
        }
        let conn: DefaultConn = std::mem::transmute(conn);
        let set: SetProp = std::mem::transmute(set);
        let key = CFStringCreateWithCString(std::ptr::null(), c"SetsCursorInBackground".as_ptr(), 0x0800_0100);
        let c = conn();
        set(c, c, key, kCFBooleanTrue);
        CFRelease(key);
    }
}

pub fn hide_cursor() {
    unsafe { CGDisplayHideCursor(CGMainDisplayID()) };
}

pub fn show_cursor() {
    unsafe { CGDisplayShowCursor(CGMainDisplayID()) };
}

/// Frozen: mouse motion no longer moves the cursor (events still carry their deltas).
/// Unfreezing also cancels the 250 ms event suppression that follows a warp.
pub fn freeze_cursor(frozen: bool) {
    unsafe { CGAssociateMouseAndMouseCursorPosition(!frozen as i32) };
}

pub fn warp_cursor(at: (f64, f64)) {
    unsafe { CGWarpMouseCursorPosition(CGPoint { x: at.0, y: at.1 }) };
}

/// This process's physical memory footprint in MB (what Activity Monitor shows) and its total
/// CPU time in seconds.
pub fn usage() -> (f64, f64) {
    extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut libc::rusage_info_v2) -> i32;
    }
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let memory = if unsafe { proc_pid_rusage(libc::getpid(), 2, &mut info) } == 0 {
        info.ri_phys_footprint as f64 / 1_048_576.0
    } else {
        0.0
    };
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    (memory, t(u.ru_utime) + t(u.ru_stime))
}


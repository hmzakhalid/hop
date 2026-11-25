//! The on-screen indicator: a small pill at the top of the Mac's main screen that says the
//! keyboard and mouse are on the PC, and how to come back. It never takes focus or clicks.
//!
//! `show`/`hide` may be called from any thread; the AppKit work runs on the main thread.

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSBox, NSBoxType, NSColor, NSFont, NSPanel, NSScreen,
    NSStatusWindowLevel, NSTextField, NSTitlePosition, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

struct Hud {
    panel: Retained<NSPanel>,
    pill: Retained<NSBox>,
    label: Retained<NSTextField>,
}

thread_local! {
    static HUD: RefCell<Option<Hud>> = const { RefCell::new(None) };
}

static WANT: AtomicBool = AtomicBool::new(false);
static TEXT: Mutex<String> = Mutex::new(String::new());

/// The indicator's text while the PC at `side` has the keyboard and mouse.
pub fn badge_text(pc_name: &str, side: hop_core::layout::Side) -> String {
    format!("Keyboard & mouse → {pc_name}   ·   ⌃⌥⌘{} to come back", side.opposite().arrow())
}

pub fn show(text: &str) {
    *TEXT.lock().unwrap() = text.to_string();
    WANT.store(true, Ordering::SeqCst);
    schedule();
}

pub fn hide() {
    WANT.store(false, Ordering::SeqCst);
    schedule();
}

fn schedule() {
    crate::macos::on_main(update);
}

fn update() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    HUD.with(|cell| {
        let mut hud = cell.borrow_mut();
        if !WANT.load(Ordering::SeqCst) {
            if let Some(h) = hud.as_ref() {
                h.panel.orderOut(None);
            }
            return;
        }
        let h = hud.get_or_insert_with(|| build(mtm));
        let text = TEXT.lock().unwrap().clone();
        h.label.setStringValue(&NSString::from_str(&text));
        h.label.sizeToFit();
        let ls = h.label.frame().size;
        let (pad_x, pad_y) = (18.0, 9.0);
        let size = NSSize::new(ls.width + 2.0 * pad_x, ls.height + 2.0 * pad_y);
        h.label.setFrameOrigin(NSPoint::new(pad_x, pad_y));
        h.pill.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), size));
        let origin = match NSScreen::mainScreen(mtm) {
            Some(s) => {
                let vf = s.visibleFrame();
                NSPoint::new(vf.origin.x + (vf.size.width - size.width) / 2.0, vf.origin.y + vf.size.height - size.height - 10.0)
            }
            None => NSPoint::new(100.0, 100.0),
        };
        h.panel.setFrame_display(NSRect::new(origin, size), true);
        h.panel.orderFrontRegardless();
    });
}

fn build(mtm: MainThreadMarker) -> Hud {
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(300.0, 34.0));
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        rect,
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setLevel(NSStatusWindowLevel);
    panel.setIgnoresMouseEvents(true);
    panel.setOpaque(false);
    panel.setHasShadow(true);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    let pill = NSBox::new(mtm);
    pill.setBoxType(NSBoxType::Custom);
    pill.setTitlePosition(NSTitlePosition::NoTitle);
    pill.setBorderWidth(0.0);
    pill.setCornerRadius(17.0);
    pill.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(0.10, 0.36, 0.95, 0.92));
    let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    label.setTextColor(Some(&NSColor::whiteColor()));
    label.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
    pill.addSubview(&label);
    panel.setContentView(Some(&pill));
    Hud { panel, pill, label }
}

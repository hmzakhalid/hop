//! Hop.app: the menu-bar app. It runs the same live engine as `hop run`, and does in menus and
//! dialogs what the terminal would otherwise do:
//! - ask for Accessibility access;
//! - pair;
//! - change settings;
//! - check for updates.
//!
//! The menu is rebuilt each time it opens, from [`Shared`], which a background supervisor keeps
//! current by finding, connecting to and following the paired PC.

use crate::live::{self, Live, SignalsBlocked};
use crate::{hud, login, macos, pc, prefs, updates};
use hop_core::controller::{Controller, ControllerConfig};
use hop_core::keymap::mac;
use hop_core::layout::Side;
use hop_core::link::{Found, Link};
use hop_core::session::Session;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSApplication, NSControlStateValueOff, NSControlStateValueOn, NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSPopUpButton,
    NSStatusBar, NSStatusItem, NSTextField, NSVariableStatusItemLength, NSView, NSWorkspace,
};
use objc2_foundation::{ns_string, MainThreadMarker, NSDictionary, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL};
use std::cell::OnceCell;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A PC silent this long is gone (asleep, off, or at a new address).
const SILENT_AFTER: Duration = Duration::from_secs(15);
const RETRY: Duration = Duration::from_secs(3);
const ACCESSIBILITY_SETTINGS: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// Where things stand, for the menu.
#[derive(Clone, PartialEq)]
enum Conn {
    NeedsAccess,
    NotPaired,
    Looking(String),
    Unreachable(String),
    Connected { name: String, addr: SocketAddr, public: [u8; 32] },
}

impl Conn {
    fn describe(&self) -> String {
        match self {
            Conn::NeedsAccess => "Hop needs Accessibility access to start".into(),
            Conn::NotPaired => "Not paired with a PC".into(),
            Conn::Looking(name) => format!("Looking for {name}…"),
            Conn::Unreachable(name) => format!("{name} is not answering"),
            Conn::Connected { name, addr, .. } => format!("Connected to {name} ({})", addr.ip()),
        }
    }
}

/// What the menu (main thread) and the supervisor (background) share.
struct Shared {
    dir: PathBuf,
    prefs: Mutex<ControllerConfig>,
    /// Bumped on every settings change; the supervisor reconnects with the new settings.
    prefs_changed: AtomicU64,
    saving: Mutex<()>,
    conn: Mutex<Conn>,
    live: OnceLock<&'static Live>,
    signals: SignalsBlocked,
}

impl Shared {
    fn set_conn(&self, c: Conn) {
        *self.conn.lock().unwrap() = c;
    }

    /// Change the settings now; save them off the main thread, which also runs the event tap.
    fn change_prefs(self: &Arc<Self>, f: impl FnOnce(&mut ControllerConfig)) {
        f(&mut self.prefs.lock().unwrap());
        self.prefs_changed.fetch_add(1, Ordering::SeqCst);
        let s = self.clone();
        std::thread::spawn(move || s.save_prefs());
    }

    /// Write the newest settings. One writer at a time, each writing the newest settings, so the
    /// last write always holds the latest state. A failed write only loses the choice at the next
    /// launch.
    fn new(prefs: ControllerConfig, dir: PathBuf, conn: Conn, signals: SignalsBlocked) -> Arc<Shared> {
        Arc::new(Shared {
            prefs: Mutex::new(prefs),
            dir,
            prefs_changed: AtomicU64::new(0),
            saving: Mutex::new(()),
            conn: Mutex::new(conn),
            live: OnceLock::new(),
            signals,
        })
    }

    fn save_prefs(&self) {
        let _g = self.saving.lock().unwrap();
        let latest = self.prefs.lock().unwrap().clone();
        let _ = prefs::save(&self.dir, &latest);
    }
}

pub fn run() -> ! {
    let signals = live::block_signals(); // before any thread starts
    hud::init();
    let mtm = MainThreadMarker::new().expect("the app runs on the main thread");
    let dir = pc::store().dir;
    let shared = Shared::new(prefs::load(&dir), dir, Conn::NeedsAccess, signals);
    let s = shared.clone();
    live::on_quit(move || s.save_prefs()); // a save still in flight must not be lost
    let menu = Menu::new(mtm, shared.clone());
    std::mem::forget(menu); // lives as long as the app

    if macos::accessibility_trusted() {
        start_live(&shared);
    } else {
        let s = shared.clone();
        std::thread::spawn(move || {
            while !macos::accessibility_granted() {
                std::thread::sleep(Duration::from_secs(1));
            }
            macos::on_main(move || start_live(&s));
        });
    }
    let s = shared.clone();
    std::thread::spawn(move || supervise(&s));
    hud::run()
}

/// Main thread, once Accessibility access is granted.
fn start_live(shared: &Shared) {
    if shared.live.get().is_some() {
        return;
    }
    match Live::start(shared.signals, false) {
        Ok(live) => {
            let _ = shared.live.set(live);
            // First run: go straight to pairing instead of leaving people to find the menu.
            if pc::store().peer().is_none() {
                search_then_pair();
            }
        }
        Err(e) => {
            let mtm = MainThreadMarker::new().unwrap();
            show_alert(mtm, "Hop could not start", &format!("{e}. Quit Hop and open it again."), &["OK"], None);
        }
    }
}

/// Keep the live engine attached to the paired PC: find it, connect, follow it to a new
/// address, and reconnect when the pairing or the settings change.
fn supervise(shared: &Shared) {
    struct Current {
        public: [u8; 32],
        prefs: u64,
        /// The live link, which follows the PC's address as its packets arrive.
        link: Arc<Link>,
    }
    let mut current: Option<Current> = None;
    let mut next_try = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let Some(live) = shared.live.get().copied() else { continue };
        let Some(rec) = pc::store().peer() else {
            if current.take().is_some() {
                live.detach();
            }
            shared.set_conn(Conn::NotPaired);
            continue;
        };
        let prefs_now = shared.prefs_changed.load(Ordering::SeqCst);
        if let Some(c) = &current {
            let same = c.public == rec.public && c.prefs == prefs_now;
            let silent = c.link.quiet_for().is_none_or(|q| q > SILENT_AFTER);
            if same && !silent {
                shared.set_conn(Conn::Connected { name: rec.name.clone(), addr: c.link.peer(), public: rec.public });
                continue;
            }
            if same {
                // Silent: the link keeps trying the old address; move only if the PC has.
                shared.set_conn(Conn::Looking(rec.name.clone()));
                if Instant::now() < next_try {
                    continue;
                }
                next_try = Instant::now() + RETRY;
                match pc::locate(&rec, Duration::from_millis(600)) {
                    Some(a) if a != c.link.peer() => {}
                    _ => continue,
                }
            }
            live.detach();
            current = None;
        }
        if Instant::now() < next_try {
            continue;
        }
        next_try = Instant::now() + RETRY;
        shared.set_conn(Conn::Looking(rec.name.clone()));
        let Some(addr) = pc::locate(&rec, Duration::from_millis(600)) else { continue };
        match pc::open(rec.clone(), addr, Duration::from_secs(3)) {
            Err(_) => shared.set_conn(Conn::Unreachable(rec.name.clone())),
            Ok(_) if !pc::still_paired(&rec.public) => {} // forgotten or replaced meanwhile
            Ok(pc) => {
                let cfg = shared.prefs.lock().unwrap().clone();
                let effects = live.effects(&pc.name, cfg.side);
                let session = Session::new(Controller::new(cfg, macos::displays()), pc.link.clone(), effects);
                live.attach(session, |_| {});
                shared.set_conn(Conn::Connected { name: pc.name, addr: pc.link.peer(), public: rec.public });
                current = Some(Current { public: rec.public, prefs: prefs_now, link: pc.link });
            }
        }
    }
}

// ---- the menu --------------------------------------------------------------------------------

struct MenuIvars {
    shared: Arc<Shared>,
    item: OnceCell<Retained<NSStatusItem>>,
    updater: OnceCell<Retained<AnyObject>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Menu` does not implement Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = MenuIvars]
    struct Menu;

    unsafe impl NSObjectProtocol for Menu {}

    unsafe impl NSMenuDelegate for Menu {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            self.fill(menu);
        }
    }

    impl Menu {
        #[unsafe(method(switchToPC:))]
        fn switch_to_pc(&self, _: Option<&AnyObject>) {
            let side = self.shared().prefs.lock().unwrap().side;
            let arrow = match side {
                Side::Left => mac::LEFT,
                Side::Right => mac::RIGHT,
                Side::Top => mac::UP,
                Side::Bottom => mac::DOWN,
            };
            if let Some(s) = self.shared().live.get().and_then(|l| l.session()) {
                let at = macos::cursor_location();
                s.act(|c, now| c.key(now, arrow, true, false, mac::HYPER, at));
            }
        }

        #[unsafe(method(showScreen:))]
        fn show_screen(&self, _: Option<&AnyObject>) {
            let shared = self.shared();
            let Conn::Connected { name, addr, public } = shared.conn.lock().unwrap().clone() else { return };
            crate::screen::open(self.mtm(), crate::screen::Target { name, addr, public }, shared.live.get().copied(), || {});
        }

        #[unsafe(method(grantAccess:))]
        fn grant_access(&self, _: Option<&AnyObject>) {
            open_url(ACCESSIBILITY_SETTINGS);
        }

        #[unsafe(method(pairPC:))]
        fn pair_pc(&self, _: Option<&AnyObject>) {
            search_then_pair();
        }

        #[unsafe(method(forgetPC:))]
        fn forget_pc(&self, _: Option<&AnyObject>) {
            let name = pc::store().peer().map_or("the PC".into(), |r| r.name);
            let choice = show_alert(self.mtm(), &format!("Forget {name}?"), "Hop will stop using it until you pair again.", &["Forget", "Cancel"], None);
            if choice == 0 {
                if let Err(e) = pc::forget() {
                    show_alert(self.mtm(), "Could not forget the PC", &e.to_string(), &["OK"], None);
                }
            }
        }

        #[unsafe(method(setSide:))]
        fn set_side(&self, sender: &NSMenuItem) {
            let side = SIDES[sender.tag().clamp(0, 3) as usize].0;
            self.shared().change_prefs(|p| p.side = side);
        }

        #[unsafe(method(toggleEdges:))]
        fn toggle_edges(&self, _: Option<&AnyObject>) {
            self.shared().change_prefs(|p| p.edges = !p.edges);
        }

        #[unsafe(method(toggleCmdAsWin:))]
        fn toggle_cmd_as_win(&self, _: Option<&AnyObject>) {
            self.shared().change_prefs(|p| p.map.cmd_as_win = !p.map.cmd_as_win);
        }

        #[unsafe(method(toggleChords:))]
        fn toggle_chords(&self, _: Option<&AnyObject>) {
            self.shared().change_prefs(|p| p.map.chords = !p.map.chords);
        }

        #[unsafe(method(toggleLogin:))]
        fn toggle_login(&self, _: Option<&AnyObject>) {
            if let Err(e) = login::set(!login::enabled()) {
                show_alert(self.mtm(), "Could not change Open at Login", &e, &["OK"], None);
            }
        }

        #[unsafe(method(showAbout:))]
        fn show_about(&self, _: Option<&AnyObject>) {
            let app = NSApplication::sharedApplication(self.mtm());
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
            // The panel shows the version from Info.plist; this adds the commit it was built from.
            let build = NSString::from_str(&format!("build {}", hop_core::BUILD));
            let options = NSDictionary::from_slices(&[unsafe { objc2_app_kit::NSAboutPanelOptionVersion }], &[build.as_ref() as &AnyObject]);
            unsafe { app.orderFrontStandardAboutPanelWithOptions(&options) };
        }

        #[unsafe(method(quitHop:))]
        fn quit_hop(&self, _: Option<&AnyObject>) {
            live::quit()
        }
    }
);

/// The four places the PC can be, in menu order (the item's tag is the index).
const SIDES: [(Side, &str); 4] =
    [(Side::Right, "Right of this Mac"), (Side::Left, "Left of this Mac"), (Side::Top, "Above this Mac"), (Side::Bottom, "Below this Mac")];

impl Menu {
    fn new(mtm: MainThreadMarker, shared: Arc<Shared>) -> Retained<Menu> {
        let this = Self::alloc(mtm).set_ivars(MenuIvars { shared, item: OnceCell::new(), updater: OnceCell::new() });
        let this: Retained<Menu> = unsafe { msg_send![super(this), init] };
        if let Some(u) = updates::start() {
            let _ = this.ivars().updater.set(u);
        }
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        if let Some(button) = item.button(mtm) {
            let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(ns_string!("keyboard"), Some(ns_string!("Hop")));
            if let Some(image) = image {
                image.setTemplate(true);
                button.setImage(Some(&image));
            } else {
                button.setTitle(ns_string!("Hop"));
            }
        }
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        menu.setDelegate(Some(ProtocolObject::from_ref(&*this)));
        item.setMenu(Some(&menu));
        let _ = this.ivars().item.set(item);
        this
    }

    fn shared(&self) -> &Arc<Shared> {
        &self.ivars().shared
    }

    fn add(&self, menu: &NSMenu, title: &str, action: Option<Sel>) -> Retained<NSMenuItem> {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(self.mtm()), &NSString::from_str(title), action, ns_string!(""))
        };
        match action {
            Some(_) => unsafe { item.setTarget(Some(self)) },
            None => item.setEnabled(false),
        }
        menu.addItem(&item);
        item
    }

    fn check(item: &NSMenuItem, on: bool) {
        item.setState(if on { NSControlStateValueOn } else { NSControlStateValueOff });
    }

    /// Rebuild the menu from the current state; called each time it opens.
    fn fill(&self, menu: &NSMenu) {
        let mtm = self.mtm();
        menu.removeAllItems();
        let shared = self.shared();
        let conn = shared.conn.lock().unwrap().clone();
        let prefs = shared.prefs.lock().unwrap().clone();
        let live = shared.live.get().copied();

        self.add(menu, &conn.describe(), None);
        if live.is_some() && macos::secure_input() {
            // macOS reports only the last app to turn it on, which is often not the one holding it.
            self.add(menu, "Paused: Secure Input is on", None);
            self.add(menu, "   A password field or password manager is active. Close it,", None);
            self.add(menu, "   or lock and unlock the Mac (⌃⌘Q) to clear it.", None);
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        if conn == Conn::NeedsAccess {
            self.add(menu, "Allow Accessibility Access…", Some(sel!(grantAccess:)));
        }
        if let Conn::Connected { name, .. } = &conn {
            self.add(menu, &format!("Switch to {name}   ⌃⌥⌘{}", prefs.side.arrow()), Some(sel!(switchToPC:)));
            self.add(menu, &format!("Show {name}'s Screen"), Some(sel!(showScreen:)));
        }
        let paired = !matches!(conn, Conn::NotPaired | Conn::NeedsAccess) || pc::store().peer().is_some();
        self.add(menu, if paired { "Pair with Another PC…" } else { "Pair with a PC…" }, Some(sel!(pairPC:)));
        if paired {
            self.add(menu, "Forget This PC…", Some(sel!(forgetPC:)));
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let settings = NSMenu::new(mtm);
        settings.setAutoenablesItems(false);
        self.add(&settings, "Where is the PC?", None);
        for (i, (side, label)) in SIDES.iter().enumerate() {
            let item = self.add(&settings, &format!("   {label}"), Some(sel!(setSide:)));
            item.setTag(i as isize);
            Self::check(&item, prefs.side == *side);
        }
        settings.addItem(&NSMenuItem::separatorItem(mtm));
        Self::check(&self.add(&settings, "Switch by Pushing Past the Screen Edge", Some(sel!(toggleEdges:))), prefs.edges);
        Self::check(&self.add(&settings, "⌘ Is the Windows Key (⌃ Is Ctrl)", Some(sel!(toggleCmdAsWin:))), prefs.map.cmd_as_win);
        Self::check(&self.add(&settings, "Translate Mac Shortcuts (⌘Tab, ⌥←, …)", Some(sel!(toggleChords:))), prefs.map.chords);
        settings.addItem(&NSMenuItem::separatorItem(mtm));
        Self::check(&self.add(&settings, "Open at Login", Some(sel!(toggleLogin:))), login::enabled());
        let settings_item = self.add(menu, "Settings", None);
        settings_item.setEnabled(true);
        settings_item.setSubmenu(Some(&settings));

        if let Some(updater) = self.ivars().updater.get() {
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), ns_string!("Check for Updates…"), Some(sel!(checkForUpdates:)), ns_string!(""))
            };
            unsafe { item.setTarget(Some(updater)) };
            menu.addItem(&item);
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        self.add(menu, "About Hop", Some(sel!(showAbout:)));
        self.add(menu, "Quit Hop", Some(sel!(quitHop:)));
    }
}

/// `hop menu` (developer tool): build the real menu for a few states and print it, without
/// starting input capture or showing anything.
#[cfg(feature = "dev-tools")]
pub fn print_menus() {
    let mtm = MainThreadMarker::new().expect("main thread");
    hud::init();
    let states = [
        Conn::NeedsAccess,
        Conn::NotPaired,
        Conn::Looking("Desk PC".into()),
        Conn::Connected { name: "Desk PC".into(), addr: ([192, 168, 1, 20], 47820).into(), public: [0; 32] },
    ];
    let signals = live::block_signals();
    for conn in states {
        let shared = Shared::new(ControllerConfig::default(), std::env::temp_dir(), conn, signals);
        let m = Menu::new(mtm, shared);
        let menu = NSMenu::new(mtm);
        m.fill(&menu);
        println!("──────────");
        print_menu(&menu, 0);
        if let Some(item) = m.ivars().item.get() {
            NSStatusBar::systemStatusBar().removeStatusItem(item);
        }
    }
}

#[cfg(feature = "dev-tools")]
fn print_menu(menu: &NSMenu, depth: usize) {
    for item in menu.itemArray().iter() {
        if item.isSeparatorItem() {
            println!("{}----", "  ".repeat(depth));
            continue;
        }
        let state = if item.state() == NSControlStateValueOn { "✓ " } else { "  " };
        let enabled = if item.isEnabled() { "" } else { "  (disabled)" };
        let action = item.action().map_or(String::new(), |sel| {
            let answers = item.target().is_some_and(|t| unsafe { msg_send![&*t, respondsToSelector: sel] });
            let check = if answers || item.submenu().is_some() { "" } else { "  ✗ NO HANDLER" };
            format!("  → {}{check}", sel.name().to_string_lossy())
        });
        println!("{}{state}{}{enabled}{action}", "  ".repeat(depth), item.title());
        if let Some(sub) = item.submenu() {
            print_menu(&sub, depth + 1);
        }
    }
}

// ---- dialogs ---------------------------------------------------------------------------------

/// Show a modal alert; returns the index of the button pressed.
fn show_alert(mtm: MainThreadMarker, title: &str, info: &str, buttons: &[&str], accessory: Option<&NSView>) -> usize {
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(info));
    for b in buttons {
        alert.addButtonWithTitle(&NSString::from_str(b));
    }
    if let Some(v) = accessory {
        alert.setAccessoryView(Some(v));
        alert.layout();
        if let Some(first) = v.subviews().iter().last() {
            alert.window().makeFirstResponder(Some(&first));
        }
    }
    // NSAlertFirstButtonReturn is 1000, the second 1001, …
    (alert.runModal() - 1000).max(0) as usize
}

fn open_url(url: &str) {
    if let Some(u) = NSURL::URLWithString(&NSString::from_str(url)) {
        NSWorkspace::sharedWorkspace().openURL(&u);
    }
}

/// Search the network (off the main thread), then ask which PC and for its code.
fn search_then_pair() {
    std::thread::spawn(|| {
        let found = pc::discover(Duration::from_millis(1200));
        macos::on_main(move || ask_to_pair(found));
    });
}

fn ask_to_pair(found: Result<Vec<Found>, String>) {
    let mtm = MainThreadMarker::new().unwrap();
    let found = match found {
        Ok(f) if !f.is_empty() => f,
        other => {
            let why = other.err().unwrap_or_else(|| {
                "Install Hop on your Windows PC, on the same network as this Mac. Then, on the PC, open the Hop icon in the taskbar and choose \"Pair a Mac\".".into()
            });
            if show_alert(mtm, "No PC found", &why, &["Search Again", "Cancel"], None) == 0 {
                search_then_pair();
            }
            return;
        }
    };
    let width = 260.0;
    let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, 58.0)));
    let popup = NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), NSRect::new(NSPoint::new(0.0, 32.0), NSSize::new(width, 26.0)), false);
    for f in &found {
        popup.addItemWithTitle(&NSString::from_str(&format!("{} ({})", f.name, f.addr.ip())));
    }
    view.addSubview(&popup);
    let field = NSTextField::initWithFrame(NSTextField::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, 24.0)));
    field.setPlaceholderString(Some(ns_string!("Code shown on the PC")));
    view.addSubview(&field);
    let info = "On the PC, open the Hop icon in the taskbar and choose \"Pair a Mac\", then type the code it shows.";
    if show_alert(mtm, "Pair with a PC", info, &["Pair", "Cancel"], Some(&view)) != 0 {
        return;
    }
    let pick = popup.indexOfSelectedItem().clamp(0, found.len() as isize - 1) as usize;
    let (addr, code) = (found[pick].addr, field.stringValue().to_string());
    std::thread::spawn(move || {
        let r = pc::pair_with(addr, &code);
        macos::on_main(move || {
            let mtm = MainThreadMarker::new().unwrap();
            match r {
                Ok(rec) => {
                    let info = "Press ⌃⌥⌘ and the arrow toward the PC to use it; the same with the other arrow brings you back. The Settings menu sets where the PC is.";
                    show_alert(mtm, &format!("Paired with {}", rec.name), info, &["OK"], None);
                }
                Err(e) => {
                    if show_alert(mtm, "Pairing did not work", &e, &["Try Again", "Cancel"], None) == 0 {
                        search_then_pair();
                    }
                }
            }
        });
    });
}

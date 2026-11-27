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
use crate::{login, macos, pc, prefs, updates};
use hop_core::controller::ControllerConfig;
use hop_core::layout::Side;
use hop_core::link::{Found, Link};
use hop_core::ui::{HOTKEY, MAC_PAIR_ITEM, PC_PAIR_ITEM};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSApplication, NSControlStateValueOff, NSModalResponseContinue, NSControlStateValueOn, NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSPopUpButton,
    NSStatusBar, NSStatusItem, NSTextField, NSVariableStatusItemLength, NSView, NSWorkspace,
};
use objc2_foundation::{ns_string, MainThreadMarker, NSDictionary, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL};
use hop_core::store::PcRecord;
use std::cell::{Cell, OnceCell};
use std::net::{Shutdown, SocketAddr, TcpStream};
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

    /// Write the newest settings. One writer at a time, each writing the newest settings, so the
    /// last write always holds the latest state. A failed write only loses the choice at the next
    /// launch.
    fn save_prefs(&self) {
        let _g = self.saving.lock().unwrap();
        let latest = self.prefs.lock().unwrap().clone();
        let _ = prefs::save(&self.dir, &latest);
    }
}

pub fn run() -> ! {
    let signals = live::block_signals(); // before any thread starts
    let mtm = macos::start_app(macos::AppKind::Background);
    let dir = pc::store().dir;
    let shared = Shared::new(prefs::load(&dir), dir, Conn::NeedsAccess, signals);
    let s = shared.clone();
    live::on_quit(move || s.save_prefs()); // a save still in flight must not be lost
    let menu = Menu::new(mtm, shared.clone());
    std::mem::forget(menu); // lives as long as the app

    if macos::ask_for_accessibility() {
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
    macos::run_app()
}

/// Main thread, once Accessibility access is granted.
fn start_live(shared: &Shared) {
    if shared.live.get().is_some() {
        return;
    }
    match Live::start(shared.signals, false) {
        Ok(live) => {
            let _ = shared.live.set(live);
            // A Mac that has never paired goes straight to pairing, rather than leaving people to
            // find the menu. (After a Forget, it waits to be asked.)
            if pc::never_paired() {
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
        let mut moved_to = None;
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
                match pc::locate(&rec) {
                    Some(a) if a != c.link.peer() => moved_to = Some(a),
                    _ => continue,
                }
            }
            live.detach();
            current = None;
        }
        let addr = match moved_to {
            Some(a) => a, // just found at a new address: connect there now
            None => {
                if Instant::now() < next_try {
                    continue;
                }
                next_try = Instant::now() + RETRY;
                shared.set_conn(Conn::Looking(rec.name.clone()));
                let Some(a) = pc::locate(&rec) else { continue };
                a
            }
        };
        match pc::open(rec.clone(), addr) {
            Err(_) => shared.set_conn(Conn::Unreachable(rec.name.clone())),
            Ok(_) if !pc::still_paired(&rec.public) => {} // forgotten or replaced meanwhile
            Ok(pc) => {
                let cfg = shared.prefs.lock().unwrap().clone();
                live.connect(pc.link.clone(), &pc.name, cfg, |_| {});
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
            if let Some(s) = self.shared().live.get().and_then(|l| l.session()) {
                let at = macos::cursor_location();
                s.act(|c, now| c.switch(now, at, 0));
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
            self.add(menu, &format!("Switch to {name}   {HOTKEY}{}", prefs.side.arrow()), Some(sel!(switchToPC:)));
            self.add(menu, &format!("Show {name}'s Screen"), Some(sel!(showScreen:)));
        }
        // Each state knows, except before Accessibility, when the supervisor has not run yet.
        let paired = match conn {
            Conn::NotPaired => false,
            Conn::NeedsAccess => pc::store().peer().is_some(),
            Conn::Looking(_) | Conn::Unreachable(_) | Conn::Connected { .. } => true,
        };
        let pair = if paired { "Pair with Another PC…".to_string() } else { format!("{MAC_PAIR_ITEM}…") };
        self.add(menu, &pair, Some(sel!(pairPC:)));
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
    let mtm = macos::start_app(macos::AppKind::Background);
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

thread_local! {
    /// A pairing dialog is up, or about to be (main thread).
    static PAIRING: Cell<bool> = const { Cell::new(false) };
}

/// Pair with a PC: search the network (off the main thread), then offer the PCs found. Main
/// thread. Does nothing while a pairing is already under way.
fn search_then_pair() {
    if !PAIRING.with(|p| p.replace(true)) {
        search();
    }
}

fn search() {
    std::thread::spawn(|| {
        let found = pc::discover();
        macos::on_main(move || match choose_pc(found) {
            Choice::Ask(pc) => ask(pc),
            Choice::Code(found) => match ask_for_code(&found) {
                Some((addr, code)) => pair_with_code(addr, code),
                None => done(),
            },
            Choice::SearchAgain => search(),
            Choice::Cancel => done(),
        });
    });
}

/// The pairing dialogs have closed.
fn done() {
    PAIRING.with(|p| p.set(false));
}

enum Choice {
    Ask(Found),
    Code(Vec<Found>),
    SearchAgain,
    Cancel,
}

/// A menu of the PCs found, `y` points up a view `height` tall and `width` wide.
fn pc_menu(mtm: MainThreadMarker, found: &[Found], y: f64, width: f64, height: f64) -> (Retained<NSView>, Retained<NSPopUpButton>) {
    let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)));
    let popup = NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), NSRect::new(NSPoint::new(0.0, y), NSSize::new(width, 26.0)), false);
    for f in found {
        popup.addItemWithTitle(&NSString::from_str(&format!("{} ({})", f.name, f.addr.ip())));
    }
    view.addSubview(&popup);
    (view, popup)
}

fn picked(popup: &NSPopUpButton, found: &[Found]) -> usize {
    popup.indexOfSelectedItem().clamp(0, found.len() as isize - 1) as usize
}

/// Offer the PCs found: connect to one (it asks whoever is there to allow this Mac), or pair with
/// a code.
fn choose_pc(found: Result<Vec<Found>, String>) -> Choice {
    let mtm = MainThreadMarker::new().unwrap();
    let found = match found {
        Ok(f) if !f.is_empty() => f,
        other => {
            let why = other.err().unwrap_or_else(|| {
                "Install Hop on your Windows PC (gethop.vercel.app), on the same network as this Mac, then search again.".into()
            });
            return if show_alert(mtm, "No PC found", &why, &["Search Again", "Cancel"], None) == 0 { Choice::SearchAgain } else { Choice::Cancel };
        }
    };
    let buttons = ["Connect", "Use a Code…", "Cancel"];
    let choice = if let [pc] = &found[..] {
        let info = format!("Hop found {} on your network. The PC will ask whoever is there to allow this Mac.", pc.name);
        (show_alert(mtm, &format!("Connect to {}?", pc.name), &info, &buttons, None), 0)
    } else {
        let (view, popup) = pc_menu(mtm, &found, 0.0, 260.0, 26.0);
        let info = "Hop found these PCs on your network. The one you choose asks whoever is there to allow this Mac.";
        let button = show_alert(mtm, "Connect to a PC", info, &buttons, Some(&view));
        (button, picked(&popup, &found))
    };
    match choice {
        (0, pick) => Choice::Ask(found[pick].clone()),
        (1, _) => Choice::Code(found),
        _ => Choice::Cancel,
    }
}

/// A request to pair while it runs: the asking thread fills it in, the dialog reads it.
#[derive(Default)]
struct Request {
    /// The number both screens show, once known.
    number: Option<String>,
    /// Shut down to give up, once the number is known.
    cancel: Option<TcpStream>,
    cancelled: bool,
    result: Option<Result<PcRecord, String>>,
}

/// Ask `pc` to pair: the asking runs on its own thread, while a dialog says "Asking…", then
/// "Click Yes on the PC" with the number, until the answer or Cancel. Main thread.
fn ask(pc: Found) {
    let req = Arc::new(Mutex::new(Request::default()));
    let name = pc.name.clone();
    let shared = req.clone();
    std::thread::spawn(move || {
        let asked = pc::ask(pc.addr, &pc.name, Some(pc.public), |number, cancel| {
            let mut r = shared.lock().unwrap();
            if r.cancelled {
                let _ = cancel.shutdown(Shutdown::Both);
            }
            (r.number, r.cancel) = (Some(number), Some(cancel));
        });
        // Under the lock, so Cancel either comes first (nothing is stored) or finds the result.
        let mut r = shared.lock().unwrap();
        r.result = Some(match asked {
            Ok(_) if r.cancelled => return,
            Ok(allowed) => allowed.keep(),
            Err(e) => Err(e),
        });
    });
    let mtm = MainThreadMarker::new().unwrap();
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(&format!("Asking {name}…")));
    alert.setInformativeText(ns_string!("Hop is reaching the PC."));
    alert.addButtonWithTitle(ns_string!("Cancel"));
    let mut shown = false;
    let answer = alert_until(mtm, &alert, || {
        let mut r = req.lock().unwrap();
        if let Some(result) = r.result.take() {
            return Some(result);
        }
        if let (false, Some(number)) = (shown, &r.number) {
            shown = true;
            alert.setMessageText(&NSString::from_str(&format!("Click Yes on {name}")));
            alert.setInformativeText(&NSString::from_str(&format!(
                "{name} is asking whoever is there to allow this Mac. It shows this number too; if its number is different, click No there.\n\n{number}"
            )));
            alert.layout();
        }
        None
    });
    let answer = answer.or_else(|| {
        // Cancel: give up, unless the answer is already in (and stored).
        let mut r = req.lock().unwrap();
        r.result.take().or_else(|| {
            r.cancelled = true;
            if let Some(c) = r.cancel.take() {
                let _ = c.shutdown(Shutdown::Both);
            }
            None
        })
    });
    match answer {
        Some(result) => finished(result),
        None => done(),
    }
}

/// Show `alert` until `poll` returns something (it is checked about 50 times a second), or until
/// a button is pressed (`None`). It polls because it runs inside a block from the main queue,
/// which runs no other block until this one returns: an answer sent there would wait for it.
fn alert_until<T>(mtm: MainThreadMarker, alert: &NSAlert, mut poll: impl FnMut() -> Option<T>) -> Option<T> {
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    alert.layout();
    let window = alert.window();
    window.center();
    let session = app.beginModalSessionForWindow(&window);
    let result = loop {
        // Handles the waiting events, including a button, which ends the session.
        if unsafe { app.runModalSession(session) } != NSModalResponseContinue {
            break None;
        }
        if let Some(t) = poll() {
            break Some(t);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    unsafe { app.endModalSession(session) };
    window.orderOut(None);
    result
}

/// Ask for the code a PC shows; `None` if cancelled.
fn ask_for_code(found: &[Found]) -> Option<(SocketAddr, String)> {
    let mtm = MainThreadMarker::new().unwrap();
    let width = 260.0;
    let (view, popup) = pc_menu(mtm, found, 32.0, width, 58.0);
    let field = NSTextField::initWithFrame(NSTextField::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, 24.0)));
    field.setPlaceholderString(Some(ns_string!("Code shown on the PC")));
    view.addSubview(&field);
    let info = format!("On the PC, click the Hop icon in the taskbar and choose \"{PC_PAIR_ITEM}\", then type the code it shows.");
    if show_alert(mtm, "Pair with a Code", &info, &["Pair", "Cancel"], Some(&view)) != 0 {
        return None;
    }
    Some((found[picked(&popup, found)].addr, field.stringValue().to_string()))
}

fn pair_with_code(addr: SocketAddr, code: String) {
    std::thread::spawn(move || {
        let r = pc::pair_with(addr, &code);
        macos::on_main(move || finished(r));
    });
}

/// Say how pairing ended, and close the pairing dialogs (or start over). Main thread.
fn finished(r: Result<PcRecord, String>) {
    let mtm = MainThreadMarker::new().unwrap();
    match r {
        Ok(rec) => {
            let info = format!("Press {HOTKEY} and the arrow toward the PC to use it; the same with the other arrow brings you back. The Settings menu sets where the PC is.");
            show_alert(mtm, &format!("Paired with {}", rec.name), &info, &["OK"], None);
        }
        Err(e) => {
            if show_alert(mtm, "Pairing did not work", &e, &["Try Again", "Cancel"], None) == 0 {
                return search();
            }
        }
    }
    done();
}

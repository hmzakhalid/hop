//! The live machinery that `hop run` and the menu-bar app share: the event tap, the watchdog,
//! signal handling, the system watcher, and what a crossing means on the Mac. It drives whichever
//! [`Session`] is attached, so the app can connect, reconnect or re-pair without rebuilding any
//! of it.

use crate::cursor::Cursor;
use crate::hud;
use crate::macos::{self, Ev};
use hop_core::clock::now_us;
use hop_core::controller::{Effect, LeaveReason, Output};
use hop_core::layout::Side;
use hop_core::link::Event;
use hop_core::session::{Effects, Session};
use std::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

fn signal_set() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in SIGNALS {
            libc::sigaddset(&mut set, s);
        }
        set
    }
}

/// Proof that [`block_signals`] ran, which [`Live::start`] requires.
pub struct SignalsBlocked(());

/// Block Ctrl+C, `kill` and hang-up in this thread and every thread it starts, so only
/// [`Live`]'s waiter receives them and can give the cursor back first. Call before starting any
/// thread: threads inherit the mask they start with.
pub fn block_signals() -> SignalsBlocked {
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &signal_set(), std::ptr::null_mut()) };
    SignalsBlocked(())
}

struct Attached {
    session: Arc<Session>,
    stop: Arc<AtomicBool>,
}

pub struct Live {
    /// Written only to swap sessions (never held across a blocking call), so the tap and the
    /// watchdog never wait on it for long.
    current: RwLock<Option<Attached>>,
    cursor: Arc<Cursor>,
    port: AtomicPtr<c_void>,
    verbose: bool,
    /// Secure Input as last seen, given to every session attached later.
    secure_input: AtomicBool,
}

impl Live {
    /// Install the event tap on this (the main) thread's run loop and start the watchdog, the
    /// signal waiter and the system watcher. Fails without Accessibility access.
    pub fn start(_: SignalsBlocked, verbose: bool) -> Result<&'static Live, String> {
        let live: &'static Live = Box::leak(Box::new(Live {
            current: RwLock::new(None),
            cursor: Arc::new(Cursor::default()),
            port: AtomicPtr::new(std::ptr::null_mut()),
            verbose,
            secure_input: AtomicBool::new(macos::secure_input()),
        }));
        let port = macos::create_tap(on_event, live as *const Live as *mut c_void)
            .ok_or("macOS refused the input tap; check Accessibility access")?;
        live.port.store(port, Ordering::SeqCst);
        macos::attach_tap(port);
        macos::allow_background_cursor_hiding();
        live.wait_for_signals();
        live.watchdog();
        live.watch_system();
        Ok(live)
    }

    pub fn session(&self) -> Option<Arc<Session>> {
        self.current.read().unwrap().as_ref().map(|a| a.session.clone())
    }

    /// What a crossing means on the Mac, for sessions with the PC called `pc_name` at `side`.
    pub fn effects(&self, pc_name: &str, side: Side) -> Arc<dyn Effects> {
        Arc::new(MacEffects { cursor: self.cursor.clone(), badge: hud::badge_text(pc_name, side), pc_name: pc_name.into(), verbose: self.verbose })
    }

    /// End the live session, if any: the Mac gets its input back.
    pub fn detach(&self) {
        let old = self.current.write().unwrap().take();
        if let Some(old) = old {
            retire(old);
        }
    }

    /// Make `session` the live one and run its threads. A previous session hands control back
    /// and stops.
    pub fn attach(&self, session: Arc<Session>, on_event: impl Fn(&io::Result<Event>) + Send + Sync + 'static) {
        let secure = self.secure_input.load(Ordering::SeqCst);
        session.act(|c, now| c.set_secure_input(now, secure));
        let stop = Arc::new(AtomicBool::new(false));
        session.spawn(stop.clone(), on_event);
        let old = self.current.write().unwrap().replace(Attached { session, stop });
        if let Some(old) = old {
            retire(old);
        }
    }

    /// Print round trip, memory and CPU every 5 seconds.
    pub fn print_stats(&'static self) {
        std::thread::spawn(move || {
            let mut last_cpu = macos::usage().1;
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let (memory, cpu) = macos::usage();
                let rtt = match self.session().and_then(|s| s.read(|c| c.rtt_summary())) {
                    Some(r) => format!("round trip p50 {:.2} ms p99 {:.2} ms ({})", r.p50, r.p99, r.count),
                    None => "no round trips yet".into(),
                };
                println!("stats: {rtt} · memory {memory:.1} MB · cpu {:.2}%", (cpu - last_cpu) / 5.0 * 100.0);
                last_cpu = cpu;
            }
        });
    }

    /// Ctrl+C, `kill`, closing the terminal: give the cursor back, tell the PC, exit.
    fn wait_for_signals(&'static self) {
        std::thread::spawn(move || {
            let mut sig = 0;
            unsafe { libc::sigwait(&signal_set(), &mut sig) };
            self.cursor.release(None);
            if let Some(s) = self.session() {
                s.try_act(|c, now| c.hand_back(now, LeaveReason::Quit));
            }
            println!();
            std::process::exit(0);
        });
    }

    /// Takes the cursor back at the lease deadline without the controller lock, so it works even
    /// if the session's threads are stuck.
    fn watchdog(&'static self) {
        let deadline = move || self.session().map_or(0, |s| s.lease_deadline());
        std::thread::spawn(move || loop {
            let (d, now) = (deadline(), now_us());
            if d == 0 || now < d {
                let wait = if d == 0 { 5_000 } else { (d - now).clamp(50, 5_000) };
                std::thread::sleep(Duration::from_micros(wait));
                continue;
            }
            if self.cursor.revoke_if(|| {
                let d = deadline();
                d != 0 && now_us() >= d
            }) {
                hud::hide();
                if self.verbose {
                    eprintln!("watchdog: no answer from the PC, the Mac has its input back");
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        });
    }

    /// Secure Input (password fields) and display changes, which macOS does not push to us. Each
    /// poll also reconciles the current session with Secure Input, so a change
    /// that raced a session swap is corrected within 100 ms.
    fn watch_system(&'static self) {
        std::thread::spawn(move || {
            let mut displays_at = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let secure = macos::secure_input();
                self.secure_input.store(secure, Ordering::SeqCst);
                let Some(session) = self.session() else { continue };
                if session.read(|c| c.secure_input() != secure) {
                    session.act(|c, now| c.set_secure_input(now, secure));
                }
                if displays_at.elapsed() > Duration::from_secs(2) {
                    displays_at = Instant::now();
                    let displays = macos::displays();
                    session.act(|c, _| {
                        c.set_mac_displays(displays);
                        Output::default()
                    });
                }
            }
        });
    }
}

/// End a session for good. Retiring (rather than only handing back) matters: a tap callback that
/// fetched this session just before the swap may still run an event through it, and that must
/// not capture the cursor where no watchdog is watching any more.
fn retire(old: Attached) {
    old.session.act(|c, now| c.retire(now));
    old.stop.store(true, Ordering::Relaxed);
}

/// Cursor and badge: what a crossing means on the Mac.
struct MacEffects {
    cursor: Arc<Cursor>,
    badge: String,
    pc_name: String,
    verbose: bool,
}

impl Effects for MacEffects {
    fn apply(&self, effect: Effect) {
        match effect {
            Effect::Enter => {
                self.cursor.capture();
                hud::show(&self.badge);
                if self.verbose {
                    println!("→ {}", self.pc_name);
                }
            }
            Effect::Leave { warp_to, reason } => {
                self.cursor.release(Some(warp_to));
                hud::hide();
                if self.verbose {
                    println!("← Mac ({reason:?})");
                }
            }
        }
    }
}

const MOTION: [u32; 4] = [macos::T_MOVED, macos::T_LEFT_DRAGGED, macos::T_RIGHT_DRAGGED, macos::T_OTHER_DRAGGED];

extern "C" fn on_event(_proxy: *mut c_void, etype: u32, event: macos::CGEventRef, user: *mut c_void) -> macos::CGEventRef {
    let live = unsafe { &*(user as *const Live) };
    if etype == macos::T_TAP_DISABLED_TIMEOUT || etype == macos::T_TAP_DISABLED_USER {
        // Re-enable even with no session attached, or switching stays dead until a restart.
        macos::enable_tap(live.port.load(Ordering::SeqCst));
        if let Some(s) = live.session() {
            s.act(|c, now| c.hand_back(now, LeaveReason::TapDisabled));
        }
        return event;
    }
    let Some(session) = live.session() else { return event };
    if live.cursor.revoked() {
        // The watchdog took over: never swallow. Bring the controller along if it is free.
        if session.try_act(|c, now| c.hand_back(now, LeaveReason::Watchdog)).is_some() && session.lease_deadline() == 0 {
            live.cursor.settle();
        }
        return event;
    }
    let deadline = session.lease_deadline();
    if deadline != 0 && now_us() >= deadline {
        return event; // the PC went quiet; the session is about to hand control back
    }
    let ev = Ev(event);
    let flags = ev.flags();
    let swallow = session.act(|c, now| {
        let out = match etype {
            t if MOTION.contains(&t) => c.mouse_moved(now, ev.loc(), ev.delta(), flags, t != macos::T_MOVED),
            macos::T_LEFT_DOWN | macos::T_LEFT_UP => c.mouse_button(now, 0, etype == macos::T_LEFT_DOWN),
            macos::T_RIGHT_DOWN | macos::T_RIGHT_UP => c.mouse_button(now, 1, etype == macos::T_RIGHT_DOWN),
            macos::T_OTHER_DOWN | macos::T_OTHER_UP => c.mouse_button(now, ev.button(), etype == macos::T_OTHER_DOWN),
            macos::T_KEY_DOWN | macos::T_KEY_UP => c.key(now, ev.keycode(), etype == macos::T_KEY_DOWN, ev.autorepeat(), flags, ev.loc()),
            macos::T_FLAGS => c.flags_changed(now, ev.keycode(), flags),
            macos::T_SCROLL => {
                let (v, h, continuous) = ev.scroll();
                c.scroll(now, v, h, continuous)
            }
            _ => Output::default(),
        };
        // Still under the controller lock, so this cannot race a crossing.
        if out.swallow && out.effect.is_none() && c.is_remote() && MOTION.contains(&etype) {
            live.cursor.keep_parked(ev.loc());
        }
        out
    });
    if swallow {
        std::ptr::null_mut()
    } else {
        event
    }
}

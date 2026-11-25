//! The live screen view: the PC's screen in a Mac window, over the video channel.
//!
//! Threads:
//! - the receiver puts frames back together and hands them to the hardware decoder;
//! - the requester keeps asking the PC for frames while the window is open;
//! - with control, the cursor follower draws the PC cursor from the Mac's own position, so the
//!   cursor never waits for the network or the encoder.
//!
//! The window itself lives on the main thread (`WINDOW`); the threads reach it with
//! [`macos::on_main`].

use crate::live::Live;
use crate::view::{self, Pushed, Window};
use crate::{macos, pc};
use hop_core::stream::{Client, Received};
use hop_core::video::{self, Frame};
use objc2::MainThreadMarker;
use std::cell::RefCell;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

thread_local! {
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
}

/// How often the PC cursor is redrawn while the Mac drives it (above any display's refresh).
const CURSOR_EVERY: Duration = Duration::from_micros(4_000);

/// The PC answers every request (10 a second). This long without an answer says so in the
/// title, so a frozen picture is never mistaken for live. (A still screen sends no frames, so
/// frames alone cannot tell.)
const SILENT_AFTER: Duration = Duration::from_secs(2);

/// The PC to watch.
pub struct Target {
    pub name: String,
    /// The PC's input address; video is on the next port.
    pub addr: SocketAddr,
    pub public: [u8; 32],
}

/// Open the viewer, or bring it forward if it is already open. Main thread. With `live`, the
/// cursor follows the Mac's own control of the PC.
pub fn open(mtm: MainThreadMarker, target: Target, live: Option<&'static Live>) {
    let already = WINDOW.with(|w| w.borrow().as_ref().is_some_and(|w| w.is_open()));
    if already {
        objc2_app_kit::NSApplication::sharedApplication(mtm).activate();
        return;
    }
    let (window, decoder) = Window::open(mtm, &format!("{} — connecting…", target.name), 2560, 1440);
    WINDOW.with(|w| *w.borrow_mut() = Some(window));
    std::thread::spawn(move || run(target, decoder, live));
}

fn with_window(f: impl FnOnce(&Window) + Send + 'static) {
    macos::on_main(move || WINDOW.with(|w| {
        if let Some(w) = w.borrow().as_ref() {
            f(w);
        }
    }));
}

fn run(target: Target, mut decoder: view::Decoder, live: Option<&'static Live>) {
    let name = target.name.clone();
    let video_addr = SocketAddr::new(target.addr.ip(), target.addr.port().wrapping_add(video::PORT_OFFSET));
    let client = match pc::store().keys().map_err(|e| e.to_string()).and_then(|keys| {
        Client::connect(keys, target.public, video_addr, Duration::from_secs(3)).map_err(|e| e.to_string())
    }) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("{name} — no picture: {e}. Is Hop up to date on the PC?");
            with_window(move |w| w.set_title(&msg));
            return;
        }
    };
    let open = Arc::new(AtomicBool::new(true));
    let local_cursor = Arc::new(AtomicBool::new(false));
    request_frames(client.clone(), open.clone());
    if let Some(live) = live {
        follow_cursor(live, open.clone(), local_cursor.clone());
    }

    let mut stats = Stats::new();
    let (mut size, mut note, mut hevc) = (None::<(u32, u32)>, None::<String>, false);
    while open.load(Ordering::Relaxed) {
        let received = match client.recv() {
            Ok(r) => r,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(50));
                None
            }
        };
        match received {
            Some(Received::Frame(frame)) => {
                if frame.keyframe {
                    if let Some(dims) = view::dimensions(&frame.data, frame.hevc).filter(|d| Some(*d) != size) {
                        size = Some(dims);
                        with_window(move |w| w.set_picture_size(dims.0, dims.1));
                    }
                }
                let t = Instant::now();
                let pushed = decoder.push(&frame.data, frame.hevc);
                if pushed == Pushed::NeedKeyframe {
                    decoder.reset();
                    client.decoder_failed();
                    continue;
                }
                stats.shown(&frame, client.age_us(&frame), t.elapsed());
                (note, hevc) = (None, frame.hevc);
            }
            Some(Received::Cursor(at)) => show_cursor(&local_cursor, at),
            Some(Received::Note(text)) => {
                if note.as_ref() != Some(&text) {
                    println!("{name} — {text}");
                }
                note = Some(text);
            }
            None => {}
        }
        if stats.since.elapsed() >= Duration::from_secs(1) {
            let silent = client.silent_for().unwrap_or_default();
            let title = match &note {
                _ if silent >= SILENT_AFTER => format!("{name} — no answer from the PC for {} s (reconnecting)", silent.as_secs()),
                Some(n) => format!("{name} — {n}"),
                None => stats.line(&name, size, hevc, &client),
            };
            if note.is_none() {
                println!("{title}"); // for `hop view` in a terminal
            }
            with_window(move |win| win.set_title(&title));
            stats = Stats::new();
        }
    }
    let _ = client.want(false); // the PC stops capturing
}

/// Draw the PC pointer the PC reported, unless the Mac is driving it (then the Mac's own
/// position is newer).
fn show_cursor(local: &AtomicBool, at: Option<(i32, i32)>) {
    if !local.load(Ordering::Relaxed) {
        let c = at.map(|(x, y)| (x as f64, y as f64));
        with_window(move |w| w.set_cursor(c));
    }
}

/// One second of viewer numbers, for the title.
struct Stats {
    since: Instant,
    shown: u32,
    age_sum: u64,
    age_n: u64,
    push_us: u64,
    /// Time on the PC from grabbing the image to sending it.
    pc_us: u64,
    pc_max: u32,
}

impl Stats {
    fn new() -> Stats {
        Stats { since: Instant::now(), shown: 0, age_sum: 0, age_n: 0, push_us: 0, pc_us: 0, pc_max: 0 }
    }

    fn shown(&mut self, f: &Frame, age_us: Option<u64>, push: Duration) {
        self.shown += 1;
        if let Some(a) = age_us {
            (self.age_sum, self.age_n) = (self.age_sum + a, self.age_n + 1);
        }
        self.push_us += push.as_micros() as u64;
        self.pc_us += f.pc_us as u64;
        self.pc_max = self.pc_max.max(f.pc_us);
    }

    fn line(&self, name: &str, size: Option<(u32, u32)>, hevc: bool, client: &Client) -> String {
        let fps = self.shown as f64 / self.since.elapsed().as_secs_f64();
        let (w, h) = size.unwrap_or((0, 0));
        let n = self.shown.max(1) as f64;
        // How long since the PC grabbed a frame: capture, encode, network and reassembly.
        let arrive = if self.age_n > 0 { format!(" · {:.1} ms to arrive", self.age_sum as f64 / self.age_n as f64 / 1000.0) } else { String::new() };
        let (_, lost, repaired) = client.counts();
        format!(
            "{name} — {w}×{h} {} · {fps:.0} fps · {:.0} Mbps · network {:.1} ms{arrive} (on the PC {:.1} ms, max {:.1}) · {:.2} ms to queue for display · {repaired} repaired, {lost} lost",
            if hevc { "HEVC" } else { "H.264" },
            client.bitrate() as f64 / 1e6,
            client.network_delay_us() as f64 / 1000.0,
            self.pc_us as f64 / n / 1000.0,
            self.pc_max as f64 / 1000.0,
            self.push_us as f64 / n / 1000.0,
        )
    }
}

/// Ask for frames while the window is open; notice when it closes.
fn request_frames(client: Arc<Client>, open: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while open.load(Ordering::Relaxed) {
            let _ = client.want(true);
            let o = open.clone();
            macos::on_main(move || {
                let still = WINDOW.with(|w| {
                    let mut w = w.borrow_mut();
                    let open = w.as_ref().is_some_and(|w| w.is_open());
                    if !open {
                        *w = None; // closed by the user: release it
                    }
                    open
                });
                o.store(still, Ordering::Relaxed);
            });
            std::thread::sleep(Duration::from_micros(video::WANT_EVERY_US));
        }
    });
}

/// While the Mac drives the PC, draw the PC cursor where the Mac is putting it.
fn follow_cursor(live: &'static Live, open: Arc<AtomicBool>, local: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut last = None;
        while open.load(Ordering::Relaxed) {
            let at = live.session().and_then(|s| s.read(|c| c.pc_cursor()));
            local.store(at.is_some(), Ordering::Relaxed);
            if at.is_some() && at != last {
                with_window(move |w| w.set_cursor(at));
            }
            last = at;
            std::thread::sleep(CURSOR_EVERY);
        }
    });
}

/// `hop view`: watch the paired PC's screen from a terminal (no keyboard or mouse control).
pub fn view_cli(host: Option<&str>) -> ! {
    let rec = pc::store().peer().unwrap_or_else(|| crate::cli::die("not paired yet. Pair first (hop pair)."));
    let addr = match host {
        Some(h) => pc::parse_host(h),
        None => pc::locate(&rec, Duration::from_millis(600)).unwrap_or_else(|| crate::cli::die(format!("cannot find {} on the network", rec.name))),
    };
    crate::hud::init();
    let mtm = MainThreadMarker::new().expect("main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Regular);
    open(mtm, Target { name: rec.name, addr, public: rec.public }, None);
    // Quit when the window closes.
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_millis(500));
        let gone = Arc::new(AtomicBool::new(false));
        let g = gone.clone();
        macos::on_main(move || g.store(WINDOW.with(|w| w.borrow().is_none()), Ordering::Relaxed));
        std::thread::sleep(Duration::from_millis(100));
        if gone.load(Ordering::Relaxed) {
            std::process::exit(0);
        }
    });
    crate::hud::run()
}

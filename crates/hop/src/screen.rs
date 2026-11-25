//! The live screen view: the PC's screen in a Mac window, over the video channel.
//!
//! Threads:
//! - the receiver puts frames back together and hands them to the hardware decoder (its
//!   [`Client`] keeps asking the PC for frames until the window closes);
//! - with control, the cursor follower draws the PC cursor from the Mac's own position, so the
//!   cursor never waits for the network or the encoder.
//!
//! Each opening of the window is one viewer with its own id and stop flag. The window lives on
//! the main thread (`CURRENT`); a viewer's threads reach it with [`macos::on_main`], and only
//! while it is still theirs. Closing the window stops them.

use crate::live::Live;
use crate::view::{self, Pushed, Window};
use crate::{macos, pc};
use hop_core::stream::{Client, Received};
use hop_core::video::{self, Frame};
use objc2::MainThreadMarker;
use std::cell::RefCell;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The open viewer window, and which viewer it belongs to.
struct Current {
    id: u64,
    window: Window,
}

thread_local! {
    static CURRENT: RefCell<Option<Current>> = const { RefCell::new(None) };
    /// A window just closed, released once its close notification has returned.
    static CLOSED: RefCell<Option<Current>> = const { RefCell::new(None) };
}
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

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
/// cursor follows the Mac's own control of the PC. `on_close` runs after the user closes it.
pub fn open(mtm: MainThreadMarker, target: Target, live: Option<&'static Live>, on_close: impl FnOnce() + 'static) {
    if CURRENT.with(|c| c.borrow().is_some()) {
        objc2_app_kit::NSApplication::sharedApplication(mtm).activate();
        return;
    }
    let viewer = Viewer { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), stop: Arc::new(AtomicBool::new(false)) };
    let closing = viewer.clone();
    let (window, decoder) = Window::open(mtm, &format!("{} — connecting…", target.name), 2560, 1440, move || {
        closing.stop.store(true, Ordering::Relaxed);
        let gone = CURRENT.with(|c| c.borrow_mut().take_if(|c| c.id == closing.id));
        CLOSED.with(|c| *c.borrow_mut() = gone);
        macos::on_main(|| drop(CLOSED.with(|c| c.borrow_mut().take())));
        on_close();
    });
    CURRENT.with(|c| *c.borrow_mut() = Some(Current { id: viewer.id, window }));
    std::thread::spawn(move || run(viewer, target, decoder, live));
}

/// One opening of the window: its threads run until `stop`, and touch only their own window.
#[derive(Clone)]
struct Viewer {
    id: u64,
    stop: Arc<AtomicBool>,
}

impl Viewer {
    fn open(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }

    fn with_window(&self, f: impl FnOnce(&Window) + Send + 'static) {
        let id = self.id;
        macos::on_main(move || {
            CURRENT.with(|c| {
                if let Some(c) = c.borrow().as_ref().filter(|c| c.id == id) {
                    f(&c.window);
                }
            })
        });
    }
}

fn run(viewer: Viewer, target: Target, mut decoder: view::Decoder, live: Option<&'static Live>) {
    let name = target.name.clone();
    let video_addr = video::port_for(target.addr.port()).map(|p| SocketAddr::new(target.addr.ip(), p)).ok_or("the PC's port is too high");
    let client = match video_addr.map_err(String::from).and_then(|addr| {
        let keys = pc::store().keys().map_err(|e| e.to_string())?;
        Client::connect(keys, target.public, addr, Duration::from_secs(3)).map_err(|e| e.to_string())
    }) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("{name} — no picture: {e}. Is Hop up to date on the PC?");
            viewer.with_window(move |w| w.set_title(&msg));
            return;
        }
    };
    let local_cursor = Arc::new(AtomicBool::new(false));
    if let Some(live) = live {
        follow_cursor(live, viewer.clone(), local_cursor.clone());
    }

    let mut stats = Stats::new();
    let (mut size, mut note, mut hevc) = (None::<(u32, u32)>, None::<String>, false);
    while viewer.open() {
        let received = match client.recv() {
            Ok(r) => r,
            Err(e) => {
                note = Some(format!("cannot reach the PC: {e}"));
                std::thread::sleep(Duration::from_millis(50)); // the error came at once
                None
            }
        };
        match received {
            Some(Received::Frame(frame)) => {
                let image = &frame.encoded;
                if image.keyframe {
                    if let Some(dims) = view::dimensions(&image.data, image.hevc).filter(|d| Some(*d) != size) {
                        size = Some(dims);
                        viewer.with_window(move |w| w.set_picture_size(dims.0, dims.1));
                    }
                }
                let t = Instant::now();
                let pushed = decoder.push(&image.data, image.hevc);
                if pushed == Pushed::NeedKeyframe {
                    decoder.reset();
                    client.decoder_failed();
                    continue;
                }
                stats.shown(&frame, client.age_us(&frame), t.elapsed());
                (note, hevc) = (None, image.hevc);
            }
            Some(Received::Cursor(at)) => show_cursor(&viewer, &local_cursor, at),
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
            viewer.with_window(move |win| win.set_title(&title));
            stats = Stats::new();
        }
    }
    client.stop();
}

/// Draw the PC pointer the PC reported, unless the Mac is driving it (then the Mac's own
/// position is newer).
fn show_cursor(viewer: &Viewer, local: &AtomicBool, at: Option<(i32, i32)>) {
    if !local.load(Ordering::Relaxed) {
        let c = at.map(|(x, y)| (x as f64, y as f64));
        viewer.with_window(move |w| w.set_cursor(c));
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
        self.pc_us += f.encoded.pc_us as u64;
        self.pc_max = self.pc_max.max(f.encoded.pc_us);
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

/// While the Mac drives the PC, draw the PC cursor where the Mac is putting it.
fn follow_cursor(live: &'static Live, viewer: Viewer, local: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut last = None;
        while viewer.open() {
            let at = live.session().and_then(|s| s.read(|c| c.pc_cursor()));
            local.store(at.is_some(), Ordering::Relaxed);
            if at.is_some() && at != last {
                viewer.with_window(move |w| w.set_cursor(at));
            }
            last = at;
            std::thread::sleep(CURSOR_EVERY);
        }
    });
}

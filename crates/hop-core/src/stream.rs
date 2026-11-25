//! The video channel at both ends: the PC's [`serve`] loop and the Mac's [`Client`]. The rules
//! for frames are in [`crate::video`]; this module moves them over an encrypted session.

use crate::clock::now_us;
use crate::crypto::StaticKeys;
use crate::link::{Event, Link};
use crate::responder::{Datagram, Responder};
use crate::video::{self, ClockOffset, Encoded, Fragmenter, Frame, RateControl, Reassembler};
use crate::wire::Want;
use crate::wire::{Msg, MAX_PACKET};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What [`Source::poll`] saw.
pub struct Polled {
    /// The image changed since the last poll.
    pub changed: bool,
    /// The pointer, in pixels of the streamed monitor, while visible.
    pub cursor: Option<(i32, i32)>,
}

/// Where frames come from: the screen capturer on Windows, recorded or synthetic frames in tests.
///
/// Looking and encoding are separate, so the pointer can be followed every few milliseconds
/// while frames are encoded at a steady pace from the newest image.
pub trait Source {
    /// Wait up to `timeout_ms` for the screen or the pointer to change; keep the newest image.
    fn poll(&mut self, timeout_ms: u32) -> io::Result<Polled>;

    /// Encode the newest image (as a keyframe with `keyframe`). Everything the encoder handed
    /// back, in order: usually one frame, none before the first image.
    fn encode(&mut self, keyframe: bool) -> io::Result<Vec<Encoded>>;

    /// Aim for this many bits per second from now on.
    fn set_bitrate(&mut self, _bps: u32) {}
}

/// Longest wait for the screen to change in one pass, so the pointer and requests stay fresh.
const POLL_MAX_US: u64 = 4_000;
/// Capture problems are repeated to the viewer this often while they last.
const NOTE_EVERY_US: u64 = 2_000_000;
/// While nobody watches, how often to look for a request.
const IDLE_POLL: Duration = Duration::from_millis(10);
/// After the capturer fails to open, wait this long before trying again.
const REOPEN_AFTER_US: u64 = 1_000_000;

/// Serve the screen to the paired Mac until `stop`. `paired` gives the paired Mac's key now (it
/// may change while serving); `open` starts the capturer when someone begins to watch, and it is
/// dropped again after [`video::IDLE_RELEASE_US`] without a viewer.
pub fn serve(
    sock: UdpSocket,
    keys: &StaticKeys,
    paired: impl Fn() -> Option<[u8; 32]>,
    mut open: impl FnMut() -> io::Result<Box<dyn Source>>,
    stop: &AtomicBool,
    log: impl Fn(&str),
) -> io::Result<()> {
    sock.set_read_timeout(Some(IDLE_POLL))?;
    let mut responder = Responder::new(keys.clone());
    let mut source: Option<Box<dyn Source>> = None;
    let (mut last_want, mut key_wanted, mut last_key, mut frame_id) = (0u64, false, 0u64, 0u32);
    let (mut idle_since, mut reopen_at) = (None::<u64>, 0u64);
    let mut buf = vec![0u8; MAX_PACKET];
    let mut plain = Vec::with_capacity(MAX_PACKET);
    let mut watching = false;
    let mut fragmenter = Fragmenter::default();
    let mut rate = RateControl::default();
    let mut last_frame = 0u64;
    let frame_interval = 1_000_000 / video::MAX_FPS as u64;
    // The image changed since the last frame went out.
    let mut dirty = false;
    let (mut cursor, mut cursor_seq) = (None::<(i32, i32)>, 0u32);
    let (mut note, mut noted_at) = (None::<String>, 0u64);

    while !stop.load(Ordering::Relaxed) {
        // 1. Requests: everything waiting, without blocking while a frame is due.
        sock.set_nonblocking(watching)?;
        loop {
            let (n, from) = match sock.recv_from(&mut buf) {
                Ok(x) => x,
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::ConnectionReset) => break,
                Err(e) => return Err(e),
            };
            let now = now_us();
            match responder.handle(&sock, &buf[..n], from, &paired, &mut plain) {
                Datagram::Data { .. } => {
                    if let Some(Msg::VideoWant(w)) = Msg::decode(&plain) {
                        if w.watch {
                            if now.saturating_sub(last_want) > video::WANT_TIMEOUT_US {
                                key_wanted = true; // a new viewer starts from a keyframe
                                rate = RateControl::default();
                                if let Some(s) = source.as_mut() {
                                    s.set_bitrate(rate.bitrate);
                                }
                            }
                            if let (Some(b), Some(s)) = (rate.report(now, w.delay_us), source.as_mut()) {
                                s.set_bitrate(b);
                                log(&format!("video: {} Mbps (delay {:.1} ms)", b / 1_000_000, w.delay_us as f64 / 1000.0));
                            }
                            last_want = now;
                            key_wanted |= w.keyframe;
                        } else {
                            last_want = 0;
                        }
                        responder.send(&sock, &Msg::VideoClock { mac_time_us: w.mac_time_us, pc_time_us: now_us(), bitrate: rate.bitrate });
                        // The pointer again, in case its last move was lost.
                        if cursor_seq != 0 {
                            responder.send(&sock, &Msg::VideoCursor { seq: cursor_seq, at: cursor });
                        }
                    }
                }
                Datagram::Handled | Datagram::Other => {}
            }
            if !watching {
                break; // idle: one packet per poll is plenty
            }
        }

        // Why there is no picture, for the viewer.
        if let Some(n) = &note {
            if watching && now_us().saturating_sub(noted_at) >= NOTE_EVERY_US {
                noted_at = now_us();
                responder.send(&sock, &Msg::VideoNote(n.clone()));
            }
        }

        // 2. Frames, while someone watches.
        let now = now_us();
        watching = responder.connected() && last_want != 0 && now.saturating_sub(last_want) < video::WANT_TIMEOUT_US;
        if !watching {
            if source.is_some() && now.saturating_sub(*idle_since.get_or_insert(now)) > video::IDLE_RELEASE_US {
                source = None; // frees the capturer and the encoder
                log("video: no viewer, capture stopped");
            }
            continue;
        }
        idle_since = None;
        if source.is_none() {
            if now < reopen_at {
                std::thread::sleep(IDLE_POLL);
                continue;
            }
            match open() {
                Ok(mut s) => {
                    s.set_bitrate(rate.bitrate);
                    source = Some(s);
                    key_wanted = true;
                    note = None;
                    log("video: capture started");
                }
                Err(e) => {
                    log(&format!("video: cannot start capture: {e}"));
                    note = Some(format!("The PC cannot capture its screen right now: {e}"));
                    reopen_at = now + REOPEN_AFTER_US;
                    continue;
                }
            }
        }
        // Look often (the pointer, and the newest image), but encode no faster than MAX_FPS and
        // not at all while a queue on the path drains.
        let due = last_frame + frame_interval;
        let wait_ms = (due.saturating_sub(now).clamp(1_000, POLL_MAX_US) / 1_000) as u32;
        let polled = match source.as_mut().unwrap().poll(wait_ms) {
            Ok(p) => p,
            Err(e) => {
                log(&format!("video: capture failed, restarting it: {e}"));
                note = Some(format!("The PC's screen capture stopped: {e}"));
                source = None;
                reopen_at = now_us() + REOPEN_AFTER_US;
                continue;
            }
        };
        dirty |= polled.changed;
        if polled.cursor != cursor {
            cursor = polled.cursor;
            cursor_seq = cursor_seq.wrapping_add(1);
            responder.send(&sock, &Msg::VideoCursor { seq: cursor_seq, at: cursor });
        }
        let now = now_us();
        let force = key_wanted && now.saturating_sub(last_key) >= video::KEYFRAME_SPACING_US;
        if now < due || rate.paused(now) || !(dirty || force) {
            continue;
        }
        let encoded = match source.as_mut().unwrap().encode(force) {
            Ok(e) if e.is_empty() => continue,
            Ok(e) => e,
            Err(e) => {
                log(&format!("video: encoding failed, restarting capture: {e}"));
                note = Some(format!("The PC's video encoder stopped: {e}"));
                source = None;
                reopen_at = now_us() + REOPEN_AFTER_US;
                continue;
            }
        };
        dirty = false;
        last_frame = now; // pace from when this frame was due, so encode and send time do not slow the cadence
        sock.set_nonblocking(false)?; // a full send buffer waits rather than drops
        for e in encoded {
            if e.keyframe {
                key_wanted = false;
                last_key = now;
            }
            frame_id = frame_id.wrapping_add(1);
            let frame = Frame { id: frame_id, encoded: e };
            for f in fragmenter.split(&frame) {
                responder.send(&sock, &Msg::VideoFrag(f));
            }
        }
    }
    Ok(())
}

/// What [`Client::recv`] produced.
pub enum Received {
    Frame(Frame),
    /// The PC pointer moved (pixels of the streamed monitor), or was hidden.
    Cursor(Option<(i32, i32)>),
    /// Why there is no picture.
    Note(String),
}

/// The Mac's end: asks the PC to stream and puts frames back together.
pub struct Client {
    link: Link,
    frames: Mutex<Reassembler>,
    clock: Mutex<ClockOffset>,
    /// A keyframe request went out since the last loss was noticed.
    asked: AtomicBool,
    /// The viewer is open: only then does a loss trigger a request of its own.
    watching: AtomicBool,
    /// Smoothed first-packet delay (µs), reported to the PC's rate control.
    delay: AtomicU64,
    /// A frame updated `delay` since the last report; otherwise the report says "unknown".
    fresh: AtomicBool,
    /// The frame being received, and when its first packet arrived.
    first_packet: Mutex<(u32, u64)>,
    /// The PC's bitrate, as it last said.
    bitrate: AtomicU64,
    /// The newest cursor update seen, so an older one arriving late is ignored.
    cursor_seq: Mutex<Option<u32>>,
}

impl Client {
    /// Connect to the PC's video channel at `addr`.
    pub fn connect(keys: StaticKeys, pc_public: [u8; 32], addr: SocketAddr, timeout: Duration) -> io::Result<Arc<Client>> {
        let link = Link::new(UdpSocket::bind("0.0.0.0:0")?, keys, pc_public, addr)?;
        link.connect(timeout)?;
        Ok(Arc::new(Client {
            link,
            frames: Mutex::new(Reassembler::default()),
            clock: Mutex::new(ClockOffset::default()),
            asked: AtomicBool::new(false),
            watching: AtomicBool::new(false),
            delay: AtomicU64::new(0),
            fresh: AtomicBool::new(false),
            bitrate: AtomicU64::new(0),
            first_packet: Mutex::new((0, 0)),
            cursor_seq: Mutex::new(None),
        }))
    }

    /// Keep the stream coming (or stop it). Call every [`video::WANT_EVERY_US`] while watching.
    pub fn want(&self, watch: bool) -> io::Result<()> {
        self.watching.store(watch, Ordering::Relaxed);
        self.link.maintain()?;
        let (keyframe, complete, lost) = {
            let r = self.frames.lock().unwrap();
            (r.wants_keyframe(), r.complete, r.lost)
        };
        self.asked.store(keyframe, Ordering::Relaxed);
        let delay_us = if self.fresh.swap(false, Ordering::Relaxed) { self.delay.load(Ordering::Relaxed).min(u32::MAX as u64) as u32 } else { 0 };
        self.link.send(&Msg::VideoWant(Want { watch, keyframe, mac_time_us: now_us(), complete, lost, delay_us }))
    }

    /// The decoder could not use what it was given: start again from a keyframe.
    pub fn decoder_failed(&self) {
        self.frames.lock().unwrap().need_keyframe();
        let _ = self.want(true);
    }

    /// Wait (up to the link's short timeout) for the next complete frame, cursor move or note.
    pub fn recv(&self) -> io::Result<Option<Received>> {
        match self.link.recv()? {
            Event::Msg(Msg::VideoFrag(f)) => {
                let arrived = now_us();
                let first_arrival = {
                    let mut fp = self.first_packet.lock().unwrap();
                    if fp.0 != f.frame || fp.1 == 0 {
                        *fp = (f.frame, arrived);
                    }
                    fp.1
                };
                let (frame, needs_key) = {
                    let mut r = self.frames.lock().unwrap();
                    let frame = r.push(f);
                    (frame, r.wants_keyframe())
                };
                let captured = frame.as_ref().and_then(|f| Some((self.clock.lock().unwrap().to_mac(f.encoded.captured_us)?, f.encoded.pc_us)));
                if let Some((captured_mac, pc_us)) = captured {
                    let transit = first_arrival.saturating_sub(captured_mac).saturating_sub(pc_us as u64).max(1);
                    let d = self.delay.load(Ordering::Relaxed);
                    self.delay.store(if d == 0 { transit } else { (d * 7 + transit) / 8 }, Ordering::Relaxed);
                    self.fresh.store(true, Ordering::Relaxed);
                }
                // Ask at once on loss, rather than at the next regular request.
                if needs_key && !self.asked.load(Ordering::Relaxed) && self.watching.load(Ordering::Relaxed) {
                    let _ = self.want(true);
                }
                Ok(frame.map(Received::Frame))
            }
            Event::Msg(Msg::VideoCursor { seq, at }) => {
                let mut last = self.cursor_seq.lock().unwrap();
                // Only newer moves: the PC repeats the latest one in case it was lost.
                if last.is_some_and(|l| seq == l || seq.wrapping_sub(l) >= u32::MAX / 2) {
                    return Ok(None);
                }
                *last = Some(seq);
                Ok(Some(Received::Cursor(at)))
            }
            Event::Msg(Msg::VideoNote(text)) => Ok(Some(Received::Note(text))),
            // A new session can mean a new PC process (the PC restarted): its frame numbers,
            // clock and cursor updates all start again.
            Event::Connected => {
                self.frames.lock().unwrap().restart();
                self.clock.lock().unwrap().restart();
                *self.cursor_seq.lock().unwrap() = None;
                *self.first_packet.lock().unwrap() = (0, 0);
                if self.watching.load(Ordering::Relaxed) {
                    let _ = self.want(true);
                }
                Ok(None)
            }
            Event::Msg(Msg::VideoClock { mac_time_us, pc_time_us, bitrate }) => {
                self.clock.lock().unwrap().sample(mac_time_us, pc_time_us, now_us());
                self.bitrate.store(bitrate as u64, Ordering::Relaxed);
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// How long ago (µs) the PC grabbed this frame, once the clocks are matched.
    pub fn age_us(&self, f: &Frame) -> Option<u64> {
        self.clock.lock().unwrap().to_mac(f.encoded.captured_us).map(|t| now_us().saturating_sub(t))
    }

    /// How long since anything authentic came from the PC. It answers every request, so a
    /// long silence means it is gone (a still screen sends no frames, but still answers).
    pub fn silent_for(&self) -> Option<Duration> {
        self.link.quiet_for()
    }

    /// The smoothed first-packet delay the PC's rate control sees (µs).
    pub fn network_delay_us(&self) -> u64 {
        self.delay.load(Ordering::Relaxed)
    }

    /// The PC's current bitrate (bits per second); 0 until it says.
    pub fn bitrate(&self) -> u64 {
        self.bitrate.load(Ordering::Relaxed)
    }

    /// Frames shown, frames lost, and shards rebuilt from parity, so far.
    pub fn counts(&self) -> (u32, u32, u32) {
        let r = self.frames.lock().unwrap();
        (r.complete, r.lost, r.repaired)
    }
}

/// Plays a recorded H.264 stream (from `hop-capture`) as if it were the screen: for testing the
/// whole viewer path on any machine. A keyframe request restarts at the recording's start.
pub struct FileSource {
    frames: Vec<(Vec<u8>, bool)>,
    at: usize,
    every: std::time::Duration,
    next_at: std::time::Instant,
}

impl FileSource {
    pub fn open(path: &std::path::Path, fps: u32) -> io::Result<FileSource> {
        use crate::h264::{access_units, nal_type, nal_units, NAL_IDR};
        let data = std::fs::read(path)?;
        let frames: Vec<(Vec<u8>, bool)> = access_units(&data)
            .into_iter()
            .map(|f| {
                let key = nal_units(&f).iter().any(|n| nal_type(n) == NAL_IDR);
                (f, key)
            })
            .collect();
        if !frames.first().is_some_and(|f| f.1) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "the recording must start with a keyframe"));
        }
        let every = Duration::from_micros(1_000_000 / fps.max(1) as u64);
        Ok(FileSource { frames, at: 0, every, next_at: std::time::Instant::now() })
    }
}

impl Source for FileSource {
    fn poll(&mut self, timeout_ms: u32) -> io::Result<Polled> {
        let wait = self.next_at.saturating_duration_since(std::time::Instant::now());
        let limit = Duration::from_millis(timeout_ms as u64);
        std::thread::sleep(wait.min(limit));
        Ok(Polled { changed: wait <= limit, cursor: None })
    }

    fn encode(&mut self, keyframe: bool) -> io::Result<Vec<Encoded>> {
        self.next_at = std::time::Instant::now() + self.every;
        if keyframe || self.at >= self.frames.len() {
            self.at = 0;
        }
        let (data, key) = self.frames[self.at].clone();
        self.at += 1;
        Ok(vec![Encoded { data, keyframe: key, hevc: false, captured_us: now_us(), pc_us: 0 }])
    }
}

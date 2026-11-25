//! The video channel end to end over loopback: the real server and client, a synthetic screen,
//! and a proxy that drops datagrams.

use hop_core::clock::now_us;
use hop_core::crypto::StaticKeys;
use hop_core::stream::{self, Client, Polled, Received, Source};
use hop_core::video::{self, Encoded, Frame};
use hop_core::wire::MAX_PACKET;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// A screen that changes every 2 ms, with a pointer that moves every poll. Each frame's bytes
/// encode its own number and length, so the Mac can check it arrived intact.
struct Synthetic {
    n: u32,
    polls: i32,
    /// The image changes on this many polls, then stays still (the pointer keeps moving).
    changing_polls: i32,
    opened: Arc<AtomicU32>,
}

impl Source for Synthetic {
    fn poll(&mut self, _timeout_ms: u32) -> io::Result<Polled> {
        thread::sleep(Duration::from_millis(2));
        self.polls += 1;
        Ok(Polled { changed: self.polls <= self.changing_polls, cursor: Some((self.polls, 2)) })
    }

    fn encode(&mut self, keyframe: bool) -> io::Result<Vec<Encoded>> {
        self.n += 1;
        let len = if keyframe { 20_000 } else { 3_000 + (self.n as usize % 5) * 700 };
        Ok(vec![Encoded { data: pattern(self.n, len), keyframe, hevc: true, captured_us: now_us(), pc_us: 100 }])
    }
}

impl Drop for Synthetic {
    fn drop(&mut self) {
        self.opened.fetch_sub(1, Ordering::SeqCst);
    }
}

fn pattern(n: u32, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(&n.to_le_bytes());
    v.extend_from_slice(&(len as u32).to_le_bytes());
    v.extend((8..len).map(|i| ((i as u32).wrapping_mul(31).wrapping_add(n) % 251) as u8));
    v
}

fn intact(f: &Frame) -> bool {
    let data = &f.encoded.data;
    data.len() >= 8 && {
        let n = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let len = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
        len == data.len() && *data == pattern(n, len)
    }
}

struct Pc {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    capturers: Arc<AtomicU32>,
}

impl Drop for Pc {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn start_pc(pc: StaticKeys, mac_public: [u8; 32]) -> Pc {
    start_pc_on(UdpSocket::bind("127.0.0.1:0").unwrap(), pc, mac_public, i32::MAX)
}

fn start_pc_on(sock: UdpSocket, pc: StaticKeys, mac_public: [u8; 32], changing_polls: i32) -> Pc {
    let addr = sock.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let capturers = Arc::new(AtomicU32::new(0));
    let (s, c) = (stop.clone(), capturers.clone());
    thread::spawn(move || {
        let open = || -> io::Result<Box<dyn Source>> {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Synthetic { n: 0, polls: 0, changing_polls, opened: c.clone() }))
        };
        stream::serve(sock, &pc, || Some(mac_public), open, &s, |_| {}).unwrap();
    });
    Pc { addr, stop, capturers }
}

/// Forwards datagrams both ways, dropping `drop_pct` % of the PC's video.
fn proxy(to: SocketAddr, drop_pct: u32) -> (SocketAddr, Arc<AtomicBool>) {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    let addr = sock.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    thread::spawn(move || {
        let (mut client, mut seed, mut buf) = (None, 0x9E37_79B9_7F4A_7C15u64, [0u8; MAX_PACKET]);
        while !s2.load(Ordering::Relaxed) {
            let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
            if from == to {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                // Handshake replies (type 0x21) always pass, so the session can form.
                if buf[0] != 0x21 && (seed % 100) < drop_pct as u64 {
                    continue;
                }
                if let Some(c) = client {
                    let _ = sock.send_to(&buf[..n], c);
                }
            } else {
                client = Some(from);
                let _ = sock.send_to(&buf[..n], to);
            }
        }
    });
    (addr, stop)
}

/// Watch for `for_how_long`, returning the frames shown.
fn watch(client: &Arc<Client>, for_how_long: Duration) -> Vec<Frame> {
    watch_all(client, for_how_long).0
}

/// Watch for `for_how_long`, returning the frames shown and the cursor moves seen.
fn watch_all(client: &Arc<Client>, for_how_long: Duration) -> (Vec<Frame>, Vec<(i32, i32)>) {
    let t = Instant::now();
    let (mut frames, mut cursors) = (Vec::new(), Vec::new());
    while t.elapsed() < for_how_long {
        match client.recv().unwrap() {
            Some(Received::Frame(f)) => frames.push(f),
            Some(Received::Cursor(Some(c))) => cursors.push(c),
            _ => {}
        }
    }
    (frames, cursors)
}

fn setup(drop_pct: u32) -> (Pc, Arc<Client>, Option<Arc<AtomicBool>>) {
    let (mac, pc) = (StaticKeys::generate(), StaticKeys::generate());
    let pc_public = pc.public;
    let server = start_pc(pc, mac.public);
    let (addr, proxy_stop) = if drop_pct > 0 {
        let (a, s) = proxy(server.addr, drop_pct);
        (a, Some(s))
    } else {
        (server.addr, None)
    };
    let client = Client::connect(mac, pc_public, addr, Duration::from_secs(3)).unwrap();
    (server, client, proxy_stop)
}

#[test]
fn the_picture_comes_back_after_the_pc_restarts() {
    let (mac, pc) = (StaticKeys::generate(), StaticKeys::generate());
    let first = start_pc(pc.clone(), mac.public);
    let addr = first.addr;
    let client = Client::connect(mac.clone(), pc.public, addr, Duration::from_secs(3)).unwrap();
    // Long enough that the old frame numbers are far ahead of the new process's.
    let before = watch(&client, Duration::from_millis(2500));
    assert!(before.len() > 100, "only {} frames before the restart", before.len());

    drop(first);
    let sock = (0..100)
        .find_map(|_| UdpSocket::bind(addr).ok().or_else(|| { thread::sleep(Duration::from_millis(20)); None }))
        .expect("the old server lets go of its port");
    watch(&client, Duration::from_millis(200)); // frames the old process sent before it stopped
    let _second = start_pc_on(sock, pc, mac.public, i32::MAX);
    let (after, cursors) = watch_all(&client, Duration::from_millis(3000));
    // Reconnecting takes about a second (the restart hint is believed after one second of
    // silence); the rest of the time should be picture.
    assert!(after.len() > 60, "only {} frames after the restart", after.len());
    assert!(after[0].encoded.keyframe, "the new stream starts from a keyframe");
    assert!(after.iter().all(intact));
    // The new process numbers its cursor updates from 1 again; they must not read as old.
    assert!(cursors.len() > 30, "only {} cursor moves after the restart", cursors.len());
}

#[test]
fn frames_arrive_intact_and_in_order() {
    let (pc, client, _) = setup(0);
    let frames = watch(&client, Duration::from_millis(1500));
    // 1.5 s at the 60 fps cap is about 90 frames.
    assert!(frames.len() > 60, "only {} frames", frames.len());
    assert!(frames.len() <= 100, "the 60 fps cap holds: {} frames", frames.len());
    assert!(frames[0].encoded.keyframe, "a viewer starts from a keyframe");
    assert!(frames.iter().all(intact), "every frame intact");
    assert!(frames.windows(2).all(|w| w[1].id > w[0].id), "in order");
    assert_eq!(client.counts().1, 0, "nothing lost on a clean link");
    // Age of fresh frames: sent and received in the same process, so it should be tiny.
    let t = Instant::now();
    let (mut ages, mut true_ages) = (Vec::new(), Vec::new());
    while t.elapsed() < Duration::from_millis(500) {
        if let Some(Received::Frame(f)) = client.recv().unwrap() {
            ages.extend(client.age_us(&f));
            true_ages.push(now_us() - f.encoded.captured_us); // one process, one clock
        }
    }
    ages.sort();
    true_ages.sort();
    eprintln!(
        "fresh frame age on loopback: estimated median {} us; true median {} us, max {} us",
        ages[ages.len() / 2],
        true_ages[true_ages.len() / 2],
        true_ages[true_ages.len() - 1]
    );
    assert!(ages[ages.len() / 2] < 5_000, "a loopback frame arrives within a few ms: {ages:?}");
    assert_eq!(pc.capturers.load(Ordering::SeqCst), 1);
}

#[test]
fn loss_recovers_through_keyframes_and_never_shows_a_broken_frame() {
    let (_pc, client, stop) = setup(5);
    let frames = watch(&client, Duration::from_millis(2500));
    let (complete, lost, repaired) = client.counts();
    eprintln!("5% loss: {complete} frames shown, {lost} lost, {repaired} shards rebuilt, {} keyframes", frames.iter().filter(|f| f.encoded.keyframe).count());
    assert!(repaired > 0, "the proxy did drop video, and parity absorbed it");
    assert!(lost * 20 < complete, "almost nothing lost: {lost} of {complete}");
    assert!(frames.iter().all(intact), "no corrupted frame reaches the decoder");
    assert!(frames.len() > 100, "the stream keeps flowing: only {} frames in 2.5 s", frames.len());
    // After every gap in frame numbers comes a keyframe.
    for w in frames.windows(2) {
        assert!(w[1].id == w[0].id + 1 || w[1].encoded.keyframe, "frame {} follows {} without a keyframe", w[1].id, w[0].id);
    }
    stop.unwrap().store(true, Ordering::Relaxed);
}

#[test]
fn closing_the_viewer_stops_the_stream() {
    let (pc, client, _) = setup(0);
    assert!(!watch(&client, Duration::from_millis(400)).is_empty());
    client.stop();
    // Drain what was already in flight.
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(200) {
        client.recv().unwrap();
    }
    let t = Instant::now();
    let mut late = 0;
    while t.elapsed() < Duration::from_millis(300) {
        late += matches!(client.recv().unwrap(), Some(Received::Frame(_))) as u32;
    }
    assert_eq!(late, 0, "no frames after the viewer closed");
    assert_eq!(pc.capturers.load(Ordering::SeqCst), 1, "the capturer is kept briefly for a quick return");
}

#[test]
fn a_still_screen_does_not_raise_the_bitrate() {
    let (mac, pc) = (StaticKeys::generate(), StaticKeys::generate());
    // About 0.2 s of changes (a poll is about 2 ms), then a still screen.
    let server = start_pc_on(UdpSocket::bind("127.0.0.1:0").unwrap(), pc.clone(), mac.public, 100);
    let client = Client::connect(mac, pc.public, server.addr, Duration::from_secs(3)).unwrap();
    let frames = watch(&client, Duration::from_millis(3500));
    assert!(!frames.is_empty() && frames.len() < 30, "{} frames", frames.len());
    // With frames, a clear second raises it; without, there is no evidence either way.
    assert_eq!(client.bitrate(), video::START_BITRATE as u64);
}

#[test]
fn the_cursor_moves_between_frames() {
    let (_pc, client, _) = setup(0);
    let (frames, cursors) = watch_all(&client, Duration::from_millis(1000));
    assert!(frames.iter().all(|f| f.encoded.hevc), "the codec flag crosses the network");
    // The pointer is followed every poll (about every 2 ms here), far more often than frames go
    // out at 60 fps.
    assert!(cursors.len() > frames.len() * 2, "{} cursor moves for {} frames", cursors.len(), frames.len());
    assert!(cursors.windows(2).all(|w| w[1].0 > w[0].0), "in order");
}

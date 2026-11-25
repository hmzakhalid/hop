//! End to end over real UDP on loopback: the real agent loop and receiver on one side, the real
//! controller and link on the other, with a proxy in between that drops, duplicates and
//! reorders datagrams.

use hop_core::agent::{self, AgentConfig, AgentEvent, PairingWindow, Platform};
use hop_core::controller::{Controller, ControllerConfig, Effect, LeaveReason, Output};
use hop_core::crypto::{new_pairing_code, StaticKeys};
use hop_core::keymap::{mac, win, MacMod};
use hop_core::layout::Rect;
use hop_core::link::{self, Link};
use hop_core::receiver::{Injector, Op, Recorder};
use hop_core::session::{self, Session};
use hop_core::wire::{Monitor, LEASE_US, MAX_PACKET, PKT_DATA, PKT_SESSION_INIT};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone)]
struct TestPlatform(Arc<Mutex<Recorder>>);

impl Injector for TestPlatform {
    fn key(&mut self, c: u16, d: bool) -> bool {
        self.0.lock().unwrap().key(c, d)
    }
    fn button(&mut self, b: u8, d: bool) -> bool {
        self.0.lock().unwrap().button(b, d)
    }
    fn move_to(&mut self, x: i32, y: i32) {
        self.0.lock().unwrap().move_to(x, y)
    }
    fn wheel(&mut self, v: i32, h: i32) {
        self.0.lock().unwrap().wheel(v, h)
    }
    fn mask_key(&mut self) {
        self.0.lock().unwrap().mask_key()
    }
}

impl Platform for TestPlatform {
    fn monitors(&mut self) -> Vec<Monitor> {
        vec![Monitor { left: 0, top: 0, right: 1920, bottom: 1080, scale_milli: 1000 }]
    }
    fn cursor_pos(&mut self) -> (i32, i32) {
        self.0.lock().unwrap().pos
    }
}

struct Agent {
    addr: SocketAddr,
    rec: Arc<Mutex<Recorder>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Start an agent and pair `mac` with it.
fn start_agent(mac: &StaticKeys) -> Agent {
    let pc = StaticKeys::generate();
    let code = new_pairing_code();
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let cfg = AgentConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        name: "Test PC".into(),
        keys: pc,
        paired: None,
        pairing: Some(PairingWindow::new(code.clone())),
    };
    let mut platform = TestPlatform(rec.clone());
    let stop2 = stop.clone();
    let handle = thread::spawn(move || {
        agent::run(cfg, &mut platform, stop2, move |e| {
            let _ = tx.lock().unwrap().send(e);
        })
        .unwrap();
    });
    let addr = match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        AgentEvent::Listening(a) => a,
        e => panic!("{e:?}"),
    };
    let paired = link::pair(addr, mac, &code, "Test Mac").unwrap();
    assert_eq!(paired.name, "Test PC");
    // Wait until the agent has stored the key.
    loop {
        if let AgentEvent::Paired(p) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            assert_eq!(p.public, mac.public);
            break;
        }
    }
    Agent { addr, rec, stop, handle: Some(handle) }
}

/// A UDP relay between the Mac link and the agent that misbehaves on purpose.
struct Proxy {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    replay_init: Arc<AtomicBool>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn start_proxy(agent: SocketAddr, drop_pct: u32, dup_pct: u32, hold_pct: u32) -> Proxy {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    let addr = sock.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let replay_init = Arc::new(AtomicBool::new(false));
    let (stop2, replay2) = (stop.clone(), replay_init.clone());
    thread::spawn(move || {
        let mut client: Option<SocketAddr> = None;
        let mut held: Option<(Vec<u8>, SocketAddr)> = None;
        let mut captured_init: Option<Vec<u8>> = None;
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % 100) as u32
        };
        let mut buf = [0u8; MAX_PACKET];
        while !stop2.load(Ordering::Relaxed) {
            if replay2.swap(false, Ordering::Relaxed) {
                if let Some(p) = &captured_init {
                    sock.send_to(p, agent).unwrap();
                }
            }
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                if let Some((p, to)) = held.take() {
                    let _ = sock.send_to(&p, to);
                }
                continue;
            };
            let to = if from == agent {
                match client {
                    Some(c) => c,
                    None => continue,
                }
            } else {
                client = Some(from);
                agent
            };
            let pkt = buf[..n].to_vec();
            if pkt[0] == PKT_SESSION_INIT {
                captured_init = Some(pkt.clone());
                sock.send_to(&pkt, to).unwrap(); // handshakes pass untouched
                continue;
            }
            if rnd() < drop_pct {
                continue;
            }
            if rnd() < hold_pct && held.is_none() {
                held = Some((pkt, to)); // goes out after the next packet: reordered
                continue;
            }
            sock.send_to(&pkt, to).unwrap();
            if rnd() < dup_pct {
                sock.send_to(&pkt, to).unwrap();
            }
            if let Some((p, t)) = held.take() {
                let _ = sock.send_to(&p, t);
            }
        }
    });
    Proxy { addr, stop, replay_init }
}

/// Records the effects the app would carry out (cursor, indicator).
#[derive(Default)]
struct Effects(Mutex<Vec<Effect>>);

impl session::Effects for Effects {
    fn apply(&self, e: Effect) {
        self.0.lock().unwrap().push(e);
    }
}

/// The Mac side: the same session the app runs, minus macOS.
struct Mac {
    session: Arc<Session>,
    effects: Arc<Effects>,
    stop: Arc<AtomicBool>,
}

impl Drop for Mac {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Mac {
    fn new(keys: StaticKeys, pc_public: [u8; 32], to: SocketAddr) -> Mac {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let link = Arc::new(Link::new(sock, keys, pc_public, to).unwrap());
        link.connect(Duration::from_secs(5)).unwrap();
        let ctl = Controller::new(
            ControllerConfig { edges: true, ..ControllerConfig::default() },
            vec![Rect::new(0.0, 0.0, 1512.0, 982.0)],
        );
        let effects = Arc::new(Effects::default());
        let session = Session::new(ctl, link, effects.clone());
        let stop = Arc::new(AtomicBool::new(false));
        session.spawn(stop.clone(), |_| {});
        Mac { session, effects, stop }
    }

    fn act(&self, f: impl FnOnce(&mut Controller, u64) -> Output) {
        self.session.act(f);
    }

    fn wait_for_pc(&self) {
        let t = Instant::now();
        while self.session.read(|c| c.pc_monitors().is_empty()) {
            assert!(t.elapsed() < Duration::from_secs(5), "no ack from the PC");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn enter(&self) {
        self.act(|c, now| c.mouse_moved(now, (1511.0, 491.0), (4.0, 0.0), 0, false));
        assert_eq!(self.effects.0.lock().unwrap().last(), Some(&Effect::Enter));
    }

    fn tap(&self, code: u16) {
        self.act(|c, now| c.key(now, code, true, false, 0, (0.0, 0.0)));
        self.act(|c, now| c.key(now, code, false, false, 0, (0.0, 0.0)));
    }

    fn modifier(&self, m: MacMod, down: bool) {
        let flags = if down { m.device_bit() } else { 0 };
        self.act(|c, now| c.flags_changed(now, m.keycode(), flags));
    }

    /// Wait until the PC has acknowledged every key and button change.
    fn settle(&self) {
        eventually("transitions acknowledged", Duration::from_secs(10), || self.session.read(|c| c.pending_transitions()) == 0);
    }

    fn left_because(&self) -> Option<LeaveReason> {
        self.effects.0.lock().unwrap().iter().find_map(|e| match e {
            Effect::Leave { reason, .. } => Some(*reason),
            Effect::Enter => None,
        })
    }
}

fn eventually(what: &str, limit: Duration, mut ok: impl FnMut() -> bool) {
    let t = Instant::now();
    while !ok() {
        assert!(t.elapsed() < limit, "timed out waiting for: {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

fn discover_pc(agent: &Agent) -> [u8; 32] {
    let found = link::discover(agent.addr.port(), &["127.0.0.1".parse().unwrap()], Duration::from_millis(300)).unwrap();
    found.iter().find(|f| f.addr.port() == agent.addr.port()).expect("discovered").public
}

fn key_downs(rec: &Recorder) -> Vec<u16> {
    rec.ops
        .iter()
        .filter_map(|o| match o {
            Op::Key(c, true) => Some(*c),
            _ => None,
        })
        .collect()
}

fn scenario(drop_pct: u32, dup_pct: u32, hold_pct: u32) {
    let mac_keys = StaticKeys::generate();
    let agent = start_agent(&mac_keys);
    let proxy = start_proxy(agent.addr, drop_pct, dup_pct, hold_pct);
    let pc_public = discover_pc(&agent);
    let m = Mac::new(mac_keys, pc_public, proxy.addr);
    m.wait_for_pc();
    m.enter();

    // Type "Hi" then Cmd+C, with 30 taps of 'a' to stress the transition queue.
    m.modifier(MacMod::LShift, true);
    m.tap(0x04); // H
    m.modifier(MacMod::LShift, false);
    m.tap(0x22); // i
    m.modifier(MacMod::LCmd, true);
    m.tap(0x08); // C
    m.modifier(MacMod::LCmd, false);
    for _ in 0..30 {
        m.tap(0x00);
        thread::sleep(Duration::from_micros(300));
    }
    // Move, click, drag, scroll.
    for _ in 0..50 {
        m.act(|c, now| c.mouse_moved(now, (0.0, 0.0), (3.0, 1.0), 0, false));
    }
    m.act(|c, now| c.mouse_button(now, 0, true));
    for _ in 0..20 {
        m.act(|c, now| c.mouse_moved(now, (0.0, 0.0), (2.0, 0.0), 0, true));
    }
    m.act(|c, now| c.mouse_button(now, 0, false));
    for _ in 0..10 {
        m.act(|c, now| c.scroll(now, 4.0, 0.0, true));
    }
    m.settle();
    // Position and scroll are state, not transitions: wait for the last of it to land.
    eventually("final cursor and scroll", Duration::from_secs(5), || {
        let r = agent.rec.lock().unwrap();
        r.pos == (190, 590) && r.wheel_v == 100
    });

    {
        let rec = agent.rec.lock().unwrap();
        let downs = key_downs(&rec);
        let mut expected = vec![win::LSHIFT, 0x23, 0x17, win::LCTRL, 0x2E];
        expected.extend(std::iter::repeat_n(0x1E, 30));
        assert_eq!(downs, expected, "every key exactly once, in order");
        assert!(rec.down.is_empty(), "stuck keys: {:?}", rec.down);
        assert_eq!(rec.buttons, 0);
        let presses: Vec<_> = rec.ops.iter().filter(|o| matches!(o, Op::Button(..))).collect();
        assert_eq!(presses, vec![&Op::Button(0, true), &Op::Button(0, false)]);
    }

    // Leave with the hotkey; nothing stays held.
    m.act(|c, now| c.key(now, mac::LEFT, true, false, mac::HYPER, (100.0, 100.0)));
    assert_eq!(m.left_because(), Some(LeaveReason::Hotkey));
    assert!(agent.rec.lock().unwrap().down.is_empty());

    let r = m.session.read(|c| c.rtt_summary()).unwrap();
    eprintln!("loopback round trip over {} acks (drop {drop_pct}%): p50 {:.3} ms, p99 {:.3} ms", r.count, r.p50, r.p99);
    drop(m);
    drop(proxy);
}

#[test]
fn clean_network() {
    scenario(0, 0, 0);
}

#[test]
fn lossy_network_loses_nothing_and_sticks_nothing() {
    scenario(20, 5, 10);
}

#[test]
fn agent_death_returns_control_within_the_lease() {
    let mac_keys = StaticKeys::generate();
    let agent = start_agent(&mac_keys);
    let pc_public = discover_pc(&agent);
    let m = Mac::new(mac_keys, pc_public, agent.addr);
    m.wait_for_pc();
    m.enter();
    m.modifier(MacMod::LCmd, true);
    m.settle();
    let killed = Instant::now();
    drop(agent);
    let lease = Duration::from_micros(LEASE_US);
    eventually("the Mac takes its input back", lease + Duration::from_millis(200), || m.left_because().is_some());
    let took = killed.elapsed();
    eprintln!("control returned {took:?} after the agent died");
    assert!(took >= lease - Duration::from_millis(100), "the lease must run its course: {took:?}");
    assert_eq!(m.left_because(), Some(LeaveReason::Timeout));
    assert!(!m.session.read(|c| c.is_remote()));
}

#[test]
fn unpaired_mac_is_rejected() {
    let mac_keys = StaticKeys::generate();
    let agent = start_agent(&mac_keys);
    let pc_public = discover_pc(&agent);
    let stranger = StaticKeys::generate();
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let l = Link::new(sock, stranger, pc_public, agent.addr).unwrap();
    let e = l.connect(Duration::from_millis(800)).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
}

#[test]
fn replayed_session_init_does_not_cut_off_the_live_session() {
    let mac_keys = StaticKeys::generate();
    let agent = start_agent(&mac_keys);
    let proxy = start_proxy(agent.addr, 0, 0, 0);
    let pc_public = discover_pc(&agent);
    let m = Mac::new(mac_keys, pc_public, proxy.addr);
    m.wait_for_pc();
    m.enter();
    m.tap(0x00);
    m.settle();
    // An attacker replays the captured SESSION_INIT.
    proxy.replay_init.store(true, Ordering::Relaxed);
    eventually("the proxy replays it", Duration::from_secs(1), || !proxy.replay_init.load(Ordering::Relaxed));
    m.tap(0x0B); // B
    m.settle();
    assert_eq!(key_downs(&agent.rec.lock().unwrap()), vec![0x1E, 0x30]);
    assert!(m.session.read(|c| c.is_remote()), "still in control");
}

#[test]
fn junk_flood_cannot_starve_the_dead_man() {
    let mac_keys = StaticKeys::generate();
    let agent = start_agent(&mac_keys);
    let pc_public = discover_pc(&agent);
    let m = Mac::new(mac_keys, pc_public, agent.addr);
    m.wait_for_pc();
    m.enter();
    m.modifier(MacMod::LCmd, true);
    m.settle();
    assert_eq!(agent.rec.lock().unwrap().down, vec![win::LCTRL]);
    // The Mac vanishes while holding Cmd, and someone floods the agent with junk.
    drop(m);
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let target = agent.addr;
    let flood = thread::spawn(move || {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        let junk: Vec<u8> = std::iter::once(PKT_DATA).chain(1..30).collect();
        while !s2.load(Ordering::Relaxed) {
            let _ = s.send_to(&junk, target);
        }
    });
    let t = Instant::now();
    eventually("the dead-man releases ⌘ despite the flood", Duration::from_millis(1500), || agent.rec.lock().unwrap().down.is_empty());
    eprintln!("released {:?} after the Mac vanished, under a junk flood", t.elapsed());
    stop.store(true, Ordering::Relaxed);
    flood.join().unwrap();
}


#[test]
fn pairing_alone_works_beside_a_running_agent() {
    // As with the Windows service: the agent holds the UDP port, and a separate process pairs a
    // new Mac over TCP on the same port number.
    let agent = start_agent(&StaticKeys::generate());
    let addr = agent.addr;
    let (pc_keys, code) = (StaticKeys::generate(), new_pairing_code());
    let pc_public = pc_keys.public;
    let window = PairingWindow::new(code.clone());
    let pairing = thread::spawn(move || {
        let stop = AtomicBool::new(false);
        let t = Instant::now();
        loop {
            // The agent's own pairing listener closes just after its pairing; wait for the port.
            match agent::pair(addr, &window, &pc_keys, "Service PC", &stop, |_| {}) {
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && t.elapsed() < Duration::from_secs(2) => {
                    thread::sleep(Duration::from_millis(20))
                }
                r => return r.unwrap(),
            }
        }
    });
    let new_mac = StaticKeys::generate();
    let t = Instant::now();
    let pc = loop {
        match link::pair(addr, &new_mac, &code, "Second Mac") {
            Ok(p) => break p,
            Err(_) if t.elapsed() < Duration::from_secs(3) => thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("{e}"),
        }
    };
    assert_eq!((pc.name.as_str(), pc.public), ("Service PC", pc_public));
    let mac = pairing.join().unwrap().expect("paired");
    assert_eq!((mac.name.as_str(), mac.public), ("Second Mac", new_mac.public));
}

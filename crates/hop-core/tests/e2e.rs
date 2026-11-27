//! End to end over real UDP on loopback: the real agent loop and receiver on one side, the real
//! controller and link on the other, with a proxy in between that drops, duplicates and
//! reorders datagrams.

use hop_core::agent::{self, AgentConfig, AgentEvent, Asker, PairingWindow, Platform};
use hop_core::ask::{self, Answer};
use hop_core::controller::{Controller, ControllerConfig, Effect, LeaveReason, Output};
use hop_core::crypto::{new_pairing_code, PairError, StaticKeys};
use hop_core::keymap::{mac, win, MacMod};
use hop_core::layout::Rect;
use hop_core::link::{self, Link};
use hop_core::receiver::{Injector, Op, Recorder};
use hop_core::session::{self, Session};
use hop_core::store::AgentStore;
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
    /// Where Macs ask to pair (when the agent has an asker).
    asking: Option<SocketAddr>,
    events: mpsc::Receiver<AgentEvent>,
    store_dir: std::path::PathBuf,
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
        let _ = std::fs::remove_dir_all(&self.store_dir);
    }
}

/// A store in a fresh temporary folder.
fn temp_store() -> AgentStore {
    let dir = std::env::temp_dir().join(format!("hop-e2e-{:032x}", hop_core::crypto::random_u128()));
    AgentStore::new(dir)
}

/// Answers every request with `.0`, and records what it was shown.
struct FixedAsker(Answer, Mutex<Vec<(String, String, Option<String>)>>);

impl Asker for FixedAsker {
    fn ask(&self, mac: &str, check: &str, replaces: Option<&str>) -> Answer {
        self.1.lock().unwrap().push((mac.into(), check.into(), replaces.map(String::from)));
        self.0
    }
}

/// Start an agent and pair `mac` with it.
fn start_agent(mac: &StaticKeys) -> Agent {
    start_agent_asking(mac, None)
}

/// Start an agent that pairs `mac` with a code, and takes requests with `asker` if given.
fn start_agent_asking(mac: &StaticKeys, asker: Option<Arc<dyn Asker>>) -> Agent {
    let pc = StaticKeys::generate();
    let code = new_pairing_code();
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let store = temp_store();
    let store_dir = store.dir.clone();
    let has_asker = asker.is_some();
    let cfg = AgentConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        name: "Test PC".into(),
        keys: pc,
        store,
        pairing: Some(PairingWindow::new(code.clone())),
        asker,
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
    let asking = has_asker.then(|| match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        AgentEvent::Asking(a) => a,
        e => panic!("{e:?}"),
    });
    let paired = link::pair(addr, mac, &code, "Test Mac").unwrap();
    assert_eq!(paired.name, "Test PC");
    // Wait until the agent has stored the key.
    loop {
        if let AgentEvent::Paired(p) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            assert_eq!(p.public, mac.public);
            break;
        }
    }
    Agent { addr, asking, events: rx, store_dir, rec, stop, handle: Some(handle) }
}

/// Ask the agent to pair `mac`; returns what the PC answered (and on yes, the Mac has saved).
fn ask_agent(agent: &Agent, mac: &StaticKeys, name: &str) -> Result<(Answer, u32), hop_core::crypto::PairError> {
    let s = std::net::TcpStream::connect(agent.asking.expect("the agent takes requests"))?;
    let mut a = ask::ask(s, mac, name)?;
    let answer = a.answer()?;
    let check = a.check;
    if answer == Answer::Allowed {
        a.saved()?;
    }
    Ok((answer, check))
}

/// Wait for the agent to report that `mac` paired.
fn wait_paired(agent: &Agent, mac: &StaticKeys) {
    loop {
        match agent.events.recv_timeout(Duration::from_secs(5)).expect("a pairing event") {
            AgentEvent::Paired(p) if p.public == mac.public => return,
            AgentEvent::PairingFailed(why) => panic!("{why}"),
            _ => {}
        }
    }
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
        eventually("an ack from the PC", Duration::from_secs(5), || self.session.read(|c| !c.pc_monitors().is_empty()));
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
    // Position and scroll are state, not transitions: wait for the last of it to land where the
    // Mac put it.
    let (pos, scroll) = m.session.read(|c| (c.pc_cursor(), c.pc_scroll()));
    let pos = pos.map(|(x, y)| (x.floor() as i32, y.floor() as i32)).expect("still on the PC");
    let (scroll_v, _) = scroll.expect("still on the PC");
    assert!(scroll_v > 0, "the scrolls went somewhere");
    eventually("final cursor and scroll", Duration::from_secs(5), || {
        let r = agent.rec.lock().unwrap();
        r.pos == pos && r.wheel_v == scroll_v
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
fn pairing_closes_after_too_many_wrong_codes() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let code = new_pairing_code();
    let window = PairingWindow::new(code.clone());
    let pc = StaticKeys::generate();
    let pairing = thread::spawn(move || agent::pair(listener, &window, &pc, "PC", &AtomicBool::new(false), |_| {}));
    let mac = StaticKeys::generate();
    let wrong = if code.starts_with('0') { "1111-1111-1111" } else { "0000-0000-0000" };
    for _ in 0..agent::PAIRING_MAX_FAILURES {
        let r = link::pair(addr, &mac, wrong, "Mac");
        assert!(matches!(r, Err(PairError::WrongCode)), "{r:?}");
    }
    assert_eq!(pairing.join().unwrap(), None, "pairing closed after the last wrong code");
    assert!(link::pair(addr, &mac, &code, "Mac").is_err(), "even the right code is too late now");
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
    // The agent's own pairing has finished, so its TCP listener is closed and the port is free.
    let listener = std::net::TcpListener::bind(addr).expect("the agent released the pairing port");
    let pairing = thread::spawn(move || agent::pair(listener, &window, &pc_keys, "Service PC", &AtomicBool::new(false), |_| {}));
    let new_mac = StaticKeys::generate();
    let pc = link::pair(addr, &new_mac, &code, "Second Mac").unwrap();
    assert_eq!((pc.name.as_str(), pc.public), ("Service PC", pc_public));
    let mac = pairing.join().unwrap().expect("paired");
    assert_eq!((mac.name.as_str(), mac.public), ("Second Mac", new_mac.public));
}

#[test]
fn a_mac_that_asks_and_is_allowed_pairs_and_drives_the_pc() {
    let first = StaticKeys::generate();
    let asker = Arc::new(FixedAsker(Answer::Allowed, Mutex::default()));
    let agent = start_agent_asking(&first, Some(asker.clone()));
    let second = StaticKeys::generate();
    let (answer, check) = ask_agent(&agent, &second, "Second Mac").unwrap();
    assert_eq!(answer, Answer::Allowed);
    wait_paired(&agent, &second);
    let shown = asker.1.lock().unwrap().clone();
    assert_eq!(shown, vec![("Second Mac".to_string(), ask::check_text(check), Some("Test Mac".to_string()))]);
    assert_eq!(AgentStore::new(agent.store_dir.clone()).peer().unwrap().public, second.public, "stored for next time");
    let m = Mac::new(second, discover_pc(&agent), agent.addr);
    m.wait_for_pc();
    m.enter();
    m.tap(0x04); // H
    m.settle();
    assert_eq!(key_downs(&agent.rec.lock().unwrap()), vec![0x23]);
}

#[test]
fn allowing_another_mac_ends_the_old_ones_session_and_lets_go_of_its_keys() {
    let first = StaticKeys::generate();
    let agent = start_agent_asking(&first, Some(Arc::new(FixedAsker(Answer::Allowed, Mutex::default()))));
    let old = Mac::new(first, discover_pc(&agent), agent.addr);
    old.wait_for_pc();
    old.enter();
    old.modifier(MacMod::LShift, true);
    old.settle();
    assert_eq!(key_downs(&agent.rec.lock().unwrap()), vec![0x2A]);

    let second = StaticKeys::generate();
    ask_agent(&agent, &second, "Second Mac").unwrap();
    wait_paired(&agent, &second);
    eventually("the old Mac's Shift let go", Duration::from_secs(2), || agent.rec.lock().unwrap().ops.contains(&Op::Key(0x2A, false)));
    let before = agent.rec.lock().unwrap().ops.len();
    old.tap(0x00);
    thread::sleep(Duration::from_millis(300));
    assert!(!agent.rec.lock().unwrap().ops[before..].iter().any(|o| matches!(o, Op::Key(_, true))), "the old Mac types no more");
}

#[test]
fn a_mac_turned_down_is_not_paired_and_cannot_ask_again_at_once() {
    let first = StaticKeys::generate();
    let agent = start_agent_asking(&first, Some(Arc::new(FixedAsker(Answer::Declined, Mutex::default()))));
    let other = StaticKeys::generate();
    assert_eq!(ask_agent(&agent, &other, "Other").unwrap().0, Answer::Declined);
    assert!(ask_agent(&agent, &other, "Other").is_err(), "refused during the cool-down");
    assert_eq!(AgentStore::new(agent.store_dir.clone()).peer().unwrap().public, first.public, "still the first Mac");
}

/// Says yes, but only after an Unpair has happened while it was asking. The agent's folder is
/// known only once the agent runs, hence the slot.
struct UnpairWhileAsking(Mutex<std::path::PathBuf>);

impl Asker for UnpairWhileAsking {
    fn ask(&self, _: &str, _: &str, _: Option<&str>) -> Answer {
        AgentStore::new(self.0.lock().unwrap().clone()).lock().unwrap().forget_peer().unwrap();
        Answer::Allowed
    }
}

#[test]
fn an_unpair_while_the_pc_asks_wins() {
    let asker = Arc::new(UnpairWhileAsking(Mutex::default()));
    let agent = start_agent_asking(&StaticKeys::generate(), Some(asker.clone()));
    *asker.0.lock().unwrap() = agent.store_dir.clone();
    assert_eq!(ask_agent(&agent, &StaticKeys::generate(), "Other").unwrap().0, Answer::Declined);
    assert!(AgentStore::new(agent.store_dir.clone()).peer().is_none(), "the Unpair stands");
}

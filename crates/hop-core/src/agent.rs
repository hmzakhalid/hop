//! PC-side networking: answers discovery, runs pairing (by asking, or with a code), accepts
//! sessions only from the paired Mac, and feeds state datagrams to the [`Receiver`].

use crate::ask::{self, Answer};
use crate::clock::now_us;
use crate::crypto::{self, Paired, StaticKeys};
use crate::receiver::{Injector, Receiver};
use crate::responder::{Datagram, Responder};
use crate::store::{AgentStore, MacRecord};
use crate::wire::{self, Ack, Monitor, Msg, ACK_ACTIVE, ACK_BLOCKED, ACK_EPOCH_DEAD, MAX_PACKET};
use std::io;
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Everything the agent needs from the operating system.
pub trait Platform: Injector {
    /// Monitor rectangles in physical pixels on the virtual desktop, with DPI scale.
    fn monitors(&mut self) -> Vec<Monitor>;
    fn cursor_pos(&mut self) -> (i32, i32);
    /// True while input cannot be injected (lock screen, UAC prompt).
    fn input_blocked(&mut self) -> bool {
        false
    }
}

/// Asks whoever is at the PC whether a Mac may pair, showing the check number the Mac shows.
/// Blocks until they answer or [`ask::ANSWER_TIME`] passes.
pub trait Asker: Send + Sync {
    fn ask(&self, mac: &str, check: &str, replaces: Option<&str>) -> Answer;
}

#[derive(Debug)]
pub enum AgentEvent {
    Listening(SocketAddr),
    /// Macs may ask to pair on this TCP address.
    Asking(SocketAddr),
    /// A Mac paired, and is stored and in use.
    Paired(Paired),
    PairingFailed(String),
    SessionStarted(SocketAddr),
}

/// How long a pairing code stays valid.
pub const PAIRING_TTL: Duration = Duration::from_secs(120);
/// Wrong codes tolerated before pairing closes.
pub const PAIRING_MAX_FAILURES: u32 = 3;

/// A pairing code on offer.
pub struct PairingWindow {
    pub code: String,
    until: Instant,
}

impl PairingWindow {
    pub fn new(code: String) -> Self {
        Self { code, until: Instant::now() + PAIRING_TTL }
    }
}

pub struct AgentConfig {
    pub bind: SocketAddr,
    pub name: String,
    pub keys: StaticKeys,
    /// The paired Mac (read at start) and where a new pairing is stored.
    pub store: AgentStore,
    /// A pairing code on offer.
    pub pairing: Option<PairingWindow>,
    /// Who to ask when a Mac asks to pair; `None`: Macs cannot ask.
    pub asker: Option<Arc<dyn Asker>>,
}

/// After a request is turned down, further requests are refused this long, so nobody can keep a
/// prompt on the PC's screen.
const ASK_COOLDOWN: Duration = Duration::from_secs(10);

type Events = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// How long the loop waits for a packet before servicing timers (dead-man, monitors).
const POLL: Duration = Duration::from_millis(20);
/// Most packets handled per batch, and longest batch, so a flood cannot starve the timers.
const BATCH_PACKETS: u32 = 64;
const BATCH_TIME: Duration = Duration::from_millis(2);
/// Acks are sent at once when something changed, else at most this often.
const ACK_INTERVAL_US: u64 = 10_000;

/// What pairing needs to store a Mac and start using it.
#[derive(Clone)]
struct Pairings {
    store: Arc<AgentStore>,
    paired: Arc<Mutex<Option<[u8; 32]>>>,
    events: Events,
}

impl Pairings {
    /// A generation for a pairing that starts now (see [`crate::store::PairingLock`]).
    fn begin(&self) -> Result<String, String> {
        self.store.lock().and_then(|mut l| l.new_generation()).map_err(|e| format!("{}: {e}", self.store.dir.display()))
    }

    /// Store `p` and use it from now on, unless an Unpair or a newer pairing replaced this one.
    fn finish(&self, generation: &str, p: Paired) {
        let saved = self.store.lock().and_then(|l| {
            if l.generation().as_deref() != Some(generation) {
                return Ok(false);
            }
            l.save_peer(&MacRecord { public: p.public, name: p.name.clone() })?;
            Ok(true)
        });
        match saved {
            Ok(true) => {
                *self.paired.lock().unwrap() = Some(p.public);
                (self.events)(AgentEvent::Paired(p));
            }
            Ok(false) => (self.events)(AgentEvent::PairingFailed(format!("{} paired, but an Unpair or a newer pairing replaced it", p.name))),
            Err(e) => (self.events)(AgentEvent::PairingFailed(format!("{} paired, but saving failed: {e}", p.name))),
        }
    }
}

fn spawn_pairing(listener: TcpListener, window: PairingWindow, keys: StaticKeys, name: String, pairings: Pairings, stop: Arc<AtomicBool>) {
    let generation = match pairings.begin() {
        Ok(g) => g,
        Err(e) => return (pairings.events)(AgentEvent::PairingFailed(e)),
    };
    std::thread::spawn(move || {
        // `pair` closes the listener before it returns: the port is free again before anyone
        // hears about the pairing.
        let events = pairings.events.clone();
        if let Some(p) = pair(listener, &window, &keys, &name, &stop, |why| events(AgentEvent::PairingFailed(why))) {
            pairings.finish(&generation, p);
        }
    });
}

/// Take requests to pair, one at a time, on its own thread (a prompt can take a minute).
fn spawn_asking(listener: TcpListener, asker: Arc<dyn Asker>, keys: StaticKeys, name: String, pairings: Pairings, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let failed = |why: String| (pairings.events)(AgentEvent::PairingFailed(why));
        // Non-blocking, so `stop` is seen even if nobody asks.
        if let Err(e) = listener.set_nonblocking(true) {
            return failed(e.to_string());
        }
        let mut quiet_until = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((s, from)) => {
                    if Instant::now() < quiet_until {
                        continue; // dropped unanswered: the Mac says to try again shortly
                    }
                    if let Err(e) = s.set_nonblocking(false) {
                        failed(e.to_string());
                        continue;
                    }
                    let _ = s.set_nodelay(true); // latency only
                    if let Err(why) = answer_request(s, asker.as_ref(), &keys, &name, &pairings) {
                        failed(format!("a request to pair from {} did not finish: {why}", from.ip()));
                        quiet_until = Instant::now() + ASK_COOLDOWN;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => {
                    failed(e.to_string());
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    });
}

/// One request to pair: ask, answer the Mac, and on yes store the pairing once the Mac has.
/// `Err` when it was turned down or did not finish.
fn answer_request(s: std::net::TcpStream, asker: &dyn Asker, keys: &StaticKeys, name: &str, pairings: &Pairings) -> Result<(), String> {
    let req = ask::accept(s, keys, name).map_err(|e| e.to_string())?;
    let current = pairings.store.peer();
    let replaces = current.filter(|c| c.public != req.mac.public).map(|c| c.name);
    let answer = asker.ask(&req.mac.name, &ask::check_text(req.check), replaces.as_deref());
    if answer != Answer::Allowed {
        let who = req.mac.name.clone();
        req.refuse(answer);
        return Err(format!("{who}: {answer:?}"));
    }
    // Taken once allowed, so requests nobody allowed never disturb a pairing under way.
    let generation = pairings.begin()?;
    let p = req.allow().map_err(|e| e.to_string())?;
    pairings.finish(&generation, p);
    Ok(())
}

/// Pair once, without running the agent: accept on `listener` (TCP) until a Mac pairs with the
/// code, the window closes, or `stop`. For pairing while the Windows service runs the agent: it
/// holds the UDP port but never the TCP one. The caller binds first, so it can report a busy
/// port before showing a code; the listener closes when this returns.
pub fn pair(
    listener: TcpListener,
    window: &PairingWindow,
    keys: &StaticKeys,
    name: &str,
    stop: &AtomicBool,
    failed: impl Fn(String),) -> Option<Paired> {
    // Non-blocking, so the time limit holds even if nobody connects.
    if let Err(e) = listener.set_nonblocking(true) {
        failed(e.to_string());
        return None;
    }
    let mut failures = 0;
    while !stop.load(Ordering::Relaxed) && Instant::now() < window.until && failures < PAIRING_MAX_FAILURES {
        match listener.accept() {
            Ok((mut s, _)) => {
                if let Err(e) = s.set_nonblocking(false) {
                    failed(e.to_string());
                    continue;
                }
                let _ = s.set_nodelay(true); // latency only
                // One deadline for the whole exchange, so a client that stalls or trickles
                // cannot hold pairing open, nor finish it after the code expires.
                let until = window.until.min(Instant::now() + crypto::PAIRING_EXCHANGE);
                match crypto::pair_responder(&mut s, until, keys, &window.code, name) {
                    Ok(p) => return Some(p),
                    Err(e) => {
                        if matches!(e, crypto::PairError::WrongCode) {
                            failures += 1;
                        }
                        failed(e.to_string());
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                failed(e.to_string());
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    None
}

/// Run until `stop` is set.
pub fn run(
    cfg: AgentConfig,
    platform: &mut dyn Platform,
    stop: Arc<AtomicBool>,
    events: impl Fn(AgentEvent) + Send + Sync + 'static,
) -> io::Result<()> {
    let events: Events = Arc::new(events);
    let store = Arc::new(cfg.store);
    let paired = Arc::new(Mutex::new(store.peer().map(|p| p.public)));
    let pairings = Pairings { store, paired: paired.clone(), events: events.clone() };
    // UDP and TCP (pairing) share one port number. With port 0 (tests), the number the OS picks
    // for UDP may be taken for TCP, so try again with another.
    let mut attempt = 0;
    let (sock, listener) = loop {
        let sock = UdpSocket::bind(cfg.bind)?;
        if cfg.pairing.is_none() {
            break (sock, None);
        }
        match TcpListener::bind(sock.local_addr()?) {
            Ok(l) => break (sock, Some(l)),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && cfg.bind.port() == 0 && attempt < 20 => attempt += 1,
            Err(e) => return Err(e),
        }
    };
    let local = sock.local_addr()?;
    if let (Some(window), Some(listener)) = (cfg.pairing, listener) {
        spawn_pairing(listener, window, cfg.keys.clone(), cfg.name.clone(), pairings.clone(), stop.clone());
    }
    events(AgentEvent::Listening(local));
    if let Some(asker) = cfg.asker {
        // Its own port, so a code shown beside the service (which holds `local`'s TCP twin)
        // never collides with it. With port 0 (tests) the OS picks one.
        let port = if cfg.bind.port() == 0 { Some(0) } else { ask::port(local.port()) };
        match port.map(|p| TcpListener::bind(SocketAddr::new(cfg.bind.ip(), p))) {
            Some(Ok(l)) => {
                if let Ok(a) = l.local_addr() {
                    events(AgentEvent::Asking(a));
                }
                spawn_asking(l, asker, cfg.keys.clone(), cfg.name.clone(), pairings.clone(), stop.clone());
            }
            Some(Err(e)) => events(AgentEvent::PairingFailed(format!("Macs cannot ask to pair: {e}"))),
            None => events(AgentEvent::PairingFailed("Macs cannot ask to pair: no port".into())),
        }
    }
    sock.set_read_timeout(Some(POLL))?;

    let mut receiver = Receiver::default();
    let mut responder = Responder::new(cfg.keys.clone());
    let mut monitors = platform.monitors();
    let mut monitors_at = Instant::now();
    let mut blocked = false;
    let mut blocked_at = Instant::now();
    let mut last_ack_us = 0u64;
    let mut buf = vec![0u8; MAX_PACKET];
    let mut plain = Vec::with_capacity(MAX_PACKET);
    let mut serving = *paired.lock().unwrap();

    while !stop.load(Ordering::Relaxed) {
        // A different Mac was paired: the old one's session ends, and whatever it held is let go.
        let now_paired = *paired.lock().unwrap();
        if now_paired != serving {
            receiver.release_all(platform);
            receiver = Receiver::default();
            responder = Responder::new(cfg.keys.clone());
            serving = now_paired;
        }
        let first = match sock.recv_from(&mut buf) {
            Ok(x) => Some(x),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => None,
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => None, // Windows ICMP noise
            Err(e) => return Err(e),
        };
        let mut ack_due: Option<(crate::receiver::AckInfo, u64)> = None;
        if let Some(mut got) = first {
            sock.set_nonblocking(true)?;
            // Bounded, so a flood of junk can never starve the dead-man timer or shutdown.
            let batch_start = Instant::now();
            let mut batch = 0;
            loop {
                batch += 1;
                let (n, from) = got;
                let now = now_us();
                let pkt = &buf[..n];
                match responder.handle(&sock, pkt, from, || *paired.lock().unwrap(), &mut plain) {
                    Datagram::Other => {
                        if let Some(id) = wire::decode_probe(pkt) {
                            let _ = sock.send_to(&wire::encode_probe_reply(id, &cfg.keys.public, &cfg.name), from);
                        }
                    }
                    Datagram::Handled => {}
                    Datagram::Data { nonce, new_session } => {
                        if new_session {
                            // The receiver orders states by the session's nonces: start it over.
                            receiver.reset(platform);
                            events(AgentEvent::SessionStarted(from));
                        }
                        match Msg::decode(&plain) {
                            Some(Msg::State(s)) => {
                                if let Some(info) = receiver.on_state(now, nonce, &s, platform) {
                                    let progressed = info.progressed || ack_due.is_some_and(|(a, _)| a.progressed);
                                    ack_due = Some((crate::receiver::AckInfo { progressed, ..info }, now));
                                }
                            }
                            Some(Msg::Ping { t }) => responder.send(&sock, &Msg::Pong { t }),
                            _ => {}
                        }
                    }
                }
                if batch >= BATCH_PACKETS || batch_start.elapsed() > BATCH_TIME {
                    break;
                }
                match sock.recv_from(&mut buf) {
                    Ok(x) => got = x,
                    Err(_) => break,
                }
            }
            sock.set_nonblocking(false)?;
            receiver.flush(platform);
        }

        let now = now_us();
        receiver.tick(now, platform);
        if monitors_at.elapsed() > Duration::from_secs(1) {
            monitors = platform.monitors();
            monitors_at = Instant::now();
        }
        if blocked_at.elapsed() > Duration::from_millis(200) {
            blocked = platform.input_blocked();
            blocked_at = Instant::now();
        }
        if let Some((info, recv_at)) = ack_due.filter(|_| responder.connected()) {
            if info.progressed || !info.active || info.dead || now.saturating_sub(last_ack_us) >= ACK_INTERVAL_US {
                let (cx, cy) = platform.cursor_pos();
                let mut flags = 0;
                if info.active {
                    flags |= ACK_ACTIVE;
                }
                if info.dead {
                    flags |= ACK_EPOCH_DEAD;
                }
                if blocked {
                    flags |= ACK_BLOCKED;
                }
                let ack = Ack {
                    epoch: info.epoch,
                    applied_event: info.applied,
                    echo_time_us: info.echo_time_us,
                    hold_us: now_us().saturating_sub(recv_at).min(u32::MAX as u64) as u32,
                    flags,
                    cursor_x: cx,
                    cursor_y: cy,
                    monitors: monitors.clone(),
                };
                responder.send(&sock, &Msg::Ack(ack));
                last_ack_us = now;
            }
        }
    }
    receiver.release_all(platform);
    Ok(())
}

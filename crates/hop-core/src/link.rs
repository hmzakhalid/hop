//! Mac-side networking: discovery, pairing, and an encrypted UDP link to the paired PC.

use crate::crypto::{self, IkInitiator, PairError, Paired, Session, StaticKeys};
use crate::wire::{self, Msg, MAX_PACKET, PKT_DATA, PKT_SESSION_ACCEPT, PKT_UNKNOWN_SESSION};
use std::cell::RefCell;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// An unanswered handshake is sent again after this long. Every user of a link calls
/// [`Link::maintain`] more often than this (each checks it where it sets its cadence).
pub(crate) const HANDSHAKE_RETRY: Duration = Duration::from_millis(400);
/// Renegotiate after the PC has been silent this long. The PC only speaks when spoken to, so
/// every user of a link must speak more often than this (checked where each sets its cadence).
pub(crate) const RECONNECT_AFTER: Duration = Duration::from_secs(3);
/// How long [`Link::recv`] blocks, so its caller can check for shutdown and retry handshakes.
const RECV_TIMEOUT: Duration = Duration::from_millis(50);
const _: () = assert!(RECV_TIMEOUT.as_micros() < HANDSHAKE_RETRY.as_micros());
/// The PC's "unknown session" hint is unauthenticated, so it is believed only after this much
/// silence. Longer than the lease, so a spoofed hint can never end a crossing: by then the lease
/// already has. (On an idle link it can only cause a harmless extra handshake.)
const HINT_AFTER: Duration = Duration::from_secs(1);
const _: () = assert!((crate::wire::LEASE_US as u128) < HINT_AFTER.as_micros());

/// A PC that answered a discovery probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub addr: SocketAddr,
    pub public: [u8; 32],
    pub name: String,
}

/// Probe `targets` (broadcast or unicast addresses) on `port` and collect answers for `wait`.
/// Probing only loopback stays on loopback; anything else also tries `255.255.255.255`.
pub fn discover(port: u16, targets: &[IpAddr], wait: Duration) -> io::Result<Vec<Found>> {
    let local_only = !targets.is_empty() && targets.iter().all(IpAddr::is_loopback);
    let bind = if local_only { IpAddr::V4(Ipv4Addr::LOCALHOST) } else { IpAddr::V4(Ipv4Addr::UNSPECIFIED) };
    let sock = UdpSocket::bind((bind, 0))?;
    sock.set_broadcast(true)?;
    let id = rand::random::<u32>();
    let probe = wire::encode_probe(id);
    let mut all: Vec<IpAddr> = targets.to_vec();
    if !local_only {
        all.push(IpAddr::V4(Ipv4Addr::BROADCAST));
    }
    let mut found: Vec<Found> = Vec::new();
    let deadline = Instant::now() + wait;
    let mut next_send = Instant::now();
    let mut buf = [0u8; MAX_PACKET];
    while Instant::now() < deadline {
        if Instant::now() >= next_send {
            for t in &all {
                let _ = sock.send_to(&probe, (*t, port));
            }
            next_send = Instant::now() + Duration::from_millis(200);
        }
        let left = deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(50));
        sock.set_read_timeout(Some(left.max(Duration::from_millis(1))))?;
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Some((rid, public, name)) = wire::decode_probe_reply(&buf[..n]) {
                    if rid == id && !found.iter().any(|f| f.public == public) {
                        found.push(Found { addr: SocketAddr::new(from.ip(), port), public, name });
                    }
                }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(found)
}

/// Pair with the PC at `addr` (its UDP/TCP port) using the code it shows.
pub fn pair(addr: SocketAddr, keys: &StaticKeys, code: &str, my_name: &str) -> Result<Paired, PairError> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    let _ = s.set_nodelay(true); // latency only
    crypto::pair_initiator(&mut s, Instant::now() + crypto::PAIRING_EXCHANGE, keys, code, my_name)
}

/// What [`Link::recv`] produced.
#[derive(Debug)]
pub enum Event {
    Msg(Msg),
    Connected,
    /// The PC no longer knows our session (it restarted): reconnecting.
    Lost,
    Nothing,
}

thread_local! {
    static RECV_BUFS: RefCell<(Vec<u8>, Vec<u8>)> = RefCell::new((vec![0; MAX_PACKET], Vec::with_capacity(MAX_PACKET)));
}

/// An encrypted UDP link to one paired PC. All methods take `&self`, so one thread can block in
/// [`recv`](Self::recv) while others send. [`maintain`](Self::maintain) keeps it connected.
pub struct Link {
    sock: UdpSocket,
    keys: StaticKeys,
    peer_public: [u8; 32],
    peer: Mutex<SocketAddr>,
    session: Mutex<Option<Arc<Session>>>,
    pending: Mutex<Option<(IkInitiator, Instant)>>,
    last_authentic: Mutex<Option<Instant>>,
}

impl Link {
    /// [`recv`](Self::recv) returns `Event::Nothing` after a short timeout.
    pub fn new(sock: UdpSocket, keys: StaticKeys, peer_public: [u8; 32], peer: SocketAddr) -> io::Result<Self> {
        sock.set_read_timeout(Some(RECV_TIMEOUT))?;
        Ok(Self {
            sock,
            keys,
            peer_public,
            peer: Mutex::new(peer),
            session: Mutex::new(None),
            pending: Mutex::new(None),
            last_authentic: Mutex::new(None),
        })
    }

    pub fn peer(&self) -> SocketAddr {
        *self.peer.lock().unwrap()
    }

    fn follow(&self, from: SocketAddr) {
        *self.peer.lock().unwrap() = from;
    }

    /// How long since the last authenticated packet; `None` before the first.
    pub fn quiet_for(&self) -> Option<Duration> {
        self.last_authentic.lock().unwrap().map(|t| t.elapsed())
    }

    /// Handshake if there is no session yet, or the PC has been silent too long. Call it
    /// regularly; it rate-limits itself.
    pub fn maintain(&self) -> io::Result<()> {
        let connected = self.session.lock().unwrap().is_some();
        if !connected || self.quiet_for().is_none_or(|q| q > RECONNECT_AFTER) {
            self.handshake_if_due()?;
        }
        Ok(())
    }

    fn handshake_if_due(&self) -> io::Result<()> {
        let age = self.pending.lock().unwrap().as_ref().map(|(_, t)| t.elapsed());
        if age.is_none_or(|a| a > HANDSHAKE_RETRY) {
            self.start_handshake()?;
        }
        Ok(())
    }

    /// Send a fresh `SESSION_INIT`. The current session keeps working until the new one is up.
    fn start_handshake(&self) -> io::Result<()> {
        let (init, pkt) = crypto::ik_initiate(&self.keys, &self.peer_public)?;
        *self.pending.lock().unwrap() = Some((init, Instant::now()));
        self.sock.send_to(&pkt, self.peer())?;
        Ok(())
    }

    /// Encrypt and send one message. Fails with `NotConnected` before the first handshake.
    pub fn send(&self, msg: &Msg) -> io::Result<()> {
        let sess = self.session.lock().unwrap().clone();
        let Some(sess) = sess else { return Err(io::ErrorKind::NotConnected.into()) };
        sess.send(&self.sock, self.peer(), msg)
    }

    /// Receive one packet, blocking up to a short timeout.
    pub fn recv(&self) -> io::Result<Event> {
        RECV_BUFS.with(|b| {
            let (buf, plain) = &mut *b.borrow_mut();
            let (n, from) = match self.sock.recv_from(buf) {
                Ok(x) => x,
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    return Ok(Event::Nothing)
                }
                Err(e) => return Err(e),
            };
            self.handle(&buf[..n], from, plain)
        })
    }

    fn handle(&self, pkt: &[u8], from: SocketAddr, plain: &mut Vec<u8>) -> io::Result<Event> {
        match pkt.first() {
            Some(&PKT_DATA) => {
                let sess = self.session.lock().unwrap().clone();
                let Some(sess) = sess else { return Ok(Event::Nothing) };
                if sess.open(pkt, plain).is_none() {
                    return Ok(Event::Nothing);
                }
                *self.last_authentic.lock().unwrap() = Some(Instant::now());
                self.follow(from);
                Ok(Msg::decode(plain).map(Event::Msg).unwrap_or(Event::Nothing))
            }
            Some(&PKT_SESSION_ACCEPT) => {
                // Unauthenticated until `ik_finish`: it must answer the handshake in flight.
                let mut pending = self.pending.lock().unwrap();
                let answers_ours = matches!((pending.as_ref(), crypto::accept_index(pkt)), (Some((i, _)), Some(idx)) if i.index == idx);
                let Some((init, _)) = pending.take_if(|_| answers_ours) else { return Ok(Event::Nothing) };
                match crypto::ik_finish(init, pkt) {
                    Ok(sess) => {
                        *self.session.lock().unwrap() = Some(Arc::new(sess));
                        *self.last_authentic.lock().unwrap() = Some(Instant::now());
                        self.follow(from);
                        Ok(Event::Connected)
                    }
                    Err(_) => Ok(Event::Nothing),
                }
            }
            Some(&PKT_UNKNOWN_SESSION) => {
                let idx = wire::decode_unknown_session(pkt);
                let ours = self.session.lock().unwrap().as_ref().map(|s| s.remote_index);
                if idx.is_some() && idx == ours && self.quiet_for().is_none_or(|q| q > HINT_AFTER) {
                    self.handshake_if_due()?;
                    return Ok(Event::Lost);
                }
                Ok(Event::Nothing)
            }
            _ => Ok(Event::Nothing),
        }
    }

    /// Handshake until connected or `timeout`. Optional: [`maintain`](Self::maintain) connects
    /// too; this just waits for it, to report a PC that does not answer.
    pub fn connect(&self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.handshake_if_due()?;
            if let Event::Connected = self.recv()? {
                return Ok(());
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "the PC did not answer"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_answer_to_our_handshake_connects() {
        let pc_keys = StaticKeys::generate();
        let pc = UdpSocket::bind("127.0.0.1:0").unwrap();
        pc.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mac = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mac_addr = mac.local_addr().unwrap();
        let link = Link::new(mac, StaticKeys::generate(), pc_keys.public, pc.local_addr().unwrap()).unwrap();
        // Stray accepts, with no handshake in flight.
        for stray in [&[PKT_SESSION_ACCEPT][..], &[PKT_SESSION_ACCEPT, 1, 0, 0, 0, 2, 0, 0, 0]] {
            pc.send_to(stray, mac_addr).unwrap();
            assert!(matches!(link.recv().unwrap(), Event::Nothing));
        }
        link.maintain().unwrap();
        let mut buf = [0u8; MAX_PACKET];
        let (n, _) = pc.recv_from(&mut buf).unwrap();
        let (_, reply) = crypto::ik_respond(&pc_keys, &buf[..n], |_| true).unwrap();
        // The real answer, but naming another handshake: ignored, and ours stays in flight.
        let mut other = reply.clone();
        other[1] ^= 0xFF;
        pc.send_to(&other, mac_addr).unwrap();
        assert!(matches!(link.recv().unwrap(), Event::Nothing));
        pc.send_to(&reply, mac_addr).unwrap();
        assert!(matches!(link.recv().unwrap(), Event::Connected));
    }
}

//! The PC end of an encrypted channel, the same for input and video: it answers the paired
//! Mac's handshakes, keeps one current session, follows the Mac to a new address, and tells a
//! Mac whose session it does not know (the PC restarted) so it reconnects at once.

use crate::crypto::{self, Session, StaticKeys};
use crate::wire::{self, Msg, PKT_DATA, PKT_SESSION_INIT};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// Fewest gap between "unknown session" hints, which are unauthenticated.
const UNKNOWN_HINT_INTERVAL: Duration = Duration::from_millis(100);

struct Peer {
    session: Session,
    addr: SocketAddr,
}

/// What one datagram was.
#[derive(Debug, PartialEq, Eq)]
pub enum Datagram {
    /// An authentic message from the current session, decrypted into `plain`. `new_session`: the
    /// first of a session that has just replaced the previous one (the Mac started over).
    Data { nonce: u64, new_session: bool },
    /// A session packet with nothing more for the caller: a handshake answered, a hint sent, or
    /// one that did not authenticate.
    Handled,
    /// Not a session packet (a discovery probe, junk).
    Other,
}

pub struct Responder {
    keys: StaticKeys,
    current: Option<Peer>,
    /// Answered, but not current until its first valid datagram: a replayed handshake can
    /// never cut off the live session.
    pending: Option<Peer>,
    hinted_at: Option<Instant>,
}

impl Responder {
    pub fn new(keys: StaticKeys) -> Responder {
        Responder { keys, current: None, pending: None, hinted_at: None }
    }

    /// Handle one datagram. `paired` (the Mac allowed to connect) is asked only for handshakes.
    pub fn handle(
        &mut self,
        sock: &UdpSocket,
        pkt: &[u8],
        from: SocketAddr,
        paired: impl FnOnce() -> Option<[u8; 32]>,
        plain: &mut Vec<u8>,
    ) -> Datagram {
        match pkt.first() {
            Some(&PKT_SESSION_INIT) => {
                if let Some(mac) = paired() {
                    if let Ok((session, reply)) = crypto::ik_respond(&self.keys, pkt, |k| *k == mac) {
                        let _ = sock.send_to(&reply, from); // lost like any datagram; the Mac retries
                        self.pending = Some(Peer { session, addr: from });
                    }
                }
                Datagram::Handled
            }
            Some(&PKT_DATA) => self.data(sock, pkt, from, plain),
            _ => Datagram::Other,
        }
    }

    fn data(&mut self, sock: &UdpSocket, pkt: &[u8], from: SocketAddr, plain: &mut Vec<u8>) -> Datagram {
        let index = Session::data_index(pkt);
        if let Some(p) = self.pending.as_mut().filter(|p| Some(p.session.local_index) == index) {
            let Some(nonce) = p.session.open(pkt, plain) else { return Datagram::Handled };
            p.addr = from;
            self.current = self.pending.take();
            return Datagram::Data { nonce, new_session: true };
        }
        if let Some(c) = self.current.as_mut().filter(|c| Some(c.session.local_index) == index) {
            let Some(nonce) = c.session.open(pkt, plain) else { return Datagram::Handled };
            c.addr = from;
            return Datagram::Data { nonce, new_session: false };
        }
        if let Some(index) = index {
            if self.hinted_at.is_none_or(|t| t.elapsed() > UNKNOWN_HINT_INTERVAL) {
                self.hinted_at = Some(Instant::now());
                let _ = sock.send_to(&wire::encode_unknown_session(index), from);
            }
        }
        Datagram::Handled
    }

    /// Whether a Mac is connected.
    pub fn connected(&self) -> bool {
        self.current.is_some()
    }

    /// Send to the connected Mac, if any. Lost like any datagram: the callers' timers and
    /// repeated requests cover it.
    pub fn send(&self, sock: &UdpSocket, msg: &Msg) {
        if let Some(c) = &self.current {
            let _ = c.session.send(sock, c.addr, msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Mac on its own socket, talking to a responder on another.
    struct Mac {
        sock: UdpSocket,
        keys: StaticKeys,
    }

    impl Mac {
        fn new() -> Mac {
            let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
            sock.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
            Mac { sock, keys: StaticKeys::generate() }
        }

        fn recv(&self) -> Option<Vec<u8>> {
            let mut b = vec![0u8; 2048];
            let n = self.sock.recv(&mut b).ok()?;
            Some(b[..n].to_vec())
        }
    }

    struct Pc {
        sock: UdpSocket,
        keys: StaticKeys,
        responder: Responder,
        plain: Vec<u8>,
    }

    impl Pc {
        fn new() -> Pc {
            let keys = StaticKeys::generate();
            Pc { sock: UdpSocket::bind("127.0.0.1:0").unwrap(), responder: Responder::new(keys.clone()), keys, plain: Vec::new() }
        }

        fn take(&mut self, pkt: &[u8], from: &Mac, paired: &StaticKeys) -> Datagram {
            let from_addr = from.sock.local_addr().unwrap();
            self.responder.handle(&self.sock, pkt, from_addr, || Some(paired.public), &mut self.plain)
        }
    }

    /// Handshake: returns the Mac's session and the init packet (to replay).
    fn connect(pc: &mut Pc, mac: &Mac) -> (Session, Vec<u8>) {
        let (init, pkt) = crypto::ik_initiate(&mac.keys, &pc.keys.public).unwrap();
        assert_eq!(pc.take(&pkt, mac, &mac.keys), Datagram::Handled);
        let accept = mac.recv().expect("the PC answers the paired Mac");
        (crypto::ik_finish(init, &accept).unwrap(), pkt)
    }

    fn data(session: &Session, msg: &Msg) -> Vec<u8> {
        let mut plain = Vec::new();
        msg.encode(&mut plain);
        let mut out = Vec::new();
        session.seal(&plain, &mut out).unwrap();
        out
    }

    #[test]
    fn a_session_is_current_from_its_first_message_and_a_replayed_handshake_cannot_replace_it() {
        let (mut pc, mac) = (Pc::new(), Mac::new());
        let (session, init) = connect(&mut pc, &mac);
        assert!(!pc.responder.connected(), "not current before it has said anything");
        let first = pc.take(&data(&session, &Msg::Ping { t: 1 }), &mac, &mac.keys);
        assert!(matches!(first, Datagram::Data { new_session: true, .. }), "{first:?}");
        assert_eq!(Msg::decode(&pc.plain), Some(Msg::Ping { t: 1 }));

        // An attacker replays the handshake: answered, but the live session carries on.
        assert_eq!(pc.take(&init, &mac, &mac.keys), Datagram::Handled);
        let _ = mac.recv();
        let next = pc.take(&data(&session, &Msg::Ping { t: 2 }), &mac, &mac.keys);
        assert!(matches!(next, Datagram::Data { new_session: false, .. }), "{next:?}");
    }

    #[test]
    fn only_the_paired_mac_gets_an_answer() {
        let (mut pc, mac, stranger) = (Pc::new(), Mac::new(), Mac::new());
        let (_, pkt) = crypto::ik_initiate(&stranger.keys, &pc.keys.public).unwrap();
        assert_eq!(pc.take(&pkt, &stranger, &mac.keys), Datagram::Handled);
        assert_eq!(stranger.recv(), None);
    }

    #[test]
    fn the_mac_is_followed_to_a_new_address() {
        let (mut pc, mac) = (Pc::new(), Mac::new());
        let (session, _) = connect(&mut pc, &mac);
        pc.take(&data(&session, &Msg::Ping { t: 1 }), &mac, &mac.keys);
        let moved = Mac { sock: UdpSocket::bind("127.0.0.1:0").unwrap(), keys: mac.keys.clone() };
        moved.sock.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        pc.take(&data(&session, &Msg::Ping { t: 2 }), &moved, &mac.keys);
        pc.responder.send(&pc.sock, &Msg::Pong { t: 2 });
        assert!(moved.recv().is_some(), "replies go to where the Mac is now");
    }

    #[test]
    fn a_mac_with_an_unknown_session_is_told_so_but_not_flooded() {
        let (mut pc, mac) = (Pc::new(), Mac::new());
        let (session, _) = connect(&mut pc, &mac);
        // The PC restarted: it no longer knows this session.
        let mut restarted = Pc { keys: pc.keys.clone(), responder: Responder::new(pc.keys.clone()), sock: pc.sock, plain: Vec::new() };
        let pkt = data(&session, &Msg::Ping { t: 1 });
        assert_eq!(restarted.take(&pkt, &mac, &mac.keys), Datagram::Handled);
        let hint = mac.recv().expect("a hint");
        assert_eq!(wire::decode_unknown_session(&hint), Some(session.remote_index));
        restarted.take(&pkt, &mac, &mac.keys);
        assert_eq!(mac.recv(), None, "at most one hint per interval");
    }
}

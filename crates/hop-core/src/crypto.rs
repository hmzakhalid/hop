//! Noise-based security.
//!
//! - Pairing: `Noise_XXpsk3` over TCP. The PSK comes from a 12-character code that the PC shows
//!   and the user types on the Mac. Each side pins the other's static X25519 key.
//! - Sessions: `Noise_IK` over UDP. The PC only accepts the Mac key it pinned.
//! - Datagrams: ChaCha20-Poly1305 with an explicit 64-bit nonce and a 64-packet replay window.

use crate::wire::{truncate_utf8, Msg, Reader, Writer, MAX_NAME, MAX_PACKET, PKT_DATA, PKT_SESSION_ACCEPT, PKT_SESSION_INIT};
use blake2::{Blake2s256, Digest};
use rand::{Rng, RngCore};
use snow::{Builder, HandshakeState, StatelessTransportState};
use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const IK: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
pub const PAIR: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"hop v1";
const PAIR_CONFIRM: &[u8] = b"hop paired v1";
const TAG: usize = 16;
/// Bytes a `DATA` packet adds around its plaintext: type, receiver index, nonce and tag.
pub const DATA_OVERHEAD: usize = 1 + 4 + 8 + TAG;

fn err(kind: io::ErrorKind, msg: impl Into<String>) -> io::Error {
    io::Error::new(kind, msg.into())
}

fn noise_err(e: snow::Error) -> io::Error {
    err(io::ErrorKind::InvalidData, format!("noise: {e}"))
}

fn builder(pattern: &str) -> Builder<'_> {
    Builder::new(pattern.parse().expect("valid noise pattern"))
}

#[derive(Clone)]
pub struct StaticKeys {
    pub private: [u8; 32],
    pub public: [u8; 32],
}

impl std::fmt::Debug for StaticKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StaticKeys({})", fingerprint(&self.public))
    }
}

impl StaticKeys {
    pub fn generate() -> Self {
        let kp = builder(IK).generate_keypair().expect("keypair");
        Self { private: kp.private.try_into().unwrap(), public: kp.public.try_into().unwrap() }
    }
}

/// Short, human-comparable key fingerprint, e.g. `3f9a-1c2e-77b0`.
pub fn fingerprint(public: &[u8; 32]) -> String {
    let h = Blake2s256::digest(public);
    format!("{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}", h[0], h[1], h[2], h[3], h[4], h[5])
}

const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ"; // Crockford base32

/// A fresh pairing code: 12 characters (60 bits), shown as `XXXX-XXXX-XXXX`.
pub fn new_pairing_code() -> String {
    let mut rng = rand::thread_rng();
    let chars: String = (0..12).map(|_| CODE_ALPHABET[rng.gen_range(0..32)] as char).collect();
    format!("{}-{}-{}", &chars[0..4], &chars[4..8], &chars[8..12])
}

/// Canonical form of a typed code: upper case, separators dropped, look-alikes folded.
fn normalize_code(input: &str) -> Option<String> {
    let mut out = String::with_capacity(12);
    for c in input.chars() {
        let c = match c.to_ascii_uppercase() {
            '-' | ' ' | '_' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        if !CODE_ALPHABET.contains(&(c as u8)) {
            return None;
        }
        out.push(c);
    }
    (out.len() == 12).then_some(out)
}

/// Domain-separated 32-byte PSK from a normalised code.
fn psk_from_code(code: &str) -> [u8; 32] {
    let mut h = Blake2s256::new();
    h.update(b"hop pairing psk v1\0");
    h.update(code.as_bytes());
    h.finalize().into()
}

/// Sliding 64-packet anti-replay window.
#[derive(Debug, Default)]
struct ReplayWindow {
    top: u64,
    bits: u64,
    any: bool,
}

impl ReplayWindow {
    fn check(&mut self, n: u64) -> bool {
        if !self.any {
            self.any = true;
            self.top = n;
            self.bits = 1;
            return true;
        }
        if n > self.top {
            let shift = n - self.top;
            self.bits = if shift >= 64 { 0 } else { self.bits << shift };
            self.bits |= 1;
            self.top = n;
            return true;
        }
        let off = self.top - n;
        if off >= 64 || self.bits & (1 << off) != 0 {
            return false;
        }
        self.bits |= 1 << off;
        true
    }
}

/// An established session. Safe to share between threads.
pub struct Session {
    pub local_index: u32,
    pub remote_index: u32,
    transport: StatelessTransportState,
    send_nonce: AtomicU64,
    replay: Mutex<ReplayWindow>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Session({:08x}→{:08x})", self.local_index, self.remote_index)
    }
}

impl Session {
    fn new(hs: HandshakeState, local_index: u32, remote_index: u32) -> io::Result<Self> {
        Ok(Self {
            local_index,
            remote_index,
            transport: hs.into_stateless_transport_mode().map_err(noise_err)?,
            send_nonce: AtomicU64::new(0),
            replay: Mutex::new(ReplayWindow::default()),
        })
    }

    /// Encrypt `plaintext` into a complete `DATA` packet in `out`. Returns the nonce used.
    pub fn seal(&self, plaintext: &[u8], out: &mut Vec<u8>) -> io::Result<u64> {
        let nonce = self.send_nonce.fetch_add(1, Ordering::Relaxed);
        out.clear();
        let mut w = Writer::new(out);
        w.u8(PKT_DATA);
        w.u32(self.remote_index);
        w.u64(nonce);
        let head = out.len();
        out.resize(head + plaintext.len() + TAG, 0);
        let n = self.transport.write_message(nonce, plaintext, &mut out[head..]).map_err(noise_err)?;
        out.truncate(head + n);
        Ok(nonce)
    }

    /// Encode, seal and send one message.
    pub fn send(&self, sock: &UdpSocket, to: SocketAddr, msg: &Msg) -> io::Result<()> {
        thread_local! {
            static BUFS: RefCell<(Vec<u8>, Vec<u8>)> =
                RefCell::new((Vec::with_capacity(MAX_PACKET), Vec::with_capacity(MAX_PACKET)));
        }
        BUFS.with(|b| {
            let (plain, packet) = &mut *b.borrow_mut();
            plain.clear();
            msg.encode(plain);
            self.seal(plain, packet)?;
            sock.send_to(packet, to).map(|_| ())
        })
    }

    /// The receiver index of a `DATA` packet, to pick the session.
    pub fn data_index(packet: &[u8]) -> Option<u32> {
        let mut r = Reader::new(packet);
        (r.u8()? == PKT_DATA).then(|| r.u32()).flatten()
    }

    /// Decrypt a `DATA` packet addressed to this session. Returns the nonce, or `None` if the
    /// packet is forged, corrupt or replayed.
    pub fn open(&self, packet: &[u8], out: &mut Vec<u8>) -> Option<u64> {
        let mut r = Reader::new(packet);
        if r.u8()? != PKT_DATA || r.u32()? != self.local_index {
            return None;
        }
        let nonce = r.u64()?;
        let ct = r.rest();
        out.clear();
        out.resize(ct.len(), 0);
        let n = self.transport.read_message(nonce, ct, out).ok()?;
        out.truncate(n);
        // Only authenticated packets may move the window.
        self.replay.lock().unwrap().check(nonce).then_some(nonce)
    }
}

fn random_index() -> u32 {
    rand::thread_rng().next_u32() | 1
}

/// Mac side of a session handshake in flight.
pub struct IkInitiator {
    hs: HandshakeState,
    pub index: u32,
}

/// Start a session: returns the handshake state and the `SESSION_INIT` packet to send.
pub fn ik_initiate(local: &StaticKeys, remote_public: &[u8; 32]) -> io::Result<(IkInitiator, Vec<u8>)> {
    let mut hs = builder(IK)
        .local_private_key(&local.private)
        .and_then(|b| b.remote_public_key(remote_public))
        .and_then(|b| b.prologue(PROLOGUE))
        .and_then(|b| b.build_initiator())
        .map_err(noise_err)?;
    let index = random_index();
    let mut pkt = Vec::with_capacity(128);
    let mut w = Writer::new(&mut pkt);
    w.u8(PKT_SESSION_INIT);
    w.u32(index);
    let head = pkt.len();
    pkt.resize(head + 256, 0);
    let n = hs.write_message(&[], &mut pkt[head..]).map_err(noise_err)?;
    pkt.truncate(head + n);
    Ok((IkInitiator { hs, index }, pkt))
}

/// The initiator index a `SESSION_ACCEPT` answers.
pub fn accept_index(packet: &[u8]) -> Option<u32> {
    let mut r = Reader::new(packet);
    (r.u8()? == PKT_SESSION_ACCEPT).then(|| r.u32()).flatten()
}

/// Finish a session from the PC's `SESSION_ACCEPT`.
pub fn ik_finish(mut init: IkInitiator, packet: &[u8]) -> io::Result<Session> {
    let mut r = Reader::new(packet);
    let bad = || err(io::ErrorKind::InvalidData, "bad session accept");
    if r.u8().ok_or_else(bad)? != PKT_SESSION_ACCEPT || r.u32().ok_or_else(bad)? != init.index {
        return Err(bad());
    }
    let responder_index = r.u32().ok_or_else(bad)?;
    let mut payload = [0u8; 64];
    init.hs.read_message(r.rest(), &mut payload).map_err(noise_err)?;
    Session::new(init.hs, init.index, responder_index)
}

/// PC side: answer a `SESSION_INIT` if (and only if) it comes from an allowed key.
/// Returns the new session and the `SESSION_ACCEPT` packet.
pub fn ik_respond(
    local: &StaticKeys,
    packet: &[u8],
    allowed: impl Fn(&[u8; 32]) -> bool,
) -> io::Result<(Session, Vec<u8>)> {
    let mut r = Reader::new(packet);
    let bad = || err(io::ErrorKind::InvalidData, "bad session init");
    if r.u8().ok_or_else(bad)? != PKT_SESSION_INIT {
        return Err(bad());
    }
    let initiator_index = r.u32().ok_or_else(bad)?;
    let mut hs = builder(IK)
        .local_private_key(&local.private)
        .and_then(|b| b.prologue(PROLOGUE))
        .and_then(|b| b.build_responder())
        .map_err(noise_err)?;
    let mut payload = [0u8; 64];
    hs.read_message(r.rest(), &mut payload).map_err(noise_err)?;
    let remote: [u8; 32] = hs.get_remote_static().and_then(|k| k.try_into().ok()).ok_or_else(bad)?;
    if !allowed(&remote) {
        return Err(err(io::ErrorKind::PermissionDenied, "key is not paired"));
    }
    let index = random_index();
    let mut pkt = Vec::with_capacity(128);
    let mut w = Writer::new(&mut pkt);
    w.u8(PKT_SESSION_ACCEPT);
    w.u32(initiator_index);
    w.u32(index);
    let head = pkt.len();
    pkt.resize(head + 256, 0);
    let n = hs.write_message(&[], &mut pkt[head..]).map_err(noise_err)?;
    pkt.truncate(head + n);
    Ok((Session::new(hs, index, initiator_index)?, pkt))
}

// ---- pairing over TCP ----------------------------------------------------------------------

fn send_frame(s: &mut impl Write, b: &[u8]) -> io::Result<()> {
    let len = u16::try_from(b.len()).map_err(|_| err(io::ErrorKind::InvalidInput, "frame too big"))?;
    s.write_all(&len.to_be_bytes())?;
    s.write_all(b)?;
    s.flush()
}

fn recv_frame(s: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    s.read_exact(&mut len)?;
    let mut b = vec![0u8; u16::from_be_bytes(len) as usize];
    s.read_exact(&mut b)?;
    Ok(b)
}

fn pair_builder<'a>(local: &'a StaticKeys, psk: &'a [u8; 32]) -> Result<Builder<'a>, snow::Error> {
    builder(PAIR).local_private_key(&local.private)?.psk(3, psk)?.prologue(PROLOGUE)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    pub public: [u8; 32],
    pub name: String,
}

/// Mac side of pairing. `code` is what the user typed.
pub fn pair_initiator(s: &mut (impl Read + Write), local: &StaticKeys, code: &str, my_name: &str) -> io::Result<Paired> {
    let code = normalize_code(code).ok_or_else(|| err(io::ErrorKind::InvalidInput, "a code is 12 letters or digits"))?;
    let psk = psk_from_code(&code);
    let mut hs = pair_builder(local, &psk).and_then(|b| b.build_initiator()).map_err(noise_err)?;
    let mut buf = vec![0u8; 1024];
    let mut payload = vec![0u8; 1024];
    let n = hs.write_message(&[], &mut buf).map_err(noise_err)?;
    send_frame(s, &buf[..n])?;
    let m2 = recv_frame(s)?;
    let n = hs.read_message(&m2, &mut payload).map_err(noise_err)?;
    let pc_name = String::from_utf8_lossy(&payload[..n]).into_owned();
    let pc_key: [u8; 32] = hs
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| err(io::ErrorKind::InvalidData, "no PC key"))?;
    let n = hs.write_message(truncate_utf8(my_name, MAX_NAME).as_bytes(), &mut buf).map_err(noise_err)?;
    send_frame(s, &buf[..n])?;
    // Only a PC that knows the code can produce this confirmation.
    let confirm = recv_frame(s).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            err(io::ErrorKind::PermissionDenied, "the PC rejected the code")
        } else {
            e
        }
    })?;
    let mut t = hs.into_transport_mode().map_err(noise_err)?;
    let n = t
        .read_message(&confirm, &mut payload)
        .map_err(|_| err(io::ErrorKind::PermissionDenied, "wrong code"))?;
    if &payload[..n] != PAIR_CONFIRM {
        return Err(err(io::ErrorKind::PermissionDenied, "wrong code"));
    }
    Ok(Paired { public: pc_key, name: pc_name })
}

/// PC side of pairing. Fails with `PermissionDenied` when the Mac used a different code.
pub fn pair_responder(s: &mut (impl Read + Write), local: &StaticKeys, code: &str, my_name: &str) -> io::Result<Paired> {
    let code = normalize_code(code).ok_or_else(|| err(io::ErrorKind::InvalidInput, "bad code"))?;
    let psk = psk_from_code(&code);
    let mut hs = pair_builder(local, &psk).and_then(|b| b.build_responder()).map_err(noise_err)?;
    let mut buf = vec![0u8; 1024];
    let mut payload = vec![0u8; 1024];
    let m1 = recv_frame(s)?;
    hs.read_message(&m1, &mut payload).map_err(noise_err)?;
    let n = hs.write_message(truncate_utf8(my_name, MAX_NAME).as_bytes(), &mut buf).map_err(noise_err)?;
    send_frame(s, &buf[..n])?;
    let m3 = recv_frame(s)?;
    let n = hs
        .read_message(&m3, &mut payload)
        .map_err(|_| err(io::ErrorKind::PermissionDenied, "wrong code"))?;
    let mac_name = String::from_utf8_lossy(&payload[..n]).into_owned();
    let mac_key: [u8; 32] = hs
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| err(io::ErrorKind::InvalidData, "no Mac key"))?;
    let mut t = hs.into_transport_mode().map_err(noise_err)?;
    let n = t.write_message(PAIR_CONFIRM, &mut buf).map_err(noise_err)?;
    send_frame(s, &buf[..n])?;
    Ok(Paired { public: mac_key, name: mac_name })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn codes_normalise() {
        let c = new_pairing_code();
        assert_eq!(c.len(), 14);
        let n = normalize_code(&c).unwrap();
        assert_eq!(normalize_code(&n.to_lowercase()), Some(n.clone()));
        assert_eq!(normalize_code("o1il-0000-0000"), Some("0111".to_string() + "00000000"));
        assert_eq!(normalize_code("short"), None);
        assert_eq!(normalize_code("UUUU-UUUU-UUUU"), None); // U is not in the alphabet
        assert_ne!(psk_from_code("000000000000"), psk_from_code("000000000001"));
    }

    #[test]
    fn replay_window() {
        let mut w = ReplayWindow::default();
        assert!(w.check(5));
        assert!(!w.check(5));
        assert!(w.check(3));
        assert!(!w.check(3));
        assert!(w.check(100));
        assert!(!w.check(36)); // too old
        assert!(w.check(37));
        assert!(w.check(1000));
    }

    fn ik_pair() -> (StaticKeys, StaticKeys, Session, Session) {
        let mac = StaticKeys::generate();
        let pc = StaticKeys::generate();
        let (init, p1) = ik_initiate(&mac, &pc.public).unwrap();
        let (pc_sess, p2) = ik_respond(&pc, &p1, |k| *k == mac.public).unwrap();
        assert_eq!(accept_index(&p2), Some(init.index));
        let mac_sess = ik_finish(init, &p2).unwrap();
        (mac, pc, mac_sess, pc_sess)
    }

    #[test]
    fn session_roundtrip_and_replay_rejected() {
        let (_, _, m, p) = ik_pair();
        let mut pkt = Vec::new();
        let mut out = Vec::new();
        m.seal(b"hello", &mut pkt).unwrap();
        assert_eq!(Session::data_index(&pkt), Some(p.local_index));
        assert_eq!(p.open(&pkt, &mut out), Some(0));
        assert_eq!(out, b"hello");
        assert_eq!(p.open(&pkt, &mut out), None, "replay");
        let mut tampered = pkt.clone();
        *tampered.last_mut().unwrap() ^= 1;
        m.seal(b"x", &mut pkt).unwrap();
        // Out-of-order delivery is fine.
        let mut pkt2 = Vec::new();
        m.seal(b"y", &mut pkt2).unwrap();
        assert_eq!(p.open(&pkt2, &mut out), Some(2));
        assert_eq!(p.open(&pkt, &mut out), Some(1));
        assert_eq!(p.open(&tampered, &mut out), None);
        // And the other direction.
        p.seal(b"ack", &mut pkt).unwrap();
        assert_eq!(m.open(&pkt, &mut out), Some(0));
    }

    #[test]
    fn unpaired_key_is_rejected() {
        let mac = StaticKeys::generate();
        let stranger = StaticKeys::generate();
        let pc = StaticKeys::generate();
        let (_, p1) = ik_initiate(&stranger, &pc.public).unwrap();
        let e = ik_respond(&pc, &p1, |k| *k == mac.public).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn initiator_needs_the_real_pc_key() {
        let mac = StaticKeys::generate();
        let pc = StaticKeys::generate();
        let impostor = StaticKeys::generate();
        let (_, p1) = ik_initiate(&mac, &pc.public).unwrap();
        assert!(ik_respond(&impostor, &p1, |_| true).is_err());
    }

    fn run_pairing(mac_code: &str, pc_code: &str) -> (io::Result<Paired>, io::Result<Paired>, StaticKeys, StaticKeys) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let pc = StaticKeys::generate();
        let mac = StaticKeys::generate();
        let pc2 = pc.clone();
        let pc_code = pc_code.to_string();
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            pair_responder(&mut s, &pc2, &pc_code, "Desk PC")
        });
        let mut s = TcpStream::connect(addr).unwrap();
        let m = pair_initiator(&mut s, &mac, mac_code, "MacBook");
        drop(s);
        (m, t.join().unwrap(), mac, pc)
    }

    #[test]
    fn pairing_with_the_right_code_pins_both_keys() {
        let code = new_pairing_code();
        let (m, p, mac, pc) = run_pairing(&code.to_lowercase(), &code);
        let (m, p) = (m.unwrap(), p.unwrap());
        assert_eq!(m, Paired { public: pc.public, name: "Desk PC".into() });
        assert_eq!(p, Paired { public: mac.public, name: "MacBook".into() });
    }

    #[test]
    fn pairing_with_the_wrong_code_fails_on_both_sides() {
        let (m, p, _, _) = run_pairing("0000-0000-0000", "0000-0000-0001");
        assert_eq!(p.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(m.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
}

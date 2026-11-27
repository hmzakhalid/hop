//! Pairing by asking: the Mac asks to pair, the PC asks whoever is at it, and both screens show
//! the same six-digit check number. Nothing is typed; the number shows nobody is in the middle.
//!
//! Over TCP on [`port`], beside the code's port (which a code shown beside the Windows service
//! holds):
//!
//! 1. Mac → PC: Noise XX message 1 (`e`).
//! 2. PC → Mac: message 2 (`e, ee, s, es`). Payload: a commitment to the PC's nonce, then its name.
//! 3. Mac → PC: message 3 (`s, se`). Payload: the Mac's nonce, then its name.
//! 4. PC → Mac: the PC's nonce. The Mac checks it against the commitment.
//! 5. PC → Mac: the answer (allowed, declined, needs an administrator, nobody answered).
//! 6. Mac → PC, if allowed: "saved", once the Mac has stored the pairing. Only then does the PC
//!    store it, so a Mac that gave up never replaces the PC's pairing.
//!
//! The check number hashes the handshake with both nonces. A machine in the middle runs two
//! handshakes and must fix its own nonce in each before it learns the other side's, so it cannot
//! make the two numbers agree, except by a one-in-a-million chance per try.

use crate::crypto::{builder, err, noise_err, recv_frame, send_frame, Bounded, PairError, Paired, StaticKeys, PAIRING_EXCHANGE};
use crate::wire::{truncate_utf8, MAX_NAME};
use blake2::{Blake2s256, Digest};
use rand::RngCore;
use snow::TransportState;
use std::io;
use std::net::TcpStream;
use std::time::{Duration, Instant};

const PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"hop ask v1";
const SAVED: &[u8] = b"saved";
const NONCE: usize = 32;

/// How long the person at the PC has to answer.
pub const ANSWER_TIME: Duration = Duration::from_secs(60);
/// How much longer the Mac waits for the answer than the PC lets the person take.
const ANSWER_SLACK: Duration = Duration::from_secs(5);

/// The TCP port for asking, next to the agent's `port` (whose number the code's TCP listener
/// shares, and the screen view's UDP port is the one after).
pub fn port(port: u16) -> Option<u16> {
    port.checked_add(2)
}

/// What the PC answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Allowed,
    Declined,
    /// The person signed in at the PC is not an administrator; a code still works.
    NeedsAdmin,
    /// Nobody answered in time (nobody there, or the PC is locked).
    NoAnswer,
}

impl Answer {
    fn byte(self) -> u8 {
        match self {
            Answer::Allowed => b'y',
            Answer::Declined => b'n',
            Answer::NeedsAdmin => b'a',
            Answer::NoAnswer => b't',
        }
    }

    fn from_byte(b: u8) -> Option<Answer> {
        Some(match b {
            b'y' => Answer::Allowed,
            b'n' => Answer::Declined,
            b'a' => Answer::NeedsAdmin,
            b't' => Answer::NoAnswer,
            _ => return None,
        })
    }
}

/// The check number as both screens show it: `482 913`.
pub fn check_text(check: u32) -> String {
    format!("{:03} {:03}", check / 1000 % 1000, check % 1000)
}

/// A name from the other machine, fit to show in a prompt: no control or invisible formatting
/// characters (which could reorder or hide text), and not too long.
fn clean_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let invisible = |c: char| c.is_control() || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}');
    let name: String = s.chars().filter(|c| !invisible(*c)).take(40).collect();
    let name = name.trim();
    if name.is_empty() { "Unnamed".into() } else { name.into() }
}

fn nonce() -> [u8; NONCE] {
    let mut n = [0u8; NONCE];
    rand::thread_rng().fill_bytes(&mut n);
    n
}

fn commitment(nonce: &[u8]) -> [u8; 32] {
    let mut h = Blake2s256::new();
    h.update(b"hop ask commit v1\0");
    h.update(nonce);
    h.finalize().into()
}

fn check_number(handshake: &[u8], mac_nonce: &[u8], pc_nonce: &[u8]) -> u32 {
    let mut h = Blake2s256::new();
    h.update(b"hop ask check v1\0");
    h.update(handshake);
    h.update(mac_nonce);
    h.update(pc_nonce);
    let d = h.finalize();
    u32::from_le_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000
}

fn bad(what: &str) -> PairError {
    PairError::Io(err(io::ErrorKind::InvalidData, what))
}

fn remote_key(hs: &snow::HandshakeState) -> Result<[u8; 32], PairError> {
    hs.get_remote_static().and_then(|k| k.try_into().ok()).ok_or_else(|| bad("no key"))
}

/// One encrypted message over the stream, by `until`.
fn send(s: &mut TcpStream, until: Instant, t: &mut TransportState, msg: &[u8]) -> Result<(), PairError> {
    let mut buf = [0u8; 128];
    let n = t.write_message(msg, &mut buf).map_err(noise_err)?;
    Ok(send_frame(&mut Bounded { s, until }, &buf[..n])?)
}

fn recv(s: &mut TcpStream, until: Instant, t: &mut TransportState) -> Result<Vec<u8>, PairError> {
    let frame = recv_frame(&mut Bounded { s, until })?;
    let mut buf = vec![0u8; frame.len()];
    let n = t.read_message(&frame, &mut buf).map_err(noise_err)?;
    buf.truncate(n);
    Ok(buf)
}

/// The Mac's side, once the handshake is done and the check number known.
pub struct Asking {
    s: TcpStream,
    t: TransportState,
    pub pc: Paired,
    pub check: u32,
}

/// Ask the PC on `s` to pair (the handshake and the check number, within [`PAIRING_EXCHANGE`]).
pub fn ask(mut s: TcpStream, local: &StaticKeys, my_name: &str) -> Result<Asking, PairError> {
    let until = Instant::now() + PAIRING_EXCHANGE;
    let mut hs = builder(PATTERN)
        .local_private_key(&local.private)
        .and_then(|b| b.prologue(PROLOGUE))
        .and_then(|b| b.build_initiator())
        .map_err(noise_err)?;
    let mut buf = [0u8; 1024];
    let mut payload = [0u8; 1024];
    let n = hs.write_message(&[], &mut buf).map_err(noise_err)?;
    send_frame(&mut Bounded { s: &mut s, until }, &buf[..n])?;
    let m2 = recv_frame(&mut Bounded { s: &mut s, until })?;
    let n = hs.read_message(&m2, &mut payload).map_err(noise_err)?;
    let (promise, pc_name) = payload[..n].split_at_checked(32).ok_or_else(|| bad("short message"))?;
    let promise: [u8; 32] = promise.try_into().unwrap();
    let pc = Paired { public: remote_key(&hs)?, name: clean_name(pc_name) };
    let mine = nonce();
    let mut m3 = mine.to_vec();
    m3.extend_from_slice(truncate_utf8(my_name, MAX_NAME).as_bytes());
    let n = hs.write_message(&m3, &mut buf).map_err(noise_err)?;
    send_frame(&mut Bounded { s: &mut s, until }, &buf[..n])?;
    let handshake = hs.get_handshake_hash().to_vec();
    let mut t = hs.into_transport_mode().map_err(noise_err)?;
    let theirs = recv(&mut s, until, &mut t)?;
    if theirs.len() != NONCE || commitment(&theirs) != promise {
        return Err(bad("the PC broke its promise; something on the network may be interfering"));
    }
    let check = check_number(&handshake, &mine, &theirs);
    Ok(Asking { s, t, pc, check })
}

impl Asking {
    /// A handle that ends the wait from another thread (shut it down to cancel).
    pub fn canceller(&self) -> io::Result<TcpStream> {
        self.s.try_clone()
    }

    /// Wait for the PC's answer (the person there has [`ANSWER_TIME`]).
    pub fn answer(&mut self) -> Result<Answer, PairError> {
        let got = recv(&mut self.s, Instant::now() + ANSWER_TIME + ANSWER_SLACK, &mut self.t)?;
        match got[..] {
            [b] => Answer::from_byte(b).ok_or_else(|| bad("unknown answer")),
            _ => Err(bad("unknown answer")),
        }
    }

    /// Tell the PC this Mac has stored the pairing, so the PC stores it too.
    pub fn saved(mut self) -> Result<(), PairError> {
        send(&mut self.s, Instant::now() + PAIRING_EXCHANGE, &mut self.t, SAVED)
    }
}

/// The PC's side: a Mac that asked, with the number to show.
pub struct Request {
    s: TcpStream,
    t: TransportState,
    pub mac: Paired,
    pub check: u32,
}

/// Take a Mac's request on `s` (the handshake and the check number, within [`PAIRING_EXCHANGE`]).
pub fn accept(mut s: TcpStream, local: &StaticKeys, my_name: &str) -> Result<Request, PairError> {
    let until = Instant::now() + PAIRING_EXCHANGE;
    let mut hs = builder(PATTERN)
        .local_private_key(&local.private)
        .and_then(|b| b.prologue(PROLOGUE))
        .and_then(|b| b.build_responder())
        .map_err(noise_err)?;
    let mut buf = [0u8; 1024];
    let mut payload = [0u8; 1024];
    let m1 = recv_frame(&mut Bounded { s: &mut s, until })?;
    hs.read_message(&m1, &mut payload).map_err(noise_err)?;
    let mine = nonce();
    let mut m2 = commitment(&mine).to_vec();
    m2.extend_from_slice(truncate_utf8(my_name, MAX_NAME).as_bytes());
    let n = hs.write_message(&m2, &mut buf).map_err(noise_err)?;
    send_frame(&mut Bounded { s: &mut s, until }, &buf[..n])?;
    let m3 = recv_frame(&mut Bounded { s: &mut s, until })?;
    let n = hs.read_message(&m3, &mut payload).map_err(noise_err)?;
    let (theirs, mac_name) = payload[..n].split_at_checked(NONCE).ok_or_else(|| bad("short message"))?;
    let mac = Paired { public: remote_key(&hs)?, name: clean_name(mac_name) };
    let check = check_number(hs.get_handshake_hash(), theirs, &mine);
    let mut t = hs.into_transport_mode().map_err(noise_err)?;
    send(&mut s, until, &mut t, &mine)?;
    Ok(Request { s, t, mac, check })
}

impl Request {
    /// Tell the Mac it may not pair (best effort: it may have gone).
    pub fn refuse(mut self, why: Answer) {
        debug_assert!(why != Answer::Allowed);
        let _ = send(&mut self.s, Instant::now() + PAIRING_EXCHANGE, &mut self.t, &[why.byte()]);
    }

    /// Tell the Mac yes, and wait for it to have stored the pairing. An error means the Mac did
    /// not confirm, and the PC must not store the pairing.
    pub fn allow(mut self) -> Result<Paired, PairError> {
        let until = Instant::now() + PAIRING_EXCHANGE;
        send(&mut self.s, until, &mut self.t, &[Answer::Allowed.byte()])?;
        if recv(&mut self.s, until, &mut self.t)? != SAVED {
            return Err(bad("the Mac did not confirm"));
        }
        Ok(self.mac)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// A connected pair of TCP streams on loopback: (Mac's end, PC's end).
    fn streams() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let mac = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        (mac, l.accept().unwrap().0)
    }

    /// How the Mac's side ended: the answer, the PC, the number.
    type MacEnd = Result<(Answer, Paired, u32), PairError>;
    /// How the PC's side ended: the Mac if it paired, the number.
    type PcEnd = Result<(Option<Paired>, u32), PairError>;

    /// Run a whole exchange where the PC answers `answer`.
    fn exchange(answer: Answer) -> (MacEnd, PcEnd) {
        let (mac_s, pc_s) = streams();
        let (mac, pc) = (StaticKeys::generate(), StaticKeys::generate());
        let pc_side = thread::spawn(move || {
            let req = accept(pc_s, &pc, "Desk PC")?;
            let check = req.check;
            if answer == Answer::Allowed {
                req.allow().map(|p| (Some(p), check))
            } else {
                req.refuse(answer);
                Ok((None, check))
            }
        });
        let mac_side = (|| {
            let mut a = ask(mac_s, &mac, "MacBook")?;
            let got = a.answer()?;
            let (pc, check) = (a.pc.clone(), a.check);
            if got == Answer::Allowed {
                a.saved()?;
            }
            Ok((got, pc, check))
        })();
        (mac_side, pc_side.join().unwrap())
    }

    #[test]
    fn allowed_pairs_both_sides_with_the_same_number() {
        let (mac, pc) = exchange(Answer::Allowed);
        let (answer, pc_seen, mac_check) = mac.unwrap();
        let (mac_seen, pc_check) = pc.unwrap();
        assert_eq!(answer, Answer::Allowed);
        assert_eq!(pc_seen.name, "Desk PC");
        assert_eq!(mac_seen.unwrap().name, "MacBook");
        assert_eq!(mac_check, pc_check);
        assert!(mac_check < 1_000_000);
    }

    #[test]
    fn each_refusal_reaches_the_mac() {
        for why in [Answer::Declined, Answer::NeedsAdmin, Answer::NoAnswer] {
            let (mac, pc) = exchange(why);
            assert_eq!(mac.unwrap().0, why);
            assert!(pc.unwrap().0.is_none());
        }
    }

    #[test]
    fn the_pc_does_not_pair_with_a_mac_that_gave_up() {
        let (mac_s, pc_s) = streams();
        let (mac, pc) = (StaticKeys::generate(), StaticKeys::generate());
        let pc_side = thread::spawn(move || accept(pc_s, &pc, "PC").and_then(|r| r.allow()));
        let a = ask(mac_s, &mac, "Mac").unwrap();
        drop(a); // the Mac cancels before the answer
        assert!(pc_side.join().unwrap().is_err());
    }

    #[test]
    fn a_cancelled_wait_ends_at_once() {
        let (mac_s, pc_s) = streams();
        let (mac, pc) = (StaticKeys::generate(), StaticKeys::generate());
        let _pc_side = thread::spawn(move || {
            let r = accept(pc_s, &pc, "PC");
            thread::sleep(Duration::from_secs(3)); // the person has not answered yet
            drop(r);
        });
        let mut a = ask(mac_s, &mac, "Mac").unwrap();
        let cancel = a.canceller().unwrap();
        let start = Instant::now();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            let _ = cancel.shutdown(std::net::Shutdown::Both);
        });
        assert!(a.answer().is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    /// A PC that reveals a nonce other than the one it committed to is caught: otherwise a
    /// machine in the middle could choose its nonce after seeing the Mac's.
    #[test]
    fn a_broken_promise_is_caught() {
        let (mac_s, mut pc_s) = streams();
        let mac = StaticKeys::generate();
        let pc = StaticKeys::generate();
        let pc_side = thread::spawn(move || {
            let until = Instant::now() + PAIRING_EXCHANGE;
            let mut hs = builder(PATTERN).local_private_key(&pc.private).and_then(|b| b.prologue(PROLOGUE)).and_then(|b| b.build_responder()).unwrap();
            let (mut buf, mut payload) = ([0u8; 1024], [0u8; 1024]);
            let m1 = recv_frame(&mut Bounded { s: &mut pc_s, until }).unwrap();
            hs.read_message(&m1, &mut payload).unwrap();
            let mut m2 = commitment(&nonce()).to_vec();
            m2.extend_from_slice(b"PC");
            let n = hs.write_message(&m2, &mut buf).unwrap();
            send_frame(&mut Bounded { s: &mut pc_s, until }, &buf[..n]).unwrap();
            let m3 = recv_frame(&mut Bounded { s: &mut pc_s, until }).unwrap();
            hs.read_message(&m3, &mut payload).unwrap();
            let mut t = hs.into_transport_mode().unwrap();
            send(&mut pc_s, until, &mut t, &nonce()).unwrap(); // not the promised one
        });
        let r = ask(mac_s, &mac, "Mac");
        assert!(matches!(r, Err(PairError::Io(ref e)) if e.kind() == io::ErrorKind::InvalidData), "{:?}", r.err());
        pc_side.join().unwrap();
    }

    #[test]
    fn names_shown_in_a_prompt_are_clean() {
        assert_eq!(clean_name(b"Sam's MacBook"), "Sam's MacBook");
        assert_eq!(clean_name("evil\u{202E}koobcaM\nline".as_bytes()), "evilkoobcaMline");
        assert_eq!(clean_name(b"  \x07 "), "Unnamed");
        assert_eq!(clean_name("x".repeat(100).as_bytes()).len(), 40);
    }

    #[test]
    fn the_number_reads_as_two_groups_of_three() {
        assert_eq!(check_text(482_913), "482 913");
        assert_eq!(check_text(7), "000 007");
    }
}

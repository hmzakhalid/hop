//! Wire format. Everything is little-endian.
//!
//! Outer packets (UDP):
//! - `PROBE`          `[0x01][MAGIC][u32 probe_id]`
//! - `PROBE_REPLY`    `[0x02][MAGIC][u32 probe_id][32 pubkey][u8 len][name]`
//! - `SESSION_INIT`   `[0x20][u32 initiator_index][noise IK msg1]`
//! - `SESSION_ACCEPT` `[0x21][u32 initiator_index][u32 responder_index][noise IK msg2]`
//! - `DATA`           `[0x30][u32 receiver_index][u64 nonce][ciphertext]`
//! - `UNKNOWN_SESSION``[0x31][u32 index]`
//!
//! The plaintext of a `DATA` packet is one [`Msg`].

pub const MAGIC: [u8; 4] = *b"HOP1";
pub const DEFAULT_PORT: u16 = 47820;

pub const PKT_PROBE: u8 = 0x01;
pub const PKT_PROBE_REPLY: u8 = 0x02;
pub const PKT_SESSION_INIT: u8 = 0x20;
pub const PKT_SESSION_ACCEPT: u8 = 0x21;
pub const PKT_DATA: u8 = 0x30;
pub const PKT_UNKNOWN_SESSION: u8 = 0x31;

/// Most unacknowledged transitions a `State` may carry. Past this the Mac ends the epoch.
pub const MAX_TRANSITIONS: usize = 64;
pub const MAX_PRESSED: usize = 32;
pub const MAX_MONITORS: usize = 16;
/// Large enough for any packet we build (a full `State` is about 1 KB).
pub const MAX_PACKET: usize = 1400;

pub const ACK_ACTIVE: u8 = 1;
/// The PC cannot inject right now (lock screen, UAC prompt, other secure desktop).
pub const ACK_BLOCKED: u8 = 2;
/// The PC's dead-man timer fired for this epoch; the Mac must end it.
pub const ACK_EPOCH_DEAD: u8 = 4;

/// The Mac takes its input back when no state it sent in this long has been acknowledged.
pub const LEASE_US: u64 = 500_000;
/// The PC releases everything held after this long without a datagram from the Mac.
pub const DEADMAN_US: u64 = 750_000;
// The Mac must give up first. If the PC let go first, a later datagram's state repair could
// press a dropped drag again before the Mac noticed the silence.
const _: () = assert!(DEADMAN_US > LEASE_US);

/// Longest machine name sent in a probe reply or during pairing, in bytes.
pub const MAX_NAME: usize = 64;

/// Windows key codes carried on the wire (`u16`): `0x00xx` is a set-1 scan code, `0x01xx` an
/// extended (E0-prefixed) one.
pub mod code {
    pub const EXTENDED: u16 = 0x0100;
    pub fn is_extended(c: u16) -> bool {
        c & 0xFF00 == EXTENDED
    }
    pub fn low(c: u16) -> u8 {
        (c & 0xFF) as u8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransKind {
    KeyDown = 0,
    KeyUp = 1,
    KeyRepeat = 2,
    ButtonDown = 3,
    ButtonUp = 4,
}

impl TransKind {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::KeyDown,
            1 => Self::KeyUp,
            2 => Self::KeyRepeat,
            3 => Self::ButtonDown,
            4 => Self::ButtonUp,
            _ => return None,
        })
    }
}

/// One discrete input change. `x`/`y` is the PC cursor position when it happened,
/// so a click replayed after loss still lands where the user clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition {
    pub kind: TransKind,
    /// Windows key code (see [`code`]) or mouse button index (0 left, 1 right, 2 middle, 3/4 X1/X2).
    pub code: u16,
    pub x: i32,
    pub y: i32,
}

/// Mac → PC. Every datagram is the full state, so any one of them repairs earlier loss.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct State {
    pub epoch: u32,
    pub active: bool,
    /// Mac monotonic clock, echoed back in the ack for round-trip time.
    pub send_time_us: u64,
    pub x: i32,
    pub y: i32,
    /// Cumulative scroll for this epoch, in Windows wheel units (120 = one notch).
    pub scroll_v: i64,
    pub scroll_h: i64,
    pub buttons: u8,
    /// Windows keys that should be down right now.
    pub pressed: Vec<u16>,
    /// Event id of `transitions[0]`; ids are consecutive and start at 1 each epoch.
    pub first_event: u64,
    pub transitions: Vec<Transition>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Monitor {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    /// DPI scale × 1000 (1000 = 100 %, 1500 = 150 %).
    pub scale_milli: u16,
}

/// PC → Mac.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Ack {
    pub epoch: u32,
    /// Highest contiguous event id applied in `epoch`.
    pub applied_event: u64,
    pub echo_time_us: u64,
    /// Time the PC held the echoed state before acking, for an honest RTT.
    pub hold_us: u32,
    pub flags: u8,
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub monitors: Vec<Monitor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    State(State),
    Ack(Ack),
    Ping { t: u64 },
    Pong { t: u64 },
    /// Screen view, on the video channel: see [`crate::video`].
    VideoWant(crate::video::Want),
    VideoFrag(crate::video::Frag),
    /// Echo of a `VideoWant`, for the clock offset, with the PC's current bitrate.
    VideoClock { mac_time_us: u64, pc_time_us: u64, bitrate: u32 },
    /// The PC pointer moved (pixels of the streamed monitor), or was hidden. Sent the moment it
    /// changes, apart from frames, so the viewer's cursor moves smoothly; `seq` orders them.
    VideoCursor { seq: u32, at: Option<(i32, i32)> },
    /// Something the viewer should show: why there is no picture.
    VideoNote(String),
}

const MSG_STATE: u8 = 1;
const MSG_ACK: u8 = 2;
const MSG_PING: u8 = 3;
const MSG_PONG: u8 = 4;
const MSG_VIDEO_WANT: u8 = 5;
const MSG_VIDEO_FRAG: u8 = 6;
const MSG_VIDEO_CLOCK: u8 = 7;
const MSG_VIDEO_CURSOR: u8 = 8;
const MSG_VIDEO_NOTE: u8 = 9;

pub struct Writer<'a> {
    buf: &'a mut Vec<u8>,
}

impl<'a> Writer<'a> {
    pub fn new(buf: &'a mut Vec<u8>) -> Self {
        Self { buf }
    }
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v)
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes())
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes())
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes())
    }
    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes())
    }
    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes())
    }
    pub fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v)
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    pub fn rest(&mut self) -> &'a [u8] {
        let s = &self.buf[self.pos..];
        self.pos = self.buf.len();
        s
    }
    pub fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    pub fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    pub fn done(&self) -> bool {
        self.pos == self.buf.len()
    }
}

impl Msg {
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut w = Writer::new(out);
        match self {
            Msg::State(s) => {
                w.u8(MSG_STATE);
                w.u32(s.epoch);
                w.u8(s.active as u8);
                w.u64(s.send_time_us);
                w.i32(s.x);
                w.i32(s.y);
                w.i64(s.scroll_v);
                w.i64(s.scroll_h);
                w.u8(s.buttons);
                let np = s.pressed.len().min(MAX_PRESSED);
                w.u8(np as u8);
                for &c in &s.pressed[..np] {
                    w.u16(c);
                }
                w.u64(s.first_event);
                let nt = s.transitions.len().min(MAX_TRANSITIONS);
                w.u8(nt as u8);
                for t in &s.transitions[..nt] {
                    w.u8(t.kind as u8);
                    w.u16(t.code);
                    w.i32(t.x);
                    w.i32(t.y);
                }
            }
            Msg::Ack(a) => {
                w.u8(MSG_ACK);
                w.u32(a.epoch);
                w.u64(a.applied_event);
                w.u64(a.echo_time_us);
                w.u32(a.hold_us);
                w.u8(a.flags);
                w.i32(a.cursor_x);
                w.i32(a.cursor_y);
                let nm = a.monitors.len().min(MAX_MONITORS);
                w.u8(nm as u8);
                for m in &a.monitors[..nm] {
                    w.i32(m.left);
                    w.i32(m.top);
                    w.i32(m.right);
                    w.i32(m.bottom);
                    w.u16(m.scale_milli);
                }
            }
            Msg::Ping { t } => {
                w.u8(MSG_PING);
                w.u64(*t);
            }
            Msg::Pong { t } => {
                w.u8(MSG_PONG);
                w.u64(*t);
            }
            Msg::VideoWant(v) => {
                w.u8(MSG_VIDEO_WANT);
                w.u8(v.watch as u8 | (v.keyframe as u8) << 1);
                w.u64(v.mac_time_us);
                w.u32(v.complete);
                w.u32(v.lost);
                w.u32(v.delay_us);
            }
            Msg::VideoFrag(f) => {
                w.u8(MSG_VIDEO_FRAG);
                w.u32(f.frame);
                w.u32(f.len);
                w.u8(f.block);
                w.u8(f.blocks);
                w.u8(f.shard);
                w.u8(f.data_shards);
                w.u8(f.parity_shards);
                w.u8(f.flags);
                w.u64(f.captured_us);
                w.u32(f.pc_us);
                w.u16(f.data.len() as u16);
                w.bytes(&f.data);
            }
            Msg::VideoClock { mac_time_us, pc_time_us, bitrate } => {
                w.u8(MSG_VIDEO_CLOCK);
                w.u64(*mac_time_us);
                w.u64(*pc_time_us);
                w.u32(*bitrate);
            }
            Msg::VideoCursor { seq, at } => {
                w.u8(MSG_VIDEO_CURSOR);
                w.u32(*seq);
                w.u8(at.is_some() as u8);
                let (x, y) = at.unwrap_or((0, 0));
                w.i32(x);
                w.i32(y);
            }
            Msg::VideoNote(text) => {
                w.u8(MSG_VIDEO_NOTE);
                let t = truncate_utf8(text, 400);
                w.u16(t.len() as u16);
                w.bytes(t.as_bytes());
            }
        }
    }

    pub fn decode(buf: &[u8]) -> Option<Msg> {
        let mut r = Reader::new(buf);
        let msg = match r.u8()? {
            MSG_STATE => {
                let epoch = r.u32()?;
                let active = r.u8()? != 0;
                let send_time_us = r.u64()?;
                let x = r.i32()?;
                let y = r.i32()?;
                let scroll_v = r.i64()?;
                let scroll_h = r.i64()?;
                let buttons = r.u8()?;
                let np = r.u8()? as usize;
                if np > MAX_PRESSED {
                    return None;
                }
                let mut pressed = Vec::with_capacity(np);
                for _ in 0..np {
                    pressed.push(r.u16()?);
                }
                let first_event = r.u64()?;
                let nt = r.u8()? as usize;
                if nt > MAX_TRANSITIONS {
                    return None;
                }
                let mut transitions = Vec::with_capacity(nt);
                for _ in 0..nt {
                    let kind = TransKind::from_u8(r.u8()?)?;
                    let code = r.u16()?;
                    let x = r.i32()?;
                    let y = r.i32()?;
                    transitions.push(Transition { kind, code, x, y });
                }
                Msg::State(State {
                    epoch,
                    active,
                    send_time_us,
                    x,
                    y,
                    scroll_v,
                    scroll_h,
                    buttons,
                    pressed,
                    first_event,
                    transitions,
                })
            }
            MSG_ACK => {
                let epoch = r.u32()?;
                let applied_event = r.u64()?;
                let echo_time_us = r.u64()?;
                let hold_us = r.u32()?;
                let flags = r.u8()?;
                let cursor_x = r.i32()?;
                let cursor_y = r.i32()?;
                let nm = r.u8()? as usize;
                if nm > MAX_MONITORS {
                    return None;
                }
                let mut monitors = Vec::with_capacity(nm);
                for _ in 0..nm {
                    monitors.push(Monitor {
                        left: r.i32()?,
                        top: r.i32()?,
                        right: r.i32()?,
                        bottom: r.i32()?,
                        scale_milli: r.u16()?,
                    });
                }
                Msg::Ack(Ack {
                    epoch,
                    applied_event,
                    echo_time_us,
                    hold_us,
                    flags,
                    cursor_x,
                    cursor_y,
                    monitors,
                })
            }
            MSG_PING => Msg::Ping { t: r.u64()? },
            MSG_PONG => Msg::Pong { t: r.u64()? },
            MSG_VIDEO_WANT => {
                let bits = r.u8()?;
                Msg::VideoWant(crate::video::Want {
                    watch: bits & 1 != 0,
                    keyframe: bits & 2 != 0,
                    mac_time_us: r.u64()?,
                    complete: r.u32()?,
                    lost: r.u32()?,
                    delay_us: r.u32()?,
                })
            }
            MSG_VIDEO_FRAG => {
                let (frame, len) = (r.u32()?, r.u32()?);
                let (block, blocks, shard, data_shards, parity_shards, flags) = (r.u8()?, r.u8()?, r.u8()?, r.u8()?, r.u8()?, r.u8()?);
                let (captured_us, pc_us) = (r.u64()?, r.u32()?);
                let n = r.u16()? as usize;
                let data = r.take(n)?.to_vec();
                Msg::VideoFrag(crate::video::Frag {
                    frame,
                    len,
                    block,
                    blocks,
                    shard,
                    data_shards,
                    parity_shards,
                    flags,
                    captured_us,
                    pc_us,
                    data,
                })
            }
            MSG_VIDEO_CLOCK => Msg::VideoClock { mac_time_us: r.u64()?, pc_time_us: r.u64()?, bitrate: r.u32()? },
            MSG_VIDEO_CURSOR => {
                let (seq, visible, x, y) = (r.u32()?, r.u8()?, r.i32()?, r.i32()?);
                Msg::VideoCursor { seq, at: (visible != 0).then_some((x, y)) }
            }
            MSG_VIDEO_NOTE => {
                let n = r.u16()? as usize;
                Msg::VideoNote(String::from_utf8_lossy(r.take(n)?).into_owned())
            }
            _ => return None,
        };
        if !r.done() {
            return None;
        }
        Some(msg)
    }
}

pub fn encode_probe(probe_id: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(9);
    let mut w = Writer::new(&mut v);
    w.u8(PKT_PROBE);
    w.bytes(&MAGIC);
    w.u32(probe_id);
    v
}

pub fn decode_probe(buf: &[u8]) -> Option<u32> {
    let mut r = Reader::new(buf);
    if r.u8()? != PKT_PROBE || r.take(4)? != MAGIC {
        return None;
    }
    r.u32()
}

pub fn encode_probe_reply(probe_id: u32, pubkey: &[u8; 32], name: &str) -> Vec<u8> {
    let name = truncate_utf8(name, MAX_NAME);
    let mut v = Vec::with_capacity(42 + name.len());
    let mut w = Writer::new(&mut v);
    w.u8(PKT_PROBE_REPLY);
    w.bytes(&MAGIC);
    w.u32(probe_id);
    w.bytes(pubkey);
    w.u8(name.len() as u8);
    w.bytes(name.as_bytes());
    v
}

pub fn decode_probe_reply(buf: &[u8]) -> Option<(u32, [u8; 32], String)> {
    let mut r = Reader::new(buf);
    if r.u8()? != PKT_PROBE_REPLY || r.take(4)? != MAGIC {
        return None;
    }
    let id = r.u32()?;
    let key: [u8; 32] = r.take(32)?.try_into().ok()?;
    let n = r.u8()? as usize;
    let name = String::from_utf8_lossy(r.take(n)?).into_owned();
    Some((id, key, name))
}

/// The PC's answer to a `DATA` packet for a session it does not have (it restarted).
pub fn encode_unknown_session(index: u32) -> [u8; 5] {
    let i = index.to_le_bytes();
    [PKT_UNKNOWN_SESSION, i[0], i[1], i[2], i[3]]
}

pub fn decode_unknown_session(buf: &[u8]) -> Option<u32> {
    let mut r = Reader::new(buf);
    if r.u8()? != PKT_UNKNOWN_SESSION {
        return None;
    }
    r.u32()
}

pub fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrip() {
        let s = State {
            epoch: 7,
            active: true,
            send_time_us: 123456789,
            x: -1920,
            y: 540,
            scroll_v: -360,
            scroll_h: 17,
            buttons: 0b101,
            pressed: vec![0x1D, 0x11D, 0x0F],
            first_event: 42,
            transitions: vec![
                Transition { kind: TransKind::KeyDown, code: 0x1E, x: 1, y: 2 },
                Transition { kind: TransKind::ButtonUp, code: 0, x: -5, y: 9 },
            ],
        };
        let mut b = Vec::new();
        Msg::State(s.clone()).encode(&mut b);
        assert_eq!(Msg::decode(&b), Some(Msg::State(s)));
    }

    #[test]
    fn ack_roundtrip_and_truncation_rejected() {
        let a = Ack {
            epoch: 3,
            applied_event: 99,
            echo_time_us: 5,
            hold_us: 12,
            flags: ACK_ACTIVE,
            cursor_x: 10,
            cursor_y: 20,
            monitors: vec![Monitor { left: 0, top: 0, right: 2560, bottom: 1440, scale_milli: 1250 }],
        };
        let mut b = Vec::new();
        Msg::Ack(a.clone()).encode(&mut b);
        assert_eq!(Msg::decode(&b), Some(Msg::Ack(a)));
        for cut in 0..b.len() {
            assert_eq!(Msg::decode(&b[..cut]), None, "truncated at {cut} must not decode");
        }
        let mut extra = b.clone();
        extra.push(0);
        assert_eq!(Msg::decode(&extra), None);
    }

    #[test]
    fn full_state_fits_one_datagram() {
        let s = State {
            pressed: vec![0; MAX_PRESSED],
            transitions: vec![Transition { kind: TransKind::KeyDown, code: 0, x: 0, y: 0 }; MAX_TRANSITIONS],
            ..Default::default()
        };
        let mut b = Vec::new();
        Msg::State(s).encode(&mut b);
        assert!(b.len() + crate::crypto::DATA_OVERHEAD <= MAX_PACKET, "state is {} bytes", b.len());
    }

    #[test]
    fn probe_roundtrip() {
        let k = [7u8; 32];
        assert_eq!(decode_probe(&encode_probe(5)), Some(5));
        assert_eq!(decode_probe_reply(&encode_probe_reply(5, &k, "Desk PC")), Some((5, k, "Desk PC".into())));
        assert_eq!(decode_unknown_session(&encode_unknown_session(0xDEAD_BEEF)), Some(0xDEAD_BEEF));
    }
}

//! Screen view: how encoded frames cross the network. Pure logic: no capture, no decoding.
//!
//! Video has its own encrypted UDP channel, on the port after input's ([`port_for`]), so input
//! never waits behind video on either machine. While a viewer is open, the Mac sends a
//! [`Want`] every [`WANT_EVERY_US`]; the PC streams while those keep coming and stops capturing
//! when they stop.
//!
//! Each encoded frame is cut into [`Frag`]ments. A frame that cannot be completed, or a skipped
//! frame id, breaks the decoder's reference chain, so the Mac drops frames until a keyframe and
//! asks for one (see [`Reassembler`]). Reed–Solomon parity in every frame absorbs most losses
//! before it comes to that.

use crate::wire::{Frag, FRAG_HEVC, FRAG_KEYFRAME};
use reed_solomon_erasure::galois_8::ReedSolomon;

/// The video channel's UDP port is the input port plus this.
const PORT_OFFSET: u16 = 1;

/// The video channel's port for the input channel's `port`; `None` for the very last port.
pub fn port_for(port: u16) -> Option<u16> {
    port.checked_add(PORT_OFFSET)
}
/// Frame bytes per fragment. With its header and encryption a fragment is about 1.25 KB, under
/// any common path MTU.
pub const FRAG_DATA: usize = 1200;
const _: () = assert!(crate::wire::FRAG_HEADER + FRAG_DATA + crate::crypto::DATA_OVERHEAD <= crate::wire::MAX_PACKET);
/// The Mac repeats its request this often while watching.
pub const WANT_EVERY_US: u64 = 100_000;
/// The PC stops streaming this long after the last request.
pub const WANT_TIMEOUT_US: u64 = 1_000_000;
/// The PC frees the capturer (and the GPU encoder) after this long without a viewer.
pub const IDLE_RELEASE_US: u64 = 5_000_000;
const _: () = assert!(WANT_EVERY_US < WANT_TIMEOUT_US && WANT_TIMEOUT_US < IDLE_RELEASE_US);
/// Keyframe requests closer together than this are merged: a keyframe is large, and a burst of
/// them on a lossy link would only make things worse.
pub const KEYFRAME_SPACING_US: u64 = 250_000;

/// Data shards per FEC block at most (the code works on at most 256 shards per block).
pub const BLOCK_DATA: usize = 100;
/// Parity shards per block, as a share of its data shards (at least one). Any set of as many
/// shards as there are data shards rebuilds the block, so up to this share can be lost.
pub const PARITY_RATIO: f32 = 0.25;

/// One encoded screen image (Annex B), as the PC's encoder produced it and the Mac's decoder
/// takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoded {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// HEVC (H.265) rather than H.264.
    pub hevc: bool,
    /// When the image was grabbed, on the PC's [`crate::clock::now_us`] clock.
    pub captured_us: u64,
    /// How long the PC held it before it was ready to send: waiting for its frame slot,
    /// converting, encoding.
    pub pc_us: u32,
}

/// An [`Encoded`] image numbered for the trip: frame ids are consecutive, so a gap is a loss.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub id: u32,
    pub encoded: Encoded,
}

/// Reed–Solomon codecs by shape, built once each (building one inverts a matrix).
#[derive(Default)]
struct Codecs(std::collections::HashMap<(usize, usize), ReedSolomon>);

impl Codecs {
    fn get(&mut self, data: usize, parity: usize) -> Option<&ReedSolomon> {
        use std::collections::hash_map::Entry;
        match self.0.entry((data, parity)) {
            Entry::Occupied(e) => Some(e.into_mut()),
            Entry::Vacant(v) => Some(v.insert(ReedSolomon::new(data, parity).ok()?)),
        }
    }
}

/// The byte range of block `b` within a frame of `len` bytes.
fn block_range(len: usize, b: usize) -> std::ops::Range<usize> {
    let size = BLOCK_DATA * FRAG_DATA;
    (b * size).min(len)..((b + 1) * size).min(len)
}

/// Shape of a block of `bytes`: (data shards, shard size).
fn block_shape(bytes: usize) -> (usize, usize) {
    let k = bytes.div_ceil(FRAG_DATA).max(1);
    (k, bytes.div_ceil(k).max(1))
}

fn parity_for(data_shards: usize) -> usize {
    ((data_shards as f32 * PARITY_RATIO).ceil() as usize).max(1)
}

/// Cuts frames into FEC-protected fragments (PC side).
#[derive(Default)]
pub struct Fragmenter {
    codecs: Codecs,
}

impl Fragmenter {
    pub fn split(&mut self, f: &Frame) -> Vec<Frag> {
        let (id, e) = (f.id, &f.encoded);
        let len = e.data.len();
        let blocks = len.div_ceil(BLOCK_DATA * FRAG_DATA).max(1);
        let mut flags = if e.keyframe { FRAG_KEYFRAME } else { 0 };
        if e.hevc {
            flags |= FRAG_HEVC;
        }
        let mut out = Vec::new();
        for b in 0..blocks {
            let bytes = &e.data[block_range(len, b)];
            let (k, size) = block_shape(bytes.len());
            let m = parity_for(k);
            let mut shards: Vec<Vec<u8>> = (0..k)
                .map(|i| {
                    let mut s = bytes[(i * size).min(bytes.len())..((i + 1) * size).min(bytes.len())].to_vec();
                    s.resize(size, 0);
                    s
                })
                .chain((0..m).map(|_| vec![0; size]))
                .collect();
            let codec = self.codecs.get(k, m).expect("k and m are within the code's limits");
            codec.encode(&mut shards).expect("shards have equal sizes");
            for (i, data) in shards.into_iter().enumerate() {
                out.push(Frag {
                    frame: id,
                    len: len as u32,
                    block: b as u8,
                    blocks: blocks as u8,
                    shard: i as u8,
                    data_shards: k as u8,
                    parity_shards: m as u8,
                    flags,
                    captured_us: e.captured_us,
                    pc_us: e.pc_us,
                    data,
                });
            }
        }
        out
    }
}

/// `a` is newer than `b`, with wrap-around.
fn newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < u32::MAX / 2
}

struct Block {
    shards: Vec<Option<Vec<u8>>>,
    have: usize,
    data: Option<Vec<u8>>,
}

struct Partial {
    first: Frag,
    blocks: Vec<Block>,
    done: usize,
}

/// Puts fragments back together (rebuilding lost shards from parity), in order, and notices loss.
///
/// Frames must reach the decoder complete and in sequence. When one cannot be rebuilt (too many
/// shards lost before a newer frame starts, or a frame id is skipped), everything until the next
/// keyframe is dropped and [`wants_keyframe`](Self::wants_keyframe) says to ask for one.
#[derive(Default)]
pub struct Reassembler {
    partial: Option<Partial>,
    /// The last frame handed out, or abandoned.
    last: Option<u32>,
    waiting_for_key: bool,
    codecs: Codecs,
    pub complete: u32,
    pub lost: u32,
    /// Shards rebuilt from parity (losses FEC absorbed).
    pub repaired: u32,
}

impl Reassembler {
    pub fn wants_keyframe(&self) -> bool {
        self.waiting_for_key || self.last.is_none()
    }

    /// The decoder failed (or was reset): drop everything until a keyframe.
    pub fn need_keyframe(&mut self) {
        self.waiting_for_key = true;
    }

    /// The sender started over (its process restarted), so its frame numbers start again:
    /// forget the old ones and wait for a keyframe. The counts carry on.
    pub fn restart(&mut self) {
        (self.partial, self.last, self.waiting_for_key) = (None, None, false);
    }

    pub fn push(&mut self, f: Frag) -> Option<Frame> {
        if self.last.is_some_and(|l| !newer(f.frame, l)) {
            return None; // late or duplicate
        }
        match &self.partial {
            Some(p) if p.first.frame == f.frame => {}
            Some(p) if newer(p.first.frame, f.frame) => return None, // older than the one in progress
            Some(p) => {
                // A newer frame started before this one could be rebuilt: this one is lost.
                let abandoned = p.first.frame;
                self.lose_through(abandoned);
                self.partial = None;
            }
            None => {}
        }
        if !self.accept(&f) {
            return None;
        }
        let p = self.partial.get_or_insert_with(|| Partial {
            blocks: (0..f.blocks).map(|_| Block { shards: Vec::new(), have: 0, data: None }).collect(),
            done: 0,
            first: f.clone(),
        });
        let (b, i) = (f.block as usize, f.shard as usize);
        let (k, m) = (f.data_shards as usize, f.parity_shards as usize);
        let block = &mut p.blocks[b];
        if block.data.is_some() {
            return None; // already rebuilt; a spare shard
        }
        if block.shards.is_empty() {
            block.shards = vec![None; k + m];
        }
        if block.shards.len() != k + m || block.shards.iter().flatten().next().is_some_and(|s| s.len() != f.data.len()) {
            return None; // inconsistent with the shards before it
        }
        if block.shards[i].is_some() {
            return None;
        }
        block.shards[i] = Some(f.data);
        block.have += 1;
        if block.have < k {
            return None;
        }
        let missing = block.shards[..k].iter().filter(|s| s.is_none()).count();
        if missing > 0 && self.codecs.get(k, m).is_none_or(|c| c.reconstruct_data(&mut block.shards).is_err()) {
            return None;
        }
        self.repaired += missing as u32;
        let len = p.first.len as usize;
        let mut data: Vec<u8> = block.shards[..k].iter().flatten().flatten().copied().collect();
        data.truncate(block_range(len, b).len());
        block.data = Some(data);
        block.shards = Vec::new();
        p.done += 1;
        if p.done < p.blocks.len() {
            return None;
        }
        let p = self.partial.take().unwrap();
        self.finish(p)
    }

    /// A fragment fits the frame in progress (or can start one).
    fn accept(&self, f: &Frag) -> bool {
        let shape_ok = f.blocks > 0
            && f.block < f.blocks
            && f.data_shards > 0
            && f.parity_shards > 0
            && (f.shard as usize) < f.data_shards as usize + f.parity_shards as usize
            && f.blocks as usize == (f.len as usize).div_ceil(BLOCK_DATA * FRAG_DATA).max(1)
            && f.data_shards as usize == block_shape(block_range(f.len as usize, f.block as usize).len()).0;
        let same_frame = self.partial.as_ref().is_none_or(|p| p.first.frame != f.frame || (p.first.len == f.len && p.first.blocks == f.blocks));
        shape_ok && same_frame
    }

    fn finish(&mut self, p: Partial) -> Option<Frame> {
        let id = p.first.frame;
        let keyframe = p.first.flags & FRAG_KEYFRAME != 0;
        // Anything skipped between the last frame and this one was lost entirely.
        if let Some(l) = self.last {
            if id.wrapping_sub(l) > 1 {
                self.lost += id.wrapping_sub(l) - 1;
                self.waiting_for_key = true;
            }
        }
        let waiting = self.wants_keyframe(); // also before the very first frame
        self.last = Some(id);
        if waiting && !keyframe {
            self.waiting_for_key = true;
            return None;
        }
        self.waiting_for_key = false;
        self.complete += 1;
        Some(Frame {
            id,
            encoded: Encoded {
                data: p.blocks.into_iter().flat_map(|b| b.data.unwrap_or_default()).collect(),
                keyframe,
                hevc: p.first.flags & FRAG_HEVC != 0,
                captured_us: p.first.captured_us,
                pc_us: p.first.pc_us,
            },
        })
    }

    fn lose_through(&mut self, id: u32) {
        let gap = match self.last {
            Some(l) => id.wrapping_sub(l),
            None => 1,
        };
        self.lost += gap.max(1);
        self.last = Some(id);
        self.waiting_for_key = true;
    }
}

/// Frames are never encoded faster than this (a 144 Hz display would otherwise double the data).
pub const MAX_FPS: u32 = 60;
pub const START_BITRATE: u32 = 12_000_000;
pub const MIN_BITRATE: u32 = 2_000_000;
pub const MAX_BITRATE: u32 = 40_000_000;
/// Delay above the path's floor that means a queue is building.
const CONGESTED_US: u64 = 15_000;
/// Delay close enough to the floor to try a higher bitrate.
const CLEAR_US: u64 = 6_000;
/// The floor is the lowest delay seen recently (it absorbs any clock-offset error).
const FLOOR_WINDOW_US: u64 = 10_000_000;

/// Keeps the stream from sending faster than the network carries.
///
/// Delay-based, like real-time video calls: a queue anywhere on the path shows up as frames
/// taking longer to arrive, long before anything is dropped. When the Mac's reported delay rises
/// [`CONGESTED_US`] above its recent floor, cut the bitrate and pause briefly so the queue
/// drains; while it stays near the floor, raise the bitrate step by step.
pub struct RateControl {
    pub bitrate: u32,
    floor: Option<(u64, u64)>, // (delay, when seen)
    last_change: u64,
    clear_since: Option<u64>,
    pause_until: u64,
}

impl Default for RateControl {
    fn default() -> Self {
        RateControl { bitrate: START_BITRATE, floor: None, last_change: 0, clear_since: None, pause_until: 0 }
    }
}

impl RateControl {
    /// A report from the Mac. Returns a new bitrate when it should change.
    pub fn report(&mut self, now: u64, delay_us: u32) -> Option<u32> {
        if delay_us == 0 {
            // No frames lately (a still screen): no evidence the path is clear, so no raise
            // until clear frames have been seen for a while again.
            self.clear_since = None;
            return None;
        }
        let d = delay_us as u64;
        if self.floor.is_none_or(|(f, at)| d <= f || now.saturating_sub(at) > FLOOR_WINDOW_US) {
            self.floor = Some((d, now));
        }
        let excess = d.saturating_sub(self.floor.map_or(d, |(f, _)| f));
        if excess > CONGESTED_US {
            self.clear_since = None;
            // Let the queue drain before sending more, but never stall for long.
            self.pause_until = now + excess.min(150_000);
            if now.saturating_sub(self.last_change) < 300_000 {
                return None; // give the last cut time to show
            }
            self.last_change = now;
            let next = ((self.bitrate as u64 * 3 / 4) as u32).max(MIN_BITRATE);
            return (next != self.bitrate).then(|| {
                self.bitrate = next;
                next
            });
        }
        if excess < CLEAR_US {
            let since = *self.clear_since.get_or_insert(now);
            if now.saturating_sub(since) >= 1_000_000 && now.saturating_sub(self.last_change) >= 1_000_000 {
                self.last_change = now;
                self.clear_since = Some(now);
                let next = ((self.bitrate as u64 * 11 / 10) as u32).min(MAX_BITRATE);
                return (next != self.bitrate).then(|| {
                    self.bitrate = next;
                    next
                });
            }
        } else {
            self.clear_since = None;
        }
        None
    }

    /// Whether to hold off sending new frames, so a queue can drain.
    pub fn paused(&self, now: u64) -> bool {
        now < self.pause_until
    }
}

/// The PC clock minus the Mac clock, from `VideoClock` replies. Keeps the sample with the
/// shortest round trip, the least distorted by queueing.
#[derive(Default)]
pub struct ClockOffset {
    best: Option<(u64, i64)>,
}

impl ClockOffset {
    /// The PC's clock started over (its process restarted).
    pub fn restart(&mut self) {
        self.best = None;
    }

    pub fn sample(&mut self, mac_sent_us: u64, pc_time_us: u64, mac_now_us: u64) {
        let rtt = mac_now_us.saturating_sub(mac_sent_us);
        let offset = pc_time_us as i64 - (mac_sent_us + rtt / 2) as i64;
        if self.best.is_none_or(|(b, _)| rtt <= b) {
            self.best = Some((rtt, offset));
        }
    }

    /// The Mac-clock time of a PC-clock instant, once known.
    pub fn to_mac(&self, pc_us: u64) -> Option<u64> {
        self.best.map(|(_, off)| (pc_us as i64 - off).max(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: u32, len: usize, keyframe: bool) -> Frame {
        let data = (0..len).map(|i| (i * 7 + id as usize) as u8).collect();
        Frame { id, encoded: Encoded { data, keyframe, hevc: id.is_multiple_of(2), captured_us: 5, pc_us: 2 } }
    }

    fn split(f: &Frame) -> Vec<Frag> {
        Fragmenter::default().split(f)
    }

    fn feed(r: &mut Reassembler, frags: Vec<Frag>) -> Vec<Frame> {
        frags.into_iter().filter_map(|f| r.push(f)).collect()
    }

    #[test]
    fn frames_survive_fragmentation_and_reordering() {
        let mut r = Reassembler::default();
        let key = frame(1, 5000, true);
        let mut frags = split(&key);
        assert_eq!(frags.len(), 5 + 2, "5 data shards and 2 parity");
        frags.reverse();
        frags.push(frags[0].clone()); // a duplicate
        assert_eq!(feed(&mut r, frags), vec![key]);
        let small = frame(2, 10, false);
        assert_eq!(feed(&mut r, split(&small)), vec![small]);
        assert_eq!((r.complete, r.lost, r.wants_keyframe()), (2, 0, false));
    }

    #[test]
    fn parity_rebuilds_lost_shards() {
        let mut r = Reassembler::default();
        let key = frame(1, 50_000, true);
        let mut frags = split(&key);
        let parity = frags.iter().filter(|f| f.shard >= f.data_shards).count();
        // Lose as many data shards as there is parity, spread through the frame.
        let mut lost = 0;
        frags.retain(|f| {
            let drop = f.shard < f.data_shards && f.shard % 4 == 1 && lost < parity;
            lost += drop as usize;
            !drop
        });
        assert_eq!(feed(&mut r, frags), vec![key]);
        assert_eq!((r.repaired as usize, r.lost), (parity, 0));
    }

    #[test]
    fn big_frames_span_several_blocks() {
        let mut r = Reassembler::default();
        let big = frame(1, 300_000, true); // more data than one block holds
        let mut frags = split(&big);
        assert_eq!(frags[0].blocks, 3);
        frags.retain(|f| f.shard != 0); // one lost shard in every block
        assert_eq!(feed(&mut r, frags), vec![big]);
    }

    #[test]
    fn a_frame_beyond_repair_drops_frames_until_a_keyframe() {
        let mut r = Reassembler::default();
        feed(&mut r, split(&frame(1, 100, true)));
        let mut broken = split(&frame(2, 3000, false)); // 3 data shards + 1 parity
        broken.retain(|f| f.shard >= 2); // two lost: more than parity covers
        assert!(feed(&mut r, broken).is_empty());
        // Frame 3 arrives whole, but frame 2 is gone: 3 cannot be decoded.
        assert!(feed(&mut r, split(&frame(3, 100, false))).is_empty());
        assert!(r.wants_keyframe());
        assert_eq!(r.lost, 1);
        let key = frame(4, 2000, true);
        assert_eq!(feed(&mut r, split(&key)), vec![key]);
        assert!(!r.wants_keyframe());
    }

    #[test]
    fn a_wholly_missing_frame_is_noticed() {
        let mut r = Reassembler::default();
        feed(&mut r, split(&frame(1, 10, true)));
        // Frame 2 never arrives.
        assert!(feed(&mut r, split(&frame(3, 10, false))).is_empty());
        assert!(r.wants_keyframe());
        assert_eq!(r.lost, 1);
    }

    #[test]
    fn the_first_frame_must_be_a_keyframe() {
        let mut r = Reassembler::default();
        assert!(r.wants_keyframe());
        assert!(feed(&mut r, split(&frame(7, 10, false))).is_empty());
        assert_eq!(feed(&mut r, split(&frame(8, 10, true))).len(), 1);
    }

    #[test]
    fn late_fragments_of_old_frames_are_ignored() {
        let mut r = Reassembler::default();
        let old = split(&frame(1, 3000, true));
        feed(&mut r, split(&frame(2, 10, true)));
        assert!(feed(&mut r, old).is_empty());
    }

    #[test]
    fn malformed_fragments_are_ignored() {
        let mut r = Reassembler::default();
        let mut frags = split(&frame(1, 3000, true));
        let mut bad = frags[0].clone();
        bad.data_shards = 9; // does not match the frame length
        assert!(r.push(bad).is_none());
        let mut short = frags[1].clone();
        short.data.pop(); // a different shard size
        r.push(frags.remove(0));
        assert!(r.push(short).is_none());
        assert_eq!(feed(&mut r, frags).len(), 1, "the good shards still complete it");
    }

    #[test]
    fn rate_control_backs_off_on_queueing_and_recovers_when_clear() {
        let mut r = RateControl::default();
        let ms = 1_000u64;
        assert_eq!(r.report(0, 8_000), None, "the first report sets the floor");
        // A queue builds: delay 40 ms above the 8 ms floor.
        assert_eq!(r.report(500 * ms, 48_000), Some(START_BITRATE * 3 / 4));
        assert!(r.paused(500 * ms + 10 * ms), "pause so the queue drains");
        assert_eq!(r.report(600 * ms, 60_000), None, "one cut at a time");
        assert_eq!(r.report(900 * ms, 60_000), Some(START_BITRATE * 9 / 16));
        // It clears: back near the floor for a second, then the bitrate climbs again.
        assert_eq!(r.report(1_000 * ms, 9_000), None);
        assert_eq!(r.report(2_100 * ms, 9_000), Some(START_BITRATE * 9 / 16 * 11 / 10));
        assert!(!r.paused(2_100 * ms));
    }

    #[test]
    fn rate_control_holds_while_the_screen_is_still() {
        let mut r = RateControl::default();
        let ms = 1_000u64;
        r.report(0, 8_000);
        r.report(500 * ms, 8_000);
        // Still screen: no frames, so every report says "unknown", for half a minute.
        for i in 1..300 {
            assert_eq!(r.report(500 * ms + i * 100 * ms, 0), None);
        }
        // One clear frame afterwards is not a second of evidence.
        assert_eq!(r.report(31_000 * ms, 8_000), None);
        assert_eq!(r.bitrate, START_BITRATE);
    }

    #[test]
    fn rate_control_stays_within_bounds() {
        let mut r = RateControl::default();
        r.report(0, 5_000);
        let mut t = 0;
        // Sustained queueing within the floor window drives the bitrate to the minimum. (Delay
        // that persists longer than the window at the minimum is taken as the path's new floor.)
        for _ in 0..20 {
            t += 400_000;
            r.report(t, 200_000);
        }
        assert_eq!(r.bitrate, MIN_BITRATE);
        for _ in 0..100 {
            t += 1_100_000;
            r.report(t, 5_000);
        }
        assert_eq!(r.bitrate, MAX_BITRATE);
    }

    #[test]
    fn the_clock_offset_keeps_the_fastest_round_trip() {
        let mut c = ClockOffset::default();
        c.sample(1_000, 51_000, 1_000 + 20_000); // slow: suggests offset 40 000
        c.sample(2_000, 52_500, 3_000); // fast: offset 50 000
        assert_eq!(c.to_mac(60_000), Some(10_000));
    }
}

//! The PC side, without any I/O: applies state datagrams to an [`Injector`] so that every key and
//! button transition happens exactly once and in order, nothing is ever left stuck, and
//! everything is released when the Mac goes quiet.

use crate::keymap::win;
use crate::wire::{self, State, TransKind, DEADMAN_US};

/// Shortest click the PC sees. A trackpad tap releases within a millisecond, and games that read
/// the mouse once per frame never notice such a click. A release that comes sooner waits (in
/// order, like any other transition) until the press has lasted this long.
pub const MIN_CLICK_US: u64 = 50_000;

/// What the PC can do. The Windows implementation calls `SendInput`.
pub trait Injector {
    /// `code` is a wire key code (`wire::code`): a scan code, or an extended one.
    /// Returns false if the OS refused the input.
    fn key(&mut self, code: u16, down: bool) -> bool;
    fn button(&mut self, button: u8, down: bool) -> bool;
    fn move_to(&mut self, x: i32, y: i32);
    /// Windows wheel units (120 = one notch). Positive: up / right.
    fn wheel(&mut self, vertical: i32, horizontal: i32);
    /// Tap an unassigned key so that releasing Alt/Win does not open the menu bar or Start.
    fn mask_key(&mut self);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AckInfo {
    pub epoch: u32,
    pub applied: u64,
    pub echo_time_us: u64,
    pub active: bool,
    pub dead: bool,
    /// New transitions were applied (ack promptly so the Mac can stop resending them).
    pub progressed: bool,
}

#[derive(Debug, Default)]
pub struct Receiver {
    epoch: u32,
    epoch_dead: bool,
    applied: u64,
    scroll_v: i64,
    scroll_h: i64,
    held_keys: Vec<u16>,
    held_buttons: u8,
    button_down_at: [u64; 8],
    /// Releases the OS refused (e.g. while an elevated window had focus); retried until they land.
    retry_keys: Vec<u16>,
    retry_buttons: u8,
    last_pos: Option<(i32, i32)>,
    pending_pos: Option<(i32, i32)>,
    active: bool,
    last_packet_us: u64,
    last_nonce: Option<u64>,
}

impl Receiver {
    /// A new session replaced the old one: release everything and start from scratch.
    pub fn reset(&mut self, inj: &mut dyn Injector) {
        self.release_all(inj);
        let (keys, buttons) = (std::mem::take(&mut self.retry_keys), self.retry_buttons);
        *self = Self::default();
        self.retry_keys = keys;
        self.retry_buttons = buttons;
    }

    /// Apply one decrypted state datagram. Cursor motion is deferred until [`flush`](Self::flush),
    /// so a burst of queued datagrams moves the cursor once.
    pub fn on_state(&mut self, now: u64, nonce: u64, s: &State, inj: &mut dyn Injector) -> Option<AckInfo> {
        if self.last_nonce.is_some_and(|l| nonce <= l) {
            return None;
        }
        self.last_nonce = Some(nonce);
        self.last_packet_us = now;
        self.retry(inj);
        if s.epoch < self.epoch {
            return None;
        }
        if s.epoch > self.epoch {
            self.release_all(inj);
            self.epoch = s.epoch;
            self.epoch_dead = false;
            self.applied = 0;
            self.scroll_v = 0;
            self.scroll_h = 0;
        }
        let ack = |r: &Self, progressed| AckInfo {
            epoch: r.epoch,
            applied: r.applied,
            echo_time_us: s.send_time_us,
            active: r.active,
            dead: r.epoch_dead,
            progressed,
        };
        if self.epoch_dead {
            return Some(ack(self, false));
        }
        let before = self.applied;
        if !s.active {
            if self.apply_transitions(now, s, inj) {
                // A refused release: keep everything as it is so the Mac's resend retries it in
                // order. Later closing or keepalive datagrams (or the dead-man) clean up.
                return Some(ack(self, self.applied > before));
            }
            if self.active || !self.held_keys.is_empty() || self.held_buttons != 0 {
                self.release_all(inj);
            }
            self.active = false;
            return Some(ack(self, self.applied > before));
        }
        self.active = true;
        if self.apply_transitions(now, s, inj) {
            // A transition is waiting (Windows refused it, or a click is not long enough yet):
            // the Mac resends it, and nothing after it may run first. Motion, scroll and the
            // pressed set all describe a later moment, so they wait too.
            return Some(ack(self, self.applied > before));
        }
        self.pending_pos = Some((s.x, s.y));
        let (dv, dh) = (s.scroll_v - self.scroll_v, s.scroll_h - self.scroll_h);
        if dv != 0 || dh != 0 {
            self.flush(inj);
            let c = |d: i64| d.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            inj.wheel(c(dv), c(dh));
            self.scroll_v = s.scroll_v;
            self.scroll_h = s.scroll_h;
        }
        let stale: Vec<u16> = self.held_keys.iter().copied().filter(|k| !s.pressed.contains(k)).collect();
        for k in stale {
            self.held_keys.retain(|&h| h != k);
            self.release_key(k, inj);
        }
        let stale_buttons = self.held_buttons & !s.buttons;
        if stale_buttons != 0 {
            self.flush(inj);
            for b in wire::buttons_in(stale_buttons) {
                self.release_button(b, inj);
            }
            self.held_buttons &= s.buttons;
        }
        Some(ack(self, self.applied > before))
    }

    /// Apply new transitions in order. Returns true if Windows refused one: progress stops
    /// there (it is not acknowledged), so the Mac resends it and order is kept.
    fn apply_transitions(&mut self, now: u64, s: &State, inj: &mut dyn Injector) -> bool {
        for (i, t) in s.transitions.iter().enumerate() {
            let id = s.first_event + i as u64;
            if id <= self.applied {
                continue;
            }
            if id != self.applied + 1 {
                break;
            }
            match t.kind {
                TransKind::ButtonDown | TransKind::ButtonUp => {
                    let Some(bit) = wire::button_bit(t.code) else {
                        self.applied = id; // not a button the wire carries: nothing to do
                        continue;
                    };
                    let (b, down) = (t.code as u8, t.kind == TransKind::ButtonDown);
                    if down {
                        self.move_now((t.x, t.y), inj);
                        if !inj.button(b, true) {
                            return true;
                        }
                        self.retry_buttons &= !bit;
                        self.held_buttons |= bit;
                        self.button_down_at[b as usize] = now;
                    } else if self.held_buttons & bit != 0 {
                        if now.saturating_sub(self.button_down_at[b as usize]) < MIN_CLICK_US {
                            return true; // too soon: the next datagram tries again
                        }
                        self.move_now((t.x, t.y), inj);
                        if !inj.button(b, false) {
                            return true;
                        }
                        self.held_buttons &= !bit;
                    } else if self.retry_buttons & bit != 0 {
                        if !inj.button(b, false) {
                            return true;
                        }
                        self.retry_buttons &= !bit;
                    }
                }
                TransKind::KeyDown | TransKind::KeyRepeat => {
                    if !inj.key(t.code, true) {
                        return true;
                    }
                    self.retry_keys.retain(|&k| k != t.code);
                    if !self.held_keys.contains(&t.code) {
                        self.held_keys.push(t.code);
                    }
                }
                TransKind::KeyUp => {
                    if let Some(i) = self.held_keys.iter().position(|&k| k == t.code) {
                        if !inj.key(t.code, false) {
                            return true;
                        }
                        self.held_keys.remove(i);
                    } else if let Some(i) = self.retry_keys.iter().position(|&k| k == t.code) {
                        if !inj.key(t.code, false) {
                            return true;
                        }
                        self.retry_keys.remove(i);
                    }
                }
            }
            self.applied = id;
        }
        false
    }

    fn release_key(&mut self, k: u16, inj: &mut dyn Injector) {
        if !inj.key(k, false) && !self.retry_keys.contains(&k) {
            self.retry_keys.push(k);
        }
    }

    fn release_button(&mut self, b: u8, inj: &mut dyn Injector) {
        if !inj.button(b, false) {
            self.retry_buttons |= 1 << b;
        }
    }

    /// Try again to release what the OS refused earlier.
    fn retry(&mut self, inj: &mut dyn Injector) {
        if !self.retry_keys.is_empty() {
            let keys = std::mem::take(&mut self.retry_keys);
            for k in keys {
                self.release_key(k, inj);
            }
        }
        if self.retry_buttons != 0 {
            for b in wire::buttons_in(std::mem::take(&mut self.retry_buttons)) {
                self.release_button(b, inj);
            }
        }
    }

    /// Apply the deferred cursor move.
    pub fn flush(&mut self, inj: &mut dyn Injector) {
        if let Some(p) = self.pending_pos.take() {
            if self.last_pos != Some(p) {
                inj.move_to(p.0, p.1);
                self.last_pos = Some(p);
            }
        }
    }

    fn move_now(&mut self, p: (i32, i32), inj: &mut dyn Injector) {
        self.pending_pos = None;
        if self.last_pos != Some(p) {
            inj.move_to(p.0, p.1);
            self.last_pos = Some(p);
        }
    }

    /// Dead-man and retries. Call at least every few tens of milliseconds.
    pub fn tick(&mut self, now: u64, inj: &mut dyn Injector) {
        self.retry(inj);
        // Armed whenever anything is held, not only while active: a partly applied closing
        // datagram can leave a key down with `active` already false.
        let holding = self.active || !self.held_keys.is_empty() || self.held_buttons != 0;
        if holding && now.saturating_sub(self.last_packet_us) > DEADMAN_US {
            self.release_all(inj);
            self.epoch_dead = true;
        }
    }

    pub fn release_all(&mut self, inj: &mut dyn Injector) {
        self.flush(inj);
        if self.held_keys.iter().any(|&k| win::is_menu_modifier(k)) {
            inj.mask_key();
        }
        let keys: Vec<u16> = self.held_keys.drain(..).rev().collect();
        for k in keys {
            self.release_key(k, inj);
        }
        for b in wire::buttons_in(std::mem::take(&mut self.held_buttons)) {
            self.release_button(b, inj);
        }
        self.active = false;
    }
}

/// An injector that records what it was asked to do, for tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Recorder {
    pub ops: Vec<Op>,
    pub down: Vec<u16>,
    pub buttons: u8,
    pub pos: (i32, i32),
    pub wheel_v: i64,
    pub wheel_h: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Key(u16, bool),
    Button(u8, bool),
    Move(i32, i32),
    Wheel(i32, i32),
    Mask,
}

impl Injector for Recorder {
    fn key(&mut self, code: u16, down: bool) -> bool {
        self.ops.push(Op::Key(code, down));
        if down {
            if !self.down.contains(&code) {
                self.down.push(code);
            }
        } else {
            self.down.retain(|&c| c != code);
        }
        true
    }
    fn button(&mut self, button: u8, down: bool) -> bool {
        self.ops.push(Op::Button(button, down));
        if down {
            self.buttons |= 1 << button;
        } else {
            self.buttons &= !(1 << button);
        }
        true
    }
    fn move_to(&mut self, x: i32, y: i32) {
        self.ops.push(Op::Move(x, y));
        self.pos = (x, y);
    }
    fn wheel(&mut self, v: i32, h: i32) {
        self.ops.push(Op::Wheel(v, h));
        self.wheel_v += v as i64;
        self.wheel_h += h as i64;
    }
    fn mask_key(&mut self) {
        self.ops.push(Op::Mask);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Transition;

    fn st(epoch: u32, first: u64, ts: &[(TransKind, u16, i32, i32)], pressed: &[u16], buttons: u8, pos: (i32, i32)) -> State {
        State {
            epoch,
            active: true,
            send_time_us: 1,
            x: pos.0,
            y: pos.1,
            buttons,
            pressed: pressed.to_vec(),
            first_event: first,
            transitions: ts.iter().map(|&(kind, code, x, y)| Transition { kind, code, x, y }).collect(),
            ..State::default()
        }
    }

    use TransKind::*;

    #[test]
    fn applies_each_transition_once_in_order() {
        let mut r = Receiver::default();
        let mut inj = Recorder::default();
        let s1 = st(1, 1, &[(KeyDown, 0x1E, 0, 0)], &[0x1E], 0, (5, 5));
        r.on_state(0, 1, &s1, &mut inj);
        // Resent with the next transition: the first is not applied again.
        let s2 = st(1, 1, &[(KeyDown, 0x1E, 0, 0), (KeyUp, 0x1E, 0, 0)], &[], 0, (5, 5));
        let a = r.on_state(0, 2, &s2, &mut inj).unwrap();
        r.flush(&mut inj);
        assert_eq!(a.applied, 2);
        assert_eq!(inj.ops, vec![Op::Key(0x1E, true), Op::Key(0x1E, false), Op::Move(5, 5)]);
        assert!(inj.down.is_empty());
    }

    #[test]
    fn lost_datagrams_are_repaired_by_the_next_one() {
        let mut r = Receiver::default();
        let mut inj = Recorder::default();
        // Datagrams 1–3 lost. Datagram 4 still carries every unacked transition.
        let ts = [(KeyDown, 0x1E, 0, 0), (KeyUp, 0x1E, 0, 0), (KeyDown, 0x30, 0, 0), (KeyUp, 0x30, 0, 0)];
        r.on_state(0, 4, &st(1, 1, &ts, &[], 0, (0, 0)), &mut inj);
        assert_eq!(
            inj.ops,
            vec![Op::Key(0x1E, true), Op::Key(0x1E, false), Op::Key(0x30, true), Op::Key(0x30, false)]
        );
    }

    #[test]
    fn click_lands_where_it_happened_not_where_the_cursor_ended() {
        let mut r = Receiver::default();
        let mut inj = Recorder::default();
        r.on_state(0, 1, &st(1, 1, &[], &[], 0, (10, 10)), &mut inj);
        // Click happened at (100,100), the cursor has since moved to (400,400). The release
        // waits for a full click; the move to (400,400) waits behind it.
        let click = st(1, 1, &[(ButtonDown, 0, 100, 100), (ButtonUp, 0, 100, 100)], &[], 0, (400, 400));
        r.on_state(0, 2, &click, &mut inj);
        r.flush(&mut inj);
        assert_eq!(inj.ops, vec![Op::Move(100, 100), Op::Button(0, true)], "no drag while the click is held");
        r.on_state(MIN_CLICK_US, 3, &click, &mut inj);
        r.flush(&mut inj);
        assert_eq!(
            inj.ops,
            vec![Op::Move(100, 100), Op::Button(0, true), Op::Button(0, false), Op::Move(400, 400)]
        );
    }

    #[test]
    fn stale_keys_are_released_and_reordered_datagrams_ignored() {
        let mut r = Receiver::default();
        let mut inj = Recorder::default();
        r.on_state(0, 5, &st(1, 1, &[(KeyDown, 0x1D, 0, 0)], &[0x1D], 0, (0, 0)), &mut inj);
        // An older datagram arriving late changes nothing.
        assert!(r.on_state(0, 3, &st(1, 1, &[], &[], 0, (0, 0)), &mut inj).is_none());
        assert_eq!(inj.down, vec![0x1D]);
        // A newer datagram whose pressed set lacks Ctrl (its key-up was lost beyond a gap).
        r.on_state(0, 9, &st(1, 9, &[(KeyDown, 0x10, 0, 0)], &[0x10], 0, (0, 0)), &mut inj);
        assert!(!inj.down.contains(&0x1D), "Ctrl must be released by repair");
        assert!(!inj.down.contains(&0x10), "a transition after a gap must not be applied");
    }

    #[test]
    fn deadman_releases_and_kills_the_epoch() {
        let mut r = Receiver::default();
        let mut inj = Recorder::default();
        r.on_state(0, 1, &st(1, 1, &[(KeyDown, 0x38, 0, 0), (ButtonDown, 0, 3, 3)], &[0x38], 1, (3, 3)), &mut inj);
        r.tick(DEADMAN_US, &mut inj);
        assert_eq!(inj.down, vec![0x38]);
        r.tick(DEADMAN_US + 1, &mut inj);
        assert!(inj.down.is_empty());
        assert_eq!(inj.buttons, 0);
        assert!(inj.ops.contains(&Op::Mask), "Alt release must be masked");
        // Late datagrams for the dead epoch do not re-press anything.
        let a = r.on_state(DEADMAN_US + 50_000, 2, &st(1, 1, &[(KeyDown, 0x38, 0, 0), (ButtonDown, 0, 3, 3)], &[0x38], 1, (3, 3)), &mut inj).unwrap();
        assert!(a.dead);
        assert!(inj.down.is_empty());
        // A new epoch works again.
        r.on_state(900_000, 3, &st(2, 1, &[(KeyDown, 0x1E, 0, 0)], &[0x1E], 0, (0, 0)), &mut inj);
        assert_eq!(inj.down, vec![0x1E]);
    }

    #[test]
    fn inactive_state_releases_everything() {
        let mut r = Receiver::default();
        let mut inj = Recorder::default();
        r.on_state(0, 1, &st(1, 1, &[(KeyDown, 0x15B, 0, 0)], &[0x15B], 0, (0, 0)), &mut inj);
        let off = State { epoch: 1, active: false, ..State::default() };
        let a = r.on_state(0, 2, &off, &mut inj).unwrap();
        assert!(!a.active);
        assert!(inj.down.is_empty());
        let up = inj.ops.iter().position(|o| *o == Op::Key(0x15B, false)).unwrap();
        assert_eq!(inj.ops[up - 1], Op::Mask, "Win release must be masked");
    }

    #[test]
    fn scroll_is_applied_as_deltas_of_the_totals() {
        let mut r = Receiver::default();
        let mut inj = Recorder::default();
        let mut s = st(1, 1, &[], &[], 0, (0, 0));
        s.scroll_v = 30;
        r.on_state(0, 1, &s, &mut inj);
        s.scroll_v = 30; // duplicate state: no new wheel
        r.on_state(0, 2, &s, &mut inj);
        s.scroll_v = 150;
        s.scroll_h = -10;
        r.on_state(0, 3, &s, &mut inj);
        assert_eq!((inj.wheel_v, inj.wheel_h), (150, -10));
    }

    #[test]
    fn cursor_returns_to_a_position_equal_to_the_last_flushed_one() {
        let mut r = Receiver::default();
        let mut inj = Recorder::default();
        r.on_state(0, 1, &st(1, 1, &[], &[], 0, (10, 10)), &mut inj);
        r.flush(&mut inj); // at A
        r.on_state(0, 2, &st(1, 1, &[], &[], 0, (50, 50)), &mut inj); // B, not flushed
        r.on_state(0, 3, &st(1, 1, &[], &[], 0, (10, 10)), &mut inj); // back to A
        r.flush(&mut inj);
        assert_eq!(inj.pos, (10, 10));
    }

    /// Refuses key-ups until `allow` is set, like Windows blocking input to an elevated window.
    #[derive(Default)]
    struct Stubborn {
        rec: Recorder,
        allow: bool,
    }
    impl Injector for Stubborn {
        fn key(&mut self, c: u16, d: bool) -> bool {
            if !d && !self.allow {
                return false;
            }
            self.rec.key(c, d)
        }
        fn button(&mut self, b: u8, d: bool) -> bool {
            if !d && !self.allow {
                return false;
            }
            self.rec.button(b, d)
        }
        fn move_to(&mut self, x: i32, y: i32) {
            self.rec.move_to(x, y)
        }
        fn wheel(&mut self, v: i32, h: i32) {
            self.rec.wheel(v, h)
        }
        fn mask_key(&mut self) {
            self.rec.mask_key()
        }
    }

    #[test]
    fn a_refused_transition_stops_progress_until_it_lands() {
        let mut r = Receiver::default();
        let mut inj = Stubborn::default();
        r.on_state(0, 1, &st(1, 1, &[(KeyDown, 0x1D, 0, 0)], &[0x1D], 0, (0, 0)), &mut inj);
        let ts = [(KeyDown, 0x1D, 0, 0), (KeyUp, 0x1D, 0, 0), (KeyDown, 0x1E, 0, 0)];
        let a = r.on_state(0, 2, &st(1, 1, &ts, &[0x1E], 0, (0, 0)), &mut inj).unwrap();
        assert_eq!(a.applied, 1, "the refused key-up is not acknowledged");
        assert_eq!(inj.rec.down, vec![0x1D], "and 'a' did not jump ahead of it");
        inj.allow = true;
        let a = r.on_state(0, 3, &st(1, 1, &ts, &[0x1E], 0, (0, 0)), &mut inj).unwrap();
        assert_eq!(a.applied, 3);
        assert_eq!(inj.rec.down, vec![0x1E]);
    }

    #[test]
    fn refused_cleanup_releases_are_retried() {
        let mut r = Receiver::default();
        let mut inj = Stubborn::default();
        r.on_state(0, 1, &st(1, 1, &[(KeyDown, 0x1D, 0, 0), (ButtonDown, 0, 0, 0)], &[0x1D], 1, (0, 0)), &mut inj);
        r.tick(DEADMAN_US + 1, &mut inj); // dead-man: release refused
        assert_eq!(inj.rec.down, vec![0x1D]);
        assert_eq!(inj.rec.buttons, 1);
        inj.allow = true;
        r.tick(DEADMAN_US + 10_000, &mut inj);
        assert!(inj.rec.down.is_empty());
        assert_eq!(inj.rec.buttons, 0);
    }

    #[test]
    fn a_pending_button_release_never_releases_a_newer_press() {
        let mut r = Receiver::default();
        let mut inj = Stubborn::default();
        r.on_state(0, 1, &st(1, 1, &[(ButtonDown, 0, 0, 0)], &[], 1, (0, 0)), &mut inj);
        r.tick(DEADMAN_US + 1, &mut inj); // dead-man: the release is refused and queued
        // New epoch: the user presses again and keeps holding.
        r.on_state(DEADMAN_US + 100_000, 2, &st(2, 1, &[(ButtonDown, 0, 0, 0)], &[], 1, (0, 0)), &mut inj);
        inj.allow = true;
        r.tick(DEADMAN_US + 110_000, &mut inj);
        assert_eq!(inj.rec.buttons, 1, "the newer press must stay down");
    }

    #[test]
    fn closing_state_delivers_in_flight_taps_then_releases() {
        let mut r = Receiver::default();
        let mut inj = Recorder::default();
        r.on_state(0, 1, &st(1, 1, &[(KeyDown, 0x2A, 0, 0)], &[0x2A], 0, (0, 0)), &mut inj);
        // The datagrams with the 'a' tap were lost; the Mac has already handed control back.
        let mut closing = st(1, 1, &[(KeyDown, 0x2A, 0, 0), (KeyDown, 0x1E, 0, 0), (KeyUp, 0x1E, 0, 0)], &[], 0, (0, 0));
        closing.active = false;
        let a = r.on_state(0, 2, &closing, &mut inj).unwrap();
        assert_eq!(a.applied, 3);
        assert!(!a.active);
        assert_eq!(
            inj.ops,
            vec![Op::Key(0x2A, true), Op::Key(0x1E, true), Op::Key(0x1E, false), Op::Move(0, 0), Op::Key(0x2A, false)]
        );
        assert!(inj.down.is_empty());
    }

    #[test]
    fn a_refused_release_in_a_closing_datagram_still_blocks_later_keys() {
        let mut r = Receiver::default();
        let mut inj = Stubborn::default();
        r.on_state(0, 1, &st(1, 1, &[(KeyDown, 0x1D, 0, 0)], &[0x1D], 0, (0, 0)), &mut inj);
        let mut closing = st(1, 1, &[(KeyDown, 0x1D, 0, 0), (KeyUp, 0x1D, 0, 0), (KeyDown, 0x1E, 0, 0), (KeyUp, 0x1E, 0, 0)], &[], 0, (0, 0));
        closing.active = false;
        for nonce in 2..5 {
            let a = r.on_state(0, nonce, &closing, &mut inj).unwrap();
            assert_eq!(a.applied, 1, "the refused Ctrl-up is never skipped");
            assert!(!inj.rec.ops.contains(&Op::Key(0x1E, true)), "no Ctrl+A");
        }
        inj.allow = true;
        let a = r.on_state(0, 5, &closing, &mut inj).unwrap();
        assert_eq!(a.applied, 4);
        assert!(inj.rec.down.is_empty());
    }

    #[test]
    fn a_key_up_waits_for_a_failed_cleanup_release() {
        let mut r = Receiver::default();
        let mut inj = Stubborn::default();
        r.on_state(0, 1, &st(1, 1, &[(KeyDown, 0x1D, 0, 0)], &[0x1D], 0, (0, 0)), &mut inj);
        // Repair tries to release Ctrl (pressed set lost it) and is refused.
        r.on_state(0, 2, &st(1, 9, &[], &[], 0, (0, 0)), &mut inj);
        assert_eq!(inj.rec.down, vec![0x1D]);
        // The real Ctrl-up then A-down arrive: A must not go first.
        let ts = [(KeyDown, 0x1D, 0, 0), (KeyUp, 0x1D, 0, 0), (KeyDown, 0x1E, 0, 0)];
        let a = r.on_state(0, 3, &st(1, 1, &ts, &[0x1E], 0, (0, 0)), &mut inj).unwrap();
        assert_eq!(a.applied, 1);
        assert!(!inj.rec.ops.contains(&Op::Key(0x1E, true)));
        inj.allow = true;
        r.on_state(0, 4, &st(1, 1, &ts, &[0x1E], 0, (0, 0)), &mut inj);
        assert_eq!(inj.rec.down, vec![0x1E]);
    }

    #[test]
    fn dead_man_covers_a_partly_applied_closing_datagram() {
        let mut r = Receiver::default();
        let mut inj = Stubborn::default();
        // The first datagram to arrive is a closing one; its key-up is refused; the Mac vanishes.
        let mut closing = st(1, 1, &[(KeyDown, 0x1D, 0, 0), (KeyUp, 0x1D, 0, 0)], &[], 0, (0, 0));
        closing.active = false;
        r.on_state(0, 1, &closing, &mut inj);
        assert_eq!(inj.rec.down, vec![0x1D]);
        inj.allow = true;
        r.tick(DEADMAN_US + 1, &mut inj);
        assert!(inj.rec.down.is_empty(), "the dead-man must release it");
    }

    #[test]
    fn a_tap_is_stretched_to_a_full_click_and_nothing_overtakes_it() {
        let mut r = Receiver::default();
        let mut inj = Recorder::default();
        // Tap (down + up 1 ms apart) followed by a right-button press, all in one datagram.
        let ts = [(ButtonDown, 0, 5, 5), (ButtonUp, 0, 5, 5), (ButtonDown, 1, 5, 5)];
        let a = r.on_state(0, 1, &st(1, 1, &ts, &[], 2, (5, 5)), &mut inj).unwrap();
        assert_eq!(a.applied, 1, "the release waits");
        assert_eq!(inj.buttons, 1);
        let a = r.on_state(MIN_CLICK_US - 1, 2, &st(1, 1, &ts, &[], 2, (5, 5)), &mut inj).unwrap();
        assert_eq!(a.applied, 1, "still waiting just before the minimum");
        let a = r.on_state(MIN_CLICK_US, 3, &st(1, 1, &ts, &[], 2, (5, 5)), &mut inj).unwrap();
        assert_eq!(a.applied, 3);
        let buttons: Vec<Op> = inj.ops.iter().copied().filter(|o| matches!(o, Op::Button(..))).collect();
        assert_eq!(buttons, vec![Op::Button(0, true), Op::Button(0, false), Op::Button(1, true)], "order kept");
        assert_eq!(inj.buttons, 2);
    }

    #[test]
    fn a_normal_click_is_not_delayed() {
        let mut r = Receiver::default();
        let mut inj = Recorder::default();
        r.on_state(0, 1, &st(1, 1, &[(ButtonDown, 0, 0, 0)], &[], 1, (0, 0)), &mut inj);
        let a = r.on_state(90_000, 2, &st(1, 1, &[(ButtonDown, 0, 0, 0), (ButtonUp, 0, 0, 0)], &[], 0, (0, 0)), &mut inj).unwrap();
        assert_eq!(a.applied, 2);
        assert_eq!(inj.buttons, 0);
    }
}

//! The Mac side, without any I/O: decides when control crosses to the PC and back, owns the
//! virtual PC cursor, and builds the state datagrams. The macOS binary feeds it events and
//! carries out what it returns.
//!
//! Time is microseconds on a monotonic clock, passed in so tests can drive it.

use crate::keymap::{mac, MacMod};
use crate::layout::{self, PcMove, Rect, Side};
use crate::mapper::{KeyAction, KeyMapper, KeyOut, MapConfig};
use crate::stats::Summary;
use crate::wire::{self, Ack, Monitor, State, TransKind, Transition, ACK_ACTIVE, ACK_BLOCKED, ACK_EPOCH_DEAD, LEASE_US, MAX_TRANSITIONS};
use std::collections::VecDeque;

/// Windows wheel units per Mac scroll point (a notch is 120 units, about 48 px of content).
const WHEEL_UNITS_PER_POINT: f64 = 2.5;
/// Windows wheel units per line (one notch) of a wheel mouse.
const WHEEL_UNITS_PER_LINE: f64 = 120.0;
/// Round trips kept for the statistics.
const RTT_SAMPLES: usize = 512;

/// While the PC is in control, a state goes out at least this often.
const HEARTBEAT_US: u64 = 15_000;
/// A transition the PC has not acknowledged is sent again after this long.
const RETRANSMIT_US: u64 = 2_000;
/// Keepalives while the Mac has its input, so the PC's monitors and cursor stay current.
pub(crate) const IDLE_KEEPALIVE_US: u64 = 1_000_000;
/// Faster keepalives near the crossing edge, or before the PC has answered at all.
const WARM_KEEPALIVE_US: u64 = 50_000;
/// Points from the crossing edge at which keepalives speed up.
const WARM_DISTANCE: f64 = 48.0;
/// A crossing needs an ack this recent; keepalives must arrive more often than that.
const FRESH_US: u64 = 3_000_000;
const _: () = assert!(IDLE_KEEPALIVE_US < FRESH_US);
/// Closing datagrams after a leave, one per heartbeat (about 300 ms) while taps are in flight,
/// and again each time the PC makes progress on them.
const CLOSING_DATAGRAMS: u8 = 20;
/// Inactive datagrams after a leave with nothing in flight.
const GOODBYE_DATAGRAMS: u8 = 2;

#[derive(Clone, Debug, PartialEq)]
pub struct ControllerConfig {
    pub side: Side,
    /// Switch by pushing the pointer off a screen edge, in both directions. Off by default:
    /// the hotkey switches, so both machines' own screen edges (Dock, Start menu, snapping,
    /// scrollbars) stay usable.
    pub edges: bool,
    /// Multiplier on top of macOS pointer acceleration.
    pub speed: f64,
    pub scroll_speed: f64,
    pub map: MapConfig,
}

impl ControllerConfig {
    /// The pointer and scroll speed multipliers a user may choose.
    pub fn valid_speed(x: f64) -> bool {
        x > 0.0 && x < 20.0
    }
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            side: Side::Right,
            edges: false,
            speed: 1.0,
            scroll_speed: 1.0,
            map: MapConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Effect {
    /// Hide and freeze the Mac cursor; input now goes to the PC.
    Enter,
    /// Warp the Mac cursor here, unfreeze and show it.
    Leave { warp_to: (f64, f64), reason: LeaveReason },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaveReason {
    Edge,
    Hotkey,
    Timeout,
    Blocked,
    EpochDead,
    TapDisabled,
    SecureInput,
    Overflow,
    /// The link renegotiated its session; the PC started over.
    Reconnect,
    /// The lock-free watchdog had already handed input back.
    Watchdog,
    /// The app is quitting, or switching to another PC.
    Quit,
}

impl LeaveReason {
    /// The user chose to come back, as opposed to something going wrong. After an unhealthy
    /// leave, crossing again waits for a fresh answer from the PC.
    fn healthy(self) -> bool {
        match self {
            LeaveReason::Edge | LeaveReason::Hotkey => true,
            LeaveReason::Timeout
            | LeaveReason::Blocked
            | LeaveReason::EpochDead
            | LeaveReason::TapDisabled
            | LeaveReason::SecureInput
            | LeaveReason::Overflow
            | LeaveReason::Reconnect
            | LeaveReason::Watchdog
            | LeaveReason::Quit => false,
        }
    }

    /// Whether key and button changes still in flight are worth delivering after the leave.
    /// Not when the PC cannot take them (dead, blocked, new session), or they are known lost.
    fn delivers_in_flight(self) -> bool {
        match self {
            LeaveReason::Edge
            | LeaveReason::Hotkey
            | LeaveReason::TapDisabled
            | LeaveReason::SecureInput
            | LeaveReason::Quit => true,
            LeaveReason::Timeout
            | LeaveReason::Blocked
            | LeaveReason::EpochDead
            | LeaveReason::Overflow
            | LeaveReason::Reconnect
            | LeaveReason::Watchdog => false,
        }
    }
}

#[derive(Debug, Default)]
pub struct Output {
    /// Drop the Mac event instead of letting macOS handle it.
    pub swallow: bool,
    pub send: Option<State>,
    pub effect: Option<Effect>,
}

impl Output {
    fn pass() -> Self {
        Self::default()
    }
    fn swallow() -> Self {
        Self { swallow: true, ..Self::default() }
    }
}

#[derive(Debug)]
struct Remote {
    pos: (f64, f64),
    entered_from: (f64, f64),
    scroll_v: f64,
    scroll_h: f64,
    buttons: u8,
    unacked: VecDeque<Transition>,
    first_unacked: u64,
    last_ack_us: u64,
    last_motion_us: u64,
    retx_at: Option<u64>,
}

/// Transitions still in flight when control came back to the Mac. They keep riding on the
/// closing datagrams until the PC acknowledges them.
#[derive(Debug)]
struct Closing {
    first_unacked: u64,
    unacked: VecDeque<Transition>,
}

#[derive(Debug)]
pub struct Controller {
    cfg: ControllerConfig,
    mac: Vec<Rect>,
    pc: Vec<Monitor>,
    /// `pc` as rectangles, for the geometry (made once per change, not per motion event).
    pc_rects: Vec<Rect>,
    pc_cursor: Option<(i32, i32)>,
    remote: Option<Remote>,
    mapper: KeyMapper,
    epoch: u32,
    last_ack_any_us: Option<u64>,
    /// After an unhealthy leave, crossing waits for an ack newer than this.
    needs_ack_after: Option<u64>,
    last_send_us: u64,
    near_edge: bool,
    local_buttons: u8,
    secure_input: bool,
    /// Ended for good ([`Controller::retire`]); nothing can make it cross again.
    retired: bool,
    goodbye: u8,
    closing: Option<Closing>,
    rtt: VecDeque<u32>,
}

impl Controller {
    pub fn new(cfg: ControllerConfig, mac_displays: Vec<Rect>) -> Self {
        let map = cfg.map;
        Self {
            cfg,
            mac: mac_displays,
            pc: Vec::new(),
            pc_rects: Vec::new(),
            pc_cursor: None,
            remote: None,
            mapper: KeyMapper::new(map),
            epoch: 0,
            last_ack_any_us: None,
            needs_ack_after: None,
            last_send_us: 0,
            near_edge: false,
            local_buttons: 0,
            secure_input: false,
            retired: false,
            goodbye: 0,
            closing: None,
            rtt: VecDeque::with_capacity(RTT_SAMPLES),
        }
    }

    pub fn set_mac_displays(&mut self, d: Vec<Rect>) {
        if !d.is_empty() {
            self.mac = d;
        }
    }

    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    /// Where the Mac is moving the PC cursor (PC virtual-desktop pixels), while the PC has control.
    pub fn pc_cursor(&self) -> Option<(f64, f64)> {
        self.remote.as_ref().map(|r| r.pos)
    }

    /// This crossing's scroll so far, in whole Windows wheel units (vertical, horizontal), as
    /// sent to the PC.
    pub fn pc_scroll(&self) -> Option<(i64, i64)> {
        self.remote.as_ref().map(|r| (r.scroll_v.trunc() as i64, r.scroll_h.trunc() as i64))
    }

    pub fn pc_monitors(&self) -> &[Monitor] {
        &self.pc
    }

    /// Transitions sent but not yet acknowledged by the PC.
    pub fn pending_transitions(&self) -> usize {
        self.remote.as_ref().map_or(0, |r| r.unacked.len())
    }

    /// Recent round-trip times, in milliseconds.
    pub fn rtt_summary(&self) -> Option<Summary> {
        Summary::of(self.rtt.iter().map(|&us| us as f64 / 1000.0))
    }

    /// When the PC's silence would end control (µs); `None` while the Mac has its input.
    pub fn lease_deadline(&self) -> Option<u64> {
        self.remote.as_ref().map(|r| r.last_ack_us + LEASE_US)
    }

    fn fresh(&self, now: u64) -> bool {
        match self.last_ack_any_us {
            Some(t) => {
                now.saturating_sub(t) <= FRESH_US && self.needs_ack_after.is_none_or(|after| t > after)
            }
            None => false,
        }
    }

    fn can_cross(&self, now: u64) -> bool {
        // A closing queue (taps in flight from the last crossing) must land first; on a LAN
        // that takes one round trip.
        !self.retired && !self.pc.is_empty() && self.local_buttons == 0 && !self.secure_input && self.closing.is_none() && self.fresh(now)
    }

    /// The one lease rule; the session's watchdog uses the same deadline.
    fn expired(&self, now: u64) -> bool {
        self.lease_deadline().is_some_and(|d| now >= d)
    }

    /// How often datagrams go out while the Mac has its input.
    fn idle_interval(&self) -> u64 {
        if self.goodbye > 0 {
            HEARTBEAT_US
        } else if self.near_edge || self.last_ack_any_us.is_none() {
            WARM_KEEPALIVE_US
        } else {
            IDLE_KEEPALIVE_US
        }
    }

    // ---- events from the Mac -------------------------------------------------------------

    /// `dragging` is true for drag events (a Mac button is down).
    pub fn mouse_moved(&mut self, now: u64, loc: (f64, f64), delta: (f64, f64), flags: u64, dragging: bool) -> Output {
        if self.remote.is_some() {
            return self.remote_motion(now, delta);
        }
        if !dragging {
            self.local_buttons = 0; // repair a missed button-up
        }
        self.near_edge = layout::distance_to_edge(&self.mac, self.cfg.side, loc) < WARM_DISTANCE;
        if self.cfg.edges && self.can_cross(now) && layout::mac_crossing(&self.mac, self.cfg.side, loc, delta) {
            let frac = layout::edge_fraction(&self.mac, self.cfg.side, loc);
            if let Some(entry) = layout::point_on_edge(&self.pc_rects, self.cfg.side.opposite(), frac, 0.0) {
                return self.enter(now, entry, loc, flags);
            }
        }
        Output::pass()
    }

    pub fn mouse_button(&mut self, now: u64, button: u8, down: bool) -> Output {
        let Some(bit) = wire::button_bit(button.into()) else {
            // A button the wire cannot carry stays on the Mac, and goes nowhere while the PC has
            // control.
            return if self.remote.is_some() { Output::swallow() } else { Output::pass() };
        };
        if self.remote.is_none() {
            if down {
                self.local_buttons |= bit;
            } else {
                self.local_buttons &= !bit;
            }
            return Output::pass();
        }
        if self.expired(now) {
            return self.leave(now, LeaveReason::Timeout);
        }
        let r = self.remote.as_mut().unwrap();
        if down {
            r.buttons |= bit;
        } else if r.buttons & bit != 0 {
            r.buttons &= !bit;
        } else {
            return Output::swallow(); // pressed before the crossing
        }
        // A trackpad tap releases within a millisecond; the PC stretches it (see receiver).
        let kind = if down { TransKind::ButtonDown } else { TransKind::ButtonUp };
        self.push(now, kind, button as u16);
        self.send_now(now)
    }

    /// `dv`/`dh` are point deltas when `continuous` (trackpad), else line deltas (wheel mouse).
    pub fn scroll(&mut self, now: u64, dv: f64, dh: f64, continuous: bool) -> Output {
        if self.remote.is_none() {
            return Output::pass();
        }
        if self.expired(now) {
            return self.leave(now, LeaveReason::Timeout);
        }
        let k = if continuous { WHEEL_UNITS_PER_POINT } else { WHEEL_UNITS_PER_LINE } * self.cfg.scroll_speed;
        let r = self.remote.as_mut().unwrap();
        let (bv, bh) = (r.scroll_v.trunc(), r.scroll_h.trunc());
        r.scroll_v += dv * k;
        r.scroll_h -= dh * k; // Mac: positive = left; Windows HWHEEL: positive = right
        if r.scroll_v.trunc() == bv && r.scroll_h.trunc() == bh {
            return Output::swallow();
        }
        self.send_now(now)
    }

    /// Switch sides, as the hotkey and the menu do: hand control back if the PC has it, else
    /// cross to where the PC's pointer is (or the middle of the PC's first screen). `loc` is the
    /// Mac cursor and `flags` the modifiers held now (`CGEventFlags`), which stay on the Mac.
    pub fn switch(&mut self, now: u64, loc: (f64, f64), flags: u64) -> Output {
        if self.remote.is_some() {
            return self.leave(now, LeaveReason::Hotkey);
        }
        if self.can_cross(now) {
            let rects = &self.pc_rects;
            let pos = self
                .pc_cursor
                .map(|(x, y)| (x as f64, y as f64))
                .filter(|p| rects.iter().any(|r| r.contains(*p)))
                .or_else(|| rects.first().map(|r| ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0)));
            if let Some(pos) = pos {
                return Output { swallow: true, ..self.enter(now, pos, loc, flags) };
            }
        }
        Output::pass()
    }

    /// Key down/up. `loc` is the Mac cursor (for the hotkey), `flags` the `CGEventFlags`.
    pub fn key(&mut self, now: u64, mac_code: u16, down: bool, repeat: bool, flags: u64, loc: (f64, f64)) -> Output {
        if mac::is_switch(mac_code, flags) {
            if !down || repeat {
                return if self.remote.is_some() { Output::swallow() } else { Output::pass() };
            }
            return self.switch(now, loc, flags);
        }
        if self.remote.is_none() {
            return Output::pass();
        }
        if self.expired(now) {
            return self.leave(now, LeaveReason::Timeout);
        }
        let outs = if down { self.mapper.key_down(mac_code, repeat) } else { self.mapper.key_up(mac_code) };
        self.push_keys(now, outs)
    }

    /// A `flagsChanged` event (modifier or Caps Lock).
    pub fn flags_changed(&mut self, now: u64, mac_code: u16, flags: u64) -> Output {
        if self.remote.is_none() {
            return Output::pass();
        }
        if self.expired(now) {
            return self.leave(now, LeaveReason::Timeout);
        }
        let outs = if mac_code == mac::CAPSLOCK {
            self.mapper.caps_lock()
        } else if let Some(m) = MacMod::from_keycode(mac_code) {
            self.mapper.modifier(m, m.is_down(flags))
        } else {
            Vec::new() // Fn stays on the Mac
        };
        self.push_keys(now, outs)
    }

    /// Give the Mac its input back because of something outside the controller: macOS disabled
    /// the event tap, the watchdog already took the cursor, or the app is quitting.
    pub fn hand_back(&mut self, now: u64, reason: LeaveReason) -> Output {
        self.leave(now, reason)
    }

    /// The link negotiated a new session: the PC's receiver starts over, so end the epoch.
    pub fn session_changed(&mut self, now: u64) -> Output {
        self.closing = None;
        if self.remote.is_some() {
            return self.leave(now, LeaveReason::Reconnect);
        }
        Output::pass()
    }

    /// End this controller for good: hand control back, and never cross again. For a session
    /// being replaced, whose events may still be in flight.
    pub fn retire(&mut self, now: u64) -> Output {
        self.retired = true;
        self.leave(now, LeaveReason::Quit)
    }

    pub fn secure_input(&self) -> bool {
        self.secure_input
    }

    pub fn set_secure_input(&mut self, now: u64, on: bool) -> Output {
        self.secure_input = on;
        if on && self.remote.is_some() {
            return self.leave(now, LeaveReason::SecureInput);
        }
        Output::pass()
    }

    // ---- network -------------------------------------------------------------------------

    pub fn ack(&mut self, now: u64, a: &Ack) -> Output {
        self.last_ack_any_us = Some(now);
        if !a.monitors.is_empty() {
            self.pc = a.monitors.clone();
            self.pc_rects = self.pc.iter().map(Rect::from).collect();
        }
        self.pc_cursor = Some((a.cursor_x, a.cursor_y));
        if a.echo_time_us != 0 && a.echo_time_us <= now {
            let rtt = (now - a.echo_time_us).saturating_sub(a.hold_us as u64).min(u32::MAX as u64) as u32;
            if self.rtt.len() == RTT_SAMPLES {
                self.rtt.pop_front();
            }
            self.rtt.push_back(rtt);
        }
        // Both the crossing and any closing queue belong to the current epoch.
        let current = a.epoch == self.epoch;
        if let Some(c) = self.closing.as_mut().filter(|_| current) {
            let before = c.unacked.len();
            while c.first_unacked <= a.applied_event && c.unacked.pop_front().is_some() {
                c.first_unacked += 1;
            }
            if c.unacked.is_empty() || a.flags & ACK_EPOCH_DEAD != 0 {
                self.closing = None;
                self.goodbye = self.goodbye.min(1);
            } else if c.unacked.len() < before {
                // The PC is working through the queue (stretched clicks take time): keep going.
                self.goodbye = self.goodbye.max(CLOSING_DATAGRAMS);
            }
        }
        let Some(r) = self.remote.as_mut() else { return Output::pass() };
        if !current {
            return Output::pass();
        }
        if a.flags & ACK_EPOCH_DEAD != 0 {
            return self.leave(now, LeaveReason::EpochDead);
        }
        if a.flags & ACK_BLOCKED != 0 {
            return self.leave(now, LeaveReason::Blocked);
        }
        if a.flags & ACK_ACTIVE != 0 {
            // The lease runs from when the Mac sent the state the PC answered, not from when the
            // answer arrived, so delayed or withheld acks cannot stretch it.
            let sent = a.echo_time_us.min(now);
            if sent > r.last_ack_us {
                r.last_ack_us = sent;
            }
        }
        while r.first_unacked <= a.applied_event && r.unacked.pop_front().is_some() {
            r.first_unacked += 1;
        }
        if r.unacked.is_empty() {
            r.retx_at = None;
        }
        // Someone moved the PC's own mouse while the Mac was still: follow it.
        if now.saturating_sub(r.last_motion_us) > 250_000 {
            let p = (a.cursor_x as f64, a.cursor_y as f64);
            if (p.0 - r.pos.0.floor()).abs() > 2.0 || (p.1 - r.pos.1.floor()).abs() > 2.0 {
                r.pos = p;
            }
        }
        Output::pass()
    }

    /// Call on every timer wake-up: heartbeats, retransmits, keepalives and the lease.
    pub fn tick(&mut self, now: u64) -> Output {
        if self.remote.is_some() {
            if self.expired(now) {
                return self.leave(now, LeaveReason::Timeout);
            }
            let r = self.remote.as_ref().unwrap();
            let due_retx = r.retx_at.is_some_and(|t| now >= t);
            if due_retx || now.saturating_sub(self.last_send_us) >= HEARTBEAT_US {
                if due_retx {
                    let next = now + HEARTBEAT_US;
                    self.remote.as_mut().unwrap().retx_at = Some(next);
                }
                return self.send_now(now);
            }
            return Output::pass();
        }
        if now.saturating_sub(self.last_send_us) >= self.idle_interval() {
            self.goodbye = self.goodbye.saturating_sub(1);
            if self.goodbye == 0 {
                self.closing = None;
            }
            self.last_send_us = now;
            return Output { send: Some(self.inactive_state(now)), ..Output::pass() };
        }
        Output::pass()
    }

    /// When the next `tick` has something to do.
    pub fn next_wake(&self, now: u64) -> u64 {
        if let Some(r) = &self.remote {
            let mut t = self.last_send_us + HEARTBEAT_US;
            if let Some(x) = r.retx_at {
                t = t.min(x);
            }
            let lease = self.lease_deadline().unwrap_or(t);
            return t.min(lease).max(now);
        }
        (self.last_send_us + self.idle_interval()).max(now)
    }

    // ---- internals -----------------------------------------------------------------------

    fn enter(&mut self, now: u64, pos: (f64, f64), mac_loc: (f64, f64), flags: u64) -> Output {
        self.epoch = self.epoch.wrapping_add(1);
        self.mapper.begin_epoch(flags);
        self.goodbye = 0;
        self.closing = None;
        self.remote = Some(Remote {
            pos,
            entered_from: mac_loc,
            scroll_v: 0.0,
            scroll_h: 0.0,
            buttons: 0,
            unacked: VecDeque::new(),
            first_unacked: 1,
            last_ack_us: now,
            last_motion_us: now,
            retx_at: None,
        });
        let mut out = self.send_now(now);
        out.swallow = true;
        out.effect = Some(Effect::Enter);
        out
    }

    fn leave(&mut self, now: u64, reason: LeaveReason) -> Output {
        let side = self.cfg.side;
        let warp_to = self.remote.as_ref().map(|r| layout::nudge_inside(&self.mac, side, r.entered_from, 2.0));
        match warp_to {
            Some(warp_to) => self.leave_to(now, reason, warp_to),
            None => Output::pass(),
        }
    }

    fn leave_to(&mut self, now: u64, reason: LeaveReason, warp_to: (f64, f64)) -> Output {
        let Some(r) = self.remote.take() else { return Output::pass() };
        if !reason.healthy() {
            self.needs_ack_after = Some(now);
        }
        self.closing = (reason.delivers_in_flight() && !r.unacked.is_empty()).then_some(Closing {
            first_unacked: r.first_unacked,
            unacked: r.unacked,
        });
        self.goodbye = if self.closing.is_some() { CLOSING_DATAGRAMS } else { GOODBYE_DATAGRAMS };
        self.last_send_us = now;
        let state = self.inactive_state(now);
        Output { swallow: reason.healthy(), send: Some(state), effect: Some(Effect::Leave { warp_to, reason }) }
    }

    fn remote_motion(&mut self, now: u64, delta: (f64, f64)) -> Output {
        if self.expired(now) {
            return self.leave(now, LeaveReason::Timeout);
        }
        let side = self.cfg.side;
        let r = self.remote.as_mut().unwrap();
        let k = self.cfg.speed * layout::scale_at(&self.pc, r.pos);
        r.last_motion_us = now;
        match layout::pc_move(&self.pc_rects, side, r.pos, (delta.0 * k, delta.1 * k)) {
            PcMove::Stay(p) => {
                let moved = p.0.floor() != r.pos.0.floor() || p.1.floor() != r.pos.1.floor();
                r.pos = p;
                if !moved {
                    return Output::swallow();
                }
            }
            PcMove::Exit(frac) if r.buttons == 0 && self.cfg.edges => {
                let warp = layout::point_on_edge(&self.mac, side, frac, 2.0).unwrap_or(r.entered_from);
                return self.leave_to(now, LeaveReason::Edge, warp);
            }
            PcMove::Exit(_) => {
                // Stay on the PC, pinned to its edge (hotkey-only return, or dragging).
                let p = layout::pc_clamp(&self.pc_rects, r.pos, (delta.0 * k, delta.1 * k));
                let moved = p.0.floor() != r.pos.0.floor() || p.1.floor() != r.pos.1.floor();
                r.pos = p;
                if !moved {
                    return Output::swallow();
                }
            }
        }
        self.send_now(now)
    }

    fn push(&mut self, now: u64, kind: TransKind, code: u16) {
        let r = self.remote.as_mut().unwrap();
        r.unacked.push_back(Transition { kind, code, x: r.pos.0.floor() as i32, y: r.pos.1.floor() as i32 });
        r.retx_at = Some(now + RETRANSMIT_US);
    }

    fn push_keys(&mut self, now: u64, outs: KeyOut) -> Output {
        if outs.is_empty() {
            return Output::swallow();
        }
        for (code, act) in outs {
            let kind = match act {
                KeyAction::Down => TransKind::KeyDown,
                KeyAction::Up => TransKind::KeyUp,
                KeyAction::Repeat => TransKind::KeyRepeat,
            };
            self.push(now, kind, code);
        }
        self.send_now(now)
    }

    fn send_now(&mut self, now: u64) -> Output {
        let r = self.remote.as_ref().unwrap();
        if r.unacked.len() > MAX_TRANSITIONS {
            return self.leave(now, LeaveReason::Overflow);
        }
        let state = State {
            epoch: self.epoch,
            active: true,
            send_time_us: now,
            x: r.pos.0.floor() as i32,
            y: r.pos.1.floor() as i32,
            scroll_v: r.scroll_v.trunc() as i64,
            scroll_h: r.scroll_h.trunc() as i64,
            buttons: r.buttons,
            pressed: self.mapper.pressed().to_vec(),
            first_event: r.first_unacked,
            transitions: r.unacked.iter().copied().collect(),
        };
        self.last_send_us = now;
        Output { swallow: true, send: Some(state), effect: None }
    }

    fn inactive_state(&self, now: u64) -> State {
        let mut s = State { epoch: self.epoch, active: false, send_time_us: now, ..State::default() };
        if let Some(c) = &self.closing {
            s.first_event = c.first_unacked;
            s.transitions = c.unacked.iter().copied().collect();
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::win;

    const MS: u64 = 1000;

    fn pc_ack(epoch: u32, applied: u64, flags: u8) -> Ack {
        Ack {
            epoch,
            applied_event: applied,
            flags,
            monitors: vec![Monitor { left: 0, top: 0, right: 1920, bottom: 1080, scale_milli: 1000 }],
            ..Ack::default()
        }
    }

    fn ready() -> (Controller, u64) {
        let mut c = Controller::new(
            ControllerConfig { edges: true, ..ControllerConfig::default() },
            vec![Rect::new(0.0, 0.0, 1512.0, 982.0)],
        );
        let t = 10 * MS;
        c.ack(t, &pc_ack(0, 0, 0));
        (c, t)
    }

    fn enter(c: &mut Controller, t: u64) -> Output {
        c.mouse_moved(t, (1511.0, 491.0), (4.0, 0.0), 0, false)
    }

    fn left(o: &Output) -> Option<LeaveReason> {
        match o.effect {
            Some(Effect::Leave { reason, .. }) => Some(reason),
            _ => None,
        }
    }

    #[test]
    fn no_crossing_before_the_pc_is_known() {
        let mut c = Controller::new(
            ControllerConfig { edges: true, ..ControllerConfig::default() },
            vec![Rect::new(0.0, 0.0, 1512.0, 982.0)],
        );
        let o = enter(&mut c, 0);
        assert!(!o.swallow && o.effect.is_none());
    }

    #[test]
    fn crossing_enters_and_swallows() {
        let (mut c, t) = ready();
        let o = enter(&mut c, t);
        assert_eq!(o.effect, Some(Effect::Enter));
        assert!(o.swallow);
        let s = o.send.unwrap();
        assert!(s.active);
        assert_eq!((s.x, s.y), (0, 540));
        assert!(c.is_remote());
    }

    #[test]
    fn no_crossing_while_a_button_is_held() {
        let (mut c, t) = ready();
        c.mouse_button(t, 0, true);
        assert!(c.mouse_moved(t, (1511.0, 491.0), (4.0, 0.0), 0, true).effect.is_none());
        c.mouse_button(t, 0, false);
        assert_eq!(enter(&mut c, t).effect, Some(Effect::Enter));
    }

    #[test]
    fn lease_expiry_returns_control() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        assert!(c.tick(t + LEASE_US - MS).effect.is_none());
        let o = c.tick(t + LEASE_US);
        assert_eq!(left(&o), Some(LeaveReason::Timeout));
        assert!(!o.send.unwrap().active);
        // No re-crossing until the PC answers again.
        let t = t + LEASE_US;
        assert!(enter(&mut c, t + 100 * MS).effect.is_none());
        c.ack(t + 200 * MS, &pc_ack(1, 0, 0));
        assert_eq!(enter(&mut c, t + 201 * MS).effect, Some(Effect::Enter));
    }

    #[test]
    fn acks_keep_the_lease_alive() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        for i in 1..=20 {
            let now = t + i * 100 * MS;
            let mut a = pc_ack(1, 0, ACK_ACTIVE);
            a.echo_time_us = now - MS; // answers a state sent 1 ms ago
            c.ack(now, &a);
            assert!(c.tick(now).effect.is_none(), "left at {i}");
        }
    }

    #[test]
    fn transitions_stay_until_acked() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        c.flags_changed(t, 0x37, MacMod::LCmd.device_bit()); // Cmd → Ctrl
        let s = c.key(t, 0x08, true, false, 0, (0.0, 0.0)).send.unwrap(); // C
        assert_eq!(s.first_event, 1);
        assert_eq!(s.transitions.len(), 2);
        c.ack(t + MS, &pc_ack(1, 1, ACK_ACTIVE));
        let s = c.tick(t + 20 * MS).send.unwrap();
        assert_eq!(s.first_event, 2);
        assert_eq!(s.transitions.len(), 1);
        assert_eq!(s.transitions[0].code, 0x2E);
        assert_eq!(s.pressed, vec![win::LCTRL, 0x2E]);
    }

    #[test]
    fn retransmit_two_ms_after_a_transition() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        c.key(t, 0x00, true, false, 0, (0.0, 0.0));
        assert!(c.tick(t + MS).send.is_none());
        assert!(c.tick(t + 2 * MS).send.is_some());
    }

    #[test]
    fn edges_are_off_by_default_both_ways_and_the_cursor_stays_pinned() {
        let mut c = Controller::new(ControllerConfig::default(), vec![Rect::new(0.0, 0.0, 1512.0, 982.0)]);
        let t = 10 * MS;
        c.ack(t, &pc_ack(0, 0, 0));
        assert!(enter(&mut c, t).effect.is_none(), "pushing off the Mac's edge must not cross by default");
        c.key(t, mac::RIGHT, true, false, mac::HYPER, (1511.0, 491.0));
        c.mouse_moved(t + MS, (0.0, 0.0), (300.0, 300.0), 0, false);
        let s0 = c.mouse_moved(t + MS, (0.0, 0.0), (-2000.0, 0.0), 0, false).send.unwrap();
        let s = c.mouse_moved(t + 2 * MS, (0.0, 0.0), (-5.0, 3.0), 0, false).send.unwrap();
        assert!(c.is_remote(), "pushing off the PC's edge must not return by default");
        assert_eq!(s0.x, 0);
        assert_eq!((s.x, s.y), (0, s0.y + 3), "pinned at the edge, vertical motion kept");
    }

    #[test]
    fn edge_exit_returns_to_the_mac() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        let o = c.mouse_moved(t + MS, (0.0, 0.0), (-5.0, 0.0), 0, false);
        match o.effect {
            Some(Effect::Leave { warp_to, reason: LeaveReason::Edge }) => {
                assert_eq!(warp_to.0, 1509.0);
                assert!((warp_to.1 - 491.0).abs() < 1.0);
            }
            other => panic!("{other:?}"),
        }
        assert!(!c.is_remote());
    }

    #[test]
    fn no_exit_while_dragging() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        c.mouse_button(t, 0, true);
        let o = c.mouse_moved(t + MS, (0.0, 0.0), (-5.0, 0.0), 0, true);
        assert!(o.effect.is_none());
        assert!(c.is_remote());
    }

    #[test]
    fn hotkey_toggles_both_ways_and_held_modifiers_stay_home() {
        let (mut c, t) = ready();
        let hyper = mac::HYPER
            | MacMod::LCmd.device_bit() | MacMod::LOpt.device_bit() | MacMod::LCtrl.device_bit();
        let o = c.key(t, mac::RIGHT, true, false, hyper, (700.0, 400.0));
        assert_eq!(o.effect, Some(Effect::Enter));
        assert!(o.send.unwrap().pressed.is_empty(), "⌃⌥⌘ must not reach the PC");
        // Typing 'a' with the hotkey modifiers still held sends only 'a'.
        let s = c.key(t + MS, 0x00, true, false, hyper, (0.0, 0.0)).send.unwrap();
        assert_eq!(s.pressed, vec![0x1E]);
        let o = c.key(t + 2 * MS, mac::LEFT, true, false, hyper, (0.0, 0.0));
        assert_eq!(o.effect, Some(Effect::Leave { warp_to: (700.0, 400.0), reason: LeaveReason::Hotkey }));
        assert!(o.swallow);
    }

    #[test]
    fn a_button_the_wire_cannot_carry_neither_blocks_crossing_nor_reaches_the_pc() {
        let (mut c, t) = ready();
        assert!(!c.mouse_button(t, 9, true).swallow);
        assert_eq!(enter(&mut c, t + MS).effect, Some(Effect::Enter), "button 9 is not a held button");
        let o = c.mouse_button(t + 2 * MS, 8, true);
        assert!(o.swallow && o.send.is_none(), "{o:?}");
        let o = c.mouse_button(t + 3 * MS, 4, true);
        assert_eq!(o.send.map(|s| s.buttons), Some(1 << 4));
    }

    #[test]
    fn a_retired_controller_never_crosses_again() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        assert_eq!(left(&c.retire(t + MS)), Some(LeaveReason::Quit));
        c.ack(t + 3 * MS, &pc_ack(1, 0, 0));
        assert!(enter(&mut c, t + 4 * MS).effect.is_none());
        assert!(c.key(t + 4 * MS, mac::RIGHT, true, false, mac::HYPER, (0.0, 0.0)).effect.is_none());
    }

    #[test]
    fn epoch_dead_and_blocked_acks_return_control() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        assert!(matches!(c.ack(t + MS, &pc_ack(1, 0, ACK_EPOCH_DEAD)).effect, Some(Effect::Leave { .. })));
        c.ack(t + 2 * MS, &pc_ack(1, 0, 0));
        enter(&mut c, t + 3 * MS);
        assert!(matches!(c.ack(t + 4 * MS, &pc_ack(2, 0, ACK_BLOCKED)).effect, Some(Effect::Leave { .. })));
    }

    #[test]
    fn secure_input_blocks_and_ends_control() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        assert!(matches!(c.set_secure_input(t + MS, true).effect, Some(Effect::Leave { .. })));
        c.ack(t + 2 * MS, &pc_ack(1, 0, 0));
        assert!(enter(&mut c, t + 3 * MS).effect.is_none());
        c.set_secure_input(t + 4 * MS, false);
        assert_eq!(enter(&mut c, t + 5 * MS).effect, Some(Effect::Enter));
    }

    #[test]
    fn overflow_ends_the_epoch() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        // Nothing is ever acked: the 65th pending transition ends the epoch.
        let mut sent = 0;
        for _ in 0..40 {
            for down in [true, false] {
                let o = c.key(t, 0x00, down, false, 0, (0.0, 0.0));
                sent += 1;
                if o.effect.is_some() {
                    assert_eq!(left(&o), Some(LeaveReason::Overflow));
                    assert_eq!(sent, MAX_TRANSITIONS + 1);
                    return;
                }
            }
        }
        panic!("never overflowed");
    }

    #[test]
    fn scroll_accumulates_fractions() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        assert!(c.scroll(t, 0.2, 0.0, true).send.is_none()); // 0.5 units
        let s = c.scroll(t, 0.2, 0.0, true).send.unwrap(); // 1.0 units
        assert_eq!(s.scroll_v, 1);
        let s = c.scroll(t, -1.0, 0.0, false).send.unwrap(); // one wheel line down
        assert_eq!(s.scroll_v, 1 - 120);
    }

    #[test]
    fn dpi_scale_applies_to_motion() {
        let mut c = Controller::new(
            ControllerConfig { edges: true, ..ControllerConfig::default() },
            vec![Rect::new(0.0, 0.0, 1512.0, 982.0)],
        );
        let mut a = pc_ack(0, 0, 0);
        a.monitors[0].scale_milli = 1500;
        c.ack(0, &a);
        enter(&mut c, 0);
        let s = c.mouse_moved(MS, (0.0, 0.0), (10.0, 0.0), 0, false).send.unwrap();
        assert_eq!(s.x, 15);
    }

    #[test]
    fn delayed_acks_do_not_stretch_the_lease() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        // A genuine ack that answers the entry state but arrives just before the deadline.
        let mut a = pc_ack(1, 0, ACK_ACTIVE);
        a.echo_time_us = t;
        c.ack(t + LEASE_US - MS, &a);
        assert_eq!(left(&c.tick(t + LEASE_US)), Some(LeaveReason::Timeout));
    }

    #[test]
    fn in_flight_taps_keep_riding_after_handing_back() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        c.key(t, 0x00, true, false, 0, (0.0, 0.0));
        c.key(t, 0x00, false, false, 0, (0.0, 0.0));
        // Hand back (hotkey) before the PC acknowledged the tap.
        let o = c.key(t + MS, mac::LEFT, true, false, mac::HYPER, (0.0, 0.0));
        let s = o.send.unwrap();
        assert!(!s.active);
        assert_eq!((s.first_event, s.transitions.len()), (1, 2));
        // Closing datagrams repeat every heartbeat until acknowledged…
        let s = c.tick(t + 20 * MS).send.unwrap();
        assert_eq!(s.transitions.len(), 2);
        let mut a = pc_ack(1, 2, 0);
        a.echo_time_us = t + 20 * MS;
        c.ack(t + 21 * MS, &a);
        // …then stop carrying them.
        let s = c.tick(t + 40 * MS).send.unwrap();
        assert!(s.transitions.is_empty());
        assert!(c.tick(t + 60 * MS).send.is_none());
    }

    #[test]
    fn reconnect_ends_control() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        c.key(t, 0x00, true, false, 0, (0.0, 0.0));
        let o = c.session_changed(t + MS);
        assert_eq!(left(&o), Some(LeaveReason::Reconnect));
        assert!(o.send.unwrap().transitions.is_empty(), "the new session's receiver cannot use old ids");
    }

    #[test]
    fn no_new_crossing_until_in_flight_taps_land() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        c.key(t, 0x00, true, false, 0, (0.0, 0.0));
        c.key(t, 0x00, false, false, 0, (0.0, 0.0));
        c.key(t + MS, mac::LEFT, true, false, mac::HYPER, (0.0, 0.0)); // back to the Mac
        assert!(enter(&mut c, t + 2 * MS).effect.is_none(), "must not drop the in-flight tap");
        let mut a = pc_ack(1, 2, 0);
        a.echo_time_us = t + MS;
        c.ack(t + 3 * MS, &a);
        assert_eq!(enter(&mut c, t + 4 * MS).effect, Some(Effect::Enter));
    }

    #[test]
    fn closing_queue_lives_while_the_pc_makes_progress() {
        let (mut c, t) = ready();
        enter(&mut c, t);
        for _ in 0..10 {
            c.mouse_button(t, 0, true);
            c.mouse_button(t, 0, false);
        }
        c.key(t + MS, mac::LEFT, true, false, mac::HYPER, (0.0, 0.0)); // hand back with 20 in flight
        let mut now = t + MS;
        let mut applied = 0;
        // The PC finishes one click every 50 ms: about 500 ms, longer than 20 heartbeats.
        while applied < 20 {
            now += 15 * MS;
            let s = c.tick(now).send.expect("closing datagrams keep coming while there is progress");
            assert!(!s.transitions.is_empty());
            if (now - t) % (50 * MS) < 15 * MS {
                applied += 2;
                let mut a = pc_ack(1, applied, 0);
                a.echo_time_us = now;
                c.ack(now, &a);
            }
        }
    }
}

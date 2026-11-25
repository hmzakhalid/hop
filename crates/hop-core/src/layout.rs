//! Screen geometry: where the edges are, where the cursor enters and leaves, and how the
//! Mac-owned virtual cursor moves across the PC's monitors.

use crate::wire::Monitor;
use std::str::FromStr;

/// Right and bottom are exclusive.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

impl Rect {
    pub fn new(left: f64, top: f64, right: f64, bottom: f64) -> Self {
        Self { left, top, right, bottom }
    }
    pub fn contains(&self, p: (f64, f64)) -> bool {
        p.0 >= self.left && p.0 < self.right && p.1 >= self.top && p.1 < self.bottom
    }
    pub fn clamp(&self, p: (f64, f64)) -> (f64, f64) {
        (p.0.clamp(self.left, self.right - 1.0), p.1.clamp(self.top, self.bottom - 1.0))
    }
    fn distance2(&self, p: (f64, f64)) -> f64 {
        let c = self.clamp(p);
        (c.0 - p.0).powi(2) + (c.1 - p.1).powi(2)
    }
}

impl From<&Monitor> for Rect {
    fn from(m: &Monitor) -> Self {
        Rect::new(m.left as f64, m.top as f64, m.right as f64, m.bottom as f64)
    }
}

/// Where the PC sits relative to the Mac.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub fn name(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Top => "top",
            Side::Bottom => "bottom",
        }
    }

    /// The arrow pointing from the Mac toward the PC (⌃⌥⌘ + it goes there).
    pub fn arrow(self) -> &'static str {
        match self {
            Side::Left => "←",
            Side::Right => "→",
            Side::Top => "↑",
            Side::Bottom => "↓",
        }
    }

    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }
    fn horizontal(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }
}

impl FromStr for Side {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "left" => Ok(Side::Left),
            "right" => Ok(Side::Right),
            "top" | "above" | "up" => Ok(Side::Top),
            "bottom" | "below" | "down" => Ok(Side::Bottom),
            _ => Err(format!("unknown side {s:?} (use left, right, top or bottom)")),
        }
    }
}

/// The outer edge of a set of rects on `side`: (edge coordinate, span start, span end), taken over
/// the rects that actually touch that edge.
fn outer_edge(rects: &[Rect], side: Side) -> Option<(f64, f64, f64)> {
    if rects.is_empty() {
        return None;
    }
    let edge_of = |r: &Rect| match side {
        Side::Left => r.left,
        Side::Right => r.right,
        Side::Top => r.top,
        Side::Bottom => r.bottom,
    };
    let edge = match side {
        Side::Left | Side::Top => rects.iter().map(edge_of).fold(f64::INFINITY, f64::min),
        Side::Right | Side::Bottom => rects.iter().map(edge_of).fold(f64::NEG_INFINITY, f64::max),
    };
    let touching = rects.iter().filter(|r| (edge_of(r) - edge).abs() < 0.5);
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for r in touching {
        let (a, b) = if side.horizontal() { (r.top, r.bottom) } else { (r.left, r.right) };
        lo = lo.min(a);
        hi = hi.max(b);
    }
    Some((edge, lo, hi))
}

fn nearest(rects: &[Rect], p: (f64, f64)) -> Option<&Rect> {
    rects.iter().find(|r| r.contains(p)).or_else(|| {
        rects.iter().min_by(|a, b| a.distance2(p).partial_cmp(&b.distance2(p)).unwrap())
    })
}

/// Is the Mac cursor pushing out through the Mac's `side` edge?
pub fn mac_crossing(displays: &[Rect], side: Side, loc: (f64, f64), delta: (f64, f64)) -> bool {
    let Some((edge, lo, hi)) = outer_edge(displays, side) else { return false };
    let along = if side.horizontal() { loc.1 } else { loc.0 };
    if along < lo || along >= hi {
        return false;
    }
    match side {
        Side::Right => delta.0 > 0.0 && loc.0 >= edge - 1.0,
        Side::Left => delta.0 < 0.0 && loc.0 <= edge + 0.5,
        Side::Bottom => delta.1 > 0.0 && loc.1 >= edge - 1.0,
        Side::Top => delta.1 < 0.0 && loc.1 <= edge + 0.5,
    }
}

/// Distance from `loc` to the Mac's `side` edge (for warming the link up before a crossing).
pub fn distance_to_edge(displays: &[Rect], side: Side, loc: (f64, f64)) -> f64 {
    let Some((edge, _, _)) = outer_edge(displays, side) else { return f64::INFINITY };
    match side {
        Side::Right => edge - 1.0 - loc.0,
        Side::Left => loc.0 - edge,
        Side::Bottom => edge - 1.0 - loc.1,
        Side::Top => loc.1 - edge,
    }
    .max(0.0)
}

/// Position of `loc` along the `side` edge of `rects`, from 0 to 1.
pub fn edge_fraction(rects: &[Rect], side: Side, loc: (f64, f64)) -> f64 {
    let Some((_, lo, hi)) = outer_edge(rects, side) else { return 0.5 };
    let along = if side.horizontal() { loc.1 } else { loc.0 };
    if hi - lo <= 0.0 {
        return 0.5;
    }
    ((along - lo) / (hi - lo)).clamp(0.0, 1.0)
}

/// The point at `frac` along the `side` edge of `rects`, `inset` pixels inside it.
pub fn point_on_edge(rects: &[Rect], side: Side, frac: f64, inset: f64) -> Option<(f64, f64)> {
    let (edge, lo, hi) = outer_edge(rects, side)?;
    let along = lo + frac.clamp(0.0, 1.0) * (hi - lo);
    let p = match side {
        Side::Right => (edge - 1.0 - inset, along),
        Side::Left => (edge + inset, along),
        Side::Bottom => (along, edge - 1.0 - inset),
        Side::Top => (along, edge + inset),
    };
    Some(nearest(rects, p)?.clamp(p))
}

/// Move `p` `inset` pixels away from the `side` edge, so a warp does not re-trigger a crossing.
pub fn nudge_inside(rects: &[Rect], side: Side, p: (f64, f64), inset: f64) -> (f64, f64) {
    let Some((edge, _, _)) = outer_edge(rects, side) else { return p };
    let q = match side {
        Side::Right => (p.0.min(edge - 1.0 - inset), p.1),
        Side::Left => (p.0.max(edge + inset), p.1),
        Side::Bottom => (p.0, p.1.min(edge - 1.0 - inset)),
        Side::Top => (p.0, p.1.max(edge + inset)),
    };
    nearest(rects, q).map(|r| r.clamp(q)).unwrap_or(q)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PcMove {
    Stay((f64, f64)),
    /// Left the PC through its return edge, at this fraction along it.
    Exit(f64),
}

/// Move the PC cursor by `d` pixels. `side` is where the PC sits relative to the Mac, so the
/// return edge is the PC's `side.opposite()` edge.
pub fn pc_move(monitors: &[Rect], side: Side, cur: (f64, f64), d: (f64, f64)) -> PcMove {
    let want = (cur.0 + d.0, cur.1 + d.1);
    if monitors.iter().any(|r| r.contains(want)) {
        return PcMove::Stay(want);
    }
    let ret = side.opposite();
    if let Some((edge, _, _)) = outer_edge(monitors, ret) {
        let beyond = match ret {
            Side::Left => want.0 < edge,
            Side::Right => want.0 >= edge,
            Side::Top => want.1 < edge,
            Side::Bottom => want.1 >= edge,
        };
        if beyond {
            return PcMove::Exit(edge_fraction(monitors, ret, cur));
        }
    }
    match nearest(monitors, cur) {
        Some(r) => PcMove::Stay(r.clamp(want)),
        None => PcMove::Stay(cur),
    }
}

/// Move by `d` without leaving the desktop: motion along an edge continues (onto an adjoining
/// monitor if there is one); only the part that would leave the desktop is dropped.
pub fn pc_clamp(monitors: &[Rect], cur: (f64, f64), d: (f64, f64)) -> (f64, f64) {
    let want = (cur.0 + d.0, cur.1 + d.1);
    if monitors.iter().any(|r| r.contains(want)) {
        return want;
    }
    let along_y = (cur.0, want.1);
    let along_x = (want.0, cur.1);
    let order = if d.1.abs() >= d.0.abs() { [along_y, along_x] } else { [along_x, along_y] };
    for p in order {
        if let Some(r) = monitors.iter().find(|r| r.contains(p)) {
            return r.clamp(want);
        }
    }
    nearest(monitors, cur).map(|r| r.clamp(want)).unwrap_or(cur)
}

/// DPI scale of the monitor under `p` (1.0 when unknown).
pub fn scale_at(monitors: &[Monitor], p: (f64, f64)) -> f64 {
    monitors
        .iter()
        .find(|m| Rect::from(*m).contains(p))
        .map(|m| (m.scale_milli.max(250) as f64) / 1000.0)
        .unwrap_or(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac() -> Vec<Rect> {
        vec![Rect::new(0.0, 0.0, 1512.0, 982.0)]
    }
    fn pc() -> Vec<Rect> {
        // Two monitors side by side, the right one taller and offset.
        vec![Rect::new(0.0, 0.0, 1920.0, 1080.0), Rect::new(1920.0, -200.0, 4480.0, 1240.0)]
    }

    #[test]
    fn crossing_needs_the_edge_and_outward_motion() {
        assert!(mac_crossing(&mac(), Side::Right, (1511.0, 400.0), (3.0, 0.0)));
        assert!(!mac_crossing(&mac(), Side::Right, (1511.0, 400.0), (-3.0, 0.0)));
        assert!(!mac_crossing(&mac(), Side::Right, (1400.0, 400.0), (3.0, 0.0)));
        assert!(mac_crossing(&mac(), Side::Left, (0.0, 400.0), (-1.0, 0.0)));
        assert!(mac_crossing(&mac(), Side::Top, (700.0, 0.0), (0.0, -1.0)));
    }

    #[test]
    fn entry_maps_proportionally_onto_pc_edge() {
        let frac = edge_fraction(&mac(), Side::Right, (1511.0, 491.0));
        assert!((frac - 0.5).abs() < 0.01);
        let p = point_on_edge(&pc(), Side::Left, frac, 0.0).unwrap();
        assert_eq!(p.0, 0.0);
        assert!((p.1 - 540.0).abs() < 1.0);
    }

    #[test]
    fn move_across_monitors_and_clamp() {
        // Crossing into the right-hand monitor.
        assert_eq!(pc_move(&pc(), Side::Right, (1910.0, 500.0), (20.0, 0.0)), PcMove::Stay((1930.0, 500.0)));
        // Moving up off the left monitor (it has no monitor above) clamps.
        assert_eq!(pc_move(&pc(), Side::Right, (100.0, 5.0), (0.0, -20.0)), PcMove::Stay((100.0, 0.0)));
        // The right monitor extends higher.
        assert_eq!(pc_move(&pc(), Side::Right, (2000.0, -150.0), (0.0, -20.0)), PcMove::Stay((2000.0, -170.0)));
        // Far right edge is not the return edge for a PC on the right: clamp.
        assert_eq!(pc_move(&pc(), Side::Right, (4470.0, 0.0), (50.0, 0.0)), PcMove::Stay((4479.0, 0.0)));
    }

    #[test]
    fn exit_through_return_edge() {
        match pc_move(&pc(), Side::Right, (2.0, 540.0), (-5.0, 0.0)) {
            PcMove::Exit(f) => assert!((f - 0.5).abs() < 0.01),
            other => panic!("expected exit, got {other:?}"),
        }
        let back = point_on_edge(&mac(), Side::Right, 0.5, 2.0).unwrap();
        assert_eq!(back.0, 1509.0);
        assert!((back.1 - 491.0).abs() < 1.0);
        // After the warp the Mac cursor is not on the crossing edge.
        assert!(!mac_crossing(&mac(), Side::Right, back, (0.5, 0.0)));
    }

    #[test]
    fn negative_virtual_desktop_origin() {
        let m = vec![Rect::new(-1920.0, 0.0, 0.0, 1080.0), Rect::new(0.0, 0.0, 2560.0, 1440.0)];
        let p = point_on_edge(&m, Side::Left, 0.0, 0.0).unwrap();
        assert_eq!(p, (-1920.0, 0.0));
        assert!(matches!(pc_move(&m, Side::Right, (-1919.0, 10.0), (-3.0, 0.0)), PcMove::Exit(_)));
    }

    #[test]
    fn clamped_motion_slides_onto_a_stacked_monitor() {
        let m = vec![Rect::new(0.0, 0.0, 1920.0, 1080.0), Rect::new(0.0, 1080.0, 1920.0, 2160.0)];
        assert_eq!(pc_clamp(&m, (0.0, 1078.0), (-5.0, 4.0)), (0.0, 1082.0));
        assert_eq!(pc_clamp(&m, (0.0, 500.0), (-5.0, 4.0)), (0.0, 504.0));
        assert_eq!(pc_clamp(&m, (1919.0, 2159.0), (5.0, 5.0)), (1919.0, 2159.0));
    }

    #[test]
    fn scale_lookup() {
        let mons = [Monitor { left: 0, top: 0, right: 100, bottom: 100, scale_milli: 1500 }];
        assert_eq!(scale_at(&mons, (5.0, 5.0)), 1.5);
        assert_eq!(scale_at(&mons, (500.0, 5.0)), 1.0);
    }
}

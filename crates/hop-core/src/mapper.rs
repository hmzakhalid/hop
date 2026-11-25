//! Turns Mac key events into the Windows key set the PC should hold, plus the ordered
//! transitions to get there.
//!
//! Defaults: Cmd → Ctrl, Option → Alt, Control → Win, Shift → Shift, Fn stays on the Mac.
//! A few Mac chords become their Windows equivalents (Cmd+Tab → Alt+Tab, Opt+← → Ctrl+←, …).
//!
//! The PC only ever sees the resulting Windows set, so it never has to know about chords.

use crate::keymap::{mac, mac_to_win, win, MacMod, ModClass, ALL_MODS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    Down,
    Up,
    Repeat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapConfig {
    /// Cmd → Win and Control → Ctrl (positional, for people who prefer Windows muscle memory).
    pub cmd_as_win: bool,
    /// Translate Mac chords (Cmd+Tab, Opt+←, Cmd+Q, …).
    pub chords: bool,
}

impl MapConfig {
    /// Which Mac modifier is the Windows key (the other of ⌘ and ⌃ is Ctrl).
    fn win_class(&self) -> ModClass {
        if self.cmd_as_win { ModClass::Cmd } else { ModClass::Ctrl }
    }

    /// The Mac key that acts as the Windows key, as people see it.
    pub fn win_key(&self) -> &'static str {
        match self.win_class() {
            ModClass::Cmd => "⌘",
            _ => "⌃",
        }
    }
}

impl Default for MapConfig {
    fn default() -> Self {
        Self { cmd_as_win: false, chords: true }
    }
}

#[derive(Clone, Copy, Debug)]
struct Chord {
    mac_key: u16,
    /// Mac modifiers whose Windows key is lifted while the chord is held.
    drop: [bool; 8],
    add_mod: Option<u16>,
    out_key: u16,
}

enum ChordMatch {
    AltTab,
    Remap(Chord),
}

#[derive(Debug)]
pub struct KeyMapper {
    cfg: MapConfig,
    held: [bool; 8],
    /// Held when the epoch began: not sent to the PC until released and pressed again.
    ignored: [bool; 8],
    /// Lifted by a finished chord: not pressed again until another key needs it.
    suppressed: [bool; 8],
    keys: Vec<(u16, u16)>,
    chord: Option<Chord>,
    sticky_alt: bool,
    sent: Vec<u16>,
}

pub type KeyOut = Vec<(u16, KeyAction)>;

impl KeyMapper {
    pub fn new(cfg: MapConfig) -> Self {
        Self {
            cfg,
            held: [false; 8],
            ignored: [false; 8],
            suppressed: [false; 8],
            keys: Vec::new(),
            chord: None,
            sticky_alt: false,
            sent: Vec::new(),
        }
    }

    /// Start a new epoch. Modifiers already held (`CGEventFlags`) are ignored until re-pressed,
    /// so the ⌃⌥⌘ of the switch hotkey never reaches the PC.
    pub fn begin_epoch(&mut self, flags: u64) {
        let cfg = self.cfg;
        *self = Self::new(cfg);
        for m in ALL_MODS {
            if m.is_down(flags) {
                self.held[m.index()] = true;
                self.ignored[m.index()] = true;
            }
        }
    }

    /// The Windows keys that should be down now.
    pub fn pressed(&self) -> &[u16] {
        &self.sent
    }

    pub fn modifier(&mut self, m: MacMod, down: bool) -> KeyOut {
        let i = m.index();
        if self.held[i] == down && !self.ignored[i] {
            return Vec::new();
        }
        self.held[i] = down;
        self.ignored[i] = false;
        self.suppressed[i] = false;
        if !down && m.class() == ModClass::Cmd && !self.active(ModClass::Cmd) {
            self.sticky_alt = false;
        }
        self.sync()
    }

    pub fn key_down(&mut self, mac_code: u16, repeat: bool) -> KeyOut {
        if repeat || self.keys.iter().any(|&(m, _)| m == mac_code) {
            return self.repeat(mac_code);
        }
        if !self.sticky_alt {
            match self.match_chord(mac_code) {
                Some(ChordMatch::AltTab) => self.sticky_alt = true,
                Some(ChordMatch::Remap(c)) => {
                    // Modifiers this chord keeps (e.g. Cmd for Cmd+↑ → Ctrl+Home) come back.
                    for i in 0..8 {
                        if !c.drop[i] {
                            self.suppressed[i] = false;
                        }
                    }
                    self.chord = Some(c);
                    return self.sync();
                }
                None => {}
            }
        }
        let Some(w) = mac_to_win(mac_code) else { return Vec::new() };
        self.suppressed = [false; 8];
        self.keys.push((mac_code, w));
        self.sync()
    }

    pub fn key_up(&mut self, mac_code: u16) -> KeyOut {
        if let Some(c) = self.chord.filter(|c| c.mac_key == mac_code) {
            for m in ALL_MODS {
                if c.drop[m.index()] && self.held[m.index()] {
                    self.suppressed[m.index()] = true;
                }
            }
            self.chord = None;
            return self.sync();
        }
        let before = self.keys.len();
        self.keys.retain(|&(m, _)| m != mac_code);
        if self.keys.len() == before {
            return Vec::new(); // held before the epoch began
        }
        self.sync()
    }

    /// Caps Lock on the Mac reports only toggles, so send a tap.
    pub fn caps_lock(&mut self) -> KeyOut {
        vec![(win::CAPSLOCK, KeyAction::Down), (win::CAPSLOCK, KeyAction::Up)]
    }

    fn repeat(&self, mac_code: u16) -> KeyOut {
        if let Some(c) = self.chord.filter(|c| c.mac_key == mac_code) {
            if !win::is_modifier(c.out_key) {
                return vec![(c.out_key, KeyAction::Repeat)];
            }
            return Vec::new();
        }
        match self.keys.iter().find(|&&(m, _)| m == mac_code) {
            Some(&(_, w)) => vec![(w, KeyAction::Repeat)],
            None => Vec::new(),
        }
    }

    fn active(&self, class: ModClass) -> bool {
        ALL_MODS.iter().any(|m| m.class() == class && self.held[m.index()] && !self.ignored[m.index()])
    }

    fn match_chord(&self, k: u16) -> Option<ChordMatch> {
        if !self.cfg.chords || self.cfg.cmd_as_win {
            return None;
        }
        let (cmd, opt, ctrl) = (self.active(ModClass::Cmd), self.active(ModClass::Opt), self.active(ModClass::Ctrl));
        let drop_class = |class: ModClass| {
            let mut d = [false; 8];
            for m in ALL_MODS {
                d[m.index()] = m.class() == class;
            }
            d
        };
        let remap = |drop: [bool; 8], add_mod: Option<u16>, out_key: u16| {
            Some(ChordMatch::Remap(Chord { mac_key: k, drop, add_mod, out_key }))
        };
        let none = [false; 8];
        if cmd && !opt && !ctrl {
            return match k {
                mac::TAB => Some(ChordMatch::AltTab),
                mac::LEFT => remap(drop_class(ModClass::Cmd), None, win::HOME),
                mac::RIGHT => remap(drop_class(ModClass::Cmd), None, win::END),
                mac::UP => remap(none, None, win::HOME), // Cmd is already Ctrl → Ctrl+Home
                mac::DOWN => remap(none, None, win::END),
                mac::Q => remap(drop_class(ModClass::Cmd), Some(win::LALT), win::F4),
                mac::SPACE => remap(drop_class(ModClass::Cmd), None, win::LWIN),
                _ => None,
            };
        }
        if opt && !cmd && !ctrl {
            let out = match k {
                mac::LEFT => win::LEFT,
                mac::RIGHT => win::RIGHT,
                mac::UP => win::UP,
                mac::DOWN => win::DOWN,
                mac::DELETE => win::BACKSPACE,
                _ => return None,
            };
            return remap(drop_class(ModClass::Opt), Some(win::LCTRL), out);
        }
        None
    }

    fn map_mod(&self, m: MacMod) -> u16 {
        let r = m.is_right();
        match m.class() {
            ModClass::Cmd if self.sticky_alt => win::LALT,
            c @ (ModClass::Cmd | ModClass::Ctrl) if c == self.cfg.win_class() => if r { win::RWIN } else { win::LWIN },
            ModClass::Cmd | ModClass::Ctrl => if r { win::RCTRL } else { win::LCTRL },
            ModClass::Opt => if r { win::RALT } else { win::LALT },
            ModClass::Shift => if r { win::RSHIFT } else { win::LSHIFT },
        }
    }

    fn desired(&self) -> Vec<u16> {
        let mut v: Vec<u16> = Vec::with_capacity(8);
        let mut push = |c: u16| {
            if !v.contains(&c) {
                v.push(c)
            }
        };
        let drop = self.chord.map(|c| c.drop).unwrap_or([false; 8]);
        for m in ALL_MODS {
            let i = m.index();
            if self.held[i] && !self.ignored[i] && !self.suppressed[i] && !drop[i] {
                push(self.map_mod(m));
            }
        }
        if let Some(c) = self.chord {
            if let Some(a) = c.add_mod {
                push(a);
            }
            push(c.out_key);
        }
        for &(_, w) in &self.keys {
            push(w);
        }
        v
    }

    fn sync(&mut self) -> KeyOut {
        let new = self.desired();
        let out = diff(&self.sent, &new);
        self.sent = new;
        out
    }
}

/// Order matters for Windows:
/// 1. release ordinary keys;
/// 2. release Ctrl/Shift that are going away;
/// 3. press new modifiers;
/// 4. release Alt/Win only now, after something else was pressed, so the release is never a
///    bare tap that opens the menu bar or Start;
/// 5. press ordinary keys.
fn diff(old: &[u16], new: &[u16]) -> KeyOut {
    let gone = |c: &&u16| !new.contains(c);
    let fresh = |c: &&u16| !old.contains(c);
    let mut out = Vec::new();
    out.extend(old.iter().filter(gone).filter(|c| !win::is_modifier(**c)).map(|&c| (c, KeyAction::Up)));
    out.extend(
        old.iter()
            .filter(gone)
            .filter(|c| win::is_modifier(**c) && !win::is_menu_modifier(**c))
            .map(|&c| (c, KeyAction::Up)),
    );
    out.extend(new.iter().filter(fresh).filter(|c| win::is_modifier(**c)).map(|&c| (c, KeyAction::Down)));
    out.extend(old.iter().filter(gone).filter(|c| win::is_menu_modifier(**c)).map(|&c| (c, KeyAction::Up)));
    out.extend(new.iter().filter(fresh).filter(|c| !win::is_modifier(**c)).map(|&c| (c, KeyAction::Down)));
    out
}

#[cfg(test)]
mod tests {
    use super::KeyAction::{Down as D, Repeat as R, Up as U};
    use super::*;

    const A: u16 = 0x00;
    const C: u16 = 0x08;
    const WA: u16 = 0x1E;
    const WC: u16 = 0x2E;
    const WQ: u16 = 0x10;

    fn m() -> KeyMapper {
        KeyMapper::new(MapConfig::default())
    }

    #[test]
    fn cmd_c_is_ctrl_c() {
        let mut k = m();
        assert_eq!(k.modifier(MacMod::LCmd, true), vec![(win::LCTRL, D)]);
        assert_eq!(k.key_down(C, false), vec![(WC, D)]);
        assert_eq!(k.pressed(), &[win::LCTRL, WC]);
        assert_eq!(k.key_up(C), vec![(WC, U)]);
        assert_eq!(k.modifier(MacMod::LCmd, false), vec![(win::LCTRL, U)]);
        assert!(k.pressed().is_empty());
    }

    #[test]
    fn shift_and_control_and_option() {
        let mut k = m();
        assert_eq!(k.modifier(MacMod::RShift, true), vec![(win::RSHIFT, D)]);
        assert_eq!(k.modifier(MacMod::LCtrl, true), vec![(win::LWIN, D)]);
        assert_eq!(k.modifier(MacMod::ROpt, true), vec![(win::RALT, D)]);
        assert_eq!(k.key_down(A, false), vec![(WA, D)]);
        assert_eq!(k.key_down(A, true), vec![(WA, R)]);
    }

    #[test]
    fn cmd_tab_tab_release_is_alt_tab_tab() {
        let mut k = m();
        assert_eq!(k.modifier(MacMod::LCmd, true), vec![(win::LCTRL, D)]);
        // Ctrl goes away before Alt arrives, so Windows never sees Ctrl+Alt+Tab.
        assert_eq!(k.key_down(mac::TAB, false), vec![(win::LCTRL, U), (win::LALT, D), (win::TAB, D)]);
        assert_eq!(k.key_up(mac::TAB), vec![(win::TAB, U)]);
        assert_eq!(k.key_down(mac::TAB, false), vec![(win::TAB, D)]);
        assert_eq!(k.key_up(mac::TAB), vec![(win::TAB, U)]);
        assert_eq!(k.modifier(MacMod::LCmd, false), vec![(win::LALT, U)]);
        // Next Cmd press is Ctrl again.
        assert_eq!(k.modifier(MacMod::LCmd, true), vec![(win::LCTRL, D)]);
    }

    #[test]
    fn opt_left_is_ctrl_left_without_bare_alt_release() {
        let mut k = m();
        assert_eq!(k.modifier(MacMod::LOpt, true), vec![(win::LALT, D)]);
        // Ctrl is pressed before Alt is released, so the Alt release is not a bare tap.
        assert_eq!(k.key_down(mac::LEFT, false), vec![(win::LCTRL, D), (win::LALT, U), (win::LEFT, D)]);
        assert_eq!(k.key_down(mac::LEFT, true), vec![(win::LEFT, R)]);
        assert_eq!(k.key_up(mac::LEFT), vec![(win::LEFT, U), (win::LCTRL, U)]);
        // Alt is not re-pressed while Option is still held…
        assert!(k.pressed().is_empty());
        // …so releasing Option sends nothing at all.
        assert_eq!(k.modifier(MacMod::LOpt, false), vec![]);
    }

    #[test]
    fn opt_left_twice_then_letter_restores_alt() {
        let mut k = m();
        k.modifier(MacMod::LOpt, true);
        k.key_down(mac::LEFT, false);
        k.key_up(mac::LEFT);
        assert_eq!(k.key_down(mac::LEFT, false), vec![(win::LCTRL, D), (win::LEFT, D)]);
        k.key_up(mac::LEFT);
        assert_eq!(k.key_down(A, false), vec![(win::LALT, D), (WA, D)]);
    }

    #[test]
    fn opt_shift_left_selects_word() {
        let mut k = m();
        k.modifier(MacMod::LOpt, true);
        k.modifier(MacMod::LShift, true);
        k.key_down(mac::LEFT, false);
        let p = k.pressed();
        assert!(p.contains(&win::LCTRL) && p.contains(&win::LSHIFT) && p.contains(&win::LEFT));
        assert!(!p.contains(&win::LALT));
    }

    #[test]
    fn cmd_arrows_and_q() {
        let mut k = m();
        k.modifier(MacMod::LCmd, true);
        assert_eq!(k.key_down(mac::LEFT, false), vec![(win::LCTRL, U), (win::HOME, D)]);
        assert_eq!(k.key_up(mac::LEFT), vec![(win::HOME, U)]);
        assert_eq!(k.key_down(mac::UP, false), vec![(win::LCTRL, D), (win::HOME, D)]);
        k.key_up(mac::UP);
        assert_eq!(k.key_down(mac::Q, false), vec![(win::LCTRL, U), (win::LALT, D), (win::F4, D)]);
        assert_eq!(k.key_up(mac::Q), vec![(win::F4, U), (win::LALT, U)]);
    }

    #[test]
    fn cmd_space_taps_win() {
        let mut k = m();
        k.modifier(MacMod::LCmd, true);
        assert_eq!(k.key_down(mac::SPACE, false), vec![(win::LCTRL, U), (win::LWIN, D)]);
        assert_eq!(k.key_up(mac::SPACE), vec![(win::LWIN, U)]);
    }

    #[test]
    fn modifiers_held_at_entry_are_not_sent() {
        let mut k = m();
        let flags = MacMod::LCtrl.device_bit() | MacMod::LOpt.device_bit() | MacMod::LCmd.device_bit();
        k.begin_epoch(flags);
        assert!(k.pressed().is_empty());
        // Typing with the stale modifiers still physically down sends only the letter.
        assert_eq!(k.key_down(A, false), vec![(WA, D)]);
        assert_eq!(k.key_up(A), vec![(WA, U)]);
        // Releasing them sends nothing.
        assert_eq!(k.modifier(MacMod::LCmd, false), vec![]);
        assert_eq!(k.modifier(MacMod::LOpt, false), vec![]);
        // Pressing Cmd again after release is sent.
        assert_eq!(k.modifier(MacMod::LCmd, true), vec![(win::LCTRL, D)]);
        // Ctrl still ignored (never released).
        assert_eq!(k.key_down(mac::TAB, false), vec![(win::LCTRL, U), (win::LALT, D), (win::TAB, D)]);
    }

    #[test]
    fn key_up_for_key_held_before_epoch_is_ignored() {
        let mut k = m();
        k.begin_epoch(0);
        assert_eq!(k.key_up(A), vec![]);
    }

    #[test]
    fn cmd_as_win_mode() {
        let mut k = KeyMapper::new(MapConfig { cmd_as_win: true, chords: true });
        assert_eq!(k.modifier(MacMod::LCmd, true), vec![(win::LWIN, D)]);
        assert_eq!(k.modifier(MacMod::LCtrl, true), vec![(win::LCTRL, D)]);
        // No chord translation in this mode.
        assert_eq!(k.key_down(mac::TAB, false), vec![(win::TAB, D)]);
    }

    #[test]
    fn q_without_cmd_is_q() {
        let mut k = m();
        assert_eq!(k.key_down(mac::Q, false), vec![(WQ, D)]);
    }
}

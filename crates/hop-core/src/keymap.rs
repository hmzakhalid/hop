//! Mac virtual key codes (`kVK_*`, which are positional) to Windows set-1 scan codes
//! (also positional). The PC's own keyboard layout then decides the character, exactly
//! as it would for a USB keyboard.

/// Windows key codes used by the mapper (see `wire::code`).
pub mod win {
    pub const LCTRL: u16 = 0x001D;
    pub const RCTRL: u16 = 0x011D;
    pub const LSHIFT: u16 = 0x002A;
    pub const RSHIFT: u16 = 0x0036;
    pub const LALT: u16 = 0x0038;
    pub const RALT: u16 = 0x0138;
    pub const LWIN: u16 = 0x015B;
    pub const RWIN: u16 = 0x015C;
    pub const TAB: u16 = 0x000F;
    pub const BACKSPACE: u16 = 0x000E;
    pub const CAPSLOCK: u16 = 0x003A;
    pub const F4: u16 = 0x003E;
    pub const HOME: u16 = 0x0147;
    pub const END: u16 = 0x014F;
    pub const LEFT: u16 = 0x014B;
    pub const RIGHT: u16 = 0x014D;
    pub const UP: u16 = 0x0148;
    pub const DOWN: u16 = 0x0150;
    pub const L: u16 = 0x0026;

    pub fn is_modifier(c: u16) -> bool {
        matches!(c, LCTRL | RCTRL | LSHIFT | RSHIFT | LALT | RALT | LWIN | RWIN)
    }
    /// Alt and Win open the menu bar / Start menu when released with nothing pressed in between.
    pub fn is_menu_modifier(c: u16) -> bool {
        matches!(c, LALT | RALT | LWIN | RWIN)
    }
}

/// Mac key codes we treat specially.
pub mod mac {
    pub const TAB: u16 = 0x30;
    pub const SPACE: u16 = 0x31;
    pub const DELETE: u16 = 0x33; // backspace
    pub const Q: u16 = 0x0C;
    pub const CAPSLOCK: u16 = 0x39;
    pub const LEFT: u16 = 0x7B;
    pub const RIGHT: u16 = 0x7C;
    pub const DOWN: u16 = 0x7D;
    pub const UP: u16 = 0x7E;

    /// `CGEventFlags` bits.
    pub const FLAG_CONTROL: u64 = 0x0004_0000;
    pub const FLAG_OPTION: u64 = 0x0008_0000;
    pub const FLAG_COMMAND: u64 = 0x0010_0000;
    /// ⌃⌥⌘: held with an arrow, switches between the Mac and the PC.
    pub const HYPER: u64 = FLAG_CONTROL | FLAG_OPTION | FLAG_COMMAND;

    /// Whether this key with these `CGEventFlags` is the switch hotkey (⌃⌥⌘ + an arrow).
    pub fn is_switch(code: u16, flags: u64) -> bool {
        matches!(code, LEFT | RIGHT | UP | DOWN) && flags & HYPER == HYPER
    }
}

/// A physical Mac modifier key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacMod {
    LCmd = 0,
    RCmd = 1,
    LOpt = 2,
    ROpt = 3,
    LCtrl = 4,
    RCtrl = 5,
    LShift = 6,
    RShift = 7,
}

pub const ALL_MODS: [MacMod; 8] = [
    MacMod::LCmd,
    MacMod::RCmd,
    MacMod::LOpt,
    MacMod::ROpt,
    MacMod::LCtrl,
    MacMod::RCtrl,
    MacMod::LShift,
    MacMod::RShift,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModClass {
    Cmd,
    Opt,
    Ctrl,
    Shift,
}

impl MacMod {
    /// The key's `kVK_*` code.
    pub fn keycode(self) -> u16 {
        match self {
            MacMod::LCmd => 0x37,
            MacMod::RCmd => 0x36,
            MacMod::LOpt => 0x3A,
            MacMod::ROpt => 0x3D,
            MacMod::LCtrl => 0x3B,
            MacMod::RCtrl => 0x3E,
            MacMod::LShift => 0x38,
            MacMod::RShift => 0x3C,
        }
    }

    pub fn from_keycode(code: u16) -> Option<MacMod> {
        ALL_MODS.into_iter().find(|m| m.keycode() == code)
    }

    /// Device-dependent bit in `CGEventFlags` (IOLLEvent.h `NX_DEVICE*KEYMASK`).
    pub fn device_bit(self) -> u64 {
        match self {
            MacMod::LCtrl => 0x0001,
            MacMod::LShift => 0x0002,
            MacMod::RShift => 0x0004,
            MacMod::LCmd => 0x0008,
            MacMod::RCmd => 0x0010,
            MacMod::LOpt => 0x0020,
            MacMod::ROpt => 0x0040,
            MacMod::RCtrl => 0x2000,
        }
    }

    pub fn is_down(self, flags: u64) -> bool {
        flags & self.device_bit() != 0
    }

    pub fn class(self) -> ModClass {
        match self {
            MacMod::LCmd | MacMod::RCmd => ModClass::Cmd,
            MacMod::LOpt | MacMod::ROpt => ModClass::Opt,
            MacMod::LCtrl | MacMod::RCtrl => ModClass::Ctrl,
            MacMod::LShift | MacMod::RShift => ModClass::Shift,
        }
    }

    pub fn is_right(self) -> bool {
        matches!(self, MacMod::RCmd | MacMod::ROpt | MacMod::RCtrl | MacMod::RShift)
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

/// Map a non-modifier Mac key code to a Windows scan code. `None` means the key stays on the Mac.
pub fn mac_to_win(code: u16) -> Option<u16> {
    use win::*;
    const E: u16 = crate::wire::code::EXTENDED;
    let sc: u16 = match code {
        0x00 => 0x1E, // A
        0x01 => 0x1F, // S
        0x02 => 0x20, // D
        0x03 => 0x21, // F
        0x04 => 0x23, // H
        0x05 => 0x22, // G
        0x06 => 0x2C, // Z
        0x07 => 0x2D, // X
        0x08 => 0x2E, // C
        0x09 => 0x2F, // V
        0x0A => 0x56, // ISO section key → non-US backslash
        0x0B => 0x30, // B
        0x0C => 0x10, // Q
        0x0D => 0x11, // W
        0x0E => 0x12, // E
        0x0F => 0x13, // R
        0x10 => 0x15, // Y
        0x11 => 0x14, // T
        0x12 => 0x02, // 1
        0x13 => 0x03, // 2
        0x14 => 0x04, // 3
        0x15 => 0x05, // 4
        0x16 => 0x07, // 6
        0x17 => 0x06, // 5
        0x18 => 0x0D, // =
        0x19 => 0x0A, // 9
        0x1A => 0x08, // 7
        0x1B => 0x0C, // -
        0x1C => 0x09, // 8
        0x1D => 0x0B, // 0
        0x1E => 0x1B, // ]
        0x1F => 0x18, // O
        0x20 => 0x16, // U
        0x21 => 0x1A, // [
        0x22 => 0x17, // I
        0x23 => 0x19, // P
        0x24 => 0x1C, // Return
        0x25 => win::L,
        0x26 => 0x24, // J
        0x27 => 0x28, // '
        0x28 => 0x25, // K
        0x29 => 0x27, // ;
        0x2A => 0x2B, // backslash
        0x2B => 0x33, // ,
        0x2C => 0x35, // /
        0x2D => 0x31, // N
        0x2E => 0x32, // M
        0x2F => 0x34, // .
        0x30 => TAB,
        0x31 => 0x39, // Space
        0x32 => 0x29, // `
        0x33 => BACKSPACE, // Delete
        0x35 => 0x01, // Escape
        0x40 => 0x68, // F17
        0x41 => 0x53, // keypad .
        0x43 => 0x37, // keypad *
        0x45 => 0x4E, // keypad +
        0x47 => 0x45, // keypad clear → Num Lock position
        0x4B => E | 0x35, // keypad /
        0x4C => E | 0x1C, // keypad Enter
        0x4E => 0x4A, // keypad -
        0x4F => 0x69, // F18
        0x50 => 0x6A, // F19
        0x51 => 0x59, // keypad =
        0x52 => 0x52, // keypad 0
        0x53 => 0x4F, // keypad 1
        0x54 => 0x50, // keypad 2
        0x55 => 0x51, // keypad 3
        0x56 => 0x4B, // keypad 4
        0x57 => 0x4C, // keypad 5
        0x58 => 0x4D, // keypad 6
        0x59 => 0x47, // keypad 7
        0x5A => 0x6B, // F20
        0x5B => 0x48, // keypad 8
        0x5C => 0x49, // keypad 9
        0x5D => 0x7D, // JIS Yen
        0x5E => 0x73, // JIS underscore (Ro)
        0x5F => 0x7E, // JIS keypad comma
        0x60 => 0x3F, // F5
        0x61 => 0x40, // F6
        0x62 => 0x41, // F7
        0x63 => 0x3D, // F3
        0x64 => 0x42, // F8
        0x65 => 0x43, // F9
        0x66 => 0x7B, // JIS Eisu → Muhenkan
        0x67 => 0x57, // F11
        0x68 => 0x70, // JIS Kana
        0x69 => 0x64, // F13
        0x6A => 0x67, // F16
        0x6B => 0x65, // F14
        0x6D => 0x44, // F10
        0x6E => E | 0x5D, // context menu
        0x6F => 0x58, // F12
        0x71 => 0x66, // F15
        0x72 => E | 0x52, // Help → Insert
        0x73 => HOME,
        0x74 => E | 0x49, // Page Up
        0x75 => E | 0x53, // Forward Delete
        0x76 => F4,
        0x77 => END,
        0x78 => 0x3C, // F2
        0x79 => E | 0x51, // Page Down
        0x7A => 0x3B, // F1
        0x7B => LEFT,
        0x7C => RIGHT,
        0x7D => DOWN,
        0x7E => UP,
        _ => return None,
    };
    Some(sc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fn_and_modifiers_are_not_plain_keys() {
        assert_eq!(mac_to_win(0x3F), None); // Fn stays on the Mac
        for m in ALL_MODS {
            assert_eq!(MacMod::from_keycode(m.keycode()), Some(m));
            assert_eq!(mac_to_win(m.keycode()), None, "{m:?} goes through the modifier path");
        }
    }

    #[test]
    fn table_is_injective() {
        let mut seen = std::collections::HashMap::new();
        for k in 0u16..0x80 {
            if let Some(w) = mac_to_win(k) {
                if let Some(prev) = seen.insert(w, k) {
                    panic!("mac {prev:#x} and {k:#x} both map to {w:#x}");
                }
                assert!(!win::is_modifier(w));
            }
        }
    }
}

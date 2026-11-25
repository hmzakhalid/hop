//! Words one machine shows that the other machine's instructions quote, so a renamed menu item
//! changes both.

/// The PC's menu item (Hop's taskbar icon) that shows a pairing code.
pub const PC_PAIR_ITEM: &str = "Pair a Mac";
/// The Mac's menu item (Hop's menu-bar icon) that pairs.
pub const MAC_PAIR_ITEM: &str = "Pair with a PC";
/// The switch hotkey's modifiers ([`crate::keymap::mac::HYPER`]), held with an arrow.
pub const HOTKEY: &str = "⌃⌥⌘";

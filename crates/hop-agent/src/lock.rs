//! ⌃L on the Mac (Win+L on the PC) locks the PC. Windows ignores an injected Win+L (locking
//! answers only a real keyboard), so the injector calls `LockWorkStation` instead. This decides
//! when, without Windows, so it can be tested anywhere.

use hop_core::keymap::win;

/// What the injector does with one key change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Send it to Windows as usual.
    Send,
    /// Lock the PC instead, then report whether that worked ([`LockKey::locked`]).
    Lock,
    /// Drop it: a repeat or the release of an L press that locked (Windows never saw the press).
    Drop,
}

#[derive(Default)]
pub struct LockKey {
    /// The L press that locked is still down.
    holding: bool,
}

impl LockKey {
    pub fn step(&mut self, code: u16, down: bool, win_held: bool) -> Step {
        if code != win::L {
            return Step::Send;
        }
        if self.holding {
            self.holding = down;
            return Step::Drop;
        }
        if down && win_held {
            Step::Lock
        } else {
            Step::Send
        }
    }

    /// The lock asked for by [`Step::Lock`] happened, so the rest of that press is dropped.
    pub fn locked(&mut self) {
        self.holding = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_press_locks_once_and_windows_never_sees_its_repeats_or_release() {
        let mut k = LockKey::default();
        assert_eq!(k.step(win::L, true, true), Step::Lock);
        k.locked();
        assert_eq!(k.step(win::L, true, true), Step::Drop, "auto-repeat");
        assert_eq!(k.step(win::L, true, false), Step::Drop, "repeat after Win went up");
        assert_eq!(k.step(win::L, false, false), Step::Drop, "the release");
        assert_eq!(k.step(win::L, true, false), Step::Send, "a later plain L types");
        assert_eq!(k.step(win::L, false, false), Step::Send);
    }

    #[test]
    fn a_failed_lock_lets_the_keys_through() {
        let mut k = LockKey::default();
        assert_eq!(k.step(win::L, true, true), Step::Lock);
        // LockWorkStation failed: no `locked()`, so the press goes to Windows and so does the rest.
        assert_eq!(k.step(win::L, true, true), Step::Lock);
        assert_eq!(k.step(win::L, false, true), Step::Send);
        assert_eq!(k.step(win::TAB, true, true), Step::Send);
    }
}

//! The Mac cursor while the PC has control: hidden, frozen, parked, and always recoverable.
//!
//! Three threads can touch it: the session (crossings), the tap (motion) and the watchdog
//! (recovery when nothing else can run). A generation number makes their order safe: a freeze or
//! re-park that finishes after the watchdog revoked control is undone, and a revoke that was
//! decided before a newer crossing does not apply to it.

use crate::macos;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

/// The hidden cursor counts as wandering once it is this far (points) from where it was parked.
const DRIFT: f64 = 200.0;

#[derive(Default)]
pub struct Cursor {
    park: Mutex<(f64, f64)>,
    generation: AtomicU64,
    hidden: AtomicBool,
    revoked: AtomicBool,
}

impl Cursor {
    /// Input goes to the PC: park the cursor where it is, hide it and freeze it.
    pub fn capture(&self) {
        *self.park.lock().unwrap() = macos::cursor_location();
        let g = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if !self.hidden.swap(true, Ordering::SeqCst) {
            macos::hide_cursor();
        }
        macos::freeze_cursor(true);
        self.undo_if_revoked(g);
    }

    /// Input comes back: put the cursor at `warp_to` (or leave it), unfreeze and show it.
    pub fn release(&self, warp_to: Option<(f64, f64)>) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.restore(warp_to);
        self.revoked.store(false, Ordering::SeqCst);
    }

    /// Freezing a background process's cursor is not guaranteed: if the hidden cursor wandered
    /// (toward hot corners or the Dock), put it back.
    pub fn keep_parked(&self, loc: (f64, f64)) {
        let park = *self.park.lock().unwrap();
        if (loc.0 - park.0).abs() + (loc.1 - park.1).abs() > DRIFT {
            let g = self.generation.load(Ordering::SeqCst);
            macos::warp_cursor(park);
            macos::freeze_cursor(false);
            macos::freeze_cursor(true);
            self.undo_if_revoked(g);
        }
    }

    /// Watchdog: take the cursor back if `expired()` still holds and no crossing happened since
    /// this call started. Returns true if it did.
    pub fn revoke_if(&self, expired: impl Fn() -> bool) -> bool {
        let g = self.generation.load(Ordering::SeqCst);
        if !expired() || self.generation.compare_exchange(g, g + 1, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return false;
        }
        self.revoked.store(true, Ordering::SeqCst);
        self.restore(None);
        true
    }

    /// The watchdog took the cursor back and nobody has released it properly yet.
    pub fn revoked(&self) -> bool {
        self.revoked.load(Ordering::SeqCst)
    }

    /// The controller agrees the Mac has its input again.
    pub fn settle(&self) {
        self.revoked.store(false, Ordering::SeqCst);
    }

    fn restore(&self, warp_to: Option<(f64, f64)>) {
        if let Some(p) = warp_to {
            macos::warp_cursor(p);
        }
        macos::freeze_cursor(false);
        if self.hidden.swap(false, Ordering::SeqCst) {
            macos::show_cursor();
        }
    }

    fn undo_if_revoked(&self, g: u64) {
        if self.generation.load(Ordering::SeqCst) != g || self.revoked() {
            self.restore(None);
        }
    }
}

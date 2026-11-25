//! The Mac side of a live connection, without macOS: a [`Controller`] driven over a [`Link`].
//!
//! The session sends whatever the controller decides, runs its timers, keeps the link up,
//! answers acks and link events, and publishes the lease deadline for lock-free watchdogs.
//! What happens on the Mac itself (cursor, indicator) is an [`Effects`] implementation, called
//! in decision order with the controller locked, so a late "hide" can never undo a "restore".

use crate::clock::now_us;
use crate::controller::{Controller, Effect, Output};
use crate::link::{Event, Link, HANDSHAKE_RETRY};
use crate::wire::Msg;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The timer never sleeps longer than this, so link upkeep runs at least this often.
const MAX_TIMER_SLEEP: Duration = Duration::from_millis(100);
const _: () = assert!(MAX_TIMER_SLEEP.as_micros() < HANDSHAKE_RETRY.as_micros());

/// What a crossing means on this machine, beyond the network.
pub trait Effects: Send + Sync {
    fn apply(&self, effect: Effect);
}

/// For headless use (tests, the scripted driver).
pub struct NoEffects;

impl Effects for NoEffects {
    fn apply(&self, _: Effect) {}
}

pub struct Session {
    ctl: Mutex<Controller>,
    link: Arc<Link>,
    effects: Arc<dyn Effects>,
    deadline: AtomicU64,
    wake: (Mutex<bool>, Condvar),
}

impl Session {
    pub fn new(controller: Controller, link: Arc<Link>, effects: Arc<dyn Effects>) -> Arc<Session> {
        Arc::new(Session {
            ctl: Mutex::new(controller),
            link,
            effects,
            deadline: AtomicU64::new(0),
            wake: (Mutex::new(false), Condvar::new()),
        })
    }

    /// One controller step: send what it decided and carry out its effect. Returns whether the
    /// Mac event that caused it should be swallowed.
    pub fn act(&self, f: impl FnOnce(&mut Controller, u64) -> Output) -> bool {
        let mut c = self.ctl.lock().unwrap();
        let out = f(&mut c, now_us());
        let swallow = self.finish(&c, out);
        self.wake_timer_if_in_control(&c);
        swallow
    }

    /// [`act`](Self::act), unless another thread holds the controller (never blocks).
    pub fn try_act(&self, f: impl FnOnce(&mut Controller, u64) -> Output) -> Option<bool> {
        let mut c = self.ctl.try_lock().ok()?;
        let out = f(&mut c, now_us());
        let swallow = self.finish(&c, out);
        self.wake_timer_if_in_control(&c);
        Some(swallow)
    }

    /// While the PC has control, an event may need a sooner timer (a retransmit).
    fn wake_timer_if_in_control(&self, c: &Controller) {
        if c.is_remote() {
            *self.wake.0.lock().unwrap() = true;
            self.wake.1.notify_one();
        }
    }

    /// Whether something asked the timer to run again at once; clears the request.
    #[cfg(test)]
    fn take_wake(&self) -> bool {
        std::mem::take(&mut *self.wake.0.lock().unwrap())
    }

    fn finish(&self, c: &Controller, out: Output) -> bool {
        // Arm the watchdog before any effect (Enter hides and holds the Mac's cursor, and if
        // anything after it stalls, the watchdog must already know when to take it back), and
        // disarm it only after the effect (a stalled Leave stays covered).
        let deadline = c.lease_deadline().unwrap_or(0);
        if deadline != 0 {
            self.deadline.store(deadline, Ordering::SeqCst);
        }
        if let Some(s) = out.send {
            // A failed send needs no handling here: the lease returns control, and the same
            // failure shows up when `tick` tries to reconnect.
            let _ = self.link.send(&Msg::State(s));
        }
        if let Some(e) = out.effect {
            self.effects.apply(e);
        }
        self.deadline.store(deadline, Ordering::SeqCst);
        out.swallow
    }

    pub fn read<R>(&self, f: impl FnOnce(&Controller) -> R) -> R {
        f(&self.ctl.lock().unwrap())
    }

    /// When the PC's silence ends control (µs on [`now_us`]); 0 while the Mac has its input.
    /// Lock-free, so a watchdog can use it even if another thread hangs.
    pub fn lease_deadline(&self) -> u64 {
        self.deadline.load(Ordering::SeqCst)
    }

    /// Handle one packet from the PC, blocking up to the link's read timeout.
    pub fn receive(&self) -> io::Result<Event> {
        let event = self.link.recv()?;
        match &event {
            Event::Msg(Msg::Ack(a)) => {
                self.act(|c, now| c.ack(now, a));
            }
            // A new session means the PC's receiver started over: end any crossing.
            Event::Connected => {
                self.act(|c, now| c.session_changed(now));
            }
            _ => {}
        }
        Ok(event)
    }

    /// Heartbeats, retransmits, keepalives, the lease and reconnects. Returns how long until it
    /// next has work; an error means the link could not even try to reconnect.
    pub fn tick(&self) -> (Duration, io::Result<()>) {
        let next = {
            let mut c = self.ctl.lock().unwrap();
            let out = c.tick(now_us());
            self.finish(&c, out);
            c.next_wake(now_us())
        };
        let link = self.link.maintain();
        let wait = Duration::from_micros(next.saturating_sub(now_us())).clamp(Duration::from_micros(300), MAX_TIMER_SLEEP);
        (wait, link)
    }

    /// Run [`receive`](Self::receive) and [`tick`](Self::tick) on their own threads until `stop`.
    /// `on_event` sees every link event and error (for status messages).
    pub fn spawn(
        self: &Arc<Self>,
        stop: Arc<AtomicBool>,
        on_event: impl Fn(&io::Result<Event>) + Send + Sync + 'static,
    ) {
        let on_event = Arc::new(on_event);
        let (s, st, report) = (self.clone(), stop.clone(), on_event.clone());
        std::thread::spawn(move || {
            while !st.load(Ordering::Relaxed) {
                let r = s.receive();
                report(&r);
                if r.is_err() {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        });
        let s = self.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (wait, link) = s.tick();
                if let Err(e) = link {
                    on_event(&Err(e));
                }
                let (m, cv) = &s.wake;
                let mut woke = cv.wait_timeout_while(m.lock().unwrap(), wait, |woke| !*woke).unwrap().0;
                *woke = false;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::ControllerConfig;
    use crate::crypto::StaticKeys;
    use crate::keymap::mac;
    use crate::layout::Rect;
    use crate::wire::{Ack, Monitor};
    use std::net::UdpSocket;

    fn session(effects: Arc<dyn Effects>) -> Arc<Session> {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let nowhere = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let link = Arc::new(Link::new(sock, StaticKeys::generate(), [0; 32], nowhere).unwrap());
        let ctl = Controller::new(ControllerConfig::default(), vec![Rect::new(0.0, 0.0, 1512.0, 982.0)]);
        Session::new(ctl, link, effects)
    }

    fn enter(session: &Session) {
        let monitors = vec![Monitor { left: 0, top: 0, right: 1920, bottom: 1080, scale_milli: 1000 }];
        session.act(|c, now| c.ack(now, &Ack { monitors, ..Ack::default() }));
        session.act(|c, now| c.key(now, mac::RIGHT, true, false, mac::HYPER, (0.0, 0.0)));
    }

    /// Records the watchdog's deadline as each effect starts.
    #[derive(Default)]
    struct DeadlineAtEffect {
        session: std::sync::OnceLock<std::sync::Weak<Session>>,
        seen: Mutex<Vec<(bool, u64)>>,
    }

    impl Effects for DeadlineAtEffect {
        fn apply(&self, effect: Effect) {
            let deadline = self.session.get().and_then(|w| w.upgrade()).map_or(0, |s| s.lease_deadline());
            self.seen.lock().unwrap().push((matches!(effect, Effect::Enter), deadline));
        }
    }

    #[test]
    fn the_watchdog_is_armed_before_entering_and_disarmed_after_leaving() {
        let effects = Arc::new(DeadlineAtEffect::default());
        let session = session(effects.clone());
        effects.session.set(Arc::downgrade(&session)).unwrap();
        enter(&session);
        session.act(|c, now| c.key(now, mac::LEFT, true, false, mac::HYPER, (0.0, 0.0)));
        let seen = effects.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "one Enter and one Leave: {seen:?}");
        assert!(seen[0].0 && seen[0].1 != 0, "Enter must run with the watchdog armed: {seen:?}");
        assert!(!seen[1].0 && seen[1].1 != 0, "Leave must run before the watchdog is disarmed: {seen:?}");
        assert_eq!(session.lease_deadline(), 0, "disarmed once the Mac has its input back");
    }

    #[test]
    fn the_timer_sleeps_while_the_pc_has_control() {
        let session = session(Arc::new(NoEffects));
        enter(&session);
        assert!(session.read(|c| c.is_remote()));
        assert!(session.take_wake(), "an event while in control wakes the timer");
        let (wait, _) = session.tick();
        assert!(!session.take_wake(), "a tick must not wake the timer again, or it spins");
        assert!(wait >= Duration::from_millis(1), "nothing is due for a while: {wait:?}");
    }
}

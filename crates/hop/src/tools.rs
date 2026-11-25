//! `hop bench`: round trips to the PC.

use crate::cli::die;
use crate::{macos, pc};
use hop_core::clock::now_us;
use hop_core::link::Event;
use hop_core::stats::Summary;
use hop_core::wire::Msg;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Round trips to the PC: pings 5 ms apart.
pub fn bench(host: Option<&str>, n: usize) {
    let pc = pc::connect(host);
    println!("Measuring {n} round trips to {} ({})…", pc.name, pc.link.peer().ip());
    let done = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(Mutex::new(Vec::with_capacity(n)));
    let (link, d2, s2) = (pc.link.clone(), done.clone(), samples.clone());
    let rx = std::thread::spawn(move || {
        while !d2.load(Ordering::Relaxed) {
            if let Ok(Event::Msg(Msg::Pong { t })) = link.recv() {
                s2.lock().unwrap().push(now_us().saturating_sub(t) as f64 / 1000.0);
            }
        }
    });
    for _ in 0..n {
        let _ = pc.link.send(&Msg::Ping { t: now_us() });
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(500));
    done.store(true, Ordering::Relaxed);
    let _ = rx.join();
    let got = samples.lock().unwrap().clone();
    let lost = 100.0 * n.saturating_sub(got.len()) as f64 / n as f64;
    let Some(s) = Summary::of(got) else { die("no replies") };
    println!(
        "round trip  p50 {:.2} ms   p90 {:.2} ms   p99 {:.2} ms   p99.9 {:.2} ms   max {:.2} ms   lost {lost:.1}%",
        s.p50, s.p90, s.p99, s.p999, s.max
    );
    println!("One way is about half of that; Hop itself adds about 0.1 ms per side.");
    if macos::awdl_active() == Some(true) {
        println!("Tip: AWDL (AirDrop/Handoff) is active and causes periodic Wi-Fi spikes. Compare after `sudo ifconfig awdl0 down` (undo: `sudo ifconfig awdl0 up`).");
    }
}

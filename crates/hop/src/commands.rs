//! The terminal commands that talk to the PC: `hop pair`, `hop unpair`, `hop bench` and
//! `hop view`. Each returns an error sentence for `main` to print.

use crate::cli::prompt;
use crate::{macos, net, pc, screen};
use hop_core::clock::now_us;
use hop_core::crypto::fingerprint;
use hop_core::link::Event;
use hop_core::stats::Summary;
use hop_core::wire::Msg;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn pair(host: Option<&str>) -> Result<(), String> {
    let (addr, label) = match host {
        Some(h) => (pc::parse_host(h)?, h.to_string()),
        None => {
            println!("Looking for PCs running hop-agent…");
            let found = pc::discover()?;
            let f = match found.len() {
                0 => return Err("no PC answered. Is hop-agent running there, on the same network? You can also pass --host <PC IP>.".into()),
                1 => &found[0],
                _ => {
                    for (i, f) in found.iter().enumerate() {
                        println!("  {}. {} ({})", i + 1, f.name, f.addr.ip());
                    }
                    let pick = prompt("Which one? ").parse::<usize>().ok().filter(|n| (1..=found.len()).contains(n));
                    &found[pick.ok_or("no such PC")? - 1]
                }
            };
            (f.addr, f.name.clone())
        }
    };
    println!("Pairing with {label} at {}.", addr.ip());
    let code = prompt("Type the code shown on the PC: ");
    let rec = pc::pair_with(addr, &code)?;
    println!("Paired with {} (key {}).", rec.name, fingerprint(&rec.public));
    println!("Now run: hop");
    Ok(())
}

pub fn unpair() -> Result<(), String> {
    let forgot = pc::forget().map_err(|e| e.to_string())?;
    println!("{}", if forgot { "Forgot the PC." } else { "Not paired." });
    Ok(())
}

/// Round trips to the PC: `n` pings 5 ms apart.
pub fn bench(host: Option<&str>, n: usize) -> Result<(), String> {
    /// How long to wait for the last replies once every ping is out.
    const LAST_REPLIES: Duration = Duration::from_millis(500);
    let pc = pc::connect(host)?;
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
        let _ = pc.link.send(&Msg::Ping { t: now_us() }); // a lost ping counts as lost below
        std::thread::sleep(Duration::from_millis(5));
    }
    let sent = Instant::now();
    while samples.lock().unwrap().len() < n && sent.elapsed() < LAST_REPLIES {
        std::thread::sleep(Duration::from_millis(5));
    }
    done.store(true, Ordering::Relaxed);
    let _ = rx.join();
    let got = samples.lock().unwrap().clone();
    let lost = 100.0 * n.saturating_sub(got.len()) as f64 / n as f64;
    let s = Summary::of(got).ok_or("no replies")?;
    println!(
        "round trip  p50 {:.2} ms   p90 {:.2} ms   p99 {:.2} ms   p99.9 {:.2} ms   max {:.2} ms   lost {lost:.1}%",
        s.p50, s.p90, s.p99, s.p999, s.max
    );
    println!("One way is about half of that; Hop itself adds about 0.1 ms per side.");
    if net::awdl_active() == Some(true) {
        println!("Tip: AWDL (AirDrop/Handoff) is active and causes periodic Wi-Fi spikes. Compare after `sudo ifconfig awdl0 down` (undo: `sudo ifconfig awdl0 up`).");
    }
    Ok(())
}

/// Watch the paired PC's screen in a window (no keyboard or mouse control), until it closes.
pub fn view(host: Option<&str>) -> Result<(), String> {
    let (rec, addr) = pc::target(host)?;
    let mtm = macos::start_app(macos::AppKind::Window);
    screen::open(mtm, screen::Target { name: rec.name, addr, public: rec.public }, None, || std::process::exit(0));
    macos::run_app()
}

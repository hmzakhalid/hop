//! `hop run`: connect to the paired PC from a terminal and run until Ctrl+C.

use crate::cli::RunOptions;
use crate::live::{self, Live};
use crate::{macos, pc};
use hop_core::link::Event;
use hop_core::ui::HOTKEY;

pub fn run(host: Option<&str>, o: &RunOptions) -> Result<(), String> {
    let signals = live::block_signals(); // before any thread starts
    if !macos::ask_for_accessibility() {
        return Err("hop needs Accessibility access to read the keyboard and trackpad.\n\
             Open System Settings → Privacy & Security → Accessibility, turn on the app you run hop from\n\
             (Terminal, iTerm, …), then run hop again."
            .into());
    }
    let pc = pc::connect(host)?;
    let (side, edges, win) = (o.cfg.side, o.cfg.edges, o.cfg.map.win_key());

    macos::start_app(macos::AppKind::Background);
    let live = Live::start(signals, o.verbose)?;
    let verbose = o.verbose;
    live.connect(pc.link.clone(), &pc.name, o.cfg.clone(), move |e| match e {
        Ok(Event::Connected) if verbose => println!("link: connected"),
        Ok(Event::Lost) if verbose => println!("link: the PC restarted, reconnecting"),
        Err(e) if verbose => eprintln!("link: {e}"),
        _ => {}
    });
    if o.stats {
        print_stats(live);
    }

    let (go, back) = (side.arrow(), side.opposite().arrow());
    println!("Connected to {} ({}). {HOTKEY}{go} takes the keyboard and mouse there, {HOTKEY}{back} brings them back.", pc.name, pc.link.peer().ip());
    if edges {
        println!("Or push the pointer off the {} edge of this screen, and off the PC's {} edge to come back.", side.name(), side.opposite().name());
    }
    println!("On the PC, {win} is the Windows key ({win}L = Win+L). Ctrl+C quits.");
    macos::run_app()
}

/// How often `--stats` prints.
pub const STATS_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Print the round trip, memory and CPU every [`STATS_EVERY`].
fn print_stats(live: &'static Live) {
    std::thread::spawn(move || {
        let mut last_cpu = macos::usage().1;
        loop {
            std::thread::sleep(STATS_EVERY);
            let (memory, cpu) = macos::usage();
            let rtt = match live.session().and_then(|s| s.read(|c| c.rtt_summary())) {
                Some(r) => format!("round trip p50 {:.2} ms p99 {:.2} ms ({})", r.p50, r.p99, r.count),
                None => "no round trips yet".into(),
            };
            println!("stats: {rtt} · memory {memory:.1} MB · cpu {:.2}%", (cpu - last_cpu) / STATS_EVERY.as_secs_f64() * 100.0);
            last_cpu = cpu;
        }
    });
}

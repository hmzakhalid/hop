//! `hop run`: connect to the paired PC from a terminal and run until Ctrl+C.

use crate::cli::{die, RunOptions};
use crate::live::{self, Live};
use crate::{hud, macos, pc};
use hop_core::controller::Controller;
use hop_core::link::Event;
use hop_core::session::Session;

pub fn run(host: Option<&str>, o: &RunOptions) -> ! {
    let signals = live::block_signals(); // before any thread starts
    if !macos::accessibility_trusted() {
        die(
            "hop needs Accessibility access to read the keyboard and trackpad.\n\
             Open System Settings → Privacy & Security → Accessibility, turn on the app you run hop from\n\
             (Terminal, iTerm, …), then run hop again.",
        );
    }
    let pc = pc::connect(host);
    let (side, edges, win) = (o.cfg.side, o.cfg.edges, o.cfg.map.win_key());

    hud::init();
    let live = Live::start(signals, o.verbose).unwrap_or_else(|e| die(e));
    let session = Session::new(Controller::new(o.cfg.clone(), macos::displays()), pc.link.clone(), live.effects(&pc.name, side));
    let verbose = o.verbose;
    live.attach(session, move |e| match e {
        Ok(Event::Connected) if verbose => println!("link: connected"),
        Ok(Event::Lost) if verbose => println!("link: the PC restarted, reconnecting"),
        Err(e) if verbose => eprintln!("link: {e}"),
        _ => {}
    });
    if o.stats {
        live.print_stats();
    }

    let (go, back) = (side.arrow(), side.opposite().arrow());
    println!("Connected to {} ({}). ⌃⌥⌘{go} takes the keyboard and mouse there, ⌃⌥⌘{back} brings them back.", pc.name, pc.link.peer().ip());
    if edges {
        println!("Or push the pointer off the {} edge of this screen, and off the PC's {} edge to come back.", side.name(), side.opposite().name());
    }
    println!("On the PC, {win} is the Windows key ({win}L = Win+L). Ctrl+C quits.");
    hud::run()
}

//! hop: use this Mac's keyboard and trackpad on a Windows PC running hop-agent.

mod app;
mod cli;
mod cursor;
#[cfg(feature = "dev-tools")]
mod devtools;
mod hud;
mod login;
mod prefs;
mod live;
mod macos;
mod pc;
mod run;
mod screen;
mod tools;
mod updates;
mod view;

use cli::{Command, ParseError};

fn main() {
    // Opening Hop.app starts the menu-bar app; everything else is the command line.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() && macos::in_app_bundle() {
        app::run();
    }
    let cmd = match cli::parse(args) {
        Ok(c) => c,
        Err(ParseError::Help) => {
            print!("{}", cli::usage());
            std::process::exit(0);
        }
        Err(ParseError::Bad(e)) => {
            eprint!("error: {e}\n\n{}", cli::usage());
            std::process::exit(2);
        }
    };
    match cmd {
        Command::App => app::run(),
        Command::Run { host, opts } => run::run(host.as_deref(), &opts),
        Command::Pair { host } => pc::pair(host.as_deref()),
        Command::Bench { host, count } => tools::bench(host.as_deref(), count),
        Command::Unpair => pc::unpair(),
        #[cfg(feature = "dev-tools")]
        Command::Drive { host } => devtools::drive(host.as_deref()),
        Command::View { host, file: None } => screen::view_cli(host.as_deref()),
        #[cfg(feature = "dev-tools")]
        Command::View { file: Some(f), .. } => devtools::view_file(&f),
        #[cfg(not(feature = "dev-tools"))]
        Command::View { file: Some(_), .. } => cli::die("playing a recording is a developer tool"),
        #[cfg(feature = "dev-tools")]
        Command::Badge { side } => devtools::badge(side),
        #[cfg(feature = "dev-tools")]
        Command::Menu => app::print_menus(),
    }
}

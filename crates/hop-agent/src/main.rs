//! hop-agent: runs on the Windows PC and receives keyboard and mouse input from a paired Mac.
//!
//! Off Windows it builds with a dry-run injector that prints what it would do.

mod dryrun;
mod platform;
mod role;
#[cfg(windows)]
mod hooks;
#[cfg(windows)]
mod selftest;
#[cfg(windows)]
mod scm;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod win;

use hop_core::wire::DEFAULT_PORT;
use role::{Role, RunOptions, HELPER_COMMAND, STOP_EVENT_FLAG};

fn usage() -> String {
    format!(
        "\
hop-agent: let a paired Mac drive this PC's keyboard and mouse.

Usage:
  hop-agent            run (starts pairing automatically if no Mac is paired)
  hop-agent pair       show a new pairing code, then run
  hop-agent unpair     forget the paired Mac
  hop-agent selftest   check input injection on this PC (nothing reaches your apps)
  hop-agent install    (as administrator) run as a Windows service, so the Mac also works on
                       the lock screen, UAC prompts and admin windows such as Task Manager;
                       keeps the current pairing and starts with Windows
  hop-agent uninstall  (as administrator) remove the service

Options:
  --port N             UDP/TCP port (default {DEFAULT_PORT})
  --name NAME          name shown to the Mac (default: computer name)
  --stats              print memory and CPU every 10 seconds
"
    )
}

enum Command {
    Run(Role, RunOptions),
    /// Show a pairing code. `keep_open`: wait for Enter before exiting (when the tray opened a
    /// console window just for this).
    Pair { opts: RunOptions, keep_open: bool },
    /// The screen-view server, started by the agent in its own process.
    Video { role: Role, port: u16, file: Option<std::path::PathBuf> },
    Unpair,
    Windows(WinCommand),
}

/// Commands that only make sense on Windows.
enum WinCommand {
    Selftest,
    Install,
    Uninstall,
    /// Started by the Service Control Manager.
    Service,
}

enum ParseError {
    Help,
    Bad(String),
}

impl From<&str> for ParseError {
    fn from(s: &str) -> Self {
        ParseError::Bad(s.into())
    }
}

/// Command words, each once.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Word {
    Run,
    Pair,
    Unpair,
    Selftest,
    Install,
    Uninstall,
    Service,
    Helper,
    Video,
}

const WORDS: [(&str, Word); 9] = [
    ("run", Word::Run),
    ("pair", Word::Pair),
    ("unpair", Word::Unpair),
    ("selftest", Word::Selftest),
    ("install", Word::Install),
    ("uninstall", Word::Uninstall),
    ("service", Word::Service),
    (HELPER_COMMAND, Word::Helper),
    ("video", Word::Video),
];

fn parse() -> Result<Command, ParseError> {
    let mut opts = RunOptions {
        port: DEFAULT_PORT,
        name: None,
        pair: false,
        dry_run: !cfg!(windows),
        swallow: false,
        stats: false,
        stop_event: None,
        video_file: None,
    };
    let (mut for_service, mut file) = (false, None);
    let (mut word, mut run_flags, mut keep_open) = (Word::Run, false, false);
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        if let Some(&(_, w)) = WORDS.iter().find(|(name, _)| *name == x) {
            word = w;
            continue;
        }
        match x.as_str() {
            "--port" => (opts.port, run_flags) = (it.next().and_then(|v| v.parse().ok()).ok_or("--port needs a number")?, true),
            "--name" => (opts.name, run_flags) = (Some(it.next().ok_or("--name needs a value")?), true),
            "--stats" => (opts.stats, run_flags) = (true, true),
            // Development and testing:
            "--dry-run" => (opts.dry_run, run_flags) = (true, true),
            "--swallow" => (opts.swallow, run_flags) = (true, true),
            // Passed by the tray, which opens a window just for pairing:
            "--keep-open" => keep_open = true,
            // `video` (started by the agent itself):
            "--for-service" => for_service = true,
            "--file" => file = Some(std::path::PathBuf::from(it.next().ok_or("--file needs a path")?)),
            // Development: serve a recording as the screen.
            "--video-file" => (opts.video_file, run_flags) = (Some(std::path::PathBuf::from(it.next().ok_or("--video-file needs a path")?)), true),
            // Passed by the service to its helper:
            STOP_EVENT_FLAG => opts.stop_event = Some(it.next().ok_or("--stop-event needs a name")?),
            "-h" | "--help" | "help" => return Err(ParseError::Help),
            _ => return Err(ParseError::Bad(format!("unknown argument {x:?}"))),
        }
    }
    let name = WORDS.iter().find(|(_, w)| *w == word).map_or("run", |(n, _)| n);
    if (for_service || file.is_some()) && word != Word::Video {
        return Err(ParseError::Bad("--for-service and --file are only for `hop-agent video`".into()));
    }
    if run_flags && !matches!(word, Word::Run | Word::Pair | Word::Helper | Word::Video) {
        return Err(ParseError::Bad(format!("`hop-agent {name}` takes no options")));
    }
    if keep_open && word != Word::Pair {
        return Err(ParseError::Bad("--keep-open is only for `hop-agent pair`".into()));
    }
    if opts.stop_event.is_some() != (word == Word::Helper) {
        return Err(ParseError::Bad(format!("{STOP_EVENT_FLAG} is only for `{HELPER_COMMAND}`, which the Hop service starts")));
    }
    Ok(match word {
        Word::Run => Command::Run(Role::Console, opts),
        Word::Pair => Command::Pair { opts: RunOptions { pair: true, ..opts }, keep_open },
        Word::Helper => Command::Run(Role::Service, opts),
        Word::Video => Command::Video { role: if for_service { Role::Service } else { Role::Console }, port: opts.port, file },
        Word::Unpair => Command::Unpair,
        Word::Selftest => Command::Windows(WinCommand::Selftest),
        Word::Install => Command::Windows(WinCommand::Install),
        Word::Uninstall => Command::Windows(WinCommand::Uninstall),
        Word::Service => Command::Windows(WinCommand::Service),
    })
}

/// With the Windows service installed, pair beside it (into its store); otherwise run the agent
/// with a fresh code.
fn pair(opts: RunOptions) -> Result<(), String> {
    #[cfg(windows)]
    if scm::service_state()?.is_some() {
        return service::pair_beside_service(opts.name, opts.port);
    }
    role::run(Role::Console, opts)
}

fn unpair() -> Result<(), String> {
    let forgot = Role::Console.store().forget_peer().map_err(|e| e.to_string())?;
    println!("{}", if forgot { "Forgot the paired Mac." } else { "No Mac was paired." });
    #[cfg(windows)]
    if service::revoke_service_pairing()? {
        println!("The Hop service forgot it too and restarted.");
    }
    Ok(())
}

#[cfg(windows)]
fn windows_only(cmd: WinCommand) -> Result<(), String> {
    match cmd {
        WinCommand::Selftest => {
            if selftest::run() {
                Ok(())
            } else {
                Err("selftest failed".into())
            }
        }
        WinCommand::Install => service::install(&Role::Console.store()),
        WinCommand::Uninstall => service::uninstall(),
        WinCommand::Service => {
            service::run_service();
            Ok(())
        }
    }
}

#[cfg(not(windows))]
fn windows_only(_: WinCommand) -> Result<(), String> {
    Err("this command runs on Windows only".into())
}

fn main() {
    let cmd = match parse() {
        Ok(c) => c,
        Err(ParseError::Help) => {
            print!("{}", usage());
            std::process::exit(0);
        }
        Err(ParseError::Bad(e)) => {
            eprint!("error: {e}\n\n{}", usage());
            std::process::exit(2);
        }
    };
    let mut keep_window = false;
    let result = match cmd {
        Command::Run(role, opts) => role::run(role, opts),
        Command::Pair { opts, keep_open } => {
            keep_window = keep_open;
            pair(opts)
        }
        Command::Video { role, port, file } => role::run_video(role, port, file),
        Command::Unpair => unpair(),
        Command::Windows(w) => windows_only(w),
    };
    if let Err(e) = &result {
        eprintln!("error: {e}");
    }
    if keep_window {
        println!("\nPress Enter to close this window.");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}

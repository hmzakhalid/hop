//! The agent's command line: a command word, then that command's own options.

use crate::role::{Role, RunOptions, HELPER_COMMAND, STOP_EVENT_FLAG};
use crate::video::{self, VideoArgs};
use hop_core::wire::DEFAULT_PORT;
use std::path::PathBuf;

pub fn usage() -> String {
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
                       starts with Windows (a first install takes this PC's pairing)
  hop-agent uninstall  (as administrator) remove the service

Options:
  --port N             UDP/TCP port (default {DEFAULT_PORT})
  --name NAME          name shown to the Mac (default: computer name)
  --stats              print memory and CPU every {} seconds
",
        crate::role::STATS_EVERY.as_secs()
    )
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(Role, RunOptions),
    /// Show a pairing code. `keep_open`: wait for Enter before exiting (when the tray opened a
    /// console window just for this).
    Pair { opts: RunOptions, keep_open: bool },
    /// The screen-view server, started by the agent in its own process.
    Video(VideoArgs),
    Unpair,
    Windows(WinCommand),
}

/// Commands that only make sense on Windows.
#[derive(Debug, PartialEq, Eq)]
pub enum WinCommand {
    Selftest,
    Install,
    Uninstall,
    /// Started by the Service Control Manager.
    Service,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    Help,
    Bad(String),
}

/// Passed by the tray (and the installer), which open a console window just for pairing.
const KEEP_OPEN_FLAG: &str = "--keep-open";

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
    (video::COMMAND, Word::Video),
];

/// Parse the arguments after the program name: the command word (anywhere; `run` if none),
/// then only the options that command takes.
/// Options followed by a value (which is never a command word or a request for help).
const TAKES_VALUE: [&str; 5] = ["--port", "--name", "--video-file", STOP_EVENT_FLAG, "--file"];

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, ParseError> {
    let mut args: Vec<String> = args.into_iter().collect();
    // The command word and help are found among the tokens that are not option values.
    let mut word = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if matches!(a, "-h" | "--help" | "help") {
            return Err(ParseError::Help);
        }
        if TAKES_VALUE.contains(&a) {
            i += 2;
            continue;
        }
        if let Some(&(_, w)) = WORDS.iter().find(|(name, _)| *name == a).filter(|_| word.is_none()) {
            word = Some(w);
            args.remove(i);
            continue;
        }
        i += 1;
    }
    let word = word.unwrap_or(Word::Run);
    let name = WORDS.iter().find(|(_, w)| *w == word).map_or("run", |(n, _)| n);
    let mut opts = RunOptions {
        port: DEFAULT_PORT,
        name: None,
        pair: false,
        // Only Windows can inject: elsewhere the agent can only print what it would do.
        dry_run: !cfg!(windows),
        swallow: false,
        stats: false,
        stop_event: None,
        video_file: None,
    };
    let (mut keep_open, mut for_service, mut file) = (false, false, None);
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let mut value = |what: &str| it.next().ok_or_else(|| ParseError::Bad(format!("{flag} needs {what}")));
        let run_like = matches!(word, Word::Run | Word::Pair | Word::Helper);
        match flag.as_str() {
            "--port" if run_like || word == Word::Video => opts.port = value("a number")?.parse().map_err(|_| ParseError::Bad("--port needs a number".into()))?,
            "--name" if run_like => opts.name = Some(value("a value")?),
            "--stats" if run_like => opts.stats = true,
            // Development and testing:
            "--dry-run" if run_like => opts.dry_run = true,
            "--swallow" if run_like => opts.swallow = true,
            "--video-file" if run_like => opts.video_file = Some(PathBuf::from(value("a path")?)),
            KEEP_OPEN_FLAG if word == Word::Pair => keep_open = true,
            STOP_EVENT_FLAG if word == Word::Helper => opts.stop_event = Some(value("a name")?),
            "--for-service" if word == Word::Video => for_service = true,
            "--file" if word == Word::Video => file = Some(PathBuf::from(value("a path")?)),
            _ if flag.starts_with('-') => return Err(ParseError::Bad(format!("`hop-agent {name}` does not take {flag}"))),
            _ => return Err(ParseError::Bad(format!("unknown argument {flag:?}"))),
        }
    }
    Ok(match word {
        Word::Run => Command::Run(Role::Console, opts),
        Word::Pair => Command::Pair { opts: RunOptions { pair: true, ..opts }, keep_open },
        Word::Helper => Command::Run(Role::Service, opts),
        Word::Video => Command::Video(VideoArgs { port: opts.port, for_service, file }),
        Word::Unpair => Command::Unpair,
        Word::Selftest => Command::Windows(WinCommand::Selftest),
        Word::Install => Command::Windows(WinCommand::Install),
        Word::Uninstall => Command::Windows(WinCommand::Uninstall),
        Word::Service => Command::Windows(WinCommand::Service),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(s: &str) -> Result<Command, ParseError> {
        parse(s.split_whitespace().map(String::from))
    }

    #[test]
    fn the_video_command_line_the_agent_builds_parses_back() {
        for args in [
            VideoArgs { port: 47820, for_service: false, file: None },
            VideoArgs { port: 5000, for_service: true, file: Some(PathBuf::from("/tmp/a b.h264")) },
            VideoArgs { port: 1, for_service: false, file: Some(PathBuf::from("help")) },
        ] {
            let line = args.to_args().into_iter().map(|a| a.into_string().unwrap());
            assert_eq!(parse(line), Ok(Command::Video(args)));
        }
    }

    #[test]
    fn the_service_helper_command_line_parses_back() {
        match parse_str(&format!("{HELPER_COMMAND} {STOP_EVENT_FLAG} Global\\hop-stop-1")) {
            Ok(Command::Run(Role::Service, o)) => assert_eq!(o.stop_event.as_deref(), Some("Global\\hop-stop-1")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn each_command_takes_only_its_own_options() {
        assert!(matches!(parse_str("--stats --port 5000"), Ok(Command::Run(Role::Console, o)) if o.stats && o.port == 5000));
        assert!(matches!(parse_str(&format!("pair {KEEP_OPEN_FLAG}")), Ok(Command::Pair { keep_open: true, .. })));
        for bad in [
            "video --name x",
            "video --stats",
            "video --dry-run",
            "install --port 5000",
            "unpair --stats",
            "run --keep-open",
            "run --stop-event x",
            "pair --for-service",
            "run --port",
            "run --port many",
            "frobnicate",
        ] {
            assert!(matches!(parse_str(bad), Err(ParseError::Bad(_))), "{bad} must be refused");
        }
        assert_eq!(parse_str("pair --help"), Err(ParseError::Help));
        assert!(matches!(parse_str("--name video"), Ok(Command::Run(_, o)) if o.name.as_deref() == Some("video")), "an option's value is not a command");
        assert!(matches!(parse_str("--name help"), Ok(Command::Run(_, o)) if o.name.as_deref() == Some("help")));
        assert!(matches!(parse_str("--port 5000 pair"), Ok(Command::Pair { opts, .. }) if opts.port == 5000), "the command word may come later");
    }
}

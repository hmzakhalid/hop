//! Command line: each command and only the options it understands.

use hop_core::controller::ControllerConfig;
#[cfg(any(feature = "dev-tools", test))]
use hop_core::layout::Side;
use std::io::{BufRead, Write};

const USAGE: &str = "\
hop: use this Mac's keyboard and trackpad on a Windows PC.

Usage:
  hop app                the menu-bar app (opening Hop.app does this)
  hop pair [--host IP]   pair with a PC running hop-agent (type the code it shows)
  hop [run] [options]    start; switch to the PC and back with ⌃⌥⌘ + arrow
  hop bench [--host IP] [--count N]
                         measure the round trip to the PC
  hop view [--host IP]   watch the PC's screen in a window
  hop unpair             forget the PC

Run options:
  --host IP[:PORT]       PC address (default: find it on the network)
  --side right|left|top|bottom   where the PC's screen is (default: right)
  --edges                also switch by pushing the pointer off the screen edge, both ways
  --speed X              pointer speed multiplier (default 1.0)
  --scroll-speed X       scroll speed multiplier (default 1.0)
  --cmd-as-win           ⌘ is the Windows key and ⌃ is Ctrl (default: ⌘ is Ctrl, ⌃ is Win)
  --no-chords            do not translate ⌘Tab, ⌥←, ⌘Q, …
  --verbose              print crossings and link status
  --stats                print round trip, memory and CPU every 5 seconds

While the PC has the keyboard and mouse, a blue badge at the top of the Mac's screen says so.
Keys on the PC: ⌘ = Ctrl, ⌥ = Alt, ⌃ = Windows key (⌃L is Win+L). ⌘Space opens Start.
";

pub fn usage() -> String {
    let dev = if cfg!(feature = "dev-tools") {
        "\nDeveloper tools: hop drive [--host IP], hop view --file F.h264 (play a recording), hop badge [--side S], hop menu.\n"
    } else {
        ""
    };
    format!("{USAGE}{dev}")
}

pub struct RunOptions {
    /// Where the PC is, edges, key mapping and speeds: the same choices the app saves.
    pub cfg: ControllerConfig,
    pub verbose: bool,
    pub stats: bool,
}

/// Command words, each once.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Word {
    App,
    Run,
    Pair,
    Bench,
    Unpair,
    View,
    #[cfg(feature = "dev-tools")]
    Drive,
    #[cfg(feature = "dev-tools")]
    Badge,
    #[cfg(feature = "dev-tools")]
    Menu,
}

const WORDS: &[(&str, Word)] = &[
    ("app", Word::App),
    ("run", Word::Run),
    ("pair", Word::Pair),
    ("bench", Word::Bench),
    ("unpair", Word::Unpair),
    ("view", Word::View),
    #[cfg(feature = "dev-tools")]
    ("drive", Word::Drive),
    #[cfg(feature = "dev-tools")]
    ("badge", Word::Badge),
    #[cfg(feature = "dev-tools")]
    ("menu", Word::Menu),
];

pub enum Command {
    App,
    Run { host: Option<String>, opts: RunOptions },
    Pair { host: Option<String> },
    Bench { host: Option<String>, count: usize },
    Unpair,
    /// A scripted session through the real controller (agent in `--swallow` mode).
    #[cfg(feature = "dev-tools")]
    Drive { host: Option<String> },
    /// Watch the PC's screen.
    View { host: Option<String> },
    /// Play a recording in the viewer (`hop view --file F`).
    #[cfg(feature = "dev-tools")]
    Play { file: String },
    /// Preview the indicator badge.
    #[cfg(feature = "dev-tools")]
    Badge { side: Side },
    /// Print the menu-bar app's menus.
    #[cfg(feature = "dev-tools")]
    Menu,
}

#[derive(Debug)]
pub enum ParseError {
    /// `--help`: print the usage and exit successfully.
    Help,
    Bad(String),
}

impl From<String> for ParseError {
    fn from(s: String) -> Self {
        ParseError::Bad(s)
    }
}

impl From<&str> for ParseError {
    fn from(s: &str) -> Self {
        ParseError::Bad(s.into())
    }
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, ParseError> {
    let mut it = args.into_iter();
    let mut word = Word::Run;
    let mut host = None;
    let mut count = None;
    #[cfg(feature = "dev-tools")]
    let mut file: Option<String> = None;
    let mut run = RunOptions { cfg: ControllerConfig::default(), verbose: false, stats: false };
    let (mut run_flags, mut side_given) = (false, false);
    let number = |v: Option<String>, name: &str| -> Result<f64, String> {
        v.and_then(|s| s.parse().ok()).filter(|x: &f64| *x > 0.0 && *x < 20.0).ok_or(format!("{name} needs a positive number"))
    };
    while let Some(a) = it.next() {
        if let Some(&(_, w)) = WORDS.iter().find(|(name, _)| *name == a) {
            word = w;
            continue;
        }
        match a.as_str() {
            "--host" => host = Some(it.next().ok_or("--host needs an address")?),
            "--count" => count = Some(it.next().and_then(|v| v.parse().ok()).filter(|n| *n >= 10).ok_or("--count needs a number of at least 10")?),
            #[cfg(feature = "dev-tools")]
            "--file" => file = Some(it.next().ok_or("--file needs a path")?),
            "--side" => (run.cfg.side, side_given) = (it.next().ok_or("--side needs a value")?.parse()?, true),
            "--edges" => (run.cfg.edges, run_flags) = (true, true),
            "--speed" => (run.cfg.speed, run_flags) = (number(it.next(), "--speed")?, true),
            "--scroll-speed" => (run.cfg.scroll_speed, run_flags) = (number(it.next(), "--scroll-speed")?, true),
            "--cmd-as-win" => (run.cfg.map.cmd_as_win, run_flags) = (true, true),
            "--no-chords" => (run.cfg.map.chords, run_flags) = (false, true),
            "--verbose" | "-v" => (run.verbose, run_flags) = (true, true),
            "--stats" => (run.stats, run_flags) = (true, true),
            "-h" | "--help" | "help" => return Err(ParseError::Help),
            _ => return Err(format!("unknown argument {a:?}").into()),
        }
    }
    let name = WORDS.iter().find(|(_, w)| *w == word).map_or("run", |(n, _)| n);
    let only = |ok: bool, what: &str| if ok { Ok(()) } else { Err(format!("`hop {name}` does not take {what}")) };
    only(name == "bench" || count.is_none(), "--count")?;
    #[cfg(feature = "dev-tools")]
    only(name == "view" || file.is_none(), "--file")?;
    only(name == "run" || !run_flags, "run options")?;
    only(matches!(name, "run" | "badge") || !side_given, "--side")?;
    only(!matches!(name, "app" | "unpair" | "badge") || host.is_none(), "--host")?;
    Ok(match word {
        Word::App => Command::App,
        Word::Run => Command::Run { host, opts: run },
        Word::Pair => Command::Pair { host },
        Word::Bench => Command::Bench { host, count: count.unwrap_or(2000) },
        Word::Unpair => Command::Unpair,
        #[cfg(feature = "dev-tools")]
        Word::Drive => Command::Drive { host },
        #[cfg(feature = "dev-tools")]
        Word::View if file.is_some() => Command::Play { file: file.unwrap() },
        Word::View => Command::View { host },
        #[cfg(feature = "dev-tools")]
        Word::Badge => Command::Badge { side: run.cfg.side },
        #[cfg(feature = "dev-tools")]
        Word::Menu => Command::Menu,
    })
}

pub fn prompt(q: &str) -> String {
    print!("{q}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Command, ParseError> {
        parse(s.split_whitespace().map(String::from))
    }

    #[test]
    fn commands_take_only_their_own_options() {
        assert!(matches!(p("").unwrap(), Command::Run { host: None, .. }));
        assert!(matches!(p("--side left --edges").unwrap(), Command::Run { opts: RunOptions { cfg: ControllerConfig { side: Side::Left, edges: true, .. }, .. }, .. }));
        assert!(matches!(p("bench --count 50").unwrap(), Command::Bench { count: 50, .. }));
        assert!(p("run --count 5").is_err());
        assert!(p("pair --edges").is_err());
        assert!(p("unpair --host 1.2.3.4").is_err());
        assert!(matches!(p("view").unwrap(), Command::View { host: None }));
        assert!(p("--speed 0").is_err());
        #[cfg(feature = "dev-tools")]
        assert!(matches!(p("badge --side left").unwrap(), Command::Badge { side: Side::Left }));
        assert!(p("badge --edges").is_err());
        assert!(p("app --host 1.2.3.4").is_err());
        assert!(matches!(p("--help"), Err(ParseError::Help)));
    }
}

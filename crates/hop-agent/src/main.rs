//! hop-agent: runs on the Windows PC and receives keyboard and mouse input from a paired Mac.
//!
//! Off Windows it builds with a dry-run injector that prints what it would do.

mod asker;
mod backoff;
mod cli;
mod dryrun;
#[cfg(any(windows, test))]
mod lock;
mod platform;
mod role;
mod video;
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

use cli::{Command, ParseError, WinCommand};
use role::{Role, RunOptions};

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
    let forgot = Role::Console.store().lock().and_then(|mut l| l.forget_peer()).map_err(|e| e.to_string())?;
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
    let cmd = match cli::parse(std::env::args().skip(1)) {
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
    let mut keep_window = false;
    let result = match cmd {
        Command::Run(role, opts) => role::run(role, opts),
        Command::Pair { opts, keep_open } => {
            keep_window = keep_open;
            pair(opts)
        }
        Command::Video(args) => video::run(args),
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

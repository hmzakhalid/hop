//! Screen view on the PC: `hop-agent video`, the server in a process of its own so a fault in
//! capture or in a GPU driver can never take input down with it. The agent starts it and keeps
//! it running; this module owns its command line, its supervision and what it runs.

use crate::backoff::Backoff;
use crate::role::Role;
use std::ffi::OsString;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

pub const COMMAND: &str = "video";

/// What `hop-agent video` is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoArgs {
    /// The input port; video uses the next one.
    pub port: u16,
    /// Started by the service helper: follow input onto the lock screen.
    pub for_service: bool,
    /// Development: serve this recording instead of the screen.
    pub file: Option<PathBuf>,
}

impl VideoArgs {
    /// The command line that starts it (after the program name); [`crate::cli::parse`] reads it.
    pub fn to_args(&self) -> Vec<OsString> {
        let mut a: Vec<OsString> = vec![COMMAND.into(), "--port".into(), self.port.to_string().into()];
        if self.for_service {
            a.push("--for-service".into());
        }
        if let Some(f) = &self.file {
            a.push("--file".into());
            a.push(f.into());
        }
        a
    }

    fn role(&self) -> Role {
        if self.for_service {
            Role::Service
        } else {
            Role::Console
        }
    }
}

/// Start `hop-agent video` and keep it running while the agent runs, relaunching with a growing
/// delay if it exits. Everything it prints, and why it stops, goes to `say`. On Windows it always
/// runs (capturing the screen); elsewhere only to replay a recording, for testing.
pub fn supervise(args: VideoArgs, say: impl Fn(&str) + Send + 'static, stop: Arc<AtomicBool>) {
    if !cfg!(windows) && args.file.is_none() {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return say(&format!("video: cannot find this program to start the video process: {e}")),
    };
    std::thread::spawn(move || {
        let mut backoff = Backoff::new();
        while !stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            let mut cmd = std::process::Command::new(&exe);
            cmd.args(args.to_args()).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
            match crate::platform::spawn_tied(&mut cmd) {
                Ok(mut child) => {
                    // Its errors come on stderr: read them alongside, so a full pipe never blocks it.
                    let errors = child.stderr.take().map(|e| std::thread::spawn(move || std::io::read_to_string(e).unwrap_or_default()));
                    if let Some(out) = child.stdout.take() {
                        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                            say(&line);
                        }
                    }
                    let status = child.wait();
                    let why = errors.and_then(|t| t.join().ok()).unwrap_or_default();
                    say(&format!("video: the video process exited ({status:?}) {}", why.trim()));
                }
                Err(e) => say(&format!("video: cannot start the video process: {e}")),
            }
            let ran = if stop.load(Ordering::Relaxed) { break } else { started.elapsed() };
            std::thread::sleep(backoff.failed_after(ran));
        }
    });
}

/// `hop-agent video`: serve the screen until the agent that started it stops.
pub fn run(args: VideoArgs) -> Result<(), String> {
    use hop_core::stream::{self, FileSource, Source};
    let role = args.role();
    let store = role.store();
    // Never create a key here: the agent made it before starting us, and a second one would
    // not be the key the Mac paired with.
    let keys = store.existing_keys().map_err(|e| format!("cannot read the key in {}: {e}", store.dir.display()))?;
    let video_port = hop_core::video::port_for(args.port).ok_or("the port is too high")?;
    let sock = std::net::UdpSocket::bind(std::net::SocketAddr::from(([0, 0, 0, 0], video_port))).map_err(|e| format!("video port {video_port}: {e}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    if role == Role::Console {
        crate::platform::on_ctrl_c(stop.clone());
    }
    crate::platform::stop_with_parent(stop.clone());
    let open = |bitrate| -> std::io::Result<Box<dyn Source>> {
        match &args.file {
            Some(f) => Ok(Box::new(FileSource::open(f)?)),
            None => screen_source(role == Role::Service, bitrate),
        }
    };
    // stdout is the agent's pipe (see `supervise`).
    let say = |m: &str| {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{m}");
        let _ = out.flush();
    };
    say(&format!("video: serving on port {video_port}"));
    // The pairing is read again for every handshake, so a new pairing applies at once.
    stream::serve(sock, &keys, || store.peer().map(|p| p.public), open, &stop, say).map_err(|e| e.to_string())
}

/// The screen, captured and encoded at `bitrate` (bits per second) and up to the video channel's
/// frame rate.
#[cfg(windows)]
fn screen_source(follow_input_desktop: bool, bitrate: u32) -> std::io::Result<Box<dyn hop_core::stream::Source>> {
    use hop_video::capture::Settings;
    let s = hop_video::source::ScreenSource::open(&Settings {
        // The primary monitor: its pixels are the PC's desktop coordinates, which the Mac's
        // cursor overlay relies on.
        monitor: 0,
        fps: hop_core::video::MAX_FPS,
        bitrate,
        hardware: true,
        follow_input_desktop,
        hevc: true,
    })?;
    println!("video: capturing {}", s.describe());
    Ok(Box::new(s))
}

#[cfg(not(windows))]
fn screen_source(_: bool, _: u32) -> std::io::Result<Box<dyn hop_core::stream::Source>> {
    Err(std::io::Error::other("screen capture needs Windows; use --file to replay a recording"))
}

//! How the agent runs: in a console the user started, or as the background helper of the
//! Windows service. Everything that differs between the two is decided here.

use hop_core::agent::{self, AgentConfig, AgentEvent, PairingWindow, Platform, PAIRING_MAX_FAILURES, PAIRING_TTL};
use hop_core::crypto::{fingerprint, new_pairing_code};
use hop_core::store::{AgentStore, MacRecord};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The command line the service starts its helper with: `hop-agent system-helper --stop-event NAME`.
pub const HELPER_COMMAND: &str = "system-helper";
pub const STOP_EVENT_FLAG: &str = "--stop-event";
/// How often `--stats` prints.
pub const STATS_EVERY: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq, Eq)]
pub struct RunOptions {
    pub port: u16,
    pub name: Option<String>,
    /// Offer a fresh pairing code even if a Mac is already paired.
    pub pair: bool,
    pub dry_run: bool,
    pub swallow: bool,
    pub stats: bool,
    /// Service helper only: the event the service signals to stop us.
    pub stop_event: Option<String>,
    /// Development: serve this recording as the screen (any OS) instead of capturing.
    pub video_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Console,
    /// The SYSTEM helper the service keeps in the user's session.
    Service,
}

impl Role {
    /// Keys and pairing. The service's copy is protected (see `service::install`).
    pub fn store(self) -> AgentStore {
        AgentStore::new(match self {
            Role::Console => user_dir(),
            Role::Service => system_data_dir(),
        })
    }

    /// The service helper has no console and nobody to show a pairing code to.
    fn offers_pairing(self) -> bool {
        self == Role::Console
    }

    /// Where the helper writes its messages; the console role prints them.
    pub fn log_path(self) -> Option<PathBuf> {
        match self {
            Role::Console => None,
            Role::Service => Some(self.store().dir.join("agent.log")),
        }
    }
}

/// The service's keys, pairing and logs: `%ProgramData%\Hop`. `hop-agent install` creates it
/// readable and writable by SYSTEM and Administrators only.
pub fn system_data_dir() -> PathBuf {
    if cfg!(windows) {
        let pd = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\ProgramData"));
        pd.join("Hop")
    } else {
        user_dir().join("service")
    }
}

/// Logs start over once they pass this size.
const LOG_LIMIT: u64 = 1 << 20;

/// Open a log for appending, starting it over if it has grown past [`LOG_LIMIT`].
fn open_log(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let big = std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_LIMIT);
    std::fs::OpenOptions::new().create(true).append(!big).write(true).truncate(big).open(path)
}

fn stamp() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Append a line to the service's own log (the helper has no console).
pub fn append_service_log(msg: &str) {
    if let Ok(mut f) = open_log(&system_data_dir().join("service.log")) {
        let _ = writeln!(f, "[{}] {msg}", stamp());
    }
}

pub fn user_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("HOP_AGENT_DIR") {
        return PathBuf::from(d);
    }
    #[cfg(windows)]
    if let Some(d) = std::env::var_os("APPDATA") {
        return PathBuf::from(d).join("Hop");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("hop-agent")
}

/// Where messages go: the console, or `agent.log` for the service helper.
pub struct Log(Option<Mutex<std::fs::File>>);

impl Log {
    fn for_role(role: Role) -> Log {
        let Some(path) = role.log_path() else { return Log(None) };
        match open_log(&path) {
            Ok(f) => Log(Some(Mutex::new(f))),
            Err(e) => {
                append_service_log(&format!("helper cannot write {}: {e}", path.display()));
                Log(None)
            }
        }
    }

    pub fn say(&self, msg: impl AsRef<str>) {
        match &self.0 {
            None => println!("{}", msg.as_ref()),
            Some(f) => {
                let _ = writeln!(f.lock().unwrap(), "[{}] {}", stamp(), msg.as_ref());
            }
        }
    }
}

pub fn computer_name() -> String {
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "Windows PC".into())
}

fn lan_address() -> Option<String> {
    // The address the OS would use to reach the LAN (connecting a UDP socket sends nothing).
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.168.0.1:9").ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

/// How a pairing code is shown to the user.
pub fn pairing_banner(code: &str) -> String {
    use hop_core::ui::MAC_PAIR_ITEM;
    format!(
        "\n  Pairing code:  {code}\n\n  On the Mac, choose \"{MAC_PAIR_ITEM}\" from the Hop menu (or run: hop pair)\n  and type the code. It works once, expires in {} minutes,\n  and closes after {PAIRING_MAX_FAILURES} wrong tries.\n",
        PAIRING_TTL.as_secs() / 60
    )
}

/// Stop on Ctrl+C or window close (console), or when the service says so. The helper must be
/// able to hear the service: if it could only be terminated, it could not release held keys.
fn stop_signal(role: Role, stop_event: Option<String>) -> Result<Arc<AtomicBool>, String> {
    let stop = Arc::new(AtomicBool::new(false));
    match role {
        Role::Console => crate::platform::on_ctrl_c(stop.clone()),
        Role::Service => {
            let name = stop_event.ok_or("the helper needs the service's stop event")?;
            crate::platform::on_service_stop(stop.clone(), &name)?;
        }
    }
    Ok(stop)
}

pub fn run(role: Role, opts: RunOptions) -> Result<(), String> {
    let store = role.store();
    if role == Role::Service && !store.dir.is_dir() {
        // The service creates it protected; never create it here without that protection.
        let why = format!("{} is missing; restart the Hop service or run hop-agent install again", store.dir.display());
        append_service_log(&why);
        return Err(why);
    }
    let log = Arc::new(Log::for_role(role));
    let result = serve(role, opts, store, &log);
    if let (Role::Service, Err(e)) = (role, &result) {
        log.say(format!("stopped: {e}")); // the helper has no console
    }
    result
}

fn serve(role: Role, opts: RunOptions, store: AgentStore, log: &Arc<Log>) -> Result<(), String> {
    let keys = store.keys().map_err(|e| format!("cannot read or create the key in {}: {e}", store.dir.display()))?;
    let peer = store.peer();
    let name = opts.name.clone().unwrap_or_else(computer_name);
    let mode = if role == Role::Service { "  ·  service" } else { "" };
    log.say(format!("hop-agent {} (build {})  ·  {name}  ·  key {}{mode}", env!("CARGO_PKG_VERSION"), hop_core::BUILD, fingerprint(&keys.public)));
    if let Some(p) = &peer {
        log.say(format!("Paired with: {}", p.name));
    }
    if let Some(ip) = lan_address() {
        log.say(format!("Address: {ip}:{}", opts.port));
    }
    let pairing = if role.offers_pairing() && (opts.pair || peer.is_none()) {
        let code = new_pairing_code();
        log.say(pairing_banner(&code));
        Some(PairingWindow::new(code))
    } else {
        if peer.is_none() {
            log.say("Not paired. Pair once without the service (hop-agent pair), then run hop-agent install again.");
        }
        None
    };

    let stop = stop_signal(role, opts.stop_event.clone())?;
    if opts.stats {
        let log = log.clone();
        std::thread::spawn(move || {
            let mut last = crate::platform::usage().cpu_seconds;
            loop {
                std::thread::sleep(STATS_EVERY);
                let u = crate::platform::usage();
                log.say(format!(
                    "stats: memory {:.1} MB (private {:.1} MB) · cpu {:.2}%",
                    u.memory_mb,
                    u.private_mb,
                    (u.cpu_seconds - last) / STATS_EVERY.as_secs_f64() * 100.0
                ));
                last = u.cpu_seconds;
            }
        });
    }

    let video_log = log.clone();
    let video = crate::video::VideoArgs { port: opts.port, for_service: role == Role::Service, file: opts.video_file.clone() };
    crate::video::supervise(video, move |m| video_log.say(m), stop.clone());

    let cfg = AgentConfig {
        bind: ([0, 0, 0, 0], opts.port).into(),
        name,
        keys,
        paired: peer.map(|p| p.public),
        pairing,
    };
    let ev_log = log.clone();
    let events = move |e: AgentEvent| match e {
        AgentEvent::Listening(a) => ev_log.say(format!("Listening on {a}.")),
        AgentEvent::Paired(p) => {
            let rec = MacRecord { public: p.public, name: p.name.clone() };
            match store.lock().and_then(|l| l.save_peer(&rec)) {
                Ok(()) => ev_log.say(format!("Paired with {} (key {}).", p.name, fingerprint(&p.public))),
                Err(e) => ev_log.say(format!("Paired with {}, but saving failed: {e}", p.name)),
            }
        }
        AgentEvent::PairingFailed(why) => ev_log.say(format!("Pairing attempt failed: {why}")),
        AgentEvent::SessionStarted(a) => ev_log.say(format!("Mac connected from {a}.")),
    };

    let mut platform: Box<dyn Platform> = if opts.dry_run {
        log.say("Dry run: input is printed, not injected.");
        Box::new(crate::dryrun::DryRun::default())
    } else {
        let p = crate::platform::injector(role == Role::Service, opts.swallow)?;
        if opts.swallow {
            log.say("Swallow mode: injected input is caught before apps see it and printed here.");
        }
        p
    };
    agent::run(cfg, platform.as_mut(), stop, events).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            format!("{e}. Is another hop-agent already running? (The service counts: hop-agent uninstall stops it.)")
        } else {
            e.to_string()
        }
    })
}

//! The paired PC: pairing, forgetting, finding it and connecting to it. The functions here
//! return errors as sentences for the user; the CLI wrappers at the bottom print them and exit.

use crate::cli::{die, prompt};
use crate::macos;
use hop_core::crypto::fingerprint;
use hop_core::link::{self, Found, Link};
use hop_core::store::{read_kv, write_kv, MacStore, PcRecord};
use std::collections::BTreeMap;
use hop_core::wire::DEFAULT_PORT;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Every change to the pairing record happens under this lock, which every Hop process shares
/// (the app and the command line): an OS file lock on `pairing.lock`, released when dropped.
///
/// Under it, `pairing.generation` holds a random token, replaced by every pairing that starts and
/// every Forget, so a pairing that finishes after a newer change (in any process) knows it was
/// superseded. The token is random and replaced atomically, so neither a crash mid-write nor a
/// lost file can ever bring back an old token.
struct PairingLock(#[allow(dead_code)] std::fs::File);

impl PairingLock {
    fn take() -> io::Result<PairingLock> {
        use std::os::fd::AsRawFd;
        let dir = store().dir;
        std::fs::create_dir_all(&dir)?;
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("pairing.lock"))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(PairingLock(f))
    }

    fn token_file() -> PathBuf {
        store().dir.join("pairing.generation")
    }

    /// The current token; `None` if missing or unreadable (never equal to an issued token).
    fn token(&self) -> Option<String> {
        read_kv(&Self::token_file()).ok().flatten()?.get("token").cloned()
    }

    /// Start a new generation; returns its token.
    fn renew(&mut self) -> io::Result<String> {
        let token = format!("{:032x}", rand_u128());
        write_kv(&Self::token_file(), &BTreeMap::from([("token", token.clone())]))?;
        Ok(token)
    }
}

fn rand_u128() -> u128 {
    let mut b = [0u8; 16];
    let r = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
    // Without randomness, fall back to the clock: still unique for this purpose.
    if r.is_err() {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        return t.as_nanos() ^ ((std::process::id() as u128) << 64);
    }
    u128::from_le_bytes(b)
}

pub struct Pc {
    pub link: Arc<Link>,
    pub name: String,
}

/// This Mac's key and pairing (`HOP_DIR` overrides the folder, for tests).
pub fn store() -> MacStore {
    let dir = std::env::var_os("HOP_DIR").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        home.join("Library").join("Application Support").join("Hop")
    });
    MacStore::new(dir)
}

/// PCs running hop-agent on this network.
pub fn discover(wait: Duration) -> Result<Vec<Found>, String> {
    link::discover(DEFAULT_PORT, &macos::broadcast_addrs(), wait).map_err(|e| format!("could not search the network: {e}"))
}

/// Where the paired PC is now: found by its key on the network, else where it answered last.
pub fn locate(rec: &PcRecord, wait: Duration) -> Option<SocketAddr> {
    let found = discover(wait).unwrap_or_default();
    found.into_iter().find(|f| f.public == rec.public).map(|f| f.addr).or(rec.addr)
}

/// Pair with the PC at `addr` using the code it shows, and remember it.
pub fn pair_with(addr: SocketAddr, code: &str) -> Result<PcRecord, String> {
    let saving = |e: io::Error| format!("Could not save the pairing: {e}");
    let generation = PairingLock::take().and_then(|mut l| l.renew()).map_err(saving)?; // unlocked during the handshake
    let store = store();
    let keys = store.keys().map_err(|e| format!("cannot read or create this Mac's key in {}: {e}", store.dir.display()))?;
    let p = link::pair(addr, &keys, code, &macos::host_name()).map_err(|e| match e.kind() {
        io::ErrorKind::PermissionDenied => "That code is not right. Show a fresh one on the PC and try again.".to_string(),
        io::ErrorKind::ConnectionRefused => "The PC is not accepting pairing right now. Choose \"Pair a Mac\" on the PC first.".to_string(),
        io::ErrorKind::InvalidInput => "A code is 12 letters and digits, like 7K2P-9QXM-4D8T.".to_string(),
        _ => format!("Pairing failed: {e}"),
    })?;
    let rec = PcRecord { public: p.public, name: p.name, addr: Some(addr) };
    let lock = PairingLock::take().map_err(saving)?;
    if lock.token().as_deref() != Some(generation.as_str()) {
        return Err("A newer pairing or Forget replaced this one. Pair again if you still want this PC.".into());
    }
    store.save_peer(&rec).map_err(saving)?;
    Ok(rec)
}

/// Forget the paired PC. `Ok(false)` if there was none.
pub fn forget() -> io::Result<bool> {
    let mut lock = PairingLock::take()?;
    lock.renew()?;
    store().forget_peer()
}

/// Whether the PC with this key is still the paired one.
pub fn still_paired(public: &[u8; 32]) -> bool {
    store().peer().is_some_and(|p| &p.public == public)
}

/// Connect to the paired PC at `addr`, and remember the address it answered from.
pub fn open(mut rec: PcRecord, addr: SocketAddr, timeout: Duration) -> Result<Pc, String> {
    let store = store();
    let keys = store.keys().map_err(|e| format!("cannot read this Mac's key in {}: {e}", store.dir.display()))?;
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    macos::set_voice_priority(&sock);
    let link = Arc::new(Link::new(sock, keys, rec.public, addr).map_err(|e| e.to_string())?);
    link.connect(timeout).map_err(|e| {
        format!("{} at {} did not answer ({e}). Is Hop running there, and is the firewall allowing it?", rec.name, addr.ip())
    })?;
    rec.addr = Some(link.peer());
    // Only a hint for next time, and only if this is still the paired PC.
    if let Ok(_lock) = PairingLock::take() {
        if still_paired(&rec.public) {
            let _ = store.save_peer(&rec);
        }
    }
    Ok(Pc { link, name: rec.name })
}

pub fn parse_host(h: &str) -> SocketAddr {
    let with_port = if h.contains(':') { h.to_string() } else { format!("{h}:{DEFAULT_PORT}") };
    with_port
        .to_socket_addrs()
        .unwrap_or_else(|e| die(format!("cannot resolve {h}: {e}")))
        .find(|a| a.is_ipv4())
        .unwrap_or_else(|| die(format!("no IPv4 address for {h}")))
}

// ---- the CLI ---------------------------------------------------------------------------------

pub fn pair(host: Option<&str>) {
    let (addr, label) = match host {
        Some(h) => (parse_host(h), h.to_string()),
        None => {
            println!("Looking for PCs running hop-agent…");
            let found = discover(Duration::from_millis(900)).unwrap_or_else(|e| die(e));
            let f = match found.len() {
                0 => die("no PC answered. Is hop-agent running there, on the same network? You can also pass --host <PC IP>."),
                1 => &found[0],
                _ => {
                    for (i, f) in found.iter().enumerate() {
                        println!("  {}. {} ({})", i + 1, f.name, f.addr.ip());
                    }
                    let pick = prompt("Which one? ").parse::<usize>().ok().filter(|n| (1..=found.len()).contains(n));
                    &found[pick.unwrap_or_else(|| die("no such PC")) - 1]
                }
            };
            (f.addr, f.name.clone())
        }
    };
    println!("Pairing with {label} at {}.", addr.ip());
    let code = prompt("Type the code shown on the PC: ");
    let rec = pair_with(addr, &code).unwrap_or_else(|e| die(e));
    println!("Paired with {} (key {}).", rec.name, fingerprint(&rec.public));
    println!("Now run: hop");
}

pub fn unpair() {
    match forget() {
        Ok(true) => println!("Forgot the PC."),
        Ok(false) => println!("Not paired."),
        Err(e) => die(e),
    }
}

/// Find the paired PC (or use `host`) and connect.
pub fn connect(host: Option<&str>) -> Pc {
    let Some(rec) = store().peer() else { die("not paired yet. Run `hop-agent` on the PC, then `hop pair` here.") };
    let addr = match host {
        Some(h) => parse_host(h),
        None => locate(&rec, Duration::from_millis(600))
            .unwrap_or_else(|| die(format!("cannot find {} on the network. Is hop-agent running? Try --host <PC IP>.", rec.name))),
    };
    open(rec, addr, Duration::from_secs(4)).unwrap_or_else(|e| die(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_late_pairing_sees_it_was_superseded() {
        let dir = std::env::temp_dir().join(format!("hop-pairing-{}", std::process::id()));
        std::env::set_var("HOP_DIR", &dir);
        let started = PairingLock::take().unwrap().renew().unwrap();
        assert_eq!(PairingLock::take().unwrap().token().as_deref(), Some(started.as_str()));
        // Another process forgets meanwhile (a separate handle on the same files).
        PairingLock::take().unwrap().renew().unwrap();
        assert_ne!(PairingLock::take().unwrap().token().as_deref(), Some(started.as_str()));
        // A lost or damaged token file never matches an issued token.
        std::fs::write(dir.join("pairing.generation"), "garbage").unwrap();
        assert_eq!(PairingLock::take().unwrap().token(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

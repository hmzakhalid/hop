//! The paired PC: pairing, forgetting, finding it and connecting to it. For the app and the
//! command line alike; errors are sentences for the user.

use crate::net;
use hop_core::crypto::PairError;
use hop_core::link::{self, Found, Link};
use hop_core::ui::PC_PAIR_ITEM;
use hop_core::store::{MacStore, PcRecord};
use hop_core::wire::DEFAULT_PORT;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How long a search for PCs listens for answers.
const DISCOVER_WAIT: Duration = Duration::from_millis(1000);
/// How long looking for the paired PC listens (it answers at once when it is there).
const LOCATE_WAIT: Duration = Duration::from_millis(600);
/// How long connecting waits for the PC to answer.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

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
pub fn discover() -> Result<Vec<Found>, String> {
    link::discover(DEFAULT_PORT, &net::broadcast_addrs(), DISCOVER_WAIT).map_err(|e| format!("could not search the network: {e}"))
}

/// Where the paired PC is now: found by its key on the network, else where it answered last.
pub fn locate(rec: &PcRecord) -> Option<SocketAddr> {
    let found = link::discover(DEFAULT_PORT, &net::broadcast_addrs(), LOCATE_WAIT).unwrap_or_default();
    found.into_iter().find(|f| f.public == rec.public).map(|f| f.addr).or(rec.addr)
}

/// Pair with the PC at `addr` using the code it shows, and remember it.
pub fn pair_with(addr: SocketAddr, code: &str) -> Result<PcRecord, String> {
    let store = store();
    let saving = |e: io::Error| format!("Could not save the pairing: {e}");
    let generation = store.lock().and_then(|mut l| l.new_generation()).map_err(saving)?; // unlocked during the handshake
    let keys = store.keys().map_err(|e| format!("cannot read or create this Mac's key in {}: {e}", store.dir.display()))?;
    let p = link::pair(addr, &keys, code, &net::host_name()).map_err(|e| match e {
        PairError::WrongCode => "That code is not right. Show a fresh one on the PC and try again.".to_string(),
        PairError::Io(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            format!("The PC is not accepting pairing right now. Choose \"{PC_PAIR_ITEM}\" on the PC first.")
        }
        PairError::NotACode | PairError::TooLong => e.to_string(),
        PairError::Io(e) => format!("Pairing failed: {e}"),
    })?;
    let rec = PcRecord { public: p.public, name: p.name, addr: Some(addr) };
    let lock = store.lock().map_err(saving)?;
    if lock.generation().as_deref() != Some(generation.as_str()) {
        return Err("A newer pairing or Forget replaced this one. Pair again if you still want this PC.".into());
    }
    lock.save_peer(&rec).map_err(saving)?;
    Ok(rec)
}

/// Forget the paired PC. `Ok(false)` if there was none.
pub fn forget() -> io::Result<bool> {
    store().lock()?.forget_peer()
}

/// Whether this Mac has never started a pairing (nor forgotten one): a first run.
pub fn never_paired() -> bool {
    store().lock().is_ok_and(|l| l.generation().is_none())
}

/// Whether the PC with this key is still the paired one.
pub fn still_paired(public: &[u8; 32]) -> bool {
    store().peer().is_some_and(|p| &p.public == public)
}

/// The paired PC and where to reach it: at `host` if given, else found on the network.
pub fn target(host: Option<&str>) -> Result<(PcRecord, SocketAddr), String> {
    let rec = store().peer().ok_or("not paired yet. Run hop-agent on the PC, then `hop pair` here.")?;
    let addr = match host {
        Some(h) => parse_host(h)?,
        None => locate(&rec).ok_or_else(|| format!("cannot find {} on the network. Is hop-agent running? Try --host <PC IP>.", rec.name))?,
    };
    Ok((rec, addr))
}

/// Find the paired PC (or use `host`) and connect.
pub fn connect(host: Option<&str>) -> Result<Pc, String> {
    let (rec, addr) = target(host)?;
    open(rec, addr)
}

/// Connect to the paired PC at `addr`, and remember the address it answered from.
pub fn open(mut rec: PcRecord, addr: SocketAddr) -> Result<Pc, String> {
    let store = store();
    let keys = store.keys().map_err(|e| format!("cannot read this Mac's key in {}: {e}", store.dir.display()))?;
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    net::set_voice_priority(&sock);
    let link = Arc::new(Link::new(sock, keys, rec.public, addr).map_err(|e| e.to_string())?);
    link.connect(CONNECT_TIMEOUT).map_err(|e| {
        format!("{} at {} did not answer ({e}). Is Hop running there, and is the firewall allowing it?", rec.name, addr.ip())
    })?;
    rec.addr = Some(link.peer());
    // Only a hint for next time, and only if this is still the paired PC.
    if let Ok(lock) = store.lock() {
        if still_paired(&rec.public) {
            let _ = lock.save_peer(&rec);
        }
    }
    Ok(Pc { link, name: rec.name })
}

/// A PC address typed by the user: `IP` or `IP:PORT`, or a name.
pub fn parse_host(h: &str) -> Result<SocketAddr, String> {
    let with_port = if h.contains(':') { h.to_string() } else { format!("{h}:{DEFAULT_PORT}") };
    with_port
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {h}: {e}"))?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| format!("no IPv4 address for {h}"))
}

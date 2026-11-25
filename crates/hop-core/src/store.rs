//! What each side keeps on disk: its static key and who it is paired with. One directory per
//! side; files are owner-only on Unix (and protected by the directory ACL on Windows).

use crate::crypto::StaticKeys;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// The paired PC, as the Mac remembers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcRecord {
    pub public: [u8; 32],
    pub name: String,
    /// Where it answered last time (discovery is tried first).
    pub addr: Option<SocketAddr>,
}

/// The paired Mac, as the PC remembers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacRecord {
    pub public: [u8; 32],
    pub name: String,
}

/// A record kept as `key = value` lines. Each side stores the other side's record, so the
/// record type also names that side's own key file.
pub trait Record: Sized {
    const FILE: &'static str;
    const KEY_FILE: &'static str;
    fn to_kv(&self) -> BTreeMap<&'static str, String>;
    fn from_kv(kv: &BTreeMap<String, String>) -> Option<Self>;
}

impl Record for PcRecord {
    const FILE: &'static str = "peer.conf";
    const KEY_FILE: &'static str = "mac.key";
    fn to_kv(&self) -> BTreeMap<&'static str, String> {
        let mut kv = BTreeMap::from([("pc_public", hex32(&self.public)), ("pc_name", self.name.clone())]);
        if let Some(a) = self.addr {
            kv.insert("pc_addr", a.to_string());
        }
        kv
    }
    fn from_kv(kv: &BTreeMap<String, String>) -> Option<Self> {
        Some(PcRecord {
            public: unhex32(kv.get("pc_public")?)?,
            name: kv.get("pc_name").cloned().unwrap_or_else(|| "PC".into()),
            addr: kv.get("pc_addr").and_then(|a| a.parse().ok()),
        })
    }
}

impl Record for MacRecord {
    const FILE: &'static str = "agent.conf";
    const KEY_FILE: &'static str = "agent.key";
    fn to_kv(&self) -> BTreeMap<&'static str, String> {
        BTreeMap::from([("mac_public", hex32(&self.public)), ("mac_name", self.name.clone())])
    }
    fn from_kv(kv: &BTreeMap<String, String>) -> Option<Self> {
        Some(MacRecord {
            public: unhex32(kv.get("mac_public")?)?,
            name: kv.get("mac_name").cloned().unwrap_or_else(|| "Mac".into()),
        })
    }
}

/// One side's directory: its key file plus the record `R` of its peer.
pub struct Store<R: Record> {
    pub dir: PathBuf,
    _record: std::marker::PhantomData<R>,
}

pub type MacStore = Store<PcRecord>;
pub type AgentStore = Store<MacRecord>;

impl<R: Record> Store<R> {
    pub fn new(dir: PathBuf) -> Self {
        Store { dir, _record: std::marker::PhantomData }
    }

    /// Take `from`'s identity and pairing, unless this store already has an identity of its own:
    /// a key a paired machine already trusts must never be replaced. `Ok(true)` if it took one.
    pub fn adopt(&self, from: &Store<R>) -> io::Result<bool> {
        if self.dir.join(R::KEY_FILE).exists() || !from.dir.join(R::KEY_FILE).exists() {
            return Ok(false);
        }
        for f in [R::KEY_FILE, R::FILE] {
            let src = from.dir.join(f);
            if src.exists() {
                fs::copy(&src, self.dir.join(f))?;
            }
        }
        Ok(true)
    }

    /// This side's static key, created on first use.
    pub fn keys(&self) -> io::Result<StaticKeys> {
        let path = self.dir.join(R::KEY_FILE);
        match fs::read_to_string(&path) {
            Ok(s) => {
                let kv = parse_kv(&s);
                match (kv.get("private").and_then(|v| unhex32(v)), kv.get("public").and_then(|v| unhex32(v))) {
                    (Some(private), Some(public)) => Ok(StaticKeys { private, public }),
                    _ => Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is damaged", path.display()))),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let k = StaticKeys::generate();
                write_private(&path, &format!("private = {}\npublic = {}\n", hex32(&k.private), hex32(&k.public)))?;
                Ok(k)
            }
            Err(e) => Err(e),
        }
    }

    pub fn peer(&self) -> Option<R> {
        R::from_kv(&read_kv(&self.dir.join(R::FILE)).ok()??)
    }

    pub fn save_peer(&self, r: &R) -> io::Result<()> {
        write_kv(&self.dir.join(R::FILE), &r.to_kv())
    }

    /// Forget the peer. `Ok(false)` if there was none.
    pub fn forget_peer(&self) -> io::Result<bool> {
        match fs::remove_file(self.dir.join(R::FILE)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

fn hex32(b: &[u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Replace `path` atomically with `contents`, readable only by its owner on Unix.
fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // Its own temporary file: `agent.key` and `agent.conf` must not share `agent.tmp`.
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path)
}

/// Read a `key = value` file; `Ok(None)` if there is none.
pub fn read_kv(path: &Path) -> io::Result<Option<BTreeMap<String, String>>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(parse_kv(&s))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Replace a `key = value` file atomically, readable only by its owner on Unix.
pub fn write_kv(path: &Path, kv: &BTreeMap<&str, String>) -> io::Result<()> {
    let body: String = kv.iter().map(|(k, v)| format!("{k} = {v}\n")).collect();
    write_private(path, &body)
}

fn parse_kv(s: &str) -> BTreeMap<String, String> {
    s.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hop-store-{name}-{}", std::process::id()))
    }

    #[test]
    fn keys_persist_owner_only() {
        let dir = tmp("keys");
        let s = AgentStore::new(dir.clone());
        let a = s.keys().unwrap();
        let b = s.keys().unwrap();
        assert_eq!((a.public, a.private), (b.public, b.private));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The literal name on purpose: renaming the file would orphan every installed key.
            assert_eq!(fs::metadata(dir.join("agent.key")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn adopting_never_replaces_an_identity() {
        let (from, to) = (AgentStore::new(tmp("adopt-from")), AgentStore::new(tmp("adopt-to")));
        fs::create_dir_all(&to.dir).unwrap();
        let theirs = from.keys().unwrap();
        from.save_peer(&MacRecord { public: [1; 32], name: "Old Mac".into() }).unwrap();
        assert!(to.adopt(&from).unwrap(), "an empty store takes the identity");
        assert_eq!(to.keys().unwrap().public, theirs.public);
        assert_eq!(to.peer().unwrap().name, "Old Mac");

        to.save_peer(&MacRecord { public: [2; 32], name: "New Mac".into() }).unwrap();
        let _ = fs::remove_dir_all(&from.dir);
        let other = AgentStore::new(tmp("adopt-other"));
        other.keys().unwrap();
        other.save_peer(&MacRecord { public: [3; 32], name: "Stale Mac".into() }).unwrap();
        assert!(!to.adopt(&other).unwrap(), "a store with its own identity keeps it");
        assert_eq!(to.keys().unwrap().public, theirs.public);
        assert_eq!(to.peer().unwrap().name, "New Mac");
        for s in [&to, &other] {
            fs::remove_dir_all(&s.dir).unwrap();
        }
    }

    #[test]
    fn peer_records_round_trip_and_forget() {
        let dir = tmp("peer");
        let s = MacStore::new(dir.clone());
        assert_eq!(s.peer(), None);
        let r = PcRecord { public: [7; 32], name: "Desk PC".into(), addr: Some("192.168.1.9:47820".parse().unwrap()) };
        s.save_peer(&r).unwrap();
        assert_eq!(s.peer(), Some(r));
        assert!(s.forget_peer().unwrap());
        assert!(!s.forget_peer().unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }
}

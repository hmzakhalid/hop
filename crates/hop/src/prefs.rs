//! The user's choices (where the PC is, edges, key mapping, speeds), saved by the app as
//! `prefs.conf` next to the pairing. They are exactly a [`ControllerConfig`].

use hop_core::controller::ControllerConfig;
use hop_core::store::{read_kv, write_kv};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

const FILE: &str = "prefs.conf";

pub fn load(dir: &Path) -> ControllerConfig {
    let mut c = ControllerConfig::default();
    let Ok(Some(kv)) = read_kv(&dir.join(FILE)) else { return c };
    let flag = |k: &str, default: bool| kv.get(k).map_or(default, |v| v == "true");
    let speed = |k: &str, default: f64| kv.get(k).and_then(|v| v.parse().ok()).filter(|x| ControllerConfig::valid_speed(*x)).unwrap_or(default);
    if let Some(side) = kv.get("side").and_then(|v| v.parse().ok()) {
        c.side = side;
    }
    c.edges = flag("edges", c.edges);
    c.map.cmd_as_win = flag("cmd_as_win", c.map.cmd_as_win);
    c.map.chords = flag("chords", c.map.chords);
    c.speed = speed("speed", c.speed);
    c.scroll_speed = speed("scroll_speed", c.scroll_speed);
    c
}

pub fn save(dir: &Path, c: &ControllerConfig) -> io::Result<()> {
    let kv = BTreeMap::from([
        ("side", c.side.name().to_string()),
        ("edges", c.edges.to_string()),
        ("cmd_as_win", c.map.cmd_as_win.to_string()),
        ("chords", c.map.chords.to_string()),
        ("speed", c.speed.to_string()),
        ("scroll_speed", c.scroll_speed.to_string()),
    ]);
    write_kv(&dir.join(FILE), &kv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hop_core::layout::Side;

    #[test]
    fn prefs_round_trip_and_bad_values_fall_back() {
        let dir = std::env::temp_dir().join(format!("hop-prefs-{}", std::process::id()));
        assert_eq!(load(&dir), ControllerConfig::default(), "nothing saved yet");
        let mut c = ControllerConfig { side: Side::Left, edges: true, speed: 1.5, ..ControllerConfig::default() };
        c.map.cmd_as_win = true;
        save(&dir, &c).unwrap();
        assert_eq!(load(&dir), c);
        std::fs::write(dir.join(FILE), "side = sideways\nspeed = -3\n").unwrap();
        assert_eq!(load(&dir), ControllerConfig::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

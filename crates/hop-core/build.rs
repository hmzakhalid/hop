//! `HOP_BUILD`: the commit this was built from, so a tester can tell two builds of the same
//! version apart. Releases are built from a committed tree (packaging/release.sh checks), so the
//! commit names exactly what is inside.

fn main() {
    let git = |args: &[&str]| {
        std::process::Command::new("git").args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // Rerun when the commit may have changed. HEAD and its log exist in every checkout and
    // worktree (a missing watched file would rebuild everything every time), and the log
    // changes on every commit, checkout and reset.
    for f in ["HEAD", "logs/HEAD"] {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", f]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let build = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=HOP_BUILD={build}");
}

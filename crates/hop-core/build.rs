//! `HOP_BUILD`: the commit this was built from, so a tester can tell two builds of the same
//! version apart. Releases are built from a committed tree (packaging/release.sh checks), so the
//! commit names exactly what is inside.

fn main() {
    let git = |args: &[&str]| {
        std::process::Command::new("git").args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // Rerun when the commit may have changed: HEAD, the branch it points to, and packed refs.
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        for f in ["HEAD", "packed-refs"] {
            println!("cargo:rerun-if-changed={dir}/{f}");
        }
        if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={dir}/{branch}");
        }
    }
    let build = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=HOP_BUILD={build}");
}

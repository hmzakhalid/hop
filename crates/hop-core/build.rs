//! `HOP_BUILD`: the commit this was built from, so a tester can tell two builds of the same
//! version apart. Releases are built from a committed tree (packaging/release.sh checks), so the
//! commit names exactly what is inside.

fn main() {
    let git = |args: &[&str]| {
        std::process::Command::new("git").args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // Rerun when the commit may have changed: HEAD, the branch refs (a directory, which catches
    // a new loose ref too), packed refs and HEAD's log. Only files that exist are watched: Cargo
    // reruns every build for a missing one.
    for f in ["HEAD", "refs/heads", "packed-refs", "logs/HEAD"] {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", f]).filter(|p| std::path::Path::new(p).exists()) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let build = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=HOP_BUILD={build}");
}

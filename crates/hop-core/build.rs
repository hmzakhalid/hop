//! `HOP_BUILD`: the commit this was built from (with `+` when tracked files had changes), so a
//! tester can tell two builds of the same version apart.

fn main() {
    let git = |args: &[&str]| {
        std::process::Command::new("git").args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // Rerun when the commit or the working tree may have changed: HEAD, the branch it points to,
    // packed refs, the index, and every source file in the workspace.
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        for f in ["HEAD", "index", "packed-refs"] {
            println!("cargo:rerun-if-changed={dir}/{f}");
        }
        if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={dir}/{branch}");
        }
    }
    println!("cargo:rerun-if-changed=../../crates");
    let build = match git(&["rev-parse", "--short", "HEAD"]) {
        Some(hash) => format!("{hash}{}", if git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty()) { "+" } else { "" }),
        None => "unknown".into(),
    };
    println!("cargo:rustc-env=HOP_BUILD={build}");
}

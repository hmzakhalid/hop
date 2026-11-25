//! Windows builds: embed the icon and version information (res/hop.rc) in every executable.

fn main() {
    println!("cargo:rerun-if-changed=res/hop.rc");
    println!("cargo:rerun-if-changed=res/hop.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<&str> = version.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).take(3).collect();
    parts.resize(3, "0");
    let comma = format!("{},0", parts.join(","));
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("hop-res.o");
    let windres = std::env::var("WINDRES").unwrap_or_else(|_| {
        if cfg!(windows) { "windres".into() } else { "x86_64-w64-mingw32-windres".into() }
    });
    let status = std::process::Command::new(&windres)
        .args(["-I", "res", &format!("-DHOP_VERSION_COMMA={comma}"), &format!("-DHOP_VERSION_STR=\\\"{version}\\\"")])
        .args(["-i", "res/hop.rc", "-o"])
        .arg(&out)
        .status()
        .unwrap_or_else(|e| panic!("running {windres}: {e} (set WINDRES to the mingw windres)"));
    assert!(status.success(), "{windres} failed");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

//! hop-capture: capture a monitor and encode it to an H.264 file, printing timings.
//! A standalone test tool: it opens no network sockets.

#[cfg(not(windows))]
fn main() {
    eprintln!("hop-capture runs on Windows");
}

#[cfg(windows)]
fn main() {
    use hop_video::capture::{list_monitors, Capturer, Settings};
    use std::io::Write;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

    let mut s = Settings::default();
    let mut seconds = 5u64;
    let mut out_path = String::from("capture.h264");
    let mut repeat = false;
    let mut key_every = 0u32;
    let mut bitrate_midway = 0u32;
    fn usage(problem: &str) -> ! {
        eprintln!("hop-capture: {problem}\nusage: hop-capture [--seconds N] [--fps N] [--mbps N] [--monitor N] [--software] [--repeat] [--key-every N] [--bitrate-midway MBPS] [--out FILE]");
        std::process::exit(2)
    }
    fn number(it: &mut impl Iterator<Item = String>, flag: &str, min: u32) -> u32 {
        it.next().and_then(|v| v.parse().ok()).filter(|n| *n >= min).unwrap_or_else(|| usage(&format!("{flag} needs a number of at least {min}")))
    }
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--seconds" => seconds = number(&mut it, "--seconds", 1) as u64,
            "--fps" => s.fps = number(&mut it, "--fps", 1),
            "--mbps" => s.bitrate = number(&mut it, "--mbps", 1) * 1_000_000,
            "--monitor" => s.monitor = number(&mut it, "--monitor", 0) as usize,
            "--software" => s.hardware = false,
            "--repeat" => repeat = true,
            "--key-every" => key_every = number(&mut it, "--key-every", 1),
            "--bitrate-midway" => bitrate_midway = number(&mut it, "--bitrate-midway", 1) * 1_000_000,
            "--out" => out_path = it.next().unwrap_or_else(|| usage("--out needs a file name")),
            other => usage(&format!("unknown argument {other:?}")),
        }
    }

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    match list_monitors() {
        Ok(ms) => {
            for (i, m) in ms.iter().enumerate() {
                println!("monitor {i}: {} {}×{} at ({}, {})", m.name, m.width, m.height, m.left, m.top);
            }
        }
        Err(e) => eprintln!("listing monitors failed: {e}"),
    }
    let mut cap = match Capturer::new(&s) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("capture setup failed: {e} ({:?})", e.code());
            std::process::exit(1);
        }
    };
    let encoder = cap.encoder_name();
    println!("capturing {}×{} at up to {} fps, {} Mbps, encoder: {encoder}", cap.width, cap.height, s.fps, s.bitrate / 1_000_000);
    let mut file = std::fs::File::create(&out_path).expect("create output");
    let frame_time = Duration::from_micros(1_000_000 / s.fps as u64);
    let start = Instant::now();
    let (mut frames, mut keys, mut bytes) = (0u64, 0u64, 0u64);
    let mut encode_times: Vec<u32> = Vec::new();
    let mut convert_times: Vec<u32> = Vec::new();
    let mut first = true;
    // Forced keyframes: how many frames until the requested keyframe comes out.
    let (mut asked_at, mut key_delays, mut since_key) = (None::<u64>, Vec::<u64>::new(), 0u32);
    let mut switched = false;
    while start.elapsed() < Duration::from_secs(seconds) {
        let tick = Instant::now();
        let force = first || (key_every > 0 && since_key + 1 >= key_every);
        if force && !first && asked_at.is_none() {
            asked_at = Some(frames);
        }
        if bitrate_midway > 0 && !switched && start.elapsed() >= Duration::from_secs(seconds) / 2 {
            switched = true;
            println!("switching to {} Mbps: {}", bitrate_midway / 1_000_000, if cap.set_bitrate(bitrate_midway) { "accepted" } else { "REFUSED" });
        }
        match cap.next(frame_time.as_millis() as u32, force, repeat) {
            Ok(Some((out, t))) => {
                first = false;
                encode_times.push(t.encode_us);
                convert_times.push(t.convert_us);
                for f in out {
                    frames += 1;
                    keys += f.keyframe as u64;
                    bytes += f.data.len() as u64;
                    since_key = if f.keyframe { 0 } else { since_key + 1 };
                    if let (true, Some(at)) = (f.keyframe, asked_at) {
                        key_delays.push(frames - 1 - at);
                        asked_at = None;
                    }
                    file.write_all(&f.data).unwrap();
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("capture failed: {e} ({:?})", e.code());
                std::process::exit(1);
            }
        }
        if repeat {
            if let Some(left) = frame_time.checked_sub(tick.elapsed()) {
                std::thread::sleep(left);
            }
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let ms = |v: &[u32]| hop_core::stats::Summary::of(v.iter().map(|&us| us as f64 / 1000.0));
    println!(
        "{frames} frames ({keys} keyframes) in {secs:.1} s = {:.1} fps, {:.1} Mbps, avg frame {:.1} KB",
        frames as f64 / secs,
        bytes as f64 * 8.0 / secs / 1e6,
        if frames > 0 { bytes as f64 / frames as f64 / 1024.0 } else { 0.0 }
    );
    if let (Some(c), Some(e)) = (ms(&convert_times), ms(&encode_times)) {
        println!("convert p50 {:.2} ms p99 {:.2} ms · encode p50 {:.2} ms p99 {:.2} ms", c.p50, c.p99, e.p50, e.p99);
    }
    if key_every > 0 {
        let max = key_delays.iter().max().copied().unwrap_or(0);
        println!("forced keyframes: {} arrived, frames until each arrived: max {max} (0 = the very frame asked for){}", key_delays.len(), if asked_at.is_some() { "; one still pending at the end" } else { "" });
    }
    if cap.encoder_name() != encoder {
        println!("encoder, after capturing: {}", cap.encoder_name()); // it refused something mid-run
    }
    println!("wrote {out_path}");
}

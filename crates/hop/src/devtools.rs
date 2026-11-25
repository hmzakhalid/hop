//! Developer tools, left out of packaged builds (`--no-default-features`): `hop drive`,
//! `hop view --file`, `hop badge`.

use crate::{hud, macos, pc, view};
use hop_core::controller::{Controller, ControllerConfig, Output};
use hop_core::keymap::{mac, MacMod};
use hop_core::layout::Side;
use hop_core::session::{NoEffects, Session};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A scripted session through the real controller and session, without the event tap. Run the
/// agent with `--swallow` so nothing is typed on the PC.
pub fn drive(host: Option<&str>) -> Result<(), String> {
    let pc = pc::connect(host)?;
    let ctl = Controller::new(ControllerConfig::default(), macos::displays());
    let session = Session::new(ctl, pc.link.clone(), Arc::new(NoEffects));
    let stop = Arc::new(AtomicBool::new(false));
    session.spawn(stop.clone(), |_| {});
    let asked = std::time::Instant::now();
    while session.read(|c| c.pc_monitors().is_empty()) {
        if asked.elapsed() > Duration::from_secs(3) {
            return Err(format!("{} did not answer", pc.name));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    println!("Driving {} ({}). Each step shows the scan codes the agent should print.", pc.name, pc.link.peer().ip());
    println!("\"acked\" means the PC applied the step; compare the codes with the agent's output.");

    let held = mac::HYPER | MacMod::LCmd.device_bit() | MacMod::LOpt.device_bit() | MacMod::LCtrl.device_bit();
    let act = |f: &dyn Fn(&mut Controller, u64) -> Output| {
        session.act(f);
        std::thread::sleep(Duration::from_millis(4));
    };
    let key = |code: u16, down: bool| act(&move |c, t| c.key(t, code, down, false, 0, (0.0, 0.0)));
    let tap = |code: u16| {
        key(code, true);
        key(code, false);
    };
    let flags = |m: MacMod, f: u64| act(&move |c, t| c.flags_changed(t, m.keycode(), f));
    let step = |label: &str| {
        std::thread::sleep(Duration::from_millis(150));
        let ok = session.read(|c| c.pending_transitions() == 0 && c.is_remote());
        println!("  {} {label}", if ok { "acked    " } else { "NOT ACKED" });
    };

    session.act(|c, t| c.key(t, mac::RIGHT, true, false, held, (100.0, 100.0)));
    std::thread::sleep(Duration::from_millis(150));
    if !session.read(|c| c.is_remote()) {
        return Err("could not take control with ⌃⌥⌘→".into());
    }
    println!("  entered   with ⌃⌥⌘→ (the held ⌃⌥⌘ must not reach the PC)");
    act(&|c, t| c.key(t, mac::RIGHT, false, false, held, (0.0, 0.0)));
    flags(MacMod::LCmd, MacMod::LOpt.device_bit() | MacMod::LCtrl.device_bit());
    flags(MacMod::LOpt, MacMod::LCtrl.device_bit());
    flags(MacMod::LCtrl, 0);
    step("release ⌃⌥⌘ (nothing sent)");
    flags(MacMod::LShift, MacMod::LShift.device_bit());
    tap(0x04);
    flags(MacMod::LShift, 0);
    tap(0x22);
    step("type \"Hi\"            → 2A↓ 23↓ 23↑ 2A↑ 17↓ 17↑");
    flags(MacMod::LCmd, MacMod::LCmd.device_bit());
    tap(0x08);
    flags(MacMod::LCmd, 0);
    step("⌘C                     → 1D↓ 2E↓ 2E↑ 1D↑ (Ctrl+C)");
    flags(MacMod::LOpt, MacMod::LOpt.device_bit());
    tap(mac::LEFT);
    flags(MacMod::LOpt, 0);
    step("⌥←                     → 38↓ 1D↓ 38↑ E0 4B↓ E0 4B↑ 1D↑ (Ctrl+←)");
    flags(MacMod::LCmd, MacMod::LCmd.device_bit());
    tap(mac::TAB);
    tap(mac::TAB);
    flags(MacMod::LCmd, 0);
    step("⌘Tab Tab               → 1D↓ 1D↑ 38↓ 0F↓ 0F↑ 0F↓ 0F↑ 38↑ (Alt+Tab+Tab)");
    for _ in 0..10 {
        act(&|c, t| c.mouse_moved(t, (0.0, 0.0), (20.0, 10.0), 0, false));
    }
    step("move 200 right, 100 down (× the PC's DPI scale)");
    act(&|c, t| c.mouse_button(t, 0, true));
    act(&|c, t| c.mouse_button(t, 0, false));
    step("left click (held 50 ms by the PC)");
    act(&|c, t| c.mouse_button(t, 0, true));
    for _ in 0..5 {
        act(&|c, t| c.mouse_moved(t, (0.0, 0.0), (20.0, 0.0), 0, true));
    }
    act(&|c, t| c.mouse_button(t, 0, false));
    step("drag 100 right");
    for _ in 0..3 {
        act(&|c, t| c.scroll(t, -1.0, 0.0, false));
    }
    step("scroll 3 notches down  → wheel −360");
    session.act(|c, t| c.key(t, mac::LEFT, true, false, mac::HYPER, (100.0, 100.0)));
    std::thread::sleep(Duration::from_millis(200));
    let left = session.read(|c| !c.is_remote());
    println!("  {} with ⌃⌥⌘← (everything released on the PC)", if left { "left     " } else { "NOT LEFT " });
    if let Some(r) = session.read(|c| c.rtt_summary()) {
        println!("round trip over {} acks: p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms", r.count, r.p50, r.p99, r.max);
    }
    stop.store(true, Ordering::Relaxed);
    Ok(())
}

/// Play a recording (from `hop-capture`) in the screen viewer, the way the PC would stream it.
pub fn view_file(path: &str) -> Result<(), String> {
    use hop_core::stream::{FileSource, Source};
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let mut source = FileSource::open(std::path::Path::new(path)).map_err(|e| format!("{path}: {e}"))?;
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let (window, mut decoder) = view::Window::open(mtm, path, 1920, 1080, || std::process::exit(0));
    println!("Playing {path} at {} fps.", hop_core::video::MAX_FPS);
    std::thread::spawn(move || {
        let mut size = None;
        loop {
            let _ = source.poll(100);
            for e in source.encode(false).unwrap_or_default() {
                if decoder.push(&e.data, e.hevc) == view::Pushed::NeedKeyframe {
                    decoder.reset();
                }
                if let Some((w, h)) = decoder.size().filter(|s| Some(*s) != size) {
                    size = Some((w, h));
                    println!("{w}×{h}");
                }
            }
        }
    });
    exit_after("HOP_VIEW_SECONDS", None);
    let _window = window;
    app.run();
    Ok(())
}

/// Show the indicator for a few seconds.
pub fn badge(side: Side) -> Result<(), String> {
    hud::init();
    hud::show(&hud::badge_text("PC", side));
    exit_after("HOP_BADGE_SECONDS", Some(3));
    hud::run()
}

/// Exit after `$var` seconds (or `default`), for previews and screenshots.
fn exit_after(var: &str, default: Option<u64>) {
    if let Some(secs) = std::env::var(var).ok().and_then(|s| s.parse::<u64>().ok()).or(default) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            std::process::exit(0);
        });
    }
}

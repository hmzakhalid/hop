//! Prints what it would inject instead of injecting it (any OS; one 1920×1080 monitor).

use hop_core::agent::Platform;
use hop_core::receiver::Injector;
use hop_core::wire::Monitor;

#[derive(Default)]
pub struct DryRun {
    pos: (i32, i32),
}

impl Injector for DryRun {
    fn key(&mut self, code: u16, down: bool) -> bool {
        println!("  key {code:#05x} {}", if down { "down" } else { "up" });
        true
    }
    fn button(&mut self, button: u8, down: bool) -> bool {
        println!("  button {button} {}", if down { "down" } else { "up" });
        true
    }
    fn move_to(&mut self, x: i32, y: i32) {
        self.pos = (x, y);
    }
    fn wheel(&mut self, v: i32, h: i32) {
        println!("  wheel {v} {h}");
    }
    fn mask_key(&mut self) {
        println!("  mask key");
    }
}

impl Platform for DryRun {
    fn monitors(&mut self) -> Vec<Monitor> {
        vec![Monitor { left: 0, top: 0, right: 1920, bottom: 1080, scale_milli: 1000 }]
    }
    fn cursor_pos(&mut self) -> (i32, i32) {
        self.pos
    }
}

//! The screen as a frame [`Source`] for the video channel.

use crate::capture::{Capturer, Settings};
use hop_core::stream::{Polled, Source};
use hop_core::video::Encoded;
use std::io;

pub struct ScreenSource {
    cap: Capturer,
}

fn io_err(e: windows::core::Error) -> io::Error {
    io::Error::other(format!("{e} ({:?})", e.code()))
}

impl ScreenSource {
    pub fn open(s: &Settings) -> io::Result<ScreenSource> {
        Ok(ScreenSource { cap: Capturer::new(s).map_err(io_err)? })
    }

    pub fn describe(&self) -> String {
        format!("{}×{}, {}", self.cap.width, self.cap.height, self.cap.encoder_name())
    }
}

impl Source for ScreenSource {
    fn poll(&mut self, timeout_ms: u32) -> io::Result<Polled> {
        let changed = self.cap.poll(timeout_ms).map_err(io_err)?;
        Ok(Polled { changed, cursor: self.cap.pointer() })
    }

    fn encode(&mut self, keyframe: bool) -> io::Result<Vec<Encoded>> {
        let Some((frames, t)) = self.cap.encode_latest(keyframe).map_err(io_err)? else { return Ok(Vec::new()) };
        let hevc = self.cap.is_hevc();
        let pc_us = hop_core::clock::now_us().saturating_sub(t.captured_us).min(u32::MAX as u64) as u32;
        Ok(frames.into_iter().map(|f| Encoded { data: f.data, keyframe: f.keyframe, hevc, captured_us: t.captured_us, pc_us }).collect())
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.cap.set_bitrate(bps);
    }
}

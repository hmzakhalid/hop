//! Screen capture (DXGI Desktop Duplication) and HEVC / H.264 encoding (Media Foundation) on
//! Windows.
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod source;

//! Screen capture (DXGI Desktop Duplication) and H.264 encoding (Media Foundation) on Windows.
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod source;

//! Hop core: everything that does not touch the operating system's input APIs.
//!
//! The Mac app runs a [`session::Session`]: a [`controller::Controller`] (when control
//! switches, what to send) driven over a [`link::Link`] (encrypted UDP). The PC agent runs
//! [`agent::run`] with a platform [`agent::Platform`] that injects input into a
//! [`receiver::Receiver`]. Both sides share the wire format and timers in [`wire`].

/// The commit this was built from, `+` if the tree had changes.
pub const BUILD: &str = env!("HOP_BUILD");

pub mod agent;
pub mod clock;
pub mod controller;
pub mod crypto;
pub mod h264;
pub mod keymap;
pub mod layout;
pub mod link;
pub mod mapper;
pub mod receiver;
pub mod session;
pub mod stats;
pub mod store;
pub mod stream;
pub mod video;
pub mod wire;

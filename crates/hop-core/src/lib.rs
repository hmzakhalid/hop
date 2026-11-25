//! Hop core: everything that does not touch the operating system's input APIs.
//!
//! The Mac app runs a [`session::Session`]: a [`controller::Controller`] (when control
//! switches, what to send) driven over a [`link::Link`] (encrypted UDP). The PC agent runs
//! [`agent::run`] with a platform [`agent::Platform`] that injects input into a
//! [`receiver::Receiver`]. Both sides share the wire format and timers in [`wire`].
//!
//! Screen view runs beside it on its own channel: [`stream::serve`] on the PC and
//! [`stream::Client`] on the Mac, with the frame rules in [`video`]. Both PC channels accept the
//! Mac through a [`responder::Responder`].

/// The commit this was built from.
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
pub mod responder;
pub mod session;
pub mod stats;
pub mod store;
pub mod stream;
pub mod ui;
pub mod video;
pub mod wire;

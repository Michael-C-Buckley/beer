//! Wayland backend for the beer terminal.
//!
//! Implements the [`beer_window`] contract: owns the sctk connection, the
//! calloop event loop, the surfaces and shm buffers, and drives a
//! [`beer_window::App`] by translating Wayland input and lifecycle events into
//! `App` calls. The app performs platform actions back through the
//! `WindowCtx` this backend supplies.
//!
//! In progress: `state` (the backend data model) is in place; the sctk handler
//! translations, the `WindowCtx` implementation, the present path, and the
//! `run` entry point are being built on top of it.

mod ctx;
mod handlers;
mod present;
mod run;
mod state;

pub use run::run;

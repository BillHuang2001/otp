//! Built-in drivers of the Rust ERTS rewrite.
//!
//! Milestone-1 ships a single built-in byte-stream driver, [`FdDriver`] (registered under
//! the name `"fd"`), which is enough to prove the poller→port→driver path end to end. The
//! legacy C's `tcp_inet`/`efile`/`forker`/`spawn` drivers
//! (`erts/emulator/drivers/common/*.c`) are documented follow-ups (see the crate
//! `CONTEXT.md`, Known Issues).

pub mod fd;

pub use crate::drivers::fd::FdDriver;

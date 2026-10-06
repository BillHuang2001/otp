//! Built-in drivers of the Rust ERTS rewrite.
//!
//! Milestone-1 ships the built-in byte-stream driver [`FdDriver`] (registered under the
//! name `"fd"`), which is enough to prove the poller→port→driver path end to end, plus
//! the [`EfileDriver`] (`"efile"`) built on top of it. The legacy C's
//! `tcp_inet`/`forker`/`spawn` drivers (`erts/emulator/drivers/common/*.c`) are documented
//! follow-ups (see the crate `CONTEXT.md`, Known Issues).
//!
//! `FdDriver` also carries the two framing facilities a real port driver needs, in
//! dedicated modules:
//!
//! * [`line_buffer`] — the `{line, N}` line-oriented buffering of `open_port/2`;
//! * [`binary_io`] — the ERTS port *phase change* between list/text and raw binary
//!   framing (`ERTS_PORT_SFLG_BINARY_IO`); and
//! * [`efile`] — the `efile` driver: `FdDriver` in read-only/write-only form plus the
//!   `efile` control family.

pub mod binary_io;
pub mod efile;
pub mod fd;
pub mod line_buffer;

pub use crate::drivers::binary_io::{Phase, PhaseChange};
pub use crate::drivers::efile::{make_efile, EfileDriver};
pub use crate::drivers::fd::FdDriver;
pub use crate::drivers::line_buffer::{LineBuffer, DEFAULT_MAX_LINE_LEN};

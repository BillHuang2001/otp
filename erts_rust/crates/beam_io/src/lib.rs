//! # `beam_io` — poller/reactor, ports and built-in drivers
//!
//! The **I/O subsystem** of the Rust ERTS rewrite (Stage 5, Part II §A/§H). It owns the
//! OS/driver seam — one of the two clean, already-existing rewrite seams — so the layer
//! above it can be reproduced with the *frozen* driver C ABI on top (workspace
//! `../CONTEXT.md` §1 **A.4**).
//!
//! Three things live here:
//!
//! 1. the **event poller/reactor** ([`poller`]) — a portable `mio`-backed readiness loop
//!    (epoll/kqueue) with a token→registration map and an asynchronous [`poller::Waker`];
//! 2. the **[`Port`](port::Port)** ([`port`]) — a process-like I/O endpoint owning an
//!    input and an output byte queue, a lifecycle and a driver handle, allocated from
//!    `beam_process`'s port table; and
//! 3. the **driver interface + built-in drivers** ([`driver`], [`drivers`]) — a Rust
//!    [`Driver`](driver::Driver) trait today, with the frozen C-ABI
//!    (`beam_abi::driver`) bridge as documented future work, plus the built-in
//!    byte-stream driver ([`drivers::FdDriver`], `"fd"`), the [`drivers::EfileDriver`]
//!    (`"efile"`) built on it, the OS-child [`drivers::SpawnDriver`] (`"spawn"`) and the
//!    TCP socket [`drivers::TcpInetDriver`] (`"tcp_inet"`).
//!
//! Supporting types: [`timer`] (poll deadlines), [`queue`] (the port byte buffers) and
//! [`port_task`] (per-port task scheduling: the ERTS port-task queue, its reduction costs
//! and the notification seam a task round reports through).
//!
//! # Status
//!
//! Milestone-1 is implemented (see this crate's `CONTEXT.md`, Milestone-1 scope): the
//! poller/reactor, ports, the driver seam and the built-in
//! `"fd"`/`"efile"`/`"spawn"`/`"tcp_inet"` drivers are real and covered by in-module unit
//! tests plus the `tests/poller.rs` / `tests/port.rs` / `tests/port_task.rs` / `tests/spawn.rs`
//! / `tests/inet.rs` integration suites (real OS fds, end-to-end `pipe(2)` round-trip, real
//! child processes, real loopback TCP connections with an `erl`-oracle reply cross-check,
//! scheduled port tasks and the wake-through-`beam_process` path). The frozen C-ABI driver bridge and the port-task
//! *scheduler* wiring (poll threads, port run queues) remain follow-ups —
//! [`PortRegistry::execute_port_tasks`] is the drive point the runtime calls.
//!
//! # Contracts
//!
//! * **Errors are return-based** (workspace `../../CONTEXT.md` §3): every fallible API
//!   returns [`beam_common::Result`]; I/O failures map to `BeamError::Io` (never a panic
//!   on runtime data and never a swallowed error).
//! * **Safe Rust by default.** `unsafe` is confined to the raw-fd syscall wrappers and
//!   the `mio::unix::SourceFd` construction, each with a `// SAFETY:` rationale.
//! * **Port ids are allocated by `beam_process::PortTable`** — this crate never adds a
//!   second number allocator, and a port id stays `beam_term`'s internal-port immediate.
//!
//! Legacy C ground truth (read-only): `erts/emulator/sys/common/erl_poll.c`,
//! `erts/emulator/beam/io.c`, `erl_port.h`, `erl_io_queue.{c,h}`, `erl_port_task.{c,h}`
//! and `erts/emulator/drivers/common/inet_drv.c`.

#![warn(missing_docs)]

pub mod driver;
pub mod drivers;
pub mod poller;
pub mod port;
pub mod port_task;
pub mod queue;
pub mod registry;
pub mod timer;

mod error; // `pub(crate)` constructors only — NOT public

pub use crate::driver::*;
pub use crate::drivers::*;
pub use crate::poller::*;
pub use crate::port::*;
pub use crate::port_task::*;
pub use crate::queue::*;
pub use crate::timer::*;

/// The conventional glob-import surface for downstream crates.
///
/// `use beam_io::prelude::*;` brings the poller ([`Poller`], [`Token`], [`Interest`],
/// [`Ready`], [`Event`], [`Waker`]), the port ([`Port`], [`PortState`],
/// [`PortRegistry`], [`PortControl`], [`PortCommandOptions`]), the port-task machinery
/// ([`PortTaskKind`], [`PortTask`], [`PortTaskQueue`], [`PortNotification`],
/// [`PortTaskOutcome`], [`Readiness`]) the driver seam ([`Driver`], [`DriverContext`],
/// [`DriverFlags`], [`DriverRegistry`], [`DriverFactory`], [`PortExitReason`], [`FdDriver`],
/// [`EfileDriver`],
/// [`SpawnDriver`], [`SpawnSpec`], [`SpawnOptions`], [`SpawnStream`], [`build_command`],
/// [`TcpInetDriver`], [`InetState`]) and the timer/queue helpers ([`Timeout`], [`Timer`],
/// [`ByteQueue`]) into scope.
pub mod prelude {
    pub use crate::driver::*;
    pub use crate::drivers::*;
    pub use crate::poller::*;
    pub use crate::port::*;
    pub use crate::port_task::*;
    pub use crate::queue::*;
    pub use crate::timer::*;
}

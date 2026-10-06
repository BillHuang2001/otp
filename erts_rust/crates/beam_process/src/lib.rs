#![doc = "Processes, scheduler, run queues, thread progress and signal handling."]

//! Erlang processes, the scheduler and the runtime's concurrency plumbing.
//!
//! Owns the **OS-thread-level concurrency plumbing** of the Rust ERTS rewrite (Stage 5,
//! Part II §A/§H): the per-process control block (`Process`), the run queue, the
//! cooperative scheduler loop, thread progress, the **exit-signal / link / monitor**
//! machinery and the process/port table (`ProcessTable`) with its local name registry.
//! It is what makes Erlang **processes** real — isolation, `spawn`, links, monitors and
//! exit propagation — the process half of workspace `../CONTEXT.md` §1 **A.1** (process
//! isolation / link / monitor / exit semantics).
//!
//! Consumed by `beam_bifs` (spawn/send/receive/`erlang:process_info`/monitor/link BIFs),
//! `beam_io` (ports and the process side of I/O completion) and `beam_rt` (boot, CLI and
//! `system_info`). This crate **drives** `beam_gc` (collector + MSO) at safepoints and
//! **uses** `beam_message` (the mailbox [`Queue`] and the `send`/`receive` copy engine) —
//! it reimplements neither (Prevent-Duplication).
//!
//! # Milestone-1 scope
//!
//! A **correct single-OS-thread** core: one [`Scheduler`], no thread pools, no dirty
//! schedulers, no lock-free/threaded run queues and no cross-scheduler barrier beyond the
//! documented single-scheduler [`ThreadProgress`] stub. Multi-scheduler balancing, the
//! decoupled signal queue, priority run queues, the process dictionary and
//! distribution-wide links/monitors are documented follow-ups (see the crate
//! `CONTEXT.md`, Known Issues).
//!
//! # Contracts
//!
//! * **Term layout is frozen** (workspace `../CONTEXT.md` §2): terms are built and
//!   inspected only through `beam_term` + `beam_alloc::Heap`; pid/port immediates are
//!   bit-exact with `beam_term::immediate`.
//! * **Errors are return-based** (§3): every fallible API returns
//!   [`beam_common::Result`] — never a crate-local error enum, never a panic on runtime
//!   data and never a swallowed error.
//! * **Root set** (§4/§5): `x_reg_array[0..Live)` plus the whole Erlang stack, with no
//!   frame liveness maps; [`Process::collect`] gathers the registers and the mailbox
//!   payloads into a flat root slice for `beam_gc`.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_process.{c,h}`,
//! `erl_proc_sig_queue.c`, `erl_monitor_link.c`, `erl_ptab.{c,h}`, `erl_process_dict.c`
//! and `erl_vm.h`.

#![warn(missing_docs)]

pub mod link;
pub mod monitor;
pub mod pid;
pub mod process;
pub mod progress;
pub mod ptab;
pub mod registers;
pub mod remote;
pub mod run_queue;
pub mod scheduler;
pub mod signal;

mod error; // pub(crate) constructors only — NOT public

pub use crate::link::*;
pub use crate::monitor::*;
pub use crate::pid::*;
pub use crate::process::*;
pub use crate::progress::*;
pub use crate::ptab::*;
pub use crate::registers::*;
pub use crate::remote::*;
pub use crate::run_queue::*;
pub use crate::scheduler::*;
pub use crate::signal::*;

/// The conventional glob-import surface for downstream crates.
pub mod prelude {
    pub use crate::link::*;
    pub use crate::monitor::*;
    pub use crate::pid::*;
    pub use crate::process::*;
    pub use crate::progress::*;
    pub use crate::ptab::*;
    pub use crate::registers::*;
    pub use crate::remote::*;
    pub use crate::run_queue::*;
    pub use crate::scheduler::*;
    pub use crate::signal::*;
}

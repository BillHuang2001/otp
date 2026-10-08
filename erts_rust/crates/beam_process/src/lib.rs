// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

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
//! The crate provides two scheduler fronts. [`Scheduler`] is the Milestone-1
//! **single-OS-thread** loop: one run queue served highest-priority first (max/high/normal/low —
//! see [`RunQueue`]), though the C's shared NORMAL/LOW physical queue with the `schedule_count`
//! deferral is approximated by four distinct FIFOs. Alongside it, the SMP module
//! ([`crate::smp::SchedulerPool`]) runs **N scheduler threads** over a shared
//! [`SharedProcessTable`], with per-scheduler run queues plus work stealing, the
//! [`ThreadProgress`] cross-scheduler barrier, and the [`TimerService`] and dirty pools
//! ([`DirtyPool`]). [`SharedProcessTable`] is `!Send` (it owns off-heap `MsoHandle`s), so the SMP
//! module carries a **documented handoff** — the two `unsafe impl Send`/`unsafe impl Sync` on
//! [`SharedProcessTable`] (see [`crate::smp`]); the durable fix belongs in `beam_gc`/`beam_alloc`.
//! Exit/link/monitor signals are **decoupled** on both fronts: they are queued on the receiver
//! ([`SignalQueue`]) and applied at schedule-in (or eagerly, for a receiver that is not currently
//! executing — see [`ProcessTable::service_signals`]). Multi-scheduler balancing refinements and
//! distribution-wide links/monitors are documented follow-ups (see the crate `CONTEXT.md`, Known
//! Issues).
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
//!   frame liveness maps; [`Process::collect`] gathers the registers, the mailbox payloads,
//!   the process dictionary ([`Dict`]), the saved BIF/NIF arguments ([`SavedArgs`]) and the
//!   pending-signal queue ([`SignalQueue`]) into a flat root slice for `beam_gc`.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_process.{c,h}`,
//! `erl_proc_sig_queue.c`, `erl_monitor_link.c`, `erl_ptab.{c,h}`, `erl_process_dict.c`
//! and `erl_vm.h`.

#![warn(missing_docs)]

pub mod dict;
pub mod dirty;
pub mod exit_observer;
pub mod fragments;
pub mod link;
pub mod monitor;
pub mod pid;
pub mod port;
pub mod process;
pub mod progress;
pub mod ptab;
pub mod registers;
pub mod remote;
pub mod run_queue;
pub mod scheduler;
pub mod signal;
pub mod signal_queue;
pub mod smp;
pub mod timer;

mod error; // pub(crate) constructors only — NOT public

pub use crate::dict::*;
pub use crate::dirty::*;
pub use crate::exit_observer::*;
pub use crate::fragments::*;
pub use crate::link::*;
pub use crate::monitor::*;
pub use crate::pid::*;
pub use crate::port::*;
pub use crate::process::*;
pub use crate::progress::*;
pub use crate::ptab::*;
pub use crate::registers::*;
pub use crate::remote::*;
pub use crate::run_queue::*;
pub use crate::scheduler::*;
pub use crate::signal::*;
pub use crate::signal_queue::*;
pub use crate::smp::*;
pub use crate::timer::*;

/// The conventional glob-import surface for downstream crates.
pub mod prelude {
    pub use crate::dict::*;
    pub use crate::dirty::*;
    pub use crate::exit_observer::*;
    pub use crate::fragments::*;
    pub use crate::link::*;
    pub use crate::monitor::*;
    pub use crate::pid::*;
    pub use crate::port::*;
    pub use crate::process::*;
    pub use crate::progress::*;
    pub use crate::ptab::*;
    pub use crate::registers::*;
    pub use crate::remote::*;
    pub use crate::run_queue::*;
    pub use crate::scheduler::*;
    pub use crate::signal::*;
    pub use crate::signal_queue::*;
    pub use crate::smp::*;
    pub use crate::timer::*;
}

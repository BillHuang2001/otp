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

//! The **process runner**: the runtime's [`ProcessRunner`](beam_process::ProcessRunner)
//! behind the scheduler.
//!
//! `beam_process` owns the scheduler but must not know how a process executes — it
//! reaches the execution engine through the
//! [`ProcessRunner`](beam_process::ProcessRunner) seam
//! (`../beam_process/src/scheduler.rs`, and the orchestration contract in that crate's
//! `CONTEXT.md`). This module is the concrete implementation: it turns one reduction
//! slice into one engine entry (`run_export_outcome` for a process's first run,
//! `resume_outcome` for a process parked at a blocking `receive`) over a
//! [`RuntimeContext`](crate::context::RuntimeContext) that runs **as that pid** in the
//! caller-supplied process table.
//!
//! # The borrow strategy (why the table is moved out of the runtime)
//!
//! A scheduler pass needs, at the same time,
//!
//! * `&mut Runtime` — BIF dispatch, the shared atom table, the heaps (what
//!   [`RuntimeContext`](crate::context::RuntimeContext) borrows), and
//! * `&mut ProcessTable` — the PCBs the scheduler runs, which
//!   [`Context::for_process`](crate::context::RuntimeContext::for_process) resolves each
//!   mailbox against.
//!
//! The second is a **field** of the first, so handing both out of one `Runtime` value is
//! impossible. [`Runtime::run_scheduler`](crate::runtime::Runtime::run_scheduler)
//! therefore *moves* the table (and this module's [`RunnerState`]) out of the runtime for
//! the duration of the pass and puts them back afterwards — the same shape as any
//! split-borrow, written out explicitly because the crate is `#![deny(unsafe_code)]` and
//! may not alias.
//!
//! # What the runner records (the observable result)
//!
//! A process that runs to its `return` exits with the reason `normal` — **not** with the
//! value it produced (that is the BEAM contract: `x(0)` is the *return value* of the
//! function, not the process's exit reason). The value would otherwise be lost, so the
//! runner copies it onto the runtime's pointer-stable arena
//! ([`Runtime::stabilize_term`](crate::Runtime::stabilize_term)) and records it per pid in
//! [`RunnerState::result`], where
//! [`Runtime::process_result`](crate::runtime::Runtime::process_result) exposes it after
//! the pass. The copy matters: a value taken out of the mailbox (a port's stream, say)
//! points into the *process's own* heap, which is dropped the moment it terminates. The
//! arena never moves its terms and Milestone 1 never resets it, so the recorded value
//! stays valid for the runtime's lifetime.
//!
//! A process that fails records the raise's reason term in [`RunnerState::exit`]
//! ([`Runtime::process_exit`](crate::runtime::Runtime::process_exit)) and exits with that
//! very term, so the failure is observable through the process table's normal exit
//! propagation too — never swallowed, never invented.
//!
//! Legacy C ground truth (read-only): `../../erts/emulator/beam/erl_process.c`
//! (`schedule`/`erts_schedule`), `beam_common.c` (`apply`/`return`).
//!
//! # Layout
//!
//! * [`entry`] — the runtime-scheduled entry point ([`ProcessEntry`]) and the per-pid
//!   engine state + recorded outcomes ([`RunnerState`]).
//! * [`run`] — the [`ProcessRunner`](beam_process::ProcessRunner) implementation
//!   ([`RuntimeRunner`]): one reduction slice of one scheduled process.
//! * [`budget`] — the slice's reduction budget and the wrapper context that charges it.
//! * [`reason`] — the exit-reason translation and the finite-`receive` deadline recovery.
//! * [`perf`] — the `+JPperf` code-map feed (contract A.8).

mod budget;
mod entry;
mod perf;
mod reason;
mod run;
#[cfg(test)]
mod tests;

pub(crate) use entry::{ProcessEntry, RunnerState};
pub(crate) use run::RuntimeRunner;

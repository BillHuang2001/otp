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
//! # The execution tier of a first entry
//!
//! A process's **first** entry can run on either execution tier (`beam_engine::Tier`); a
//! *resume* continues the machine it suspended in, on the interpreter, with no tier entry.
//! [`tier`] owns the selection and the soundness argument for it: the baseline JIT is only
//! chosen when the entry module is **provably free of a module-context switch**
//! (`Step::Enter`) — no `apply`/`apply_last`, and every `call_ext*` target provably
//! synchronous — because the JIT driver has no in-machine module switch and would turn such
//! a step into a bogus exit reason. The mode is [`JitMode`](tier::JitMode):
//! [`Interpreter`](tier::JitMode::Interpreter) — **the default**, the always-correct tier —
//! or [`Auto`](tier::JitMode::Auto) / [`Baseline`](tier::JitMode::Baseline) from
//! `BEAM_RT_TIER` / a test.
//!
//! # Why the JIT is opt-in (`BEAM_RT_TIER=auto`), not the default
//!
//! The tier is taken on the **first** entry only, and a pre-empted process is **resumed on
//! the interpreter** (a tier-aware resume is a missing engine seam), so a module whose hot
//! loop reaches a heap-admission point (`test_heap`/`allocate_heap`) is ≈neutral: all but
//! its first slice run interpreted. The baseline JIT's large win is confined to a hot loop
//! with **no** heap-admission point anywhere, which stays in the one tier-chosen slice.
//! Measured on the release build with `$ERTS_STDLIB_DIR` set (no per-process `erl`
//! start-up), alternating runs, min/median seconds:
//!
//! | workload | interpreter | `auto` (JIT where safe) |
//! |---|---|---|
//! | `walk:cnt/1 20000000` (non-allocating tail loop) | 16.05 / 17.05 | 0.116 / 0.127 |
//! | `al:tailnoalloc/1 20000000` (non-allocating tail loop) | 19.35 | 0.114 |
//! | `fibn:fib/1 27` (allocating tree recursion) | 0.384 / 0.403 | 0.273 / 0.284 |
//! | `al:list/1 3000000` (allocating tail loop) | 1.34 / 1.40 | 1.27 / 1.43 |
//! | `slice:fib/1 300000` (the slice fixture) | 0.135 / 0.183 | 0.119 / 0.226 |
//! | `slice:fib/1 32` (the slice fixture) | 0.026 / 0.033 | 0.022 / 0.029 |
//!
//! The JIT is never *clearly* faster on the named switch-free fixture (`slice.beam`:
//! ≈1.0–1.15×, inside the run-to-run noise) and never beyond-noise *slower* either, and its
//! 139–170× win needs the no-heap-admission-point shape. With the `Step::Enter` deopt and
//! the tier-aware-resume seams missing, a slower-by-default runtime is unacceptable, so the
//! default is the interpreter and `BEAM_RT_TIER=auto` opts **in** to the JIT exactly where
//! [`tier::jit_is_switch_safe`] proves it safe. The selection machinery, the predicate and
//! the per-module memo are permanent; only the default is conservative.
//!
//! # Layout
//!
//! * [`entry`] — the runtime-scheduled entry point ([`ProcessEntry`]) and the per-pid
//!   engine state + recorded outcomes ([`RunnerState`]).
//! * [`run`] — the [`ProcessRunner`](beam_process::ProcessRunner) implementation
//!   ([`RuntimeRunner`]): one reduction slice of one scheduled process.
//! * [`budget`] — the slice's reduction budget and the wrapper context that charges it.
//! * [`tier`] — the static execution-tier selection and the switch-safety predicate.
//! * [`reason`] — the exit-reason translation and the finite-`receive` deadline recovery.
//! * [`perf`] — the `+JPperf` code-map feed (contract A.8).

mod budget;
mod entry;
mod perf;
mod reason;
mod run;
mod tier;
#[cfg(test)]
mod tests;

pub(crate) use entry::{ProcessEntry, RunnerState};
pub(crate) use run::RuntimeRunner;

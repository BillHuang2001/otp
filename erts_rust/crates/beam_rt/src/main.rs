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

//! The `beam_rt` executable: a thin shim over [`beam_rt::cli::run`].
//!
//! Every piece of logic lives in the library so it is unit- and integration-testable;
//! this file only forwards the command line (minus `argv[0]`) and turns the returned
//! exit code into the process status. See `beam_rt::cli` for the supported switch
//! subset (contract A.6) and `../CONTEXT.md` for the frozen surface.
//!
//! The shim owns two things the library deliberately does not: the **stack the runtime runs on**
//! (the driver runs on a named thread with an explicit [`MAIN_STACK_BYTES`] stack instead of Rust's
//! default 8 MiB main-thread stack — see the constant for why), and the **allocator's free-page
//! release** ([`beam_rt::release_free_pages`], called once after that thread has been joined —
//! see [`run_on_driver_stack`]).

use std::thread;

/// The OS thread stack the runtime driver is given: **64 MiB**.
///
/// # Why the driver does not run on the default 8 MiB main-thread stack
///
/// A **BIF-initiated fun application** — `lists:map/2`, `foldr/3`, `foldl/3`, `mapfoldl/3` and
/// the rest of the family — enters a *fresh nested engine run on the calling thread*
/// (`src/context/bridges.rs::FunBridge::nested` → `src/context/execution.rs::apply_fun` →
/// `src/context/pump.rs::run_label_blocking`). Nothing bounds that nesting: each level costs
/// **≈ 8.4 KB** of OS thread stack (the measured 29 Rust frames of the nested-run prologue), so
/// Rust's default 8 MiB main-thread stack caps a recursive Erlang computation that goes through a
/// fun-taking BIF at **≈ 996 levels** — and the recursion overflows the *thread* stack, which the
/// Rust runtime turns into `fatal runtime error: stack overflow, aborting` (SIGABRT), not into a
/// typed gap.
///
/// That is not a theoretical limit: OTP's own compiler reaches it. `beam_core_to_ssa` builds its
/// match tree with one Erlang-level fun application per decision-tree level (`match/4` via
/// `foldr`, `match_con/5` and `do_match_value/5` via `mapfoldl`), so compiling a module with a long
/// bitstring clause head — `lib/ssl/src/ssl_srp_primes.erl`'s five SRP-prime patterns, or a clause
/// with 256 variable 8-bit segments — nests ≈ 1 000 levels deep and aborted the runtime.
///
/// # The arithmetic
///
/// At the measured ≈ 8.4 KB per nesting level, 64 MiB (`64 << 20` bytes) buys
/// `67 108 864 / 8 422 ≈` **7 900** nested levels — ≈ 8× the default stack's ≈ 996 — with
/// headroom over the ≈ 1 000 levels the deepest legitimate compile needs today (a 5 000-level
/// nested-fun recursion is the shape that was verified to pass under a 64 MiB `ulimit -s`).
/// The reservation is virtual: only the touched pages are ever committed.
///
/// # This is a bound-raising mitigation, not a fix
///
/// The nesting seam is still **unbounded** — a deep enough Erlang recursion through a fun-taking
/// BIF still overruns this stack, and the recursion still costs a whole Rust frame chain per
/// level. Making the seam bounded and/or trampolined (a constant-stack fun application, as
/// `beam_abi`'s `enif_schedule_nif` trampoline already does for NIF chains) is the real fix and
/// remains a recorded Known Issue of this crate (`CONTEXT.md`, *Known Issues*); raising the bound
/// only moves the abort point past every legitimate workload in the installed corpus.
///
/// The constant lives in the binary crate rather than beside the pump bound
/// (`src/context/pump.rs::BLOCKING_PUMP_DEADLINE`) because it is a property of the *process
/// entry*, not of the nested run: the library never spawns this thread, so a host that embeds
/// `beam_rt` (or a test driving `cli::run` directly) keeps its own stack choice.
const MAIN_STACK_BYTES: usize = 64 << 20;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run_on_driver_stack(args));
}

/// Runs [`beam_rt::cli::run`] on a named thread with [`MAIN_STACK_BYTES`] of stack and answers the
/// process status: the driver's own exit code, or [`beam_rt::cli::EXIT_RUNTIME`] when the thread
/// could not be started or panicked — a failed driver must never be reported as success.
///
/// # The one allocator release point
///
/// After the join, and before the process status is handed to [`std::process::exit`],
/// [`beam_rt::release_free_pages`] returns the process-global allocator's free pages to the OS.
/// This is the only place that is both **quiescent** and **complete**:
///
/// * the driver thread has **exited**, so everything it owned — its thread-local state and,
///   crucially, the arena attachment it allocated through — is gone, and what the allocator still
///   maps is free; measured, a bare boot holds 88.8 MB of resident set with 210 kB of arena in use
///   at the moment the driver returns, and one trim here takes that 88.8 MB down to ~18 MB (an
///   in-process compile of `cerl_clauses.erl`: 206 MB → ~20 MB);
/// * a trim from **inside** the driver — even after the runtime it built has been dropped — only
///   recovers ~16 MB of that, because the thread itself still holds the rest until it exits, so
///   the release is placed here rather than in `beam_rt::cli::run`.
///
/// It touches only the allocator's free-page bookkeeping (no term, heap, table or live
/// allocation), so no result, exception or exit code can depend on it; the pages are re-faulted
/// transparently if anything else in the process needs them. `src/malloc.rs` carries the census,
/// the mechanism and the measurements.
fn run_on_driver_stack(args: Vec<String>) -> i32 {
    let driver = thread::Builder::new()
        .name("beam_rt-driver".to_string())
        .stack_size(MAIN_STACK_BYTES)
        .spawn(move || beam_rt::cli::run(args));
    let driver = match driver {
        Ok(driver) => driver,
        Err(error) => {
            eprintln!("beam_rt: could not start the runtime driver thread: {error}");
            return beam_rt::cli::EXIT_RUNTIME;
        }
    };
    let code = match driver.join() {
        Ok(code) => code,
        Err(_) => {
            // The panic itself has already been reported by the default hook; all that is left is
            // to make sure the status is never the success code.
            eprintln!("beam_rt: the runtime driver panicked");
            beam_rt::cli::EXIT_RUNTIME
        }
    };
    beam_rt::release_free_pages();
    code
}

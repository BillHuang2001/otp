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

//! The per-instruction `Step` control-flow protocol.
//!
//! [`Step`] is the control-flow contract between the interpreter's dispatch loop
//! (`crate::interpreter`) and the per-instruction-family bodies
//! (`crate::families`). An instruction family performs its register/heap effect
//! and then tells the loop where to continue — it never mutates the loop's
//! `func`/`pc` directly.
//!
//! A raise is **not** a [`Step`]: it stays an `Err(beam_common::BeamError)` (the
//! exception layer either folds it into a `Step::Goto` to a catch handler or lets
//! it propagate out of the run).
//!
//! Two variants park the process instead of continuing, and both are `Copy` (so they
//! carry no payload beyond the address): [`Step::Wait`] for a `receive` with no
//! message (released by the mailbox) and [`Step::Suspend`] for a BIF that answered
//! [`crate::BifOutcome::Suspend`] (released by the host, which re-issues the BIF).
/// The control-flow decision an instruction family returns to the dispatch loop.
///
/// Produced by the `crate::families` bodies; consumed by the interpreter's
/// `match families::dispatch(..)` in `crate::interpreter`, by the dispatch loop
/// (`Machine::run_loop_with_map`) and by the baseline JIT's per-instruction
/// helper (`crate::jit::baseline::jit_step_one`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Continue at `pc + 1` in the same function.
    Next,
    /// Jump within the module.
    Goto {
        /// The function index (into [`beam_loader::Module::functions`]).
        func: u32,
        /// The code offset within that function.
        pc: u32,
    },
    /// Suspend the process in a `receive`: the module resumes at this address when
    /// the wait is released by an arriving message.
    ///
    /// The dispatch loop reports it as [`crate::RunOutcome::Wait`] (leaving the
    /// registers/frames untouched so the host can re-enter at the same address via
    /// [`crate::Machine::resume_outcome`]); a **non-blocking** entry
    /// ([`crate::Machine::run_export`] and the JIT backends' `run_export`) folds it
    /// into the typed [`crate::error::blocked`] error instead.
    Wait {
        /// The function index (into [`beam_loader::Module::functions`]) to resume in.
        func: u32,
        /// The code offset within that function to resume at.
        pc: u32,
    },
    /// The instruction at `(func, pc)` was **not applied** and must be re-executed when
    /// the process resumes there.
    ///
    /// Produced when a BIF answers
    /// [`BifOutcome::Suspend`](crate::BifOutcome::Suspend) at a re-execution-idempotent
    /// dispatch site: the BIF could not complete without waiting (a plain
    /// `erlang:port_command/2,3` at a busy port, a blocking port write) and the host
    /// will re-issue it from the same instruction with the same operands once it has
    /// pumped. The difference from [`Step::Wait`] is only *who* releases the park: the
    /// mailbox there, the host here — with [`Step::Wait`] the instruction (`loop_rec`)
    /// *did* run and the resume address is the one it reports; with `Suspend` the
    /// instruction did **not** run and the address is the instruction itself.
    ///
    /// The dispatch loop reports it as [`crate::RunOutcome::Suspend`] (leaving the
    /// registers/frames untouched so the host can re-enter at the same address via
    /// [`crate::Machine::resume_outcome`], which re-executes the instruction); a
    /// **non-blocking** entry folds it into the typed
    /// [`crate::error::suspended`] error instead. The engine keeps no suspended-BIF
    /// state: the instruction's operands are re-read from `x`/`y`/literals/stack, i.e.
    /// from the frozen [`crate::context::RootSet`], so they are rooted by construction.
    Suspend {
        /// The function index (into [`beam_loader::Module::functions`]) holding the
        /// instruction to re-execute.
        func: u32,
        /// The code offset of the instruction to re-execute.
        pc: u32,
    },
    /// Switch the machine's **module context** and continue at `(func, pc)` in that
    /// module.
    ///
    /// Produced by the two places where control crosses a module boundary *inside*
    /// one machine: the `call_ext*` arms, when the host resolved the cross-module
    /// call through
    /// [`ExecutionContext::resolve_external_call`](crate::ExecutionContext::resolve_external_call),
    /// and a `return` (or an exception landing) whose target frame/continuation
    /// belongs to a module other than the one executing.
    ///
    /// `context` is a [`Machine`](crate::Machine)-owned module-context id: `0` is
    /// the module the run was entered in, `n > 0` indexes the seam-registered
    /// contexts. Unlike [`Step::Goto`] (a jump **within** the current context),
    /// this makes the driver rebind the module, its label map and the machine's
    /// literal area before executing `(func, pc)`.
    Enter {
        /// The module context to switch to (see [`Step::Enter`]).
        context: u32,
        /// The function index within that context's module.
        func: u32,
        /// The code offset within that function.
        pc: u32,
    },
    /// Finish the run, yielding `x(0)`.
    Done,
}

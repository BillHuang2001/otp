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

//! # `beam_engine` — typed-IR execution engine (interpreter + JIT tiers)
//!
//! Executes the **frozen typed, block-structured IR** produced by `beam_loader`: a
//! **direct-threaded interpreter** over the Milestone-1 opcode subset, behind a
//! real **execution-tier seam** ([`jit`]) that the native JIT tiers plug into.
//! Implements the Stage-1 engine half of Part II §H with the IR contract of §E and
//! the interpreter/JIT trade-offs of §F (`../../CONTEXT.md` §4).
//!
//! ## Execution tiers
//!
//! [`jit::Tier`] enumerates the tiers (`Interpreter`, `BaselineJit`, `Cranelift`),
//! [`jit::available_tiers`] reports the tier **names** compiled into this build
//! (`"interpreter"` first, then `"baseline-jit"`, plus `"jit-cranelift"` with the
//! `jit-cranelift` feature), and [`Machine::run_with_tier`] selects one. The
//! interpreter is the reference tier and
//! [`Machine::run_export`] remains its default entry point.
//!
//! The **native baseline tier is built** ([`jit::baseline`]): each basic block is
//! lowered to a real x86-64 `extern "C"` function and an export runs through a
//! driver loop that deopts to one interpreter step per instruction it does not
//! lower natively. The **optional Cranelift hot tier is built too**, behind the
//! default-off `jit-cranelift` feature; with the feature off `Cranelift` is a typed
//! `unsupported` error rather than a silent fallback. [`jit::JitBackend`] is the
//! trait those backends implement (`compile` a [`beam_loader::Module`] into their
//! own artifact, `run_export` it on a [`Machine`]); Cranelift stays off the
//! critical path — with `jit-cranelift` disabled the crate carries no Cranelift
//! dependency at all.
//!
//! This crate is on the **critical path of the vertical slice**: it loads a real
//! `.beam` (via `beam_loader`) and runs it end to end.
//!
//! The engine depends only on `beam_loader` (+ `beam_term`/`beam_common`) — never
//! on the Stage-5 runtime core. Everything the runtime must supply (BIF dispatch,
//! heap/GC, process/stack limits, the mailbox) is abstracted behind the
//! [`ExecutionContext`] trait; `beam_rt` provides the concrete implementation.
//! This keeps the Stage-1/Stage-5 DAG acyclic and lets the engine be tested with a
//! minimal context.
//!
//! A `receive` that finds no message available **suspends** the process: the
//! blocking-aware entries ([`Machine::run_export_outcome`]/
//! [`Machine::resume_outcome`], and the tier-selecting
//! [`Machine::run_export_outcome_with_tier`]/[`Machine::run_label_outcome_with_tier`])
//! report it as [`RunOutcome::Wait`], while the plain
//! [`Machine::run_export`]/[`Machine::run_label`] (and the native JIT tiers'
//! non-blocking entries) fold it into the typed [`error::blocked`] error.
//!
//! ## Status
//!
//! The crate is **implemented** for its Milestone-1 scope: the interpreter
//! dispatch loop and the operand read/write bodies are filled in, [`Machine`]
//! runs a real `.beam` (via `beam_loader`) end to end, and the vertical-slice
//! integration test exercises it. The documented coverage gaps (floats, funs,
//! maps, bitstrings, in-VM exceptions, GC) are unchanged from the design in
//! this crate's `CONTEXT.md` and each returns a typed [`beam_common::BeamError`].
//! Static literals (`Operand::Literal`) are **supported**: the running module's
//! decoded literal area is bound to the [`Machine`] for a run and read through
//! [`operand::read_operand`].
//!
//! The crate is `#![deny(unsafe_code)]`. Every exception is SAFETY-annotated and
//! confined to a single responsibility: `predicates` wraps the `beam_term`
//! boxed-term predicates (`unsafe fn` because it dereferences a header word) and
//! `terms` is the crate's single point of raw-heap term construction/reading
//! behind the [`ExecutionContext::alloc_words`] seam — both allowed where they are
//! declared below. A further set of modules carry a module-local
//! `#![allow(unsafe_code)]`: `context` (the `MinimalContext` region heap is the
//! crate's raw `beam_alloc::ReservedHeap` bump seam — see
//! [`context::MinimalContext`]), `families::bitstrings::raw` (the frozen bitstring
//! layouts), [`jit::emitter`] (the x86-64 encoder and the crate's only
//! executable-memory mapping — see that module for its W^X policy), the baseline
//! backend (`jit::baseline`), and the feature-gated `jit::cranelift`.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod context;
pub mod error;
mod families;
mod generic;
pub mod interpreter;
pub mod jit;
pub mod machine;
pub mod operand;
#[allow(unsafe_code)]
mod predicates;
mod step;
#[allow(unsafe_code)]
pub mod terms;
pub use context::{ExecutionContext, Mailbox, MinimalContext, TraceEvent};
pub use error::{bad_register, blocked, unknown_label, unsupported};
pub use jit::{available_tiers, CodeRegion, JitBackend, Tier, TierPolicy};
pub use machine::{CatchFrame, Machine, RunOutcome, MAX_REGISTER};
pub(crate) use step::Step;

/// The conventional glob-import surface for downstream crates.
///
/// `use beam_engine::prelude::*;` brings the [`ExecutionContext`] seam (with its
/// execution-trace hook [`TraceEvent`]) and the mailbox seam [`Mailbox`], the
/// [`Machine`] core (with its register-file bound [`MAX_REGISTER`] and the
/// blocking-aware run outcome [`RunOutcome`]) and the execution-tier selector
/// [`Tier`] (for [`Machine::run_with_tier`]) into scope.
pub mod prelude {
    pub use crate::context::{ExecutionContext, Mailbox, MinimalContext, TraceEvent};
    pub use crate::jit::Tier;
    pub use crate::machine::{Machine, RunOutcome, MAX_REGISTER};
}

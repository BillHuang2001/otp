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

//! **Execution-tier selection** for the runner's first entry, and the soundness argument for
//! putting every scheduled process on the native baseline tier.
//!
//! # The two tiers
//!
//! [`Tier::Interpreter`] is the direct-threaded interpreter
//! ([`beam_engine`]'s `interpreter`) — the reference semantics every other tier must match.
//! [`Tier::BaselineJit`] is the engine's native baseline tier (`beam_engine`'s `jit::baseline`):
//! it compiles each IR basic block to an x86-64 `extern "C"` function and runs the module
//! through a driver loop whose **per-instruction deopt** is literally
//! `Machine::step_instruction` — the interpreter's own step. Anything a block does not inline
//! (and every instruction the tier has no lowering for) therefore runs the *same* semantics,
//! one instruction at a time, so the tier is interpreter-identical by construction rather than
//! by parallel implementation.
//!
//! # Why the baseline tier is sound for *any* module
//!
//! A step that reports [`Step::Enter`](beam_engine::interpreter) — "continue in *another*
//! module's code" — was historically the one shape a native tier could not take: a compiled
//! block's code, label map and literal area are all bound to a single module. The engine's
//! driver now handles it. A `Step::Enter` (raised by its own interpreter step, or handed back
//! as the `ENTER` word by a compiled `call_ext`/`return`) switches the machine context
//! (`Machine::switch_context`), rebinds the module / label map / literal area and continues in
//! the **callee's** compiled blocks — including the first sight of a module the context switch
//! has not compiled yet. The transition is a driver round-trip by construction (each
//! `CompiledModule` owns its own executable mapping and the runtime's module/label pointers are
//! baked for one module), which is a performance property, not a correctness one.
//!
//! Two further producers — the interpreter's `return_step` and `Machine`'s `catch`-landing —
//! emit `Step::Enter` only when returning/raising *across* a module context the run is already
//! in, so they need no separate handling.
//!
//! There is therefore **no module the baseline tier cannot run**, and no per-module
//! switch-safety predicate is needed. One host-side obligation remains, and it is enforced
//! outright rather than by a predicate:
//!
//! * **The host's cross-module seam must be idempotent.** The native `call_ext*` arm resolves
//!   the import through [`ExecutionContext::resolve_external_call`] natively, and when the host
//!   declines (`Ok(None)`) the *whole instruction* is re-run by the interpreter step — which
//!   consults the same seam a **second** time. A seam whose answer changed between the two
//!   consultations (a module cached by the first one) would route the callee two different ways
//!   and could enter it in-machine under the caller's module identity. See
//!   [`RuntimeContext::resolve_external_call`](crate::context), which is written to be
//!   idempotent for exactly this reason.
//!
//! # The tier survives a suspension
//!
//! The tier is chosen for a process's **first** entry and remembered on its engine
//! ([`ProcessEngine::tier`](super::entry::ProcessEngine)): a slice that pre-empts on the
//! reduction budget, or a `receive` that parks, re-enters through
//! [`Machine::resume_outcome_with_tier`](beam_engine::Machine::resume_outcome_with_tier) on that
//! **same** tier instead of dropping to the interpreter, so an allocating hot loop keeps its tier
//! across its whole life. An interpreter-tier process is unaffected —
//! [`Tier::Interpreter`](beam_engine::Tier::Interpreter) delegates to the plain
//! [`Machine::resume_outcome`](beam_engine::Machine::resume_outcome).
//!
//! # Why the default is the native tier
//!
//! The tier costs nothing observable (it is interpreter-identical, including every `RunOutcome`
//! suspension address and every typed error) and never regresses a workload, so the **default
//! selects the baseline tier for every process entry**. Measured on a release build with no
//! per-process `erl` start-up (the system code path resolves natively from the filesystem),
//! alternating runs, min wall seconds:
//!
//! | workload | shape | interpreter | baseline (the default) |
//! |---|---|---|---|
//! | `slice:fib/1 300000` (committed fixture) | bignum loop | 0.141 s | 0.113 s |
//! | `preempt:alloc_loop/1 200000` (committed fixture) | allocating `test_heap` loop, pre-empted every slice | 4.51 s | 0.298 s |
//!
//! The `preempt` row is the shape that used to be *refused* the JIT entirely: `alloc_loop/1`
//! allocates in every iteration, so the slice's reduction budget pre-empts it at a
//! `test_heap` admission point and the process is resumed — on the remembered tier — many
//! times. With the tier carried across a resume and no switch-safety gate to fail, it keeps the
//! native tier for its whole life instead of running every slice on the interpreter.
//!
//! [`BEAM_RT_TIER=interpreter`](JitMode::Interpreter) is the kill switch that pins the
//! reference tier; the pre-JIT measurements and every differential-gate baseline are taken
//! there.

use std::sync::OnceLock;

use beam_engine::Tier;

use super::entry::{ProcessEntry, RunnerState};
use crate::runtime::Runtime;

/// How the runner picks the execution tier for a process.
///
/// The mode selects the tier at a process's **first** entry, and the selection is remembered
/// on the process (see [`ProcessEngine::tier`](super::entry::ProcessEngine)): every later
/// resume — a pre-emption or a `receive` park — re-enters on that same tier through
/// [`Machine::resume_outcome_with_tier`](beam_engine::Machine::resume_outcome_with_tier).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum JitMode {
    /// The native baseline tier for **every** process entry. **The default.**
    ///
    /// This is both the unset-`BEAM_RT_TIER` default and the `BEAM_RT_TIER=auto` value. It is
    /// the same selection [`JitMode::Baseline`] forces: the engine's driver handles the
    /// module-context switch, so there is no module that has to be inspected (or refused)
    /// first — see the module docs for the soundness argument.
    #[default]
    Auto,
    /// The interpreter unconditionally (the reference tier, and the kill switch).
    ///
    /// The `BEAM_RT_TIER=interpreter` value: the tier the differential gate and the pre-JIT
    /// measurements are taken against, and the one to pin when a divergence is being chased.
    Interpreter,
    /// The native baseline tier for **every** process entry, requested explicitly.
    ///
    /// The `BEAM_RT_TIER=baseline` value. It selects exactly what [`JitMode::Auto`] selects
    /// today, so it is a measurement/debugging alias of the default rather than a different
    /// policy; the two are kept distinct because the environment surface names both and
    /// `interpreter` must remain the only value that turns the tier off.
    Baseline,
}

/// [`JitMode`] for one `BEAM_RT_TIER` value — the **pure** resolution, and the single
/// definition of the variable's semantics.
///
/// * `None` (the variable is unset) → [`JitMode::Auto`] — **the default**: the native baseline
///   tier for every process entry (see the module docs).
/// * `Some("auto")` → [`JitMode::Auto`] — the same mode, named explicitly.
/// * `Some("baseline")` → [`JitMode::Baseline`] — the same selection, forced explicitly.
/// * `Some("interpreter")` and any other value → [`JitMode::Interpreter`] — an unknown value
///   resolves to the always-correct interpreter rather than being an error (so a typo re-enables
///   the reference tier instead of silently enabling the JIT).
///
/// Pure (no environment read), so a test can pin the semantics without mutating the
/// process environment (the crate is `#![deny(unsafe_code)]`, and `set_var` is `unsafe` in
/// edition 2024).
pub(super) fn mode_from_value(value: Option<&str>) -> JitMode {
    match value {
        None | Some("auto") => JitMode::Auto,
        Some("baseline") => JitMode::Baseline,
        _ => JitMode::Interpreter,
    }
}

/// The process-wide default tier mode from the `BEAM_RT_TIER` environment variable.
///
/// The variable's semantics are [`mode_from_value`]'s; an unset value is the default
/// [`JitMode::Auto`] and an unknown one the always-correct [`JitMode::Interpreter`]. Read
/// **once** (a `OnceLock`), so a long-running process pays no repeated `getenv`.
pub(super) fn env_default() -> JitMode {
    static DEFAULT: OnceLock<JitMode> = OnceLock::new();
    *DEFAULT
        .get_or_init(|| mode_from_value(std::env::var("BEAM_RT_TIER").ok().as_deref()))
}

/// The execution tier for a process's first entry.
///
/// `runtime`, `state` and `entry` are not consulted: the baseline tier is sound for any module
/// (see the module docs), so the mode alone decides. The parameters are kept because this is the
/// runner's single tier-decision seam — a future policy (a Cranelift hot tier, a per-module
/// measurement) would read them here, and the caller
/// ([`RuntimeRunner`](super::RuntimeRunner)) resolves the tier **before** it builds the
/// [`RuntimeContext`](crate::context::RuntimeContext) that borrows the runtime.
///
/// The tier is chosen from the [`ProcessEntry`] alone, i.e. **before** a fun entry's local
/// launch target has been resolved (that resolution needs the context, and happens on the same
/// first run). That is not a limitation here: the selection does not depend on the module, and
/// the remembered tier is used by the label entry the resolved fun takes.
///
/// Called **once** per process, at its first entry; the caller remembers the answer on the
/// engine, so no later slice selects a tier again (see [`JitMode`]).
pub(super) fn select_entry_tier(
    _runtime: &Runtime,
    _state: &mut RunnerState,
    _entry: &ProcessEntry,
    mode: JitMode,
) -> Tier {
    match mode {
        JitMode::Interpreter => Tier::Interpreter,
        JitMode::Auto | JitMode::Baseline => Tier::BaselineJit,
    }
}

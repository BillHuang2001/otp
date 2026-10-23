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
//! # The three tiers
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
//! [`Tier::Cranelift`] is the engine's **optional** hot tier (`beam_engine`'s
//! `jit::cranelift`): it lowers each IR function into a native Cranelift function that loops
//! over the function's control flow and delegates per-instruction semantics to the same
//! interpreter-backed helper, so it is interpreter-identical for the same reason. It is
//! **opt-in twice over** — the runtime's `cranelift` Cargo feature (default OFF) must compile
//! the backend in *and* `BEAM_RT_TIER=cranelift` must select it. See *The Cranelift tier is
//! opt-in* below.
//!
//! # Why the baseline tier is sound for *any* module
//!
//! A step that reports [`Step::Enter`](beam_engine::interpreter) — "continue in *another*
//! module's code" — is the one shape a native tier cannot take on its own: a compiled
//! block's code, label map and literal area are all bound to a single module. The engine's
//! driver handles it. A `Step::Enter` (raised by its own interpreter step, or handed back
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
//! # The Cranelift tier is opt-in, and its absence is a typed error
//!
//! [`Tier::Cranelift`] is never selected by default and never substituted for another tier.
//! It is reachable only when **both** hold: the runtime was built with its `cranelift` Cargo
//! feature, and `BEAM_RT_TIER` names it ([`JitMode::Cranelift`]). The feature is a pure
//! re-export of `beam_engine`'s `jit-cranelift` feature (see this crate's `Cargo.toml`), so the
//! **feature gate has exactly one definition and it is the engine's**: this module never tests
//! `cfg!(feature = …)` and [`select_entry_tier`] never downgrades its answer, because a
//! second gate here could disagree with the backend that is actually compiled in. With the
//! feature off the engine's Cranelift entry points answer the typed capability gap
//! (`unsupported: opcode: jit-cranelift tier not built`), which
//! [`crate::error::unsupported_reason`] classifies as unsupported — the CLI's exit code 3 when
//! that raise is the error the action reports — loud, never a silent downgrade to another tier.
//!
//! A Cranelift **resume** is an ordinary entry of this tier: `beam_engine`'s
//! `jit::run_cranelift_resume_outcome` takes the artifact its retained-artifact store hands back
//! (`beam_engine`'s `jit::cranelift::cache`, keyed by the very `jit::cache::CacheKey` the baseline
//! tier's own cross-run cache uses — a hit for a module this thread already compiled, a compile
//! otherwise) and re-enters the parked address through the shared no-reset native resume seam, so
//! the registers, the Erlang stack and every live call frame survive the suspension. An address
//! the artifact holds no compiled entry for — most visibly a `wait`/`wait_timeout` suspension pc,
//! which no block boundary names — advances by the driver's per-instruction interpreter step, the
//! same deopt every un-lowered instruction already takes. A slice that runs to completion inside
//! its first slice therefore runs on Cranelift, and one that pre-empts on the reduction budget or
//! parks in a `receive` continues **on that same tier** — by compiled code, or by that
//! per-instruction step, never by a typed error and never by a downgrade to another tier. See
//! *The tier survives a suspension* for the mechanism the tier plugs into.
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
//! The remembered tier is observable only at such a **slice boundary**: a process that runs to
//! completion inside its first slice never re-enters. How likely that is depends on the tier.
//! On the **interpreter** tier the slice's budget is charged once per interpreted instruction
//! (and again at a heap-admission point), so an ordinary hot loop — allocating or not — is
//! pre-empted at the budget. On the **native baseline** tier a compiled block runs natively and
//! does not cross the per-instruction hook, so it charges only at a deopt or a heap-admission
//! boundary: a binary-accumulating or `call_ext`-only loop, and equally a natively linked tail
//! loop with no allocation, therefore charges nothing and never pre-empts. That is why the tier
//! is selected **eagerly at the first entry** rather than after a bounded warm-up on the
//! interpreter: the warm-up was measured and declined because it would pin exactly those
//! hot loops to the reference tier for their whole life. The verified characterisation (with
//! figures) is in `PERF.md`.
//!
//! # Why the default is the native tier
//!
//! The tier costs nothing observable (it is interpreter-identical, including every `RunOutcome`
//! suspension address and every typed error), so the **default selects the baseline tier for
//! every process entry**; measured workload figures live in `erts_rust/tools/bench/RESULTS.md`
//! and are re-measured there rather than transcribed here.
//!
//! The optional Cranelift hot tier is deliberately **not** the default: it is off in the shipped
//! build (an extra codegen dependency the standalone runtime does not need to run anything), and
//! promoting a tier is a measurement-gated decision rather than a configuration default. Naming
//! it is how a session opts in.
//!
//! [`BEAM_RT_TIER=interpreter`](JitMode::Interpreter) is the kill switch that pins the
//! reference tier; every differential-gate baseline is taken there.

use std::sync::OnceLock;

use beam_common::BeamError;
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
    /// The optional Cranelift hot tier — `BEAM_RT_TIER=cranelift` (or, spelled as
    /// [`Tier::name`] spells it, `jit-cranelift`).
    ///
    /// Reachable **only** with the runtime's `cranelift` Cargo feature, which is what compiles
    /// `beam_engine`'s `jit-cranelift` backend in (see the module docs, *The Cranelift tier is
    /// opt-in*). With the feature off the selection is still made — [`select_entry_tier`]
    /// answers [`Tier::Cranelift`] and the engine's own feature-gated entry point produces the
    /// typed `unsupported: opcode: jit-cranelift tier not built` capability gap (exit code 3),
    /// so the gate lives in exactly one place and this crate never downgrades the request.
    ///
    /// It is **never substituted for another tier** and never used as a fallback: no other
    /// value resolves here, and this value resolves to nothing else.
    Cranelift,
}

/// [`JitMode`] for one `BEAM_RT_TIER` value — the **pure** resolution, and the single
/// definition of the variable's semantics.
///
/// * `None` (the variable is unset) → [`JitMode::Auto`] — **the default**: the native baseline
///   tier for every process entry (see the module docs).
/// * `Some("auto")` → [`JitMode::Auto`] — the same mode, named explicitly.
/// * `Some("baseline")` → [`JitMode::Baseline`] — the same selection, forced explicitly.
/// * `Some("cranelift")` and `Some("jit-cranelift")` → [`JitMode::Cranelift`] — the optional hot
///   tier. `"jit-cranelift"` is accepted because it is exactly [`Tier::name`]'s spelling, i.e.
///   the same name the engine reports and parses; `"hot"` is deliberately **not** accepted —
///   it names no tier, and a vague near-miss that silently changed the tier would be a
///   typo-safety hazard. Whether the backend is actually compiled in is the `cranelift` Cargo
///   feature's business (see [`JitMode::Cranelift`]), never this function's.
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
        Some("cranelift") | Some("jit-cranelift") => JitMode::Cranelift,
        _ => JitMode::Interpreter,
    }
}

/// The process-wide default tier mode from the `BEAM_RT_TIER` environment variable.
///
/// The variable's semantics are [`mode_from_value`]'s: `interpreter`, `auto`, `baseline`,
/// `cranelift` (and the engine's `jit-cranelift` spelling of it) are the accepted values, an
/// unset value is the default [`JitMode::Auto`] and an unknown one the always-correct
/// [`JitMode::Interpreter`]. Read **once** (a `OnceLock`), so a long-running process pays no
/// repeated `getenv`. The environment is read here and nowhere else — in particular a
/// `cranelift` request is *not* re-tested against the Cargo feature in this crate (see
/// [`JitMode::Cranelift`]).
///
/// # The test-only override
///
/// Under `cfg(test)` [`forced`] answers first: a test that needs a specific tier for the
/// *whole* process (a runtime driven through `Runtime::run_one_slice`, which builds its
/// [`RunnerState`](super::entry::RunnerState) internally and so cannot be reached through
/// [`RunnerState::jit_mode`](super::entry::RunnerState::jit_mode)'s field) pins it there.
/// Production never consults it — the override does not exist outside `cfg(test)`, so the
/// shipped behaviour is byte-identical.
pub(super) fn env_default() -> JitMode {
    #[cfg(test)]
    if let Some(mode) = forced() {
        return mode;
    }
    static DEFAULT: OnceLock<JitMode> = OnceLock::new();
    *DEFAULT
        .get_or_init(|| mode_from_value(std::env::var("BEAM_RT_TIER").ok().as_deref()))
}
#[cfg(test)]
thread_local! {
    /// The tier this thread's runtimes are forced onto (see [`env_default`]), or `None`.
    ///
    /// **Per thread**, not a shared static: `cargo test` runs tests concurrently, and a shared
    /// override would silently change another test's tier. The thread that spawns and drives a
    /// runtime is the thread whose slices resolve the tier, so a `thread_local!` scopes the
    /// override to exactly the test that set it.
    static FORCED: std::cell::Cell<Option<JitMode>> = const { std::cell::Cell::new(None) };
}
/// This thread's forced tier, if [`force`] set one (see [`env_default`]).
#[cfg(test)]
pub(super) fn forced() -> Option<JitMode> {
    FORCED.with(std::cell::Cell::get)
}
/// Forces every execution-tier decision on this thread onto `mode` until [`clear`].
///
/// The test-only knob behind [`env_default`]'s override: a test that must run a workload on a
/// **deterministic** tier (the pre-emption counters of a reduction-bound loop only exist on the
/// interpreter — a natively linked baseline block never crosses the per-instruction hook) calls
/// this, and calls [`clear`] when it is done so a later test on the same harness thread is not
/// affected.
#[cfg(test)]
pub(super) fn force(mode: JitMode) {
    FORCED.with(|forced| forced.set(Some(mode)));
}
/// Drops this thread's forced tier ([`force`]), restoring the production resolution.
#[cfg(test)]
pub(super) fn clear() {
    FORCED.with(|forced| forced.set(None));
}

/// The typed capability gap a build owes a `mode` it cannot serve, or `None` when it can.
///
/// This is the **CLI-facing half** of the rule *"the tier must be built to be selected"*: a
/// request for [`JitMode::Cranelift`] in a build **without** this crate's `cranelift` Cargo
/// feature (default OFF, forwarding `beam_engine/jit-cranelift`) has no backend to run on, so
/// the invocation is refused before any work is done — with a reason that names the Cargo
/// feature the caller has to rebuild with.
///
/// The engine's own feature-gated entry points remain the **second line of defence**: they
/// answer `unsupported: opcode: jit-cranelift tier not built` at the moment a Cranelift entry is
/// actually attempted. The two can never disagree, because they read the *same* feature: this
/// function returns `None` for a feature-on build (the engine's entry points are the built
/// ones) and `Some` for a feature-off one (the engine's entry points would refuse too). The
/// point of the check is *where* the refusal happens — at the CLI boundary, instead of after a
/// boot sequence has already died on it.
///
/// **Pure**: `mode` is the input and the environment is never read, so a test pins the
/// semantics without mutating the process environment. Every mode but an unbuildable
/// [`JitMode::Cranelift`] is `None` in **every** build — a served mode can never be refused
/// here — and `Cranelift` with the feature on is `None` too.
pub(super) fn tier_unavailable(mode: JitMode) -> Option<BeamError> {
    match mode {
        JitMode::Auto | JitMode::Interpreter | JitMode::Baseline => None,
        JitMode::Cranelift if cfg!(not(feature = "cranelift")) => Some(crate::error::unsupported(
            "BEAM_RT_TIER=cranelift: this build has no cranelift execution tier \
             (rebuild with the `cranelift` Cargo feature)",
        )),
        JitMode::Cranelift => None,
    }
}

/// The capability gap the **effective** `BEAM_RT_TIER` request owes this build, if any.
///
/// [`tier_unavailable`] over the process-wide default ([`env_default`]) — the seam
/// [`crate::cli`] consults **once**, before it resolves or drives the boot script, so an
/// invocation that names [`JitMode::Cranelift`] in a build without the `cranelift` Cargo feature
/// reports `beam_rt: unsupported: …` and exits 3 instead of booting processes that would die on
/// the request one at a time. `pub(crate)`: this is a runtime-internal check, not new public
/// surface, and it adds no mode.
pub(crate) fn unbuilt_tier_request() -> Option<BeamError> {
    tier_unavailable(env_default())
}
/// The execution tier for a process's first entry.
///
/// `runtime`, `state` and `entry` are not consulted: the baseline tier is sound for any module
/// (see the module docs), so the mode alone decides. The parameters are kept because this is the
/// runner's single tier-decision seam — a future policy (a per-module measurement) would read
/// them here, and the caller ([`RuntimeRunner`](super::RuntimeRunner)) resolves the tier
/// **before** it builds the [`RuntimeContext`](crate::context::RuntimeContext) that borrows the
/// runtime.
///
/// The tier is chosen from the [`ProcessEntry`] alone, i.e. **before** a fun entry's local
/// launch target has been resolved (that resolution needs the context, and happens on the same
/// first run). That is not a limitation here: the selection does not depend on the module, and
/// the remembered tier is used by the label entry the resolved fun takes.
///
/// [`JitMode::Cranelift`] is answered **unconditionally** ([`Tier::Cranelift`]); this function
/// is not the feature gate. Whether that tier has a backend is decided once, by `beam_engine`'s
/// `jit-cranelift` feature, and a build without it answers the typed capability gap at the
/// entry point — so the request is never silently turned into a different tier, and there is no
/// second copy of the gate here to disagree with the engine.
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
        JitMode::Cranelift => Tier::Cranelift,
    }
}

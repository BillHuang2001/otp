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

//! **Execution-tier selection** for the runner's first entry, and the soundness
//! argument for putting a scheduled process on the native baseline tier.
//!
//! # Why a tier cannot be chosen blindly
//!
//! The engine's baseline-JIT driver
//! ([`beam_engine`]'s `jit::baseline::run_compiled_outcome`) runs one module's compiled
//! basic blocks and deopts to the interpreter step for anything it does not inline. It has
//! **no in-machine module-context switch**: when a step reports
//! [`Step::Enter`](beam_engine::interpreter) — "continue in *another* module's code" — the
//! driver cannot rebind the block/label/literal context and returns the hard typed
//! `unsupported("jit: module context switch outside the interpreter driver")`. On the
//! runner path that error becomes the process's **exit reason**, i.e. a *wrong observable
//! result* against the oracle — so the tier must never be selected for a module from which
//! that step is reachable.
//!
//! # The only producers of a module-context switch
//!
//! `Step::Enter` is produced by exactly three instruction shapes (verified against the
//! engine source):
//!
//! * [`Instr::CallExt`]/[`Instr::CallExtOnly`]/[`Instr::CallExtLast`] — via
//!   `resolve_external_entry` → [`ExecutionContext::resolve_external_call`]; the step is
//!   produced **iff** that seam answers `Ok(Some((callee, label)))` for a *statically
//!   known* `{module, function, arity}` (the import table's).
//! * [`Instr::Apply`]/[`Instr::ApplyLast`] — via `apply_target` →
//!   [`ExecutionContext::resolve_external_call`] on a **dynamic** `x(arity)`/`x(arity+1)`
//!   module/function, so the target can never be proven synchronous statically.
//! * [`Instr::CallFun`]/[`Instr::CallFun2`] are **safe**: they go through the synchronous
//!   [`ExecutionContext::apply_fun`] and never report `Step::Enter`.
//!
//! Two further producers (`the interpreter's `return_step` and `Machine`'s
//! `catch`-landing) emit `Step::Enter` only when *returning/raising across* a module
//! context the run is **already in** — unreachable when no instruction ever switched the
//! run out of its entry module, so they add no precondition.
//!
//! # The predicate
//!
//! [`jit_is_switch_safe`] therefore holds exactly when the module contains **no
//! `Apply`/`ApplyLast`** and **every** `CallExt*` target is *provably synchronous*.
//! Synchronous is proven by the three `Ok(None)` short-circuits of
//! [`RuntimeContext::resolve_external_call`](crate::context) — the floor namespaces
//! ([`crate::native::FLOOR_MODULES`]), a module with a loaded NIF
//! ([`crate::nif::NifRegistry::is_loaded`]), and a registered `beam_bifs` key
//! ([`beam_bifs::BifRegistry::contains`]) — evaluated **in that order, with the very same
//! arguments the seam receives**. Those three checks are the *sufficient* conditions for
//! `Ok(None)`; the seam's remaining `Ok(None)` cases (a load failure, the float guard, a
//! call-site observer, an unknown export) can only add safety, so the predicate is
//! **conservative** — it may refuse the JIT for a module that would in fact have run fine,
//! but it never permits one that could switch.
//!
//! The verdict is **sticky in the safe direction**: `FLOOR_MODULES` is a constant and the
//! NIF/registry checks only ever gain entries (a `load_nif` *adds* a loaded library; no
//! registry key is ever removed at run time), so a module proven safe stays safe. That is
//! what makes the per-module memo in [`RunnerState`](super::RunnerState) sound; a stale
//! *unsafe* verdict it might retain is only a missed optimisation (the interpreter is
//! always correct), never a wrong result.

use std::sync::OnceLock;

use beam_engine::Tier;
use beam_loader::{ImportRef, Instr, Module};

use super::entry::{ProcessEntry, RunnerState};
use crate::nif::NifRegistry;
use crate::runtime::Runtime;

/// How the runner picks the execution tier for a process's **first** entry.
///
/// The tier only ever applies to a first entry: a resumed process continues the machine
/// it suspended in, on the interpreter, with no tier entry at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum JitMode {
    /// The baseline JIT exactly when [`jit_is_switch_safe`] proves the entry module free
    /// of a module-context switch; the interpreter otherwise. The default.
    #[default]
    Auto,
    /// The interpreter unconditionally (the reference tier).
    Interpreter,
    /// The baseline JIT unconditionally.
    ///
    /// This is an **explicit override of the safety predicate** (a measurement/debugging
    /// knob), so it is only ever chosen deliberately, never by [`JitMode::Auto`].
    Baseline,
}

/// The process-wide default tier mode from the `BEAM_RT_TIER` environment variable.
///
/// `"interpreter"` → [`JitMode::Interpreter`], `"baseline"` → [`JitMode::Baseline`], and
/// anything else — including an unset variable — the sound default [`JitMode::Auto`]. Read
/// **once** (a `OnceLock`), so a long-running process pays no repeated `getenv`.
pub(super) fn env_default() -> JitMode {
    static DEFAULT: OnceLock<JitMode> = OnceLock::new();
    *DEFAULT.get_or_init(|| match std::env::var("BEAM_RT_TIER").as_deref() {
        Ok("interpreter") => JitMode::Interpreter,
        Ok("baseline") => JitMode::Baseline,
        _ => JitMode::Auto,
    })
}

/// The execution tier for a process's first entry.
///
/// Resolves the entry module (a [`ProcessEntry::Fun`] whose local target has not been
/// resolved yet is conservatively the interpreter, because its module is only known once
/// the context — which this must run *before* — resolves it) and, for
/// [`JitMode::Auto`], consults the memoized switch-safety verdict.
pub(super) fn select_entry_tier(
    runtime: &Runtime,
    state: &mut RunnerState,
    entry: &ProcessEntry,
    mode: JitMode,
) -> Tier {
    match mode {
        JitMode::Interpreter => Tier::Interpreter,
        JitMode::Baseline => Tier::BaselineJit,
        JitMode::Auto => match entry.entry_module() {
            Some(module) if module_is_switch_safe(runtime, state, module) => Tier::BaselineJit,
            _ => Tier::Interpreter,
        },
    }
}

/// [`jit_is_switch_safe`] memoized per module name in `state`.
///
/// A module with no resolvable name is never memoized (and is never safe): the name is the
/// only key the runner has, so an unnamed module stays on the interpreter rather than
/// sharing a memo entry with a different one.
fn module_is_switch_safe(runtime: &Runtime, state: &mut RunnerState, module: &Module) -> bool {
    let Some(name) = module.atom_name(module.name) else {
        return false;
    };
    if let Some(&safe) = state.jit_safety.get(name) {
        return safe;
    }
    let safe = jit_is_switch_safe(runtime, module);
    state.jit_safety.insert(name.to_string(), safe);
    safe
}

/// Whether `module`'s code can run on the baseline JIT **without ever** reporting a
/// module-context switch (`Step::Enter`).
///
/// `true` iff the module has no `apply`/`apply_last` and every `call_ext*` target is
/// provably synchronous — see the module docs for the argument and the exact conditions.
/// A module that cannot be proven safe is reported unsafe, so a `false` verdict only ever
/// costs the faster tier, never a wrong result.
pub(super) fn jit_is_switch_safe(runtime: &Runtime, module: &Module) -> bool {
    // Conservative: a registry already borrowed elsewhere cannot be inspected, and the
    // borrow always succeeds on the runner path (this runs before the context is built).
    let Ok(nifs) = runtime.nifs.try_borrow() else {
        return false;
    };
    for function in &module.functions {
        for instr in &function.code {
            match instr {
                // A dynamic `M,F,A`: the target is a register value, so it can never be
                // proven synchronous.
                Instr::Apply { .. } | Instr::ApplyLast { .. } => return false,
                Instr::CallExt { target, arity }
                | Instr::CallExtOnly { target, arity }
                | Instr::CallExtLast { target, arity, .. } => {
                    if !call_ext_is_synchronous(runtime, module, *target, *arity, &nifs) {
                        return false;
                    }
                }
                _ => {}
            }
        }
    }
    true
}

/// Whether one `call_ext*`'s target is provably synchronous — i.e. whether
/// [`RuntimeContext::resolve_external_call`](crate::context) is guaranteed to answer
/// `Ok(None)` for it, leaving the call on the synchronous `call_bif` path.
///
/// The seam is handed the **import entry's** `{module, function, arity}`
/// (`beam_engine`'s `interpreter::import_target`), so the registry lookup uses that arity;
/// an instruction whose `arity` operand disagrees with the import is not trusted.
fn call_ext_is_synchronous(
    runtime: &Runtime,
    module: &Module,
    target: ImportRef,
    instruction_arity: u32,
    nifs: &NifRegistry,
) -> bool {
    let Some(import) = module.imports.get(target.0 as usize) else {
        return false;
    };
    if import.arity != instruction_arity {
        return false;
    }
    let (Some(module_name), Some(function_name)) = (
        module.atom_name(import.module),
        module.atom_name(import.function),
    ) else {
        return false;
    };
    crate::native::FLOOR_MODULES.contains(&module_name)
        || nifs.is_loaded(module_name)
        || runtime.registry.contains(module_name, function_name, import.arity)
}

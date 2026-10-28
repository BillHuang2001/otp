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

//! The direct-threaded dispatch loop (`impl Machine`).
//!
//! One `match` arm per Milestone-1 [`beam_loader::Instr`] variant implements the
//! interpreter model of Part II §F without `unsafe`/computed-goto (the IR is
//! pre-decoded, so typed dispatch is equivalent to a jump table). The
//! instruction bodies reproduce the C interpreter exactly: the frame/continuation
//! conventions of `../../erts/emulator/beam/emu/macros.tab` and the
//! `gc_bif`/common-BIF semantics of `../../erts/emulator/beam/emu/beam_emu.c`.
//!
//! Failure is return-based: a `gc_bif`/`bif`/`call_ext` error with a live fail
//! label branches (guard failure); without one it **raises** — landing in a live
//! `try`/`catch` frame when there is one (C's `handle_error`/`next_catch`,
//! `../../erts/emulator/beam/beam_common.c:580-679`, reached from `gc_bif`/
//! `bif`/`call_light_bif` in `emu/macros.tab:266-278` and
//! `emu/bif_instrs.tab:308-314`) and otherwise propagating out of the run as
//! `Err(BeamError)`. Every unsupported opcode/operand is a documented typed error,
//! never a panic.
//!
//! The **label/import index** ([`LabelMap`]) is a pure function of a module image's
//! immutable content, so it is owned by the **image**, not by the machine that runs it:
//! `beam_loader::labels` holds the type, the whole-module build ([`build_label_map`])
//! and the per-image slot (`beam_loader::labels::SharedLabelMap`, reached through
//! [`Module::label_map`](beam_loader::Module::label_map)), which builds the map **at most
//! once in the image's lifetime** and hands out an `Arc` clone after that. Every machine
//! that runs the image — and every JIT artifact compiled from it — therefore shares the
//! one allocation. This file re-exports that surface under its historical local names
//! ([`LabelMap`], [`build_label_map`], [`resolve`], [`resolve_label`],
//! [`import_target`]) so no consumer path changes.
//!
//! The module is split by responsibility: this file owns the module docs and those
//! re-exports plus the two label resolvers; the sibling submodules own the dispatch
//! loop (`dispatch`), the BIF marshalling (`bif`) and the `Test` evaluation
//! (`test`).

use beam_common::Result;
use beam_loader::Label;
// `Module`/`ImportRef` are named only by the test-only from-scratch resolver below and
// by the `tests` submodule's `use super::*`.
#[cfg(test)]
use beam_loader::{ImportRef, Module};

mod bif;
mod dispatch;
mod select;
mod test;
mod trace;
#[cfg(test)]
mod tests;

// The one definition of the failed-clause `function_clause` raise, shared with the
// instruction families (the maps read path's absent-key `NO_FAIL` branch).
pub(crate) use dispatch::raise_function_clause_no_fail;
// The `select_val` pair ladder (`select.rs`): the single decision authority the
// interpreter's `SelectVal` arm **and** both native tiers' `select_val` plans use,
// so no tier can drift from another's notion of "this alternative matches".
pub(crate) use select::constant_word;
#[cfg(test)]
pub(crate) use select::{reset_select_answers, select_answers};
// The frame discipline of the `call_ext*` forms, shared with a sibling module that
// drives `Machine::call_ext_dispatch` for a given form. Its consumer is the JIT tier
// (`crate::jit`), so the import is not itself "used" by anything in this module today
// — hence the `allow` (the same idiom `families/bitstrings/matching` uses for its
// path-stability re-exports).
#[allow(unused_imports)]
pub(crate) use dispatch::ImportFrame;
// The `tests` submodule is moved verbatim from the former single-file
// `interpreter.rs`; it relies on `use super::*` for the names that file had in
// scope, so the ones not otherwise needed here are imported test-only.
#[cfg(test)]
use beam_common::BeamError;
#[cfg(test)]
use beam_loader::{Instr, NO_FAIL, Operand};
#[cfg(test)]
use crate::machine::Machine;
#[cfg(test)]
use crate::Step;
#[cfg(test)]
use self::bif::STACK_ARGS;

// The label/import index was **lifted to the image it describes**: `beam_loader`'s
// `labels` module now owns `LabelMap`, the whole-module `build_label_map` and the
// per-image `SharedLabelMap` slot behind
// [`Module::label_map`](beam_loader::Module::label_map). These re-exports keep every
// `crate::interpreter::…` path (and every `&LabelMap` signature) unchanged, so the
// engine's call sites did not move.
//
// `LabelMap` is the type those signatures name and `build_label_map` still has a
// non-test caller (`jit::baseline::emit::regalloc::allocate_registers`, the
// `#[cfg_attr(not(test), allow(dead_code))]` convenience entry the emitter suites use),
// so both stay ungated. The bare `import_target` is only the uncached fallback the
// in-crate suites compare against, and the build tally is a diagnostic: those three are
// test-only.
pub(crate) use beam_loader::labels::{build_label_map, LabelMap};
#[cfg(test)]
pub(crate) use beam_loader::labels::{import_target, label_map_builds, reset_label_map_builds};

/// Resolve a label against an already-built index; a missing label is a typed error.
pub(crate) fn resolve(labels: &LabelMap, label: Label) -> Result<(u32, u32)> {
    labels.resolve(label)
}

/// Resolve a module-local label to its `(function index, code offset)` against a
/// **fresh** build of the module's index — the test-only convenience.
///
/// Production never calls this: a run resolves its labels against the **image's** own
/// index ([`Module::label_map`], reached through [`Machine::label_map`](crate::Machine),
/// which [`Machine::run_label_outcome`](crate::Machine::run_label_outcome), the dispatch
/// loop and the native tier all go through, so a re-entered image builds nothing), and a
/// label handed in is resolved against the map the run will execute with ([`resolve`]).
/// This per-call builder is compiled only for tests, where it pins the semantics of an
/// index built from scratch — and so that no future site can quietly reintroduce a
/// per-call whole-module build.
#[cfg(test)]
pub(crate) fn resolve_label(module: &Module, label: Label) -> Result<(u32, u32)> {
    build_label_map(module).resolve(label)
}

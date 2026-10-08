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
//! The module is split by responsibility: this file owns the module docs and the
//! [`LabelMap`] label/import index ([`build_label_map`], [`resolve`],
//! [`resolve_label`], [`import_target`]); the sibling submodules own the dispatch
//! loop (`dispatch`), the BIF marshalling (`bif`) and the `Test` evaluation
//! (`test`).

use beam_common::Result;
use beam_loader::{ImportRef, Label, Module};

use crate::error;

mod bif;
mod dispatch;
mod test;
mod trace;
#[cfg(test)]
mod tests;

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

pub(crate) struct LabelMap {
    slots: Slots,
    /// `imports[ImportRef.0]`: the resolved `(module, function, arity)` of each
    /// `Module::imports` entry (see [`ImportTarget`] and [`resolve_imports`]).
    imports: ImportTable,
    /// The address of the [`Module`] `imports` was resolved from; `0` (an address
    /// no live `Module` can occupy) marks the table as unusable, so the lookup
    /// falls back to the per-call [`import_target`] and can never feed a name from
    /// a different module.
    imports_owner: usize,
}

/// One `Module::imports` entry, resolved **once per run** into the
/// `(module name, function name, arity)` triple
/// [`ExecutionContext::call_bif`](crate::ExecutionContext::call_bif) takes.
///
/// Resolving one import costs two atom-table lookups, and the interpreter used to
/// repeat them for *every* `gc_bif`/`bif`/`call_ext*` — the hottest instruction
/// class (measured: ~5 ns per BIF call, against ~0.5 ns for a cache hit). The two
/// names are stored **owned** rather than borrowed from the running [`Module`]:
/// the map is also reached through a raw pointer inside the baseline JIT's
/// [`JitRuntime`](crate::jit::baseline), which is a non-generic `#[repr(C)]` type,
/// so the map cannot carry the module's lifetime. Copying the two names once per
/// run is the whole price of the cache.
struct ImportTarget {
    /// The imported module's name (the string `call_bif` dispatches on).
    module: Box<str>,
    /// The imported function's name.
    function: Box<str>,
    /// The import's declared arity (what `call_bif` is called with).
    arity: u32,
}

/// The per-run import table: `table[ImportRef.0]` is the resolved target, or the
/// reason string of the typed error that resolving that import produced (an atom
/// the module's atom table does not hold — see [`import_target`]). An index past
/// the end is the typed `call: import out of range`, exactly as before.
type ImportTable = Vec<std::result::Result<ImportTarget, &'static str>>;

/// The two backing representations of a [`LabelMap`].
enum Slots {
    /// `slots[label.0]` over ids `0..=max`; `slots[0]` is the unused `NO_FAIL`
    /// sentinel and stays `None`.
    Dense(Vec<Option<(u32, u32)>>),
    /// `(label id, target)` sorted by id, for a sparse id space.
    Sparse(Vec<(u32, (u32, u32))>),
}

impl LabelMap {
    /// An empty index: every lookup is the typed [`crate::error::unknown_label`],
    /// and every import falls back to the uncached [`import_target`].
    pub(crate) fn new() -> LabelMap {
        LabelMap {
            slots: Slots::Dense(Vec::new()),
            imports: Vec::new(),
            imports_owner: 0,
        }
    }
    /// Record `label -> (function index, code index)`, an existing entry for the
    /// same label being replaced (labels are module-unique, so this only matters
    /// for a malformed/duplicate table — and it matches the previous last-wins
    /// `BTreeMap::insert`).
    pub(crate) fn insert(&mut self, label: Label, target: (u32, u32)) {
        match &mut self.slots {
            Slots::Dense(slots) => {
                let index = label.0 as usize;
                if index >= slots.len() {
                    slots.resize(index + 1, None);
                }
                slots[index] = Some(target);
            }
            Slots::Sparse(entries) => match entries.binary_search_by_key(&label.0, |(id, _)| *id) {
                Ok(found) => match entries.get_mut(found) {
                    Some(entry) => entry.1 = target,
                    None => {}
                },
                Err(at) => entries.insert(at, (label.0, target)),
            },
        }
    }

    /// Resolve `label` to its `(function index, code index)`; a label the module
    /// does not define is the typed [`crate::error::unknown_label`].
    #[inline]
    pub(crate) fn resolve(&self, label: Label) -> Result<(u32, u32)> {
        match &self.slots {
            Slots::Dense(slots) => match slots.get(label.0 as usize) {
                Some(Some(target)) => Ok(*target),
                _ => Err(error::unknown_label(label)),
            },
            Slots::Sparse(entries) => {
                match entries.binary_search_by_key(&label.0, |(id, _)| *id) {
                    Ok(found) => entries
                        .get(found)
                        .map(|(_, target)| *target)
                        .ok_or_else(|| error::unknown_label(label)),
                    Err(_) => Err(error::unknown_label(label)),
                }
            }
        }
    }

    /// Resolve an import reference to `(module name, function name, arity)`.
    ///
    /// The hot path of every `gc_bif`/`bif`/`call_ext*`: a **cache hit** is one
    /// address check, one bounds check and three loads from the per-run import
    /// table ([`ImportTarget`]) instead of the two atom-table lookups
    /// [`import_target`] performs per call.
    ///
    /// Nothing about the *semantics* changes: the table only answers when it was
    /// built from **this** module (`imports_owner`), and a table miss — an index
    /// past the end (`call: import out of range`), or an import whose module /
    /// function atom is not in the module's table (`call: import module atom` /
    /// `call: import function atom`) — is the exact typed error
    /// [`import_target`] produces for the same import. A map this cache does not
    /// cover (not built from this module: [`LabelMap::new`], `From`, ...) falls
    /// straight through to [`import_target`], so the two can never disagree.
    #[inline]
    fn import_target<'a>(
        &'a self,
        module: &'a Module,
        target: ImportRef,
    ) -> Result<(&'a str, &'a str, u32)> {
        if self.imports_owner == module as *const Module as usize {
            match self.imports.get(target.0 as usize) {
                Some(Ok(entry)) => {
                    return Ok((&entry.module, &entry.function, entry.arity));
                }
                Some(Err(reason)) => return Err(error::unsupported(reason)),
                None => return Err(error::unsupported("call: import out of range")),
            }
        }
        import_target(module, target)
    }
}
impl Default for LabelMap {
    fn default() -> LabelMap {
        LabelMap::new()
    }
}

impl<const N: usize> From<[(Label, (u32, u32)); N]> for LabelMap {
    fn from(pairs: [(Label, (u32, u32)); N]) -> LabelMap {
        let mut map = LabelMap::new();
        for (label, target) in pairs {
            map.insert(label, target);
        }
        map
    }
}

// A test-only counter of `build_label_map` calls on the current thread.
//
// A run must build the module-wide label index **exactly once** (it used to build
// it twice — once to resolve the entry label and once inside the dispatch loop),
// and a focused unit test asserts that. Compiled only under `cfg(test)`, so it is
// absent from production builds and from the `tests/` integration suites (which
// link the non-test library) and cannot perturb their timing.
#[cfg(test)]
thread_local! {
    static LABEL_MAP_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Test-only: reset the current thread's [`build_label_map`] call counter.
#[cfg(test)]
pub(crate) fn reset_label_map_builds() {
    LABEL_MAP_BUILDS.with(|c| c.set(0));
}

/// Test-only: the current thread's [`build_label_map`] call count.
#[cfg(test)]
pub(crate) fn label_map_builds() -> usize {
    LABEL_MAP_BUILDS.with(|c| c.get())
}

pub(crate) fn build_label_map(module: &Module) -> LabelMap {
    #[cfg(test)]
    LABEL_MAP_BUILDS.with(|c| c.set(c.get() + 1));
    // One pass to size the index: a real `.beam` numbers its labels `1..=count`,
    // so `max + 1 == count + 1` and the dense table is exactly as large as the
    // label set (verified across the installed OTP: density is exactly 1.0).
    let mut count = 0usize;
    let mut max = 0u32;
    for function in &module.functions {
        count += function.labels.len();
        for label in function.labels.keys() {
            if label.0 > max {
                max = label.0;
            }
        }
    }
    if max as u64 <= (count as u64).saturating_mul(4) + 64 {
        let mut slots: Vec<Option<(u32, u32)>> = vec![None; max as usize + 1];
        for (func_index, function) in module.functions.iter().enumerate() {
            for (label, &code_index) in &function.labels {
                slots[label.0 as usize] = Some((func_index as u32, code_index as u32));
            }
        }
        LabelMap {
            slots: Slots::Dense(slots),
            imports: resolve_imports(module),
            imports_owner: module as *const Module as usize,
        }
    } else {
        let mut entries: Vec<(u32, (u32, u32))> = Vec::with_capacity(count);
        for (func_index, function) in module.functions.iter().enumerate() {
            for (label, &code_index) in &function.labels {
                entries.push((label.0, (func_index as u32, code_index as u32)));
            }
        }
        entries.sort_unstable_by_key(|(id, _)| *id);
        LabelMap {
            slots: Slots::Sparse(entries),
            imports: resolve_imports(module),
            imports_owner: module as *const Module as usize,
        }
    }
}

/// Resolve **every** import of `module` once, producing the per-run import table
/// [`LabelMap::import_target`] looks up.
///
/// Each entry is either the resolved [`ImportTarget`] or the reason string of the
/// typed error resolution produced, so the cached path reproduces
/// [`import_target`]'s errors exactly (a missing module/function atom is
/// remembered per import, never substituted by a different name).
fn resolve_imports(module: &Module) -> ImportTable {
    module
        .imports
        .iter()
        .map(|import| {
            let module_name = module
                .atom_name(import.module)
                .ok_or("call: import module atom")?;
            let function_name = module
                .atom_name(import.function)
                .ok_or("call: import function atom")?;
            Ok(ImportTarget {
                module: module_name.into(),
                function: function_name.into(),
                arity: import.arity,
            })
        })
        .collect()
}

/// Resolve a label against an already-built index; a missing label is a typed error.
pub(crate) fn resolve(labels: &LabelMap, label: Label) -> Result<(u32, u32)> {
    labels.resolve(label)
}

/// Resolve a module-local label to its `(function index, code offset)`.
pub(crate) fn resolve_label(module: &Module, label: Label) -> Result<(u32, u32)> {
    build_label_map(module).resolve(label)
}

/// Resolve an import reference to `(module name, function name, arity)` — **the
/// uncached fallback**.
///
/// This is the per-call resolution the interpreter used to run for every
/// `gc_bif`/`bif`/`call_ext*` (two atom-table lookups and the `Vec` bounds check).
/// [`LabelMap::import_target`] answers from the per-run cache instead; this
/// function stays the definition of the import semantics and the path taken when
/// the cache cannot answer (a map not built from this module), so both agree
/// exactly — including on the three typed errors below.
fn import_target(module: &Module, target: ImportRef) -> Result<(&str, &str, u32)> {
    let import = module
        .imports
        .get(target.0 as usize)
        .ok_or_else(|| error::unsupported("call: import out of range"))?;
    let module_name = module
        .atom_name(import.module)
        .ok_or_else(|| error::unsupported("call: import module atom"))?;
    let function_name = module
        .atom_name(import.function)
        .ok_or_else(|| error::unsupported("call: import function atom"))?;
    Ok((module_name, function_name, import.arity))
}

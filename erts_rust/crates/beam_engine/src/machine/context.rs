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

//! The **module context**: what the machine executes — a module, its label index
//! and (implicitly) its literal area.
//!
//! A run starts in the module its entry was addressed in (context `0`, the
//! `&Module` the drive loop is handed). A cross-module call the host resolved
//! through
//! [`ExecutionContext::resolve_external_call`](crate::ExecutionContext::resolve_external_call)
//! is executed **in the same machine**, so the machine has to remember the callee
//! module — and its label map — for as long as a suspended frame can return into
//! it. That is what [`ModuleContext`] holds and what
//! [`Machine::contexts`](super::Machine) stores.
//!
//! # Why the machine owns the module
//!
//! A `receive` inside the callee can [`suspend`](crate::RunOutcome::Wait) the
//! process, and [`Machine::resume_outcome`](crate::Machine::resume_outcome)
//! deliberately does **not** reset the machine. The suspended `(func, pc)` are
//! indices into the *callee's* function table, so the engine must still be able to
//! name the callee module when the host resumes the process. Holding the
//! host-supplied `Arc<Module>` here keeps it alive and unmoved for exactly that
//! window; [`Machine::reset`](crate::Machine::reset) (which starts a fresh run)
//! clears the whole set.
//!
//! # Identity and deduplication
//!
//! A context is identified by the **allocation identity** of its `Arc<Module>`
//! (`Arc::ptr_eq`): a host that loads a module once and hands the same `Arc` back
//! reuses one context no matter how many times (or how recursively) it is called,
//! so the context list stays bounded by the number of distinct modules, not by the
//! call count. Two `Arc`s with equal contents but different allocations are two
//! contexts — wasteful but never wrong (they name the same code).
//!
//! # The per-context local-call verdict table
//!
//! A context also owns its module's **local-call verdict table**
//! ([`LocalVerdicts`]), because a module context is exactly the granularity that
//! identifies an image: the running module's table can only ever be the table of the
//! context `Machine::context` names, so the local-call seam's lookup needs no module
//! key at all — see `local_callee.rs` for the whole argument.

use std::cell::RefCell;
use std::sync::Arc;

use beam_loader::Module;

use crate::families::bitstrings::programs::BitstrPrograms;
use crate::interpreter::LabelMap;

use super::LocalVerdicts;

/// One enterable module: the host's module handle plus the label index the
/// dispatch loop resolves module-local labels against.
///
/// The label map arrives **pre-built** from the machine's label-map cache
/// ([`Machine::label_map_arc`](crate::Machine)) when the context is registered, so
/// registering a callee the machine already holds a map for costs no build; it is
/// keyed on this `Arc`'s allocation address (`LabelMap::imports_owner`), exactly as the
/// entry run's own map is, so the per-run import cache works for a callee too.
pub(crate) struct ModuleContext {
    /// The host-owned module handle. Held for as long as a suspended frame can
    /// return into this module.
    pub(crate) module: Arc<Module>,
    /// The module-wide `Label -> (func, pc)` index (and the per-run import table
    /// derived from it).
    pub(crate) labels: Arc<LabelMap>,
    /// This module's **local-call verdict table**
    /// ([`crate::machine::LocalVerdicts`]): one slot per function, read by the
    /// callee's function index on every local call the module makes.
    ///
    /// It lives **here**, in the context, because that is what gives it the
    /// callee's module identity for free: the table was sized from *this*
    /// `Arc<Module>`'s function table and is only ever read while this context is
    /// the one executing (`Machine::context`), so a `Module::clone` or a separate
    /// load of the same `.beam` — a different `Arc`, hence (via `Arc::ptr_eq`
    /// deduplication) a different context — can never read another image's
    /// verdicts. The slots are `Cell`s, so the seam mutates the table through the
    /// shared `Arc`.
    pub(crate) verdicts: LocalVerdicts,
    /// This module's **prepared bitstring programs**
    /// (`crate::families::bitstrings::programs`): the pre-resolved `bs_create_bin`
    /// segment / `bs_match` command programs, addressed by the instruction's
    /// `(function, code offset)`.
    ///
    /// It lives **here**, beside the verdict table and for the same reason: a module
    /// context *is* the module-image identity (`Arc::ptr_eq`-deduplicated), so a
    /// prepared program sized/decode from *this* image can never answer for another.
    /// A prepared program is a pure function of the loaded IR plus the atom/string
    /// tables, so it cannot go stale while the image lives.
    ///
    /// A [`RefCell`] because the family **moves the table out** for the duration of one
    /// instruction (`Machine::take_bitstr_programs`) — the executing code then holds it
    /// locally, which is what lets the segment writers take `&mut Machine` without a
    /// borrow of the context. Nothing else reads it mid-instruction.
    pub(crate) programs: RefCell<BitstrPrograms>,
}

impl ModuleContext {
    /// A context for `module` and its already-built label map.
    fn new(module: Arc<Module>, labels: Arc<LabelMap>) -> ModuleContext {
        let verdicts = LocalVerdicts::for_module(&module);
        ModuleContext {
            module,
            labels,
            verdicts,
            programs: RefCell::new(BitstrPrograms::empty()),
        }
    }
}

/// The machine's registered module contexts, indexed by context id.
///
/// Context `0` is the run's **entry** module, which the machine never owns (the
/// drive loop borrows it), so it has no entry here; the id of a registered context
/// is its position in this list plus one ([`Contexts::register`]).
pub(crate) struct Contexts {
    entries: Vec<Arc<ModuleContext>>,
}

impl Contexts {
    /// An empty set (only the implicit entry context `0` exists).
    pub(crate) fn new() -> Contexts {
        Contexts {
            entries: Vec::new(),
        }
    }

    /// Forget every registered module (a fresh run).
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    /// Register `module` with its already-built label map `labels` and return the
    /// context id (`>= 1`).
    ///
    /// Reuses the existing id when the very same `Arc<Module>` was registered
    /// before (see the module docs on identity), so a repeated or recursive
    /// cross-module call does not grow the list. `labels` comes from the machine's
    /// label-map cache ([`Machine::label_map_arc`](crate::Machine)), so a callee the
    /// machine entered before costs no build; a map for the wrong module would be a
    /// wrong-answer bug, which is why the cache keys on the module's content witness
    /// and its address (`label_cache.rs`).
    pub(crate) fn register(&mut self, module: &Arc<Module>, labels: Arc<LabelMap>) -> u32 {
        for (index, entry) in self.entries.iter().enumerate() {
            if Arc::ptr_eq(&entry.module, module) {
                return index as u32 + 1;
            }
        }
        self.entries
            .push(Arc::new(ModuleContext::new(Arc::clone(module), labels)));
        self.entries.len() as u32
    }

    /// The handle for context `id`, or `None` for the entry context `0` / an id
    /// that was never registered.
    ///
    /// The returned `Arc` is a **clone of the handle**, never a borrow of the set:
    /// the drive loop keeps it for the duration of the context and can therefore
    /// hand the machine `&mut` access while executing in it.
    pub(crate) fn handle(&self, id: u32) -> Option<Arc<ModuleContext>> {
        if id == 0 {
            return None;
        }
        self.entries.get((id - 1) as usize).map(Arc::clone)
    }

    /// The local-call verdict table of context `id`, or `None` for the entry context
    /// `0` (whose table the machine owns —
    /// [`Machine::entry_verdicts`](crate::Machine)) and for an id never registered.
    ///
    /// A borrow, not a clone: this is the local-call seam's hot path, and the slots
    /// are `Cell`s, so the table is mutable through it.
    #[inline(always)]
    pub(crate) fn verdicts(&self, id: u32) -> Option<&LocalVerdicts> {
        if id == 0 {
            return None;
        }
        self.entries
            .get((id - 1) as usize)
            .map(|entry| &entry.verdicts)
    }

    /// Reset every registered context's local-call verdicts to
    /// [`Verdict::Unknown`](crate::machine::Verdict) — the other half of
    /// [`Machine::invalidate_local_call_verdicts`](crate::Machine), which must cover
    /// every context, not only the one the invalidation happened in.
    ///
    /// `&self` because the tables are `Cell`s; one byte per function per context, and
    /// only ever reached from a BIF dispatch or the seam's next local call.
    pub(crate) fn invalidate_verdicts(&self) {
        for entry in &self.entries {
            entry.verdicts.invalidate();
        }
    }

    /// The prepared-bitstring-program cell of context `id`, or `None` for the entry
    /// context `0` (whose table the machine owns —
    /// [`Machine::entry_programs`](crate::Machine)) and for an id never registered.
    ///
    /// A borrow, not a clone: the caller exchanges the table's value for the duration of
    /// one instruction, so a `&` handle and a `RefCell` are all it needs.
    #[inline(always)]
    pub(crate) fn programs(&self, id: u32) -> Option<&RefCell<BitstrPrograms>> {
        if id == 0 {
            return None;
        }
        self.entries
            .get((id - 1) as usize)
            .map(|entry| &entry.programs)
    }    /// The number of registered contexts (not counting the entry context).
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

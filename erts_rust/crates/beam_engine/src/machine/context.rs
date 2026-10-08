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

use std::sync::Arc;

use beam_loader::Module;

use crate::interpreter::LabelMap;

/// One enterable module: the host's module handle plus the label index the
/// dispatch loop resolves module-local labels against.
///
/// The label map is built **once**, when the context is registered, and is keyed
/// on this `Arc`'s allocation address (`LabelMap::imports_owner`), exactly as the
/// entry run's own map is — so the per-run import cache works for a callee too.
pub(crate) struct ModuleContext {
    /// The host-owned module handle. Held for as long as a suspended frame can
    /// return into this module.
    pub(crate) module: Arc<Module>,
    /// The module-wide `Label -> (func, pc)` index (and the per-run import table
    /// derived from it).
    pub(crate) labels: LabelMap,
}

impl ModuleContext {
    /// Build a context for `module`: its label map is derived from the module's
    /// own label table, so a `Label` resolved here matches what
    /// [`Machine::run_export`](crate::Machine::run_export) would resolve for the
    /// same export.
    fn new(module: Arc<Module>) -> ModuleContext {
        let labels = crate::interpreter::build_label_map(&module);
        ModuleContext { module, labels }
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

    /// Register `module` and return its context id (`>= 1`).
    ///
    /// Reuses the existing id when the very same `Arc<Module>` was registered
    /// before (see the module docs on identity), so a repeated or recursive
    /// cross-module call does not grow the list.
    pub(crate) fn register(&mut self, module: &Arc<Module>) -> u32 {
        for (index, entry) in self.entries.iter().enumerate() {
            if Arc::ptr_eq(&entry.module, module) {
                return index as u32 + 1;
            }
        }
        self.entries.push(Arc::new(ModuleContext::new(Arc::clone(module))));
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

    /// The number of registered contexts (not counting the entry context).
    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

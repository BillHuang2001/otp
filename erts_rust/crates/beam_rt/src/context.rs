//! The concrete [`ExecutionContext`] glue: the engine's seam over the runtime core.
//!
//! The Stage-1 engine knows nothing about BIFs, heaps, processes or ETS — it calls
//! `call_bif`/`ensure_heap`/`alloc_words`/`apply_fun` on the trait and lets the host
//! decide (see `../beam_engine/src/context.rs`). This module is the host:
//! [`RuntimeContext`] borrows the [`Runtime`] (its BIF registry, the shared
//! [`AtomTable`](beam_term::AtomTable), the process heap, the calling [`ProcessId`], the
//! ETS [`TableRegistry`] and the output sink) and adapts them onto the engine.
//!
//! It is the **only** place the engine's seam meets the Stage-5 core; that keeps the
//! Stage-1/Stage-5 dependency DAG acyclic (`../CONTEXT.md`).
//!
//! # One context, three dispatch routes
//!
//! The engine routes `call_ext*` **and** `apply`/`apply_last` through
//! [`ExecutionContext::call_bif`], and `call_fun`/`call_fun2` through
//! [`ExecutionContext::apply_fun`]. So [`RuntimeContext::call_bif`] serves, in order:
//!
//! 1. the **runtime-native floor** ([`crate::native`]: `halt`, `display`,
//!    `io:put_chars`, `erlang:apply`, `init:get_argument`);
//! 2. the **`beam_bifs` registry** (the `bif.tab` surface);
//! 3. an **external module call**, loaded on demand from the code path through the
//!    [`CodeServer`](crate::CodeServer) and run with `self` as the context — the
//!    recursion that makes the runtime a real multi-module host.
//!
//! # The shared atom table
//!
//! A [`BifContext`] takes *one* [`AtomTable`](beam_term::AtomTable). The engine resolves
//! atoms — including the ones inside `Module` code — against the **loaded module's**
//! table (`beam_engine`'s `cmp(.., &module.atom_table)`). Because every module the
//! [`CodeServer`](crate::CodeServer) loads is interned into the runtime's **one shared**
//! table, handing the BIFs that same table is exactly right: an atom a BIF builds is an
//! index the engine can compare and the printer can name, for **every** loaded module.
//!
//! # Interning new atoms (the predicted-index commit)
//!
//! Some BIFs (`binary_to_term/1`, `list_to_atom/1`, …) must be able to add a **new** atom
//! to that same shared table, but the [`BifContext`] only borrows it *immutably* for index
//! resolution. Since this crate is `#![deny(unsafe_code)]`, the interner cannot hold a
//! second, mutable borrow of the one table. The [`AtomInterner`] hook solves this without
//! aliasing: [`PendingInterner`] records the names a BIF interns during a call and
//! **predicts** each index as `base + <names already recorded>` (interning is append-only
//! and the runtime is single-threaded, so the prediction is exact); after the BIF returns
//! — releasing the table borrow — [`RuntimeContext::call_registered`] commits the recorded
//! names into the shared table in the same order, so each lands at exactly the predicted
//! index. A name seen twice in one call is deduplicated and gets the same index.
//!
//! # No collector yet
//!
//! [`ensure_heap`](ExecutionContext::ensure_heap) and
//! [`alloc_words`](ExecutionContext::alloc_words) are served by the runtime's
//! pointer-stable [`MinimalContext`](beam_engine::MinimalContext) arena. That arena
//! treats the heap as unbounded, so `ensure_heap` never reports a short heap — there is
//! no GC in the engine yet (`../beam_engine` Known Issues). The production collector is
//! Stage 4 (`beam_gc`), which will run *here* and may move terms.

use std::sync::Arc;

use beam_bifs::{AtomInterner, BifContext, OutputSink};
use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext, Machine};
use beam_loader::Module;
use beam_term::{Atom, Eterm};

use crate::error;
use crate::runtime::Runtime;

/// The engine's host: the runtime, borrowed for the duration of one run, plus the
/// output sink that run writes to.
///
/// A `RuntimeContext` with no sink ([`RuntimeContext::new`]) writes `io:format` output
/// to the process's stdout (`beam_bifs`' default); [`RuntimeContext::with_output`]
/// redirects it — that is how [`Runtime::run_module`] captures output.
pub struct RuntimeContext<'a> {
    /// The runtime: registry, shared atom table, heaps, pid, ETS tables.
    ///
    /// `pub(crate)` for the runtime-native BIF floor ([`crate::native`]), which is a
    /// sibling module of this one and therefore needs explicit visibility.
    pub(crate) runtime: &'a mut Runtime,
    /// The `io:format`/`io:put_chars`/`display` destination; stdout when unset.
    pub(crate) output: Option<&'a mut dyn OutputSink>,
}

impl<'a> RuntimeContext<'a> {
    /// A context over `runtime`, with the default (stdout) output sink.
    pub fn new(runtime: &'a mut Runtime) -> RuntimeContext<'a> {
        RuntimeContext {
            runtime,
            output: None,
        }
    }

    /// A context over `runtime` whose program output goes to `sink`.
    pub fn with_output(runtime: &'a mut Runtime, sink: &'a mut dyn OutputSink) -> RuntimeContext<'a> {
        RuntimeContext {
            runtime,
            output: Some(sink),
        }
    }

    /// Heap words still available to the running code.
    ///
    /// Milestone-1's arena is unbounded (no GC yet), so this reports
    /// `usize::MAX` — see the module docs.
    pub fn heap_available(&self) -> usize {
        self.runtime.arena_available()
    }

    /// Runs `module`'s exported `function/arity` with `args`, on this context.
    ///
    /// This is the direct entry point (no BIF/registry indirection): it is what
    /// [`Runtime::run_function`]/[`Runtime::run_module`] use for the top-level run and
    /// what [`ExecutionContext::call_bif`] uses for an external module call.
    ///
    /// # Errors
    ///
    /// A diagnostic naming `module:function/arity` when the function is not in the
    /// module's atom table or is not exported, plus any error the running code raises.
    pub(crate) fn run_export(
        &mut self,
        module: &Module,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        let name = module.atom_name(module.name).unwrap_or("<unnamed>");
        let function_atom = module
            .atom_table
            .lookup(function)
            .ok_or_else(|| error::unknown_function(name, function, arity))?;
        if module.export(function_atom, arity).is_none() {
            return Err(error::unknown_function(name, function, arity));
        }
        Machine::new().run_export(module, function_atom, arity, args, self)
    }

    /// Dispatches a `beam_bifs`-registered BIF over a [`BifContext`] built from the
    /// runtime's shared state and this context's output sink.
    ///
    /// The context is handed a [`PendingInterner`] so a BIF that builds a *new* atom
    /// (`binary_to_term/1`, `list_to_atom/1`, …) can intern it into the one shared table
    /// without aliasing it: the interner records the names, the BIF runs against the
    /// immutable table, and the names are committed (in order, at their predicted
    /// indices) once the BIF has returned and released the borrow — see the module docs.
    fn call_registered(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        let Runtime {
            registry,
            heap,
            code_server,
            self_pid,
            tables,
            ..
        } = self.runtime;
        // `base` is the shared table's length before the call: the index the first new
        // name gets, because nothing else can intern while this single-threaded call runs.
        let mut pending = PendingInterner::new(code_server.atoms().len());
        // The `BifContext` borrows `code_server`'s table immutably; the interner is a
        // separate local, so the two borrows never overlap.
        let result = {
            let mut context = BifContext::new(heap, code_server.atoms(), *self_pid)
                .with_atom_interner(&mut pending);
            if let Some(sink) = self.output.as_deref_mut() {
                context = context.with_output(sink);
            }
            context = context.with_tables(tables);
            registry.call(module, function, arity, &mut context, args)
        };
        // The BIF is done and the table borrow released, so commit the names it interned:
        // each lands at the index the interner predicted, keeping the BIF's atom terms
        // (and every printer/comparison that resolves them) consistent.
        for name in pending.into_pending() {
            code_server.atoms_mut().intern(&name);
        }
        result
    }
}

/// Interns atom names for the duration of one BIF call, predicting their indices.
///
/// A [`BifContext`] borrows the shared [`AtomTable`](beam_term::AtomTable) immutably for
/// index resolution while the interner must add names to it; this crate is
/// `#![deny(unsafe_code)]`, so the interner cannot hold a coexisting mutable borrow of the
/// same table. Interning is **append-only** and the runtime is **single-threaded**, so a
/// name added during one call always lands at `base + <names recorded so far>` — the
/// interner records the names and predicts that index; [`RuntimeContext::call_registered`]
/// commits them, in order, after the call. A name seen twice in one call is deduplicated
/// and returns the same index.
struct PendingInterner {
    /// The shared table's length when the call began: the first new name's index.
    base: usize,
    /// Names interned during this call, in order (deduplicated).
    pending: Vec<String>,
}

impl PendingInterner {
    /// A pending interner rooted at `base` (the current shared-table length).
    fn new(base: usize) -> PendingInterner {
        PendingInterner {
            base,
            pending: Vec::new(),
        }
    }

    /// The recorded names, in intern order, to be committed by the caller.
    fn into_pending(self) -> Vec<String> {
        self.pending
    }
}

impl AtomInterner for PendingInterner {
    fn intern(&mut self, name: &str) -> Atom {
        if let Some(offset) = self.pending.iter().position(|pending| pending == name) {
            // A duplicate within this call reuses the predicted index.
            return Atom::from_raw((self.base + offset) as u32);
        }
        let index = self.base + self.pending.len();
        self.pending.push(name.to_string());
        Atom::from_raw(index as u32)
    }
}

impl ExecutionContext for RuntimeContext<'_> {
    /// The runtime's unified dispatch — see the module docs.
    ///
    /// 1. The runtime-native floor ([`RuntimeContext::try_native`]).
    /// 2. A registered `beam_bifs` BIF, with the runtime's **shared** atom table.
    /// 3. Otherwise an external module call: the module is loaded on demand from the
    ///    code path and run with `self` as the context. `erlang` is the BIF namespace,
    ///    never a loadable module, so an unknown `erlang:*` call is a typed diagnostic.
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        if let Some(result) = self.try_native(module, function, arity, args) {
            return result;
        }
        if self.runtime.registry.contains(module, function, arity) {
            return self.call_registered(module, function, arity, args);
        }
        if module == "erlang" {
            return Err(error::unknown_bif(module, function, arity));
        }
        // The code server adds the system library directories (stdlib, kernel) the first
        // time a module has to be resolved, so an external call reaches the real stdlib
        // without an explicit `-pa`. A miss here is not fatal: `CodeServer::load` below
        // reports it, naming every directory searched.
        let _ = self.runtime.find_module_file(module);
        let loaded = self.runtime.code_server.load(module)?;
        self.run_export(&loaded, function, arity, args)
    }

    /// Ensures `heap_need` words are available in the runtime's arena.
    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.runtime
            .ensure_heap(heap_need, live, x_regs, stack)
    }

    /// Heap words still available (see [`RuntimeContext::heap_available`]).
    fn heap_available(&self) -> usize {
        RuntimeContext::heap_available(self)
    }

    /// Allocates `n` raw heap words from the runtime's pointer-stable arena.
    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.runtime.alloc_words(n)
    }

    /// Applies a fun/closure value.
    ///
    /// A fun built by the `-eval` evaluator (`uniq` == `eval::EVAL_FUN_UNIQ`) is applied
    /// through the evaluator's own store (`Runtime::eval_closures`): the fun may escape
    /// into a loaded module (`lists:map(fun(X) -> X*X end, …)`) which calls back into it,
    /// and the closure body can only be run by the evaluator. Every other fun is a
    /// `.beam` fun: it carries only its `uniq` (per-module) and `index` (the module-level
    /// lambda index); together they identify the [`FunDescriptor`] — hence the defining
    /// module and its entry label — unambiguously across every module the code server
    /// has loaded. The closure body is entered with the **free variables first**,
    /// followed by the call arguments (a lambda's captured variables are its leading
    /// parameters).
    ///
    /// [`FunDescriptor`]: beam_loader::FunDescriptor
    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        if !terms::is_fun(fun) {
            return Err(BeamError::BadArg);
        }
        let uniq = terms::fun_uniq(fun)?;
        if uniq == crate::eval::EVAL_FUN_UNIQ {
            let index = terms::fun_index(fun)? as usize;
            return crate::eval::apply_eval_closure(self, fun, index, args);
        }
        let index = terms::fun_index(fun)?;
        let mut combined = terms::fun_free(fun)?.to_vec();
        combined.extend_from_slice(args);

        let found = self
            .runtime
            .code_server
            .modules()
            .find(|module| descriptor_of(module, uniq, index).is_some())
            .map(Arc::clone)
            .ok_or(BeamError::BadArg)?;
        let descriptor = descriptor_of(&found, uniq, index).ok_or(BeamError::BadArg)?;
        if combined.len() != descriptor.arity as usize {
            return Err(BeamError::BadArity);
        }
        Machine::new().run_label(&found, descriptor.label, descriptor.arity, &combined, self)
    }
}

/// The `FunDescriptor` of the lambda `(uniq, index)` in `module`, if it defines one.
fn descriptor_of(
    module: &Module,
    uniq: u64,
    index: u32,
) -> Option<beam_loader::FunDescriptor> {
    module
        .funs()
        .iter()
        .find(|descriptor| u64::from(descriptor.uniq) == uniq && descriptor.index == index)
        .copied()
}

#[cfg(test)]
mod tests {
    use beam_bifs::{BifRegistry, BytesSink};
    use beam_term::{atom_val, is_small, make_small, signed_value};

    use super::*;
    use crate::config::RuntimeConfig;

    fn runtime() -> Runtime {
        Runtime::new(RuntimeConfig::default()).expect("the default runtime")
    }

    #[test]
    fn dispatches_a_registered_erlang_bif() {
        let mut rt = runtime();
        let mut ctx = RuntimeContext::new(&mut rt);
        let sum = ctx
            .call_bif("erlang", "+", 2, &[make_small(2), make_small(3)])
            .expect("2 + 3");
        assert!(is_small(sum));
        assert_eq!(signed_value(sum), 5);
        assert_eq!(
            signed_value(
                ctx.call_bif("erlang", "*", 2, &[make_small(6), make_small(7)])
                    .expect("6 * 7")
            ),
            42
        );
    }

    #[test]
    fn a_bif_builds_an_atom_from_the_shared_table() {
        let mut rt = runtime();
        let a = rt.intern("a");
        let mut ctx = RuntimeContext::new(&mut rt);
        let is_atom = ctx
            .call_bif("erlang", "is_atom", 1, &[a])
            .expect("is_atom(a)");
        assert_eq!(
            rt.atoms().name(atom_val(is_atom)),
            Some("true"),
            "the atom the BIF built resolves through the shared table"
        );
    }

    #[test]
    fn an_unknown_erlang_bif_is_a_typed_error_not_a_panic() {
        let mut rt = runtime();
        let mut ctx = RuntimeContext::new(&mut rt);
        // `erlang` is the BIF namespace, never a loadable module.
        let error = ctx
            .call_bif("erlang", "nope", 0, &[])
            .expect_err("not a BIF");
        assert!(error.to_string().contains("erlang:nope/0"), "{error}");
        assert!(ctx
            .call_bif("erlang", "+", 2, &[make_small(1)])
            .is_err(), "wrong arity");
    }

    #[test]
    fn io_format_writes_to_the_installed_sink() {
        let mut rt = runtime();
        let format: Vec<Eterm> = "hi ~w\n".bytes().map(|b| make_small(i64::from(b))).collect();
        let mut sink = BytesSink::new();
        let result = {
            let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
            let format = terms::make_list(&mut ctx, &format).expect("a format list");
            let args = terms::make_list(&mut ctx, &[make_small(7)]).expect("an argument list");
            ctx.call_bif("io", "format", 2, &[format, args])
                .expect("io:format/2")
        };
        assert_eq!(rt.atoms().name(atom_val(result)), Some("ok"));
        assert_eq!(sink.as_str(), "hi 7\n");
    }

    #[test]
    fn the_arena_is_unbounded_in_milestone_one() {
        let mut rt = runtime();
        let mut ctx = RuntimeContext::new(&mut rt);
        assert_eq!(ctx.heap_available(), usize::MAX);
        let mut x = [make_small(0)];
        let mut stack = [make_small(0)];
        assert!(ctx.ensure_heap(u32::MAX, 1, &mut x, &mut stack).is_ok());
        assert!(!ctx.alloc_words(4).expect("arena allocates").is_null());
    }

    #[test]
    fn the_registry_is_the_core_bif_set() {
        let rt = runtime();
        let core = BifRegistry::with_core_bifs();
        for (module, function, arity) in [
            ("erlang", "+", 2),
            ("lists", "reverse", 1),
            ("io", "format", 2),
        ] {
            assert!(rt.registry.contains(module, function, arity));
            // Value-for-value agreement with a freshly built core registry proves this
            // is the real `beam_bifs` one, not a hand-rolled subset.
            assert_eq!(
                rt.registry.lookup(module, function, arity).is_some(),
                core.lookup(module, function, arity).is_some()
            );
        }
        assert!(!rt.registry.contains("erlang", "nope", 0));
        assert!(!core.contains("erlang", "nope", 0));
    }
}

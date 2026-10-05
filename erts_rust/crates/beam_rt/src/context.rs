//! The concrete [`ExecutionContext`] glue: the engine's seam over the runtime core.
//!
//! The Stage-1 engine knows nothing about BIFs, heaps, processes or ETS — it calls
//! `call_bif`/`ensure_heap` on the trait and lets the host decide (see
//! `../beam_engine/src/context.rs`). This module is the host: it adapts
//! [`BifRegistry`], the process [`Heap`], the calling module's [`AtomTable`], the
//! calling process's [`ProcessId`], the [`TableRegistry`] (`ets:*`) and the output
//! sink (`io:format`) onto [`BifContext`].
//!
//! It is the **only** place the engine's seam meets the Stage-5 core; that keeps the
//! Stage-1/Stage-5 dependency DAG acyclic (`../CONTEXT.md`).
//!
//! # The atom-table bridge (design decision)
//!
//! [`BifContext::new`] takes *one* [`AtomTable`]. The engine resolves atoms — including
//! the ones inside `Module` code and the ones it compares — against the **loaded
//! module's** table (`beam_engine`'s `cmp(.., &module.atom_table)`). So this context
//! hands the BIFs the *module's* table, never the runtime's global one: an atom a BIF
//! builds is then an index the engine can compare and the printer can name.
//!
//! Consequence (documented, Milestone-1): a BIF that names an atom absent from the
//! current module's table returns [`BeamError::TermNotFound`] instead of interning it.
//! A later revision interns module atoms into the global table at load time.
//!
//! # No collector yet
//!
//! [`ensure_heap`](ExecutionContext::ensure_heap) checks capacity and reports
//! [`BeamError::SystemLimit`] when the heap is short — there is no GC in the engine yet
//! (`../beam_engine` Known Issues), so it cannot *make* room. Growth by collection is
//! Stage 4 (`beam_gc`).

use beam_alloc::Heap;
use beam_bifs::{BifContext, BifRegistry, OutputSink, TableRegistry};
use beam_common::{BeamError, Result};
use beam_engine::ExecutionContext;
use beam_process::ProcessId;
use beam_term::{AtomTable, Eterm};

/// The engine's host: everything a BIF call needs, borrowed for the call's lifetime.
pub struct RuntimeContext<'a> {
    /// The BIF registry (`beam_bifs`'s `bif.tab` surface).
    registry: &'a BifRegistry,
    /// The process heap the BIFs allocate on (and the engine checks capacity against).
    heap: &'a mut Heap,
    /// The **calling module's** atom table (see the module docs).
    atoms: &'a AtomTable,
    /// The calling process.
    self_pid: ProcessId,
    /// The runtime's ETS tables (`ets:*`); always installed by [`crate::runtime::Runtime`].
    tables: &'a mut TableRegistry,
    /// The `io:format` destination; stdout when unset.
    output: Option<&'a mut dyn OutputSink>,
}

impl<'a> RuntimeContext<'a> {
    /// A context over `registry`, `heap`, `atoms`, `self_pid` and `tables`, with no
    /// output sink installed (`io:format` then writes to stdout).
    pub fn new(
        registry: &'a BifRegistry,
        heap: &'a mut Heap,
        atoms: &'a AtomTable,
        self_pid: ProcessId,
        tables: &'a mut TableRegistry,
    ) -> Self {
        RuntimeContext {
            registry,
            heap,
            atoms,
            self_pid,
            tables,
            output: None,
        }
    }

    /// Installs the `io:format` output sink.
    pub fn with_output(mut self, sink: &'a mut dyn OutputSink) -> Self {
        self.output = Some(sink);
        self
    }

    /// Heap words still available to the running code.
    pub fn heap_available(&self) -> usize {
        self.heap.available()
    }
}

impl ExecutionContext for RuntimeContext<'_> {
    /// Dispatches `module:function/arity` through the BIF registry.
    ///
    /// The BIF sees the **calling module's** atom table, the process heap, the calling
    /// pid, the ETS registry and the output sink — everything
    /// [`BifContext`] exposes and nothing else.
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        let mut context = BifContext::new(self.heap, self.atoms, self.self_pid);
        if let Some(sink) = self.output.as_deref_mut() {
            context = context.with_output(sink);
        }
        context = context.with_tables(self.tables);
        self.registry.call(module, function, arity, &mut context, args)
    }

    /// Checks that `heap_need` words are available; there is no collector yet, so a
    /// short heap is a [`BeamError::SystemLimit`] rather than a collection.
    fn ensure_heap(
        &mut self,
        heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        if self.heap.available() >= heap_need as usize {
            Ok(())
        } else {
            Err(BeamError::SystemLimit)
        }
    }

    /// Heap words still available (`heap.available()`).
    fn heap_available(&self) -> usize {
        RuntimeContext::heap_available(self)
    }
}

#[cfg(test)]
mod tests {
    use beam_term::{is_small, make_atom, make_small, signed_value};

    use super::*;

    /// The `slice.beam` atom table (the atoms the engine resolves BIF names against).
    fn module_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["slice", "add", "erlang", "+", "fib", "-", "pick", "a"]);
        atoms
    }

    fn context<'a>(
        registry: &'a BifRegistry,
        heap: &'a mut Heap,
        atoms: &'a AtomTable,
        tables: &'a mut TableRegistry,
    ) -> RuntimeContext<'a> {
        RuntimeContext::new(registry, heap, atoms, ProcessId::new(1, 0), tables)
    }

    #[test]
    fn dispatches_an_erlang_arithmetic_bif() {
        let registry = BifRegistry::with_core_bifs();
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = module_atoms();
        let mut tables = TableRegistry::new();
        let mut ctx = context(&registry, &mut heap, &atoms, &mut tables);
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
    fn dispatches_a_bif_that_builds_an_atom_from_the_module_table() {
        let registry = BifRegistry::with_core_bifs();
        let mut heap = Heap::new(256).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["a", "erlang", "is_atom", "true", "false"]);
        let mut tables = TableRegistry::new();
        let mut ctx = context(&registry, &mut heap, &atoms, &mut tables);
        let is_atom = ctx
            .call_bif("erlang", "is_atom", 1, &[make_atom(atoms.lookup("a").expect("a"))])
            .expect("is_atom(a)");
        assert_eq!(atoms.name(beam_term::atom_val(is_atom)), Some("true"));
    }

    #[test]
    fn an_unknown_bif_is_an_error_not_a_panic() {
        let registry = BifRegistry::with_core_bifs();
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = module_atoms();
        let mut tables = TableRegistry::new();
        let mut ctx = context(&registry, &mut heap, &atoms, &mut tables);
        assert!(ctx.call_bif("nope", "nope", 0, &[]).is_err());
        assert!(ctx
            .call_bif("erlang", "+", 2, &[make_small(1)])
            .is_err());
        assert!(ctx
            .call_bif("erlang", "+", 2, &[make_atom(atoms.lookup("a").expect("a")), make_small(1)])
            .is_err());
    }

    #[test]
    fn heap_available_and_ensure_heap_track_the_heap() {
        let registry = BifRegistry::with_core_bifs();
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = module_atoms();
        let mut tables = TableRegistry::new();
        let mut ctx = context(&registry, &mut heap, &atoms, &mut tables);
        let available = ctx.heap_available();
        assert_eq!(available, 64);
        let mut x = [make_small(0)];
        let mut stack = [make_small(0)];
        assert!(ctx.ensure_heap(64, 1, &mut x, &mut stack).is_ok());
        assert!(matches!(
            ctx.ensure_heap(65, 1, &mut x, &mut stack),
            Err(BeamError::SystemLimit)
        ));
        // Inside an `ExecutionContext` call the trait method answers the same number.
        assert_eq!(ExecutionContext::heap_available(&ctx), available);
    }

    #[test]
    fn io_format_writes_to_the_installed_sink() {
        let registry = BifRegistry::with_core_bifs();
        let mut heap = Heap::new(256).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["a", "ok", "true", "false"]);

        let text = "hi ~w\n";
        let mut format = beam_term::NIL;
        for byte in text.bytes().rev() {
            format = heap
                .cons(make_small(i64::from(byte)), format)
                .expect("format fits");
        }
        let args = heap.cons(make_small(7), beam_term::NIL).expect("args fit");

        let mut tables = TableRegistry::new();
        let mut sink = beam_bifs::BytesSink::new();
        let result = {
            let mut ctx = context(&registry, &mut heap, &atoms, &mut tables)
                .with_output(&mut sink);
            ctx.call_bif("io", "format", 2, &[format, args])
                .expect("io:format/2")
        };
        assert_eq!(atoms.name(beam_term::atom_val(result)), Some("ok"));
        assert_eq!(sink.as_str(), "hi 7\n");
    }
}
